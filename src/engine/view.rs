//! Leituras de uma busca sobre um snapshot ([`View`]): nós da Trie, listas,
//! tamanhos dos documentos.

use std::cell::OnceCell;
use std::sync::Arc;

use rocksdb::DB;
use rustc_hash::FxHashMap;

use crate::{DocId, Engine, Error, IndexStats, Posting, Result, codec};
use crate::codec::{DeadBits, DocLens, NORM_BLOCK_DOCS, NormBlock, TrieNode};
use crate::engine::keys::{
    Key, META_DOC_COUNT, META_EPOCH, META_TOTAL_LEN, META_TOTAL_TITLE_LEN, NODE, NORM, POS, POST, ROOT_ID, VER,
};
use crate::engine::versions;
use crate::engine::versions::DeadSet;

type Snapshot<'a> = rocksdb::SnapshotWithThreadMode<'a, DB>;

/// Leituras de uma única busca, todas sobre o mesmo snapshot.
///
/// As listas trazem IDs internos (ver `versions`): a busca tira as versões
/// mortas que o snapshot vê ao decodificar, faz o E e o ranking com os IDs
/// internos e traduz para os do usuário no fim.
pub(super) struct View<'a> {
    pub(super) snap: Snapshot<'a>,
    /// Número do último commit visível quando o snapshot foi criado (o
    /// snapshot pode ser de um commit depois deste).
    pub(super) epoch: u64,
    /// O commit exato do snapshot, se `Engine::view` soube sem ler.
    exact: Option<u64>,
    /// Os mortos quando o snapshot foi criado (`None`: nenhum).
    deaths: Option<Arc<DeadSet>>,
    /// Os mortos que este snapshot vê, se forem só parte de `deaths`.
    visible: OnceCell<Option<DeadBits>>,
    pub(super) engine: &'a Engine,
}

impl<'a> View<'a> {
    pub(super) fn new(
        snap: Snapshot<'a>,
        epoch: u64,
        exact: Option<u64>,
        deaths: Option<Arc<DeadSet>>,
        engine: &'a Engine,
    ) -> Self {
        Self { snap, epoch, exact, deaths, visible: OnceCell::new(), engine }
    }
}

impl View<'_> {
    /// As versões mortas que este snapshot vê (vazio no caso comum). Uma
    /// morte de um commit depois do snapshot não conta: para ele, a versão
    /// ainda está viva.
    pub(super) fn dead(&self) -> Result<&DeadBits> {
        static NONE: DeadBits = DeadBits::empty();
        let Some(deaths) = &self.deaths else { return Ok(&NONE) };
        if self.visible.get().is_none() {
            let visible = if deaths.all_by(self.epoch) {
                None
            } else {
                let epoch = match self.exact {
                    Some(e) => e,
                    None => self.snap.get_bytes(META_EPOCH)?.map(|v| codec::get_varint(&v)).transpose()?.unwrap_or(0),
                };
                (!deaths.all_by(epoch)).then(|| deaths.visible(epoch))
            };
            let _ = self.visible.set(visible);
        }
        Ok(match self.visible.get().expect("acabou de ser preenchido") {
            Some(bits) => bits,
            None => deaths.bits(),
        })
    }

    /// Os documentos vivos de uma lista `post:` (IDs internos).
    pub(super) fn docs_of(&self, bytes: &[u8]) -> Result<Vec<DocId>> {
        codec::decode_docs_except(bytes, self.dead()?)
    }

    /// As entradas vivas de uma lista `post:` (IDs internos).
    pub(super) fn postings_of(&self, bytes: &[u8]) -> Result<Vec<Posting>> {
        codec::decode_postings_except(bytes, self.dead()?)
    }

    /// O ID do usuário de um ID interno.
    #[inline]
    pub(super) fn user_id(&self, id: DocId) -> Result<DocId> {
        Ok(versions::user_of(id))
    }

    /// IDs internos em ordem → IDs do usuário em ordem.
    pub(super) fn user_ids(&self, ids: Vec<DocId>) -> Result<Vec<DocId>> {
        Ok(versions::users_sorted(ids))
    }

    /// O ID interno da última versão do documento (viva ou não).
    pub(super) fn version_of(&self, doc: DocId) -> Result<u64> {
        Ok(match self.snap.get_bytes(Key::new(VER, doc).as_ref())? {
            Some(v) => codec::get_varint(&v)?,
            None => doc,
        })
    }

    /// Bytes da lista `post:` de um nó, presos ao snapshot (sem cópia).
    pub(super) fn post_bytes(&self, node: u64) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        self.snap.get_bytes(Key::new(POST, node).as_ref())
    }

    pub(super) fn stats(&self) -> Result<IndexStats> {
        let get = |key: &[u8]| -> Result<u64> {
            self.snap.get_bytes(key)?.map(|v| codec::get_varint(&v)).transpose().map(|v| v.unwrap_or(0))
        };
        Ok(IndexStats {
            doc_count: get(META_DOC_COUNT)?,
            total_len: get(META_TOTAL_LEN)?,
            total_title_len: get(META_TOTAL_TITLE_LEN)?,
        })
    }

    /// `term` já deve estar normalizado.
    pub(super) fn term_node(&self, term: &str) -> Result<Option<u64>> {
        Ok(match self.find_node(term)? {
            Some((id, node)) if node.is_end_of_word => Some(id),
            _ => None,
        })
    }

    /// `<prefixo><id>` para cada id, num `MultiGet` em lote sobre o snapshot,
    /// na ordem de `ids`.
    pub(super) fn get_many(&self, prefix: &[u8], ids: impl Iterator<Item = u64>) -> Result<Vec<Option<rocksdb::DBPinnableSlice<'_>>>> {
        let keys: Vec<Key> = ids.map(|id| Key::new(prefix, id)).collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        READS.with(|r| r.set(r.get() + keys.len() as u64));
        let db = &self.engine.db;
        let cf = db.cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME).ok_or_else(|| Error::Corrupt("família de colunas padrão ausente".into()))?;
        let mut opts = rocksdb::ReadOptions::default();
        opts.set_snapshot(&self.snap);
        db.batched_multi_get_cf_opt(cf, keys.iter().map(|k| k.as_ref()), false, &opts)
            .into_iter()
            .map(|r| r.map_err(Error::from))
            .collect()
    }

    /// Percorre a Trie caractere a caractere: um `Get` no RocksDB por nível,
    /// menos nos níveis guardados no cache de nós.
    pub(super) fn find_node(&self, term: &str) -> Result<Option<(u64, Arc<TrieNode>)>> {
        let Some(mut node) = self.node(ROOT_ID, 0)? else { return Ok(None) };
        let mut id = ROOT_ID;
        for (depth, ch) in term.chars().enumerate() {
            let Some(&child) = node.children.get(&ch) else { return Ok(None) };
            id = child;
            node = self.require_node(child, depth + 1)?;
        }
        Ok(Some((id, node)))
    }

    fn require_node(&self, id: u64, depth: usize) -> Result<Arc<TrieNode>> {
        self.node(id, depth)?.ok_or_else(|| missing_node(id))
    }

    /// Nó `id`, a `depth` passos da raiz. Os rasos vêm do cache compartilhado.
    fn node(&self, id: u64, depth: usize) -> Result<Option<Arc<TrieNode>>> {
        let cached = self.engine.config.reader_cache_depth.is_some_and(|max| depth <= max);
        if cached && let Some(node) = self.engine.nodes.get(id, self.epoch) {
            return Ok(Some(node));
        }
        let Some(node) = load_node(&self.snap, id)? else { return Ok(None) };
        let node = Arc::new(node);
        if cached {
            self.engine.nodes.offer(id, self.epoch, &node);
        }
        Ok(Some(node))
    }

    /// Os documentos do termo do nó, com os IDs do usuário, em ordem.
    pub(super) fn user_docs(&self, node_id: u64) -> Result<Vec<DocId>> {
        match self.post_bytes(node_id)? {
            Some(v) => self.user_ids(self.docs_of(&v)?),
            None => Ok(Vec::new()),
        }
    }

    /// Como [`View::user_docs`], com o tf.
    pub(super) fn user_postings(&self, node_id: u64) -> Result<Vec<Posting>> {
        let Some(v) = self.post_bytes(node_id)? else { return Ok(Vec::new()) };
        let mut list = self.postings_of(&v)?;
        if list.last().is_some_and(|p| p.doc >= versions::FIRST_VERSION) {
            for p in &mut list {
                p.doc = self.user_id(p.doc)?;
            }
            list.sort_unstable_by_key(|p| p.doc);
        }
        Ok(list)
    }

    /// As posições do termo do nó em cada documento, com os IDs do usuário.
    pub(super) fn user_positions(&self, node_id: u64) -> Result<Vec<(DocId, Vec<u32>)>> {
        let Some(v) = self.snap.get_bytes(Key::new(POS, node_id).as_ref())? else { return Ok(Vec::new()) };
        let mut dead = self.dead()?.cursor();
        let mut list = Vec::new();
        for (doc, positions) in codec::decode_positions(&v)? {
            if !dead.is_dead(doc) {
                list.push((self.user_id(doc)?, positions));
            }
        }
        list.sort_unstable_by_key(|e| e.0);
        Ok(list)
    }

    pub(super) fn norm_block(&self, block: u64) -> Result<Option<Arc<NormBlock>>> {
        if let Some(cached) = self.engine.norms.get(block, self.epoch) {
            return Ok(Some(cached));
        }
        let Some(bytes) = self.snap.get_bytes(Key::new(NORM, block).as_ref())? else { return Ok(None) };
        let decoded = Arc::new(NormBlock::decode(&bytes)?);
        self.engine.norms.offer(block, self.epoch, &decoded);
        Ok(Some(decoded))
    }
}

/// Tamanho dos documentos de uma busca, um bloco de `norm:` por vez. Rápido
/// com IDs crescentes, mas aceita qualquer ordem. Os blocos já usados ficam
/// num mapa local: voltar a um bloco não passa de novo pela trava do cache
/// compartilhado.
pub(super) struct NormCursor<'v, 'a> {
    view: &'v View<'a>,
    block_id: u64,
    block: Option<Arc<NormBlock>>,
    seen: FxHashMap<u64, Option<Arc<NormBlock>>>,
}

impl<'v, 'a> NormCursor<'v, 'a> {
    pub(super) fn new(view: &'v View<'a>) -> Self {
        Self { view, block_id: u64::MAX, block: None, seen: FxHashMap::default() }
    }

    #[inline]
    pub(super) fn lens(&mut self, doc: DocId) -> Result<DocLens> {
        let block_id = doc / NORM_BLOCK_DOCS;
        if block_id != self.block_id {
            self.block = match self.seen.get(&block_id) {
                Some(block) => block.clone(),
                None => {
                    let block = self.view.norm_block(block_id)?;
                    self.seen.insert(block_id, block.clone());
                    block
                }
            };
            self.block_id = block_id;
        }
        let offset = (doc % NORM_BLOCK_DOCS) as u16;
        self.block
            .as_ref()
            .and_then(|b| b.get_lens(offset))
            .ok_or_else(|| Error::Corrupt(format!("documento {doc} sem tamanho em norm:")))
    }
}

thread_local! {
    /// Leituras (`get`) feitas pelo motor nesta thread. Usado pelo harness
    /// para medir "Gets por consulta" sem depender do PerfContext do RocksDB.
    static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Total de `get`s que o motor fez na thread atual (só cresce).
pub fn reads_on_this_thread() -> u64 {
    READS.with(|r| r.get())
}

fn count_read() {
    READS.with(|r| r.set(r.get() + 1));
}

/// O banco direto (usado pelo lote, único escritor) ou um snapshot (buscas).
pub(super) trait Source {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<rocksdb::DBPinnableSlice<'_>>>;
}

impl Source for DB {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        count_read();
        Ok(self.get_pinned(key)?)
    }
}

impl Source for Snapshot<'_> {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        count_read();
        Ok(self.get_pinned(key)?)
    }
}

pub(super) fn load_node(src: &impl Source, id: u64) -> Result<Option<TrieNode>> {
    src.get_bytes(Key::new(NODE, id).as_ref())?.map(|v| TrieNode::decode(&v)).transpose()
}

pub(super) fn missing_node(id: u64) -> Error {
    Error::Corrupt(format!("nó {id} referenciado mas ausente"))
}
