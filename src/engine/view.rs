//! Leituras de uma busca sobre um snapshot ([`View`]): nós da Trie, listas,
//! tamanhos dos documentos.

use std::sync::Arc;

use rocksdb::DB;
use rustc_hash::FxHashMap;

use crate::{DocId, Engine, Error, IndexStats, Posting, Result, codec};
use crate::codec::{DocLens, NORM_BLOCK_DOCS, NormBlock, TrieNode};
use crate::codec::{OverlayPost, PosEdit};
use crate::engine::keys::{Key, META_DOC_COUNT, META_TOTAL_LEN, META_TOTAL_TITLE_LEN, NODE, NORM, POS, POST, POV, PSV, ROOT_ID};

type Snapshot<'a> = rocksdb::SnapshotWithThreadMode<'a, DB>;

/// Leituras de uma única busca, todas sobre o mesmo snapshot.
pub(super) struct View<'a> {
    pub(super) snap: Snapshot<'a>,
    /// Número do último commit visível quando o snapshot foi criado.
    pub(super) epoch: u64,
    pub(super) engine: &'a Engine,
}

/// Lista `post:` como a busca vê: a base (presa ao snapshot) e, se o termo
/// foi editado, o remendo já decodificado.
pub(super) struct PostSource<'a> {
    base: Option<rocksdb::DBPinnableSlice<'a>>,
    pub(super) edits: Vec<OverlayPost>,
    df_delta: i64,
}

impl PostSource<'_> {
    /// df com o remendo aplicado, sem percorrer as entradas.
    pub(super) fn len(&self) -> Result<usize> {
        Ok(codec::live_len(self.base.as_deref(), self.df_delta)? as usize)
    }

    pub(super) fn docs(&self) -> Result<Vec<DocId>> {
        codec::apply_docs(self.base.as_deref(), &self.edits)
    }

    pub(super) fn postings(&self) -> Result<Vec<Posting>> {
        codec::apply_postings(self.base.as_deref(), &self.edits)
    }

    /// Bytes da base. Só no caminho sem remendo (aí a base existe).
    pub(super) fn base(&self) -> &[u8] {
        self.base.as_deref().unwrap_or(&[])
    }
}

/// Lista `pos:` e o remendo de posições.
pub(super) struct PosSource<'a> {
    base: Option<rocksdb::DBPinnableSlice<'a>>,
    pub(super) edits: Vec<PosEdit>,
}

impl PosSource<'_> {
    /// Bytes da base, ou uma lista vazia se as posições estão só no remendo.
    pub(super) fn bytes(&self) -> &[u8] {
        self.base.as_deref().unwrap_or(&[0, 0])
    }
}

impl View<'_> {
    /// Lista `post:` do nó, com o remendo se este snapshot precisa dele.
    pub(super) fn post_source(&self, node: u64) -> Result<Option<PostSource<'_>>> {
        let base = self.snap.get_bytes(Key::new(POST, node).as_ref())?;
        if !self.engine.overlays.applies(node, self.epoch) {
            return Ok(base.map(|base| PostSource { base: Some(base), edits: Vec::new(), df_delta: 0 }));
        }
        let (df_delta, edits) = match self.snap.get_bytes(Key::new(POV, node).as_ref())? {
            Some(v) => codec::decode_post_edits(&v)?,
            None => (0, Vec::new()),
        };
        if base.is_none() && edits.is_empty() {
            return Ok(None);
        }
        Ok(Some(PostSource { base, edits, df_delta }))
    }

    /// Várias listas `post:`, na ordem de `ids`.
    pub(super) fn post_sources(&self, ids: impl Iterator<Item = u64>) -> Result<Vec<Option<PostSource<'_>>>> {
        let ids: Vec<u64> = ids.collect();
        let bases = self.get_many(POST, ids.iter().copied())?;
        let mut edits_of: FxHashMap<u64, (i64, Vec<OverlayPost>)> = FxHashMap::default();
        let need: Vec<u64> = ids.iter().copied().filter(|&id| self.engine.overlays.applies(id, self.epoch)).collect();
        if !need.is_empty() {
            for (&id, value) in need.iter().zip(self.get_many(POV, need.iter().copied())?) {
                if let Some(v) = value {
                    edits_of.insert(id, codec::decode_post_edits(&v)?);
                }
            }
        }
        Ok(ids
            .into_iter()
            .zip(bases)
            .map(|(id, base)| {
                let (df_delta, edits) = edits_of.remove(&id).unwrap_or((0, Vec::new()));
                if base.is_none() && edits.is_empty() {
                    None
                } else {
                    Some(PostSource { base, edits, df_delta })
                }
            })
            .collect())
    }

    /// Lista `pos:` do nó. Erro se o termo não tem posições neste snapshot.
    pub(super) fn pos_source(&self, node: u64) -> Result<PosSource<'_>> {
        let base = self.snap.get_bytes(Key::new(POS, node).as_ref())?;
        let edits = if self.engine.overlays.applies(node, self.epoch) {
            match self.snap.get_bytes(Key::new(PSV, node).as_ref())? {
                Some(v) => codec::decode_pos_edits(&v)?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if base.is_none() && edits.iter().all(|e| e.positions.is_none()) {
            return Err(Error::Corrupt(format!("termo do nó {node} sem posições")));
        }
        Ok(PosSource { base, edits })
    }

    /// Posições do termo, com o remendo. Vazio se não há lista.
    pub(super) fn positions_applied(&self, node: u64) -> Result<Vec<(DocId, Vec<u32>)>> {
        let base = self.snap.get_bytes(Key::new(POS, node).as_ref())?;
        let edits = if self.engine.overlays.applies(node, self.epoch) {
            match self.snap.get_bytes(Key::new(PSV, node).as_ref())? {
                Some(v) => codec::decode_pos_edits(&v)?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if base.is_none() && edits.is_empty() {
            return Ok(Vec::new());
        }
        codec::apply_positions(base.as_deref(), &edits)
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

    pub(super) fn load_docs(&self, node_id: u64) -> Result<Vec<DocId>> {
        match self.post_source(node_id)? {
            Some(src) => src.docs(),
            None => Ok(Vec::new()),
        }
    }

    pub(super) fn load_postings(&self, node_id: u64) -> Result<Vec<Posting>> {
        match self.post_source(node_id)? {
            Some(src) => src.postings(),
            None => Ok(Vec::new()),
        }
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
