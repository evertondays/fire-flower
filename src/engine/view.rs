//! Leituras de uma busca sobre um snapshot ([`View`]): nós da Trie, listas,
//! tamanhos dos documentos.

use crate::Result;
use crate::engine::keys::NODE;
use crate::codec::NORM_BLOCK_DOCS;
use crate::codec::DocLens;
use rustc_hash::FxHashMap;
use crate::engine::keys::NORM;
use crate::codec::NormBlock;
use crate::Posting;
use crate::DocId;
use crate::engine::keys::ROOT_ID;
use crate::codec::TrieNode;
use std::sync::Arc;
use crate::Error;
use crate::engine::keys::META_TOTAL_TITLE_LEN;
use crate::engine::keys::META_TOTAL_LEN;
use crate::engine::keys::META_DOC_COUNT;
use crate::codec;
use crate::IndexStats;
use crate::engine::keys::POST;
use crate::engine::keys::Key;
use crate::Engine;
use rocksdb::DB;

pub(crate) type Snapshot<'a> = rocksdb::SnapshotWithThreadMode<'a, DB>;

/// Leituras de uma única busca, todas sobre o mesmo snapshot.
pub(crate) struct View<'a> {
    pub(crate) snap: Snapshot<'a>,
    /// Número do último commit visível quando o snapshot foi criado.
    pub(crate) epoch: u64,
    pub(crate) engine: &'a Engine,
}

impl View<'_> {
    /// Bytes da lista `post:` de um nó, presos ao snapshot (sem cópia).
    pub(crate) fn post_bytes(&self, node: u64) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        self.snap.get_bytes(Key::new(POST, node).as_ref())
    }

    pub(crate) fn stats(&self) -> Result<IndexStats> {
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
    pub(crate) fn term_node(&self, term: &str) -> Result<Option<u64>> {
        Ok(match self.find_node(term)? {
            Some((id, node)) if node.is_end_of_word => Some(id),
            _ => None,
        })
    }

    /// `<prefixo><id>` para cada id, num `MultiGet` em lote sobre o snapshot,
    /// na ordem de `ids`.
    pub(crate) fn get_many(&self, prefix: &[u8], ids: impl Iterator<Item = u64>) -> Result<Vec<Option<rocksdb::DBPinnableSlice<'_>>>> {
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
    pub(crate) fn find_node(&self, term: &str) -> Result<Option<(u64, Arc<TrieNode>)>> {
        let Some(mut node) = self.node(ROOT_ID, 0)? else { return Ok(None) };
        let mut id = ROOT_ID;
        for (depth, ch) in term.chars().enumerate() {
            let Some(&child) = node.children.get(&ch) else { return Ok(None) };
            id = child;
            node = self.require_node(child, depth + 1)?;
        }
        Ok(Some((id, node)))
    }

    pub(crate) fn require_node(&self, id: u64, depth: usize) -> Result<Arc<TrieNode>> {
        self.node(id, depth)?.ok_or_else(|| missing_node(id))
    }

    /// Nó `id`, a `depth` passos da raiz. Os rasos vêm do cache compartilhado.
    pub(crate) fn node(&self, id: u64, depth: usize) -> Result<Option<Arc<TrieNode>>> {
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

    pub(crate) fn load_docs(&self, node_id: u64) -> Result<Vec<DocId>> {
        match self.snap.get_bytes(Key::new(POST, node_id).as_ref())? {
            Some(v) => codec::decode_docs(&v),
            None => Ok(Vec::new()),
        }
    }

    pub(crate) fn load_postings(&self, node_id: u64) -> Result<Vec<Posting>> {
        match self.snap.get_bytes(Key::new(POST, node_id).as_ref())? {
            Some(v) => codec::decode_postings(&v),
            None => Ok(Vec::new()),
        }
    }

    pub(crate) fn norm_block(&self, block: u64) -> Result<Option<Arc<NormBlock>>> {
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
pub(crate) struct NormCursor<'v, 'a> {
    pub(crate) view: &'v View<'a>,
    pub(crate) block_id: u64,
    pub(crate) block: Option<Arc<NormBlock>>,
    pub(crate) seen: FxHashMap<u64, Option<Arc<NormBlock>>>,
}

impl<'v, 'a> NormCursor<'v, 'a> {
    pub(crate) fn new(view: &'v View<'a>) -> Self {
        Self { view, block_id: u64::MAX, block: None, seen: FxHashMap::default() }
    }

    #[inline]
    pub(crate) fn lens(&mut self, doc: DocId) -> Result<DocLens> {
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
    pub(crate) static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Total de `get`s que o motor fez na thread atual (só cresce).
pub fn reads_on_this_thread() -> u64 {
    READS.with(|r| r.get())
}

pub(crate) fn count_read() {
    READS.with(|r| r.set(r.get() + 1));
}

/// O banco direto (usado pelo lote, único escritor) ou um snapshot (buscas).
pub(crate) trait Source {
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

pub(crate) fn load_node(src: &impl Source, id: u64) -> Result<Option<TrieNode>> {
    src.get_bytes(Key::new(NODE, id).as_ref())?.map(|v| TrieNode::decode(&v)).transpose()
}

pub(crate) fn missing_node(id: u64) -> Error {
    Error::Corrupt(format!("nó {id} referenciado mas ausente"))
}
