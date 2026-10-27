//! Indexação: o lote ([`IndexBatch`]) e o commit.

use std::sync::MutexGuard;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Instant;

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{DocId, Engine, IndexStats, Result, codec};
use crate::analysis::Analyzer;
use crate::codec::{DocLens, NORM_BLOCK_DOCS, NormBlock};
use crate::engine::reads_on_this_thread;
use crate::engine::keys::{
    DOC, Key, META_DOC_COUNT, META_NEXT_DOC, META_NEXT_NODE, META_TOTAL_LEN, META_TOTAL_TITLE_LEN,
    NODE, NORM, POS, POST, stem_key,
};
use crate::engine::meta::varint;
use crate::engine::view::Source;
use crate::engine::writer::trie::{CachedNode, TriePath, evict};

pub(crate) mod trie;


pub(crate) struct WriterState {
    pub(crate) next_node_id: u64,
    pub(crate) next_doc_id: DocId,
    pub(crate) stats: IndexStats,
    /// Nós da Trie dos lotes anteriores. Só o escritor altera a Trie, então
    /// depois de um commit o cache é igual ao banco. O lote o pega emprestado
    /// e só o devolve no commit: um lote descartado leva o cache junto, e
    /// nada que não foi gravado sobrevive.
    pub(crate) cache: NodeCache,
    /// Buffers da análise e radicais já calculados, entre um lote e outro.
    pub(crate) analyzer: Analyzer,
}

#[derive(Default)]
pub(crate) struct NodeCache {
    pub(crate) nodes: FxHashMap<u64, CachedNode>,
    /// Número do último lote gravado. Cada nó guarda o lote que o usou por último.
    pub(crate) generation: u64,
}

/// Lote de indexação. Acumula as mudanças em memória e grava tudo de uma vez,
/// atomicamente, em [`IndexBatch::commit`]. Se for descartado sem commit,
/// nada é gravado. Buscas concorrentes nunca veem um lote pela metade.
///
/// O lote é invertido: `add_document` só junta termo → documentos (com tf e
/// posições), e a Trie é percorrida no commit, uma vez por termo distinto do
/// lote (e não uma vez por documento que contém o termo).
pub struct IndexBatch<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) state: MutexGuard<'a, WriterState>,
    pub(crate) next_node_id: u64,
    pub(crate) next_doc_id: DocId,
    /// Cache de nós emprestado do `WriterState`, mais os nós lidos ou criados
    /// neste lote. Nós criados aqui ainda não existem no banco, então as
    /// travessias seguintes precisam enxergá-los por este mapa.
    pub(crate) nodes: FxHashMap<u64, CachedNode>,
    pub(crate) generation: u64,
    pub(crate) cache_limit: usize,
    /// Nós alterados neste lote, na ordem em que mudaram.
    pub(crate) dirty: Vec<u64>,
    /// Termo -> documentos deste lote que o contêm.
    pub(crate) pending: FxHashMap<String, TermAcc>,
    /// (documento, tamanho inteiro, tamanho do título, extensão do título),
    /// na ordem em que foram adicionados.
    pub(crate) lengths: Vec<(DocId, u32, u32, u32)>,
    pub(crate) labels: Vec<(DocId, String)>,
    pub(crate) analyzer: Analyzer,
}

/// O que o lote juntou de um termo.
#[derive(Default)]
pub(crate) struct TermAcc {
    /// (documento, tf, tf no título), na ordem em que os documentos foram adicionados.
    pub(crate) docs: Vec<(DocId, u32, u32)>,
    /// `tf` posições por entrada de `docs`, em sequência (vazio sem posições).
    pub(crate) positions: Vec<u32>,
}

impl TermAcc {
    /// Ordena por documento. Para o mesmo documento adicionado duas vezes no
    /// lote, a última vence (como entre lotes, no merge operator).
    pub(crate) fn normalize(&mut self, positions: bool) {
        if self.docs.windows(2).all(|w| w[0].0 < w[1].0) {
            return; // o caso comum: IDs crescentes
        }
        let mut entries: Vec<(DocId, u32, u32, usize)> = Vec::with_capacity(self.docs.len());
        let mut start = 0usize;
        for &(doc, tf, title_tf) in &self.docs {
            entries.push((doc, tf, title_tf, start));
            start += tf as usize;
        }
        entries.sort_by_key(|e| e.0); // estável: a ordem de adição se mantém
        let mut kept: Vec<(DocId, u32, u32, usize)> = Vec::with_capacity(entries.len());
        for e in entries {
            match kept.last_mut() {
                Some(last) if last.0 == e.0 => *last = e,
                _ => kept.push(e),
            }
        }
        if positions {
            let mut sorted = Vec::with_capacity(self.positions.len());
            for &(_, tf, _, start) in &kept {
                sorted.extend_from_slice(&self.positions[start..start + tf as usize]);
            }
            self.positions = sorted;
        }
        self.docs = kept.into_iter().map(|(doc, tf, title_tf, _)| (doc, tf, title_tf)).collect();
    }
}

/// Onde um commit gastou o tempo ([`IndexBatch::commit_with_stats`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct CommitStats {
    pub docs: usize,
    /// Termos distintos do lote.
    pub terms: usize,
    /// Leituras no RocksDB durante a travessia (nós fora do cache do escritor).
    pub node_reads: u64,
    /// Nós criados e nós regravados neste lote.
    pub new_nodes: usize,
    pub dirty_nodes: usize,
    /// Nós no cache do escritor depois do descarte.
    pub cache_nodes: usize,
    /// Tamanho do `WriteBatch` gravado.
    pub batch_bytes: usize,
    /// Ordenar os termos do lote.
    pub sort_secs: f64,
    /// Percorrer e criar caminhos na Trie (inclui ler os nós fora do cache).
    pub trie_secs: f64,
    /// Ordenar e codificar as listas `post:`/`pos:` e colocá-las no lote.
    pub encode_secs: f64,
    /// Codificar os nós alterados, os blocos de `norm:` e os contadores.
    pub nodes_secs: f64,
    /// `db.write` do lote inteiro.
    pub write_secs: f64,
    /// Descarte do cache de nós.
    pub evict_secs: f64,
    pub total_secs: f64,
}

impl IndexBatch<'_> {
    /// Indexa `text` sob um ID escolhido por você, sem título. Reindexar um
    /// ID troca o tf, as posições e o tamanho dele (termos que saíram do
    /// texto ficam).
    pub fn add_document(&mut self, doc_id: DocId, text: &str) -> Result<()> {
        self.add_titled(doc_id, "", text)
    }

    /// Indexa um documento com título. As palavras do título são buscáveis
    /// como as do texto, e o ranking as conta à parte (`Posting::title_tf`),
    /// para pesar o título mais que o resto.
    pub fn add_titled(&mut self, doc_id: DocId, title: &str, text: &str) -> Result<()> {
        let positions = self.engine.positions;
        let (len, title_len) = self.analyzer.run_fields(title, text, &self.engine.stopwords, &self.engine.stemmer);
        let analyzer = &self.analyzer;
        let record = |acc: &mut TermAcc, found: &[u32]| {
            acc.docs.push((doc_id, found.len() as u32, analyzer.title_tf(found)));
            if positions {
                acc.positions.extend_from_slice(found);
            }
        };
        for (term, found) in analyzer.terms() {
            // Só aloca a `String` do termo na primeira vez que ele aparece no lote.
            match self.pending.get_mut(term) {
                Some(acc) => record(acc, found),
                None => record(self.pending.entry(term.to_string()).or_default(), found),
            }
        }
        self.lengths.push((doc_id, len, title_len, analyzer.title_span()));
        // IDs automáticos de `add_labeled` nunca colidem com IDs manuais.
        self.next_doc_id = self.next_doc_id.max(doc_id.saturating_add(1));
        Ok(())
    }

    /// Indexa `text` com um ID novo e grava `label` (ex.: caminho do arquivo).
    pub fn add_labeled(&mut self, label: &str, text: &str) -> Result<DocId> {
        self.add_labeled_titled(label, "", text)
    }

    /// Como [`IndexBatch::add_labeled`], com título (ex.: o nome do arquivo).
    pub fn add_labeled_titled(&mut self, label: &str, title: &str, text: &str) -> Result<DocId> {
        let doc_id = self.next_doc_id;
        self.add_titled(doc_id, title, text)?;
        self.labels.push((doc_id, label.to_string()));
        Ok(doc_id)
    }

    /// Grava (ou troca) o rótulo de um documento junto com o commit do lote.
    pub fn set_label(&mut self, doc_id: DocId, label: &str) {
        self.labels.push((doc_id, label.to_string()));
    }

    /// Documentos adicionados desde o início do lote. Use para decidir quando
    /// fazer commit: o lote guarda em memória os termos de todos eles.
    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// Grava o lote num único `WriteBatch` atômico.
    pub fn commit(self) -> Result<()> {
        self.commit_with_stats().map(|_| ())
    }

    /// Como [`IndexBatch::commit`], devolvendo onde o commit gastou o tempo.
    /// Usado pelo harness de performance para achar o gargalo da carga.
    pub fn commit_with_stats(mut self) -> Result<CommitStats> {
        let started = Instant::now();
        let reads_before = reads_on_this_thread();
        let mut stats = CommitStats { docs: self.lengths.len(), ..CommitStats::default() };
        let positions = self.engine.positions;
        // Termos em ordem: a criação de nós fica determinística (o mesmo
        // corpus com o mesmo tamanho de lote gera sempre os mesmos IDs).
        let mut terms: Vec<(String, TermAcc)> = std::mem::take(&mut self.pending).into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        stats.terms = terms.len();
        stats.sort_secs = started.elapsed().as_secs_f64();

        let mut wb = WriteBatch::default();
        let mut buf = Vec::new();
        let mut trie = std::time::Duration::ZERO;
        let mut path = TriePath::default();
        let encode_started = Instant::now();
        // `merge` em vez de ler + reescrever a lista: o RocksDB une os
        // fragmentos na leitura e na compactação (ver `merge_operator`).
        // Palavras novas por radical, para o `stem:` (só com flexões na consulta).
        let inflector = &self.engine.inflector;
        let mut new_by_stem: FxHashMap<String, Vec<u64>> = FxHashMap::default();
        for (term, mut acc) in terms {
            let t = Instant::now();
            let (id, new_word) = self.insert_term(&term, &mut path)?;
            trie += t.elapsed();
            if new_word && inflector.is_on() {
                new_by_stem.entry(inflector.stem(&term)).or_default().push(id);
            }
            acc.normalize(positions);
            buf.clear();
            codec::encode_postings_into(&mut buf, &acc.docs);
            wb.merge(Key::new(POST, id), &buf);
            if positions {
                buf.clear();
                codec::encode_positions_into(&mut buf, &acc.docs, &acc.positions);
                wb.merge(Key::new(POS, id), &buf);
            }
        }
        for (stem, mut ids) in new_by_stem {
            ids.sort_unstable();
            wb.merge(stem_key(&stem), codec::encode_node_set(&ids));
        }
        stats.trie_secs = trie.as_secs_f64();
        stats.encode_secs = encode_started.elapsed().as_secs_f64() - stats.trie_secs;
        stats.node_reads = reads_on_this_thread() - reads_before;
        stats.new_nodes = (self.next_node_id - self.state.next_node_id) as usize;
        stats.dirty_nodes = self.dirty.len();
        let t = Instant::now();
        for id in &self.dirty {
            wb.put(Key::new(NODE, *id), self.nodes[id].node.encode());
        }
        let (index, blocks) = self.write_lengths(&mut wb)?;
        let stats_index = index;
        for (doc_id, label) in &self.labels {
            wb.put(Key::new(DOC, *doc_id), label.as_bytes());
        }
        wb.put(META_NEXT_NODE, varint(self.next_node_id));
        wb.put(META_NEXT_DOC, varint(self.next_doc_id));
        wb.put(META_DOC_COUNT, varint(stats_index.doc_count));
        wb.put(META_TOTAL_LEN, varint(stats_index.total_len));
        wb.put(META_TOTAL_TITLE_LEN, varint(stats_index.total_title_len));
        stats.nodes_secs = t.elapsed().as_secs_f64();
        stats.batch_bytes = wb.size_in_bytes();

        // Ver `EpochCache`: marca, grava, publica.
        let engine = self.engine;
        let next = engine.epoch.load(AtomicOrdering::Acquire) + 1;
        engine.norms.mark(blocks, next);
        if let Some(max) = engine.config.reader_cache_depth {
            let shallow = self.dirty.iter().copied().filter(|id| usize::from(self.nodes[id].depth) <= max);
            engine.nodes.mark(shallow, next);
        }
        let t = Instant::now();
        engine.db.write(wb)?;
        stats.write_secs = t.elapsed().as_secs_f64();
        engine.epoch.store(next, AtomicOrdering::Release);

        // Gravado: o cache volta a ser igual ao banco e fica para o próximo lote.
        for id in std::mem::take(&mut self.dirty) {
            if let Some(cached) = self.nodes.get_mut(&id) {
                cached.dirty = false;
            }
        }
        let t = Instant::now();
        let mut nodes = std::mem::take(&mut self.nodes);
        evict(&mut nodes, self.generation, self.cache_limit);
        stats.evict_secs = t.elapsed().as_secs_f64();
        stats.cache_nodes = nodes.len();
        self.state.cache = NodeCache { nodes, generation: self.generation };
        self.state.analyzer = std::mem::take(&mut self.analyzer);
        self.state.next_node_id = self.next_node_id;
        self.state.next_doc_id = self.next_doc_id;
        self.state.stats = stats_index;
        stats.total_secs = started.elapsed().as_secs_f64();
        Ok(stats)
    }

    /// Atualiza os blocos de `norm:` tocados pelo lote e as estatísticas.
    /// Um documento que já existia troca de tamanho e não conta de novo.
    /// Devolve as estatísticas novas e os blocos gravados.
    pub(crate) fn write_lengths(&self, wb: &mut WriteBatch) -> Result<(IndexStats, Vec<u64>)> {
        let mut stats = self.state.stats;
        let mut order: Vec<(u64, usize)> =
            self.lengths.iter().enumerate().map(|(i, &(doc, ..))| (doc / NORM_BLOCK_DOCS, i)).collect();
        // Estável por bloco: dentro do bloco, a ordem de adição (a última vence).
        if !order.windows(2).all(|w| w[0].0 <= w[1].0) {
            order.sort_by_key(|e| e.0);
        }
        let mut blocks = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let block_id = order[i].0;
            let mut block = match self.engine.db.get_bytes(Key::new(NORM, block_id).as_ref())? {
                Some(v) => NormBlock::decode(&v)?,
                None => NormBlock::default(),
            };
            while i < order.len() && order[i].0 == block_id {
                let (doc, len, title_len, title_span) = self.lengths[order[i].1];
                // Título com mais de 65.535 tokens: a contagem para no máximo.
                let small = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                let (title_len, title_span) = (small(title_len), small(title_span));
                match block.set((doc % NORM_BLOCK_DOCS) as u16, DocLens { len, title_len, title_span }) {
                    Some(old) => {
                        stats.total_len = (stats.total_len + u64::from(len)).saturating_sub(u64::from(old.len));
                        stats.total_title_len =
                            (stats.total_title_len + u64::from(title_len)).saturating_sub(u64::from(old.title_len));
                    }
                    None => {
                        stats.doc_count += 1;
                        stats.total_len += u64::from(len);
                        stats.total_title_len += u64::from(title_len);
                    }
                }
                i += 1;
            }
            wb.put(Key::new(NORM, block_id), block.encode());
            blocks.push(block_id);
        }
        Ok((stats, blocks))
    }
}
