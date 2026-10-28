//! Indexação: o lote ([`IndexBatch`]) e o commit.

use std::sync::MutexGuard;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Instant;

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{DocId, Engine, Error, IndexStats, Result, codec};
use crate::analysis::Analyzer;
use crate::engine::reads_on_this_thread;
use crate::engine::keys::{
    Key, META_DOC_COUNT, META_NEXT_DOC, META_NEXT_NODE, META_TOTAL_LEN, META_TOTAL_TITLE_LEN, NODE, POS, POST,
    stem_key,
};
use crate::engine::meta::varint;
use crate::engine::versions::MAX_DOC;
use crate::engine::writer::documents::Norms;
use crate::engine::writer::trie::{CachedNode, TriePath, evict};

mod documents;
pub(super) mod trie;

pub(crate) struct WriterState {
    pub(super) next_node_id: u64,
    pub(super) next_doc_id: DocId,
    pub(super) stats: IndexStats,
    /// Nós da Trie dos lotes anteriores. Só o escritor altera a Trie, então
    /// depois de um commit o cache é igual ao banco. O lote o pega emprestado
    /// e só o devolve no commit: um lote descartado leva o cache junto, e
    /// nada que não foi gravado sobrevive.
    pub(super) cache: NodeCache,
    /// Buffers da análise e radicais já calculados, entre um lote e outro.
    pub(super) analyzer: Analyzer,
    /// Documento → ID interno da última versão, para os que já mudaram
    /// (`ver:`; ver `versions`).
    pub(super) latest: FxHashMap<DocId, u64>,
}

#[derive(Default)]
pub(super) struct NodeCache {
    pub(super) nodes: FxHashMap<u64, CachedNode>,
    /// Número do último lote gravado. Cada nó guarda o lote que o usou por último.
    pub(super) generation: u64,
}

/// Lote de indexação. Acumula as mudanças em memória e grava tudo de uma vez,
/// atomicamente, em [`IndexBatch::commit`]. Se for descartado sem commit,
/// nada é gravado. Buscas concorrentes nunca veem um lote pela metade.
///
/// O lote é invertido: `add_document` só junta termo → documentos (com tf e
/// posições), e a Trie é percorrida no commit, uma vez por termo distinto do
/// lote (e não uma vez por documento que contém o termo).
pub struct IndexBatch<'a> {
    pub(super) engine: &'a Engine,
    pub(super) state: MutexGuard<'a, WriterState>,
    pub(super) next_node_id: u64,
    pub(super) next_doc_id: DocId,
    /// Cache de nós emprestado do `WriterState`, mais os nós lidos ou criados
    /// neste lote. Nós criados aqui ainda não existem no banco, então as
    /// travessias seguintes precisam enxergá-los por este mapa.
    pub(super) nodes: FxHashMap<u64, CachedNode>,
    pub(super) generation: u64,
    pub(super) cache_limit: usize,
    /// Nós alterados neste lote, na ordem em que mudaram.
    pub(super) dirty: Vec<u64>,
    /// Termo -> documentos deste lote que o contêm.
    pub(super) pending: FxHashMap<String, TermAcc>,
    /// Cada `add_*` e `delete_document`, na ordem em que foram chamados.
    pub(super) ops: Vec<DocOp>,
    /// Documento -> a última operação dele no lote (índice em `ops`): é a que vale.
    pub(super) last_op: FxHashMap<DocId, u32>,
    /// Algum documento apareceu em mais de uma operação do lote.
    pub(super) repeated: bool,
    pub(super) labels: Vec<(DocId, String)>,
    pub(super) analyzer: Analyzer,
}

/// Uma operação do lote sobre um documento.
#[derive(Debug, Clone, Copy)]
pub(super) struct DocOp {
    pub(super) doc: DocId,
    /// Tamanhos (documento inteiro, título, extensão do título); `None` é remoção.
    pub(super) lens: Option<(u32, u32, u32)>,
}

/// O que o lote juntou de um termo.
#[derive(Default)]
pub(super) struct TermAcc {
    /// As entradas, na ordem em que os documentos foram adicionados.
    docs: Vec<BatchEntry>,
    /// `tf` posições por entrada de `docs`, em sequência (vazio sem posições).
    positions: Vec<u32>,
}

/// Um documento na lista de um termo do lote. A operação vai junto na mesma
/// entrada: num `Vec` à parte, era uma alocação a mais por termo distinto do
/// lote, e a carga da Wikipédia ficava ~5% mais lenta.
#[derive(Debug, Clone, Copy)]
struct BatchEntry {
    doc: DocId,
    tf: u32,
    title_tf: u32,
    /// Índice em `IndexBatch::ops`.
    op: u32,
}

impl codec::PostingEntry for BatchEntry {
    #[inline]
    fn doc(&self) -> DocId {
        self.doc
    }

    #[inline]
    fn tf(&self) -> u32 {
        self.tf
    }

    #[inline]
    fn title_tf(&self) -> u32 {
        self.title_tf
    }
}

impl TermAcc {
    /// Ordena por documento. Se algum documento apareceu em mais de uma
    /// operação do lote (`repeated`), fica só com as entradas da última
    /// (`is_final(op)`): reindexar duas vezes no mesmo lote vale o segundo
    /// texto inteiro, e adicionar e remover não grava nada.
    fn normalize(&mut self, positions: bool, repeated: bool, is_final: impl Fn(u32) -> bool) {
        if !repeated && self.docs.windows(2).all(|w| w[0].doc < w[1].doc) {
            return; // o caso comum: IDs crescentes
        }
        // (entrada, onde começam as posições dela)
        let mut entries: Vec<(BatchEntry, usize)> = Vec::with_capacity(self.docs.len());
        let mut start = 0usize;
        for &e in &self.docs {
            if !repeated || is_final(e.op) {
                entries.push((e, start));
            }
            start += e.tf as usize;
        }
        // Sobra no máximo uma entrada por documento: a da operação que vale.
        entries.sort_unstable_by_key(|e| e.0.doc);
        if positions {
            let mut sorted = Vec::with_capacity(self.positions.len());
            for &(e, start) in &entries {
                sorted.extend_from_slice(&self.positions[start..start + e.tf as usize]);
            }
            self.positions = sorted;
        }
        self.docs = entries.into_iter().map(|(e, _)| e).collect();
    }
}

/// Onde um commit gastou o tempo ([`IndexBatch::commit_with_stats`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct CommitStats {
    /// Documentos adicionados ou substituídos.
    pub docs: usize,
    /// Documentos que já existiam e foram substituídos.
    pub replaced: usize,
    /// Documentos removidos (os que existiam).
    pub deleted: usize,
    /// Versões novas com ID da faixa alta (substituições e readições).
    pub versions: usize,
    /// Versões que morreram (ficam nas listas até a compactação).
    pub killed: usize,
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
    /// Ler e gravar os blocos de `norm:`, as versões e os mortos.
    pub documents_secs: f64,
    /// `db.write` do lote inteiro.
    pub write_secs: f64,
    /// Descarte do cache de nós.
    pub evict_secs: f64,
    pub total_secs: f64,
}

impl IndexBatch<'_> {
    /// Tira o fim de palavra do nó `id` (a limpeza de palavras vazias) e
    /// devolve o nó codificado, para quem chama gravar.
    pub(super) fn clear_end_of_word(&mut self, id: u64) -> Result<Vec<u8>> {
        // A profundidade só importa para o descarte do cache; um nó que ainda
        // não está nele entra como fundo.
        let cached = self.node_mut(id, u8::MAX)?;
        cached.node.is_end_of_word = false;
        Ok(cached.node.encode())
    }

    /// Encerra o lote sem commit, mas devolve o cache de nós ao escritor:
    /// quem chama já gravou as mudanças que fez nos nós (a limpeza de
    /// palavras vazias), então o cache continua igual ao banco.
    pub(super) fn keep_cache(mut self) {
        debug_assert!(self.dirty.is_empty() && self.ops.is_empty());
        self.state.cache = NodeCache { nodes: std::mem::take(&mut self.nodes), generation: self.generation };
        self.state.analyzer = std::mem::take(&mut self.analyzer);
    }

    /// Indexa `text` sob um ID escolhido por você, sem título. Um ID que já
    /// existe é substituído (ver [`IndexBatch::add_titled`]).
    pub fn add_document(&mut self, doc_id: DocId, text: &str) -> Result<()> {
        self.add_titled(doc_id, "", text)
    }

    /// Indexa um documento com título. As palavras do título são buscáveis
    /// como as do texto, e o ranking as conta à parte (`Posting::title_tf`),
    /// para pesar o título mais que o resto.
    ///
    /// Um ID que já existe é **substituído**: os termos que saíram do texto
    /// deixam de achar o documento no mesmo commit. No mesmo lote, a última
    /// operação de cada documento vence. IDs vão até 2^40 − 1 (ver `versions`).
    pub fn add_titled(&mut self, doc_id: DocId, title: &str, text: &str) -> Result<()> {
        let positions = self.engine.positions;
        let op = self.push_op(doc_id)?;
        let (len, title_len) = self.analyzer.run_fields(title, text, &self.engine.stopwords, &self.engine.stemmer);
        let analyzer = &self.analyzer;
        let record = |acc: &mut TermAcc, found: &[u32]| {
            acc.docs.push(BatchEntry { doc: doc_id, tf: found.len() as u32, title_tf: analyzer.title_tf(found), op });
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
        self.ops.push(DocOp { doc: doc_id, lens: Some((len, title_len, analyzer.title_span())) });
        // IDs automáticos de `add_labeled` nunca colidem com IDs manuais.
        self.next_doc_id = self.next_doc_id.max(doc_id.saturating_add(1));
        Ok(())
    }

    /// Remove o documento no commit: ele some de todos os termos, dos tamanhos,
    /// das estatísticas e do rótulo. Um ID que não existe não faz nada.
    pub fn delete_document(&mut self, doc_id: DocId) -> Result<()> {
        self.push_op(doc_id)?;
        self.ops.push(DocOp { doc: doc_id, lens: None });
        Ok(())
    }

    /// Anota que `doc_id` tem mais uma operação no lote e devolve o índice dela.
    fn push_op(&mut self, doc_id: DocId) -> Result<u32> {
        if doc_id > MAX_DOC {
            return Err(Error::Config(format!("ID de documento {doc_id} grande demais: vão até 2^40 − 1")));
        }
        let op = u32::try_from(self.ops.len())
            .map_err(|_| Error::Config("lote com mais de 4 bilhões de operações: faça commit antes".into()))?;
        if self.last_op.insert(doc_id, op).is_some() {
            self.repeated = true;
        }
        Ok(op)
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
    /// Se a última operação do documento no lote for uma remoção, o rótulo
    /// sai junto com ele.
    pub fn set_label(&mut self, doc_id: DocId, label: &str) {
        self.labels.push((doc_id, label.to_string()));
    }

    /// Operações (documentos adicionados e removidos) desde o início do lote.
    /// Use para decidir quando fazer commit: o lote guarda em memória os
    /// termos de todos eles.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Grava o lote num único `WriteBatch` atômico.
    pub fn commit(self) -> Result<()> {
        self.commit_with_stats().map(|_| ())
    }

    /// Como [`IndexBatch::commit`], devolvendo onde o commit gastou o tempo.
    /// Usado pelo harness de performance para achar o gargalo da carga.
    pub fn commit_with_stats(mut self) -> Result<CommitStats> {
        self.engine.require_writable()?;
        let started = Instant::now();
        let mut stats = CommitStats::default();
        let positions = self.engine.positions;
        // A última operação de cada documento é a que vale.
        let finals: Vec<usize> = if self.repeated {
            (0..self.ops.len()).filter(|&i| self.last_op[&self.ops[i].doc] as usize == i).collect()
        } else {
            (0..self.ops.len()).collect()
        };
        stats.docs = finals.iter().filter(|&&i| self.ops[i].lens.is_some()).count();
        // Antes da Trie: a versão de cada documento (pelos blocos de `norm:`).
        let t = Instant::now();
        let mut norms = Norms::new();
        let versions = self.plan_versions(&finals, &mut norms)?;
        let mut documents_secs = t.elapsed();
        let reads_before = reads_on_this_thread();

        // Termos em ordem: a criação de nós fica determinística (o mesmo
        // corpus com o mesmo tamanho de lote gera sempre os mesmos IDs).
        let sort_started = Instant::now();
        let mut terms: Vec<(String, TermAcc)> = std::mem::take(&mut self.pending).into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        stats.terms = terms.len();
        stats.sort_secs = sort_started.elapsed().as_secs_f64();

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
            if versions.remapped {
                // As listas guardam o ID interno da versão nova.
                for e in acc.docs.iter_mut() {
                    e.doc = versions.internal[e.op as usize];
                }
            }
            let (ops, last_op) = (&self.ops, &self.last_op);
            acc.normalize(positions, self.repeated, |op| last_op[&ops[op as usize].doc] == op);
            if acc.docs.is_empty() {
                continue; // todos os documentos deste termo foram removidos ou reindexados no lote
            }
            let t = Instant::now();
            let (id, new_word) = self.insert_term(&term, &mut path)?;
            trie += t.elapsed();
            if new_word && inflector.is_on() {
                new_by_stem.entry(inflector.stem(&term)).or_default().push(id);
            }
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
        self.write_versions(&mut wb, &versions, &mut stats);
        let (index, blocks) = self.write_lengths(&mut wb, &finals, &versions, &mut norms, &mut stats)?;
        self.write_labels(&mut wb, &finals);
        documents_secs += t.elapsed();
        stats.documents_secs = documents_secs.as_secs_f64();

        let t = Instant::now();
        for id in &self.dirty {
            wb.put(Key::new(NODE, *id), self.nodes[id].node.encode());
        }
        wb.put(META_NEXT_NODE, varint(self.next_node_id));
        wb.put(META_NEXT_DOC, varint(self.next_doc_id));
        wb.put(META_DOC_COUNT, varint(index.doc_count));
        wb.put(META_TOTAL_LEN, varint(index.total_len));
        wb.put(META_TOTAL_TITLE_LEN, varint(index.total_title_len));
        stats.nodes_secs = t.elapsed().as_secs_f64();
        stats.batch_bytes = wb.size_in_bytes();

        // Ver `EpochCache` e `versions`: marca e publica o que muda, grava, publica o commit.
        let engine = self.engine;
        let next = engine.epoch() + 1;
        engine.norms.mark(blocks, next);
        if let Some(max) = engine.config.reader_cache_depth {
            let shallow = self.dirty.iter().copied().filter(|id| usize::from(self.nodes[id].depth) <= max);
            engine.nodes.mark(shallow, next);
        }
        engine.deaths.add(&versions.kills, next);
        let t = Instant::now();
        if let Err(e) = engine.write_commit(wb, next) {
            engine.deaths.rollback(next);
            return Err(e);
        }
        stats.write_secs = t.elapsed().as_secs_f64();
        if !versions.kills.is_empty() {
            // O commit inteiro tem números de sequência até este: um snapshot
            // vê as mortes dele se e só se o número dele chega aqui.
            let seq = engine.db.latest_sequence_number();
            engine.dead.lock().unwrap_or_else(|e| e.into_inner()).extend(versions.kills.iter().map(|&id| (seq, id)));
        }
        engine.pending_edits.fetch_add((stats.replaced + stats.deleted) as u64, AtomicOrdering::AcqRel);
        self.state.latest.extend(versions.changed.iter().copied());

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
        self.state.stats = index;
        stats.total_secs = started.elapsed().as_secs_f64();
        Ok(stats)
    }
}
