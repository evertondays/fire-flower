//! Camadas de índice (Trie) e armazenamento (RocksDB).
//!
//! Espaços de chave (formato v7; a codificação está em `src/codec/`):
//! - `node:<id>`    -> [`TrieNode`] serializado (`node:0` é a raiz)
//! - `post:<id>`    -> (documento, tf, tf no título) do termo que termina no nó `<id>`
//! - `pos:<id>`     -> posições do termo em cada documento (só com posições);
//!   o título vem primeiro, e o texto começa `TITLE_GAP` posições depois
//! - `norm:<bloco>` -> tamanho de cada documento (e do título) com `id ÷ 1024 = bloco`
//! - `doc:<id>`     -> rótulo opcional do documento (ex.: caminho do arquivo)
//! - `ver:<id>`     -> ID interno da última versão de um documento que já mudou
//! - `ext:<id>`     -> ID externo do documento (a chave do usuário; ver `external`)
//! - `xid:<ID externo>` -> o documento dono do ID externo
//! - `stem:<radical>` -> nós das palavras com esse radical (só com flexões na consulta)
//! - `meta:*`       -> formato, contadores de ID, estatísticas, idiomas, stemming e mortos
//!
//! Em `post:`, `pos:` e `norm:` o documento é o ID interno da versão (ver
//! `versions`); a API recebe e devolve o ID do usuário.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

use rocksdb::{DB, Options, WriteBatch};
use rustc_hash::FxHashMap;

use crate::{DocId, Error, Hit, IndexStats, Posting, Result, TopHits};
use crate::analysis::normalize_term;
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::{Language, Stopwords};
use crate::codec::{NORM_BLOCK_DOCS, NormBlock, TrieNode};
use crate::engine::cache::EpochCache;
use crate::engine::keys::{DOC, Key, META_EPOCH};
use crate::engine::meta::varint;
use crate::engine::metrics::{Metrics, Op};
use crate::engine::metrics::sampler::Sampler;
use crate::engine::purge::Purge;
use crate::engine::versions::Deaths;
use crate::engine::view::View;
use crate::engine::writer::{EngineRef, ExternalIds, WriterSlot, WriterState};
use crate::query::{Clause, parse_query_stemmed};

mod cache;
mod config;
mod conjunction;
mod diagnostics;
mod empty_words;
mod external;
mod maintenance;
mod groups;
mod keys;
mod meta;
pub(crate) mod metrics;
mod open;
mod phrase;
mod prefix;
mod purge;
mod ranking;
mod reader;
mod search;
mod versions;
mod view;
mod writer;
#[cfg(test)]
mod tests;


pub use self::config::{Config, InflectionMode};
pub use self::empty_words::CompactStats;
pub use self::external::MAX_EXTERNAL_ID_BYTES;
pub use self::maintenance::{Maintenance, MaintenanceOptions};
pub use self::conjunction::intersect;
pub use self::meta::FORMAT_VERSION;
pub use self::reader::Reader;
pub use self::view::reads_on_this_thread;
pub use self::writer::{CommitStats, IndexBatch, OwnedBatch};

/// Motor de busca. É `Sync`: compartilhe via `Arc<Engine>`. Buscas rodam em
/// paralelo entre si e com a indexação; só um [`IndexBatch`] existe por vez.
pub struct Engine {
    db: Arc<DB>,
    writer: WriterSlot,
    config: Config,
    stopwords: Stopwords,
    /// Stemming gravado no banco (`meta:stemming`).
    stemmer: Stemmer,
    /// Stemmer das flexões na consulta (`meta:inflections`).
    inflector: Stemmer,
    /// O banco grava posições (`meta:positions`).
    positions: bool,
    /// `commit << 1 | gravando`: o número do último commit gravado (ver
    /// [`EpochCache`]) e se um commit está sendo gravado agora (ver
    /// [`Engine::view`]).
    clock: AtomicU64,
    norms: EpochCache<NormBlock>,
    /// Nós rasos da Trie (até `Config::reader_cache_depth`).
    nodes: EpochCache<TrieNode>,
    /// Versões mortas que as buscas filtram.
    deaths: Deaths,
    /// Os mortos que a compactação em andamento tira das listas.
    purge: Arc<Purge>,
    /// Mortos ainda nas listas, em ordem de commit: (número de sequência do
    /// RocksDB do commit que os matou, ID interno). O escritor acrescenta; a
    /// compactação tira os que limpou.
    dead: Mutex<Vec<(u64, u64)>>,
    /// Uma compactação por vez.
    compaction: Mutex<()>,
    /// Documentos substituídos ou removidos desde a última compactação.
    pending_edits: AtomicU64,
    /// Guardadas para ler as estatísticas depois do `open`.
    opts: Options,
    /// Aberto com [`Engine::open_read_only`]: lotes e compactação dão erro.
    read_only: bool,
    /// Com `Config::metrics`.
    metrics: Option<Arc<Metrics>>,
    /// A thread que grava o snapshot das métricas (ver `Config::metrics_file`).
    sampler: Option<Sampler>,
    /// A pasta do banco e quando ele foi aberto (Unix, s), para as métricas.
    path: PathBuf,
    opened_unix: f64,
}

impl Engine {
    /// Os contadores das métricas agora, se `Config::metrics` estiver
    /// ligado (sem as propriedades do RocksDB, que a amostradora lê). Para
    /// expor as métricas no próprio processo; o `fire-flower monitor` lê o
    /// arquivo que a amostradora grava.
    pub fn metrics_snapshot(&self) -> Option<metrics::snapshot::Snapshot> {
        Some(self.metrics()?.snapshot(&self.path.display().to_string(), self.opened_unix))
    }

    /// As métricas, se ligadas. Sem a feature `metrics` é sempre `None`, e o
    /// compilador tira os ganchos.
    #[inline(always)]
    fn metrics(&self) -> Option<&Metrics> {
        if cfg!(feature = "metrics") { self.metrics.as_deref() } else { None }
    }

    /// Roda uma consulta da API e a conta em `op`.
    #[inline]
    fn measured<T>(&self, op: Op, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(m) = self.metrics() else { return f() };
        let started = Instant::now();
        let reads = reads_on_this_thread();
        let result = f();
        m.record_query(op, started.elapsed(), reads_on_this_thread() - reads, result.is_err());
        result
    }

    /// Aberto com [`Engine::open_read_only`].
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Erro para quem tenta gravar num banco aberto só para leitura.
    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        Ok(())
    }

    /// Inicia um lote de indexação. Bloqueia se outro lote estiver aberto
    /// (inclusive na mesma thread: não chame `index_document` com um lote aberto).
    pub fn batch(&self) -> IndexBatch<'_> {
        let state = self.wait_for_writer();
        self.start_batch(EngineRef::Borrowed(self), state)
    }

    /// Como [`Engine::batch`], sem esperar: [`Error::Busy`] se outro lote
    /// estiver aberto. Para quem não pode bloquear (o event loop do Node, um
    /// request do PHP).
    pub fn try_batch(&self) -> Result<IndexBatch<'_>> {
        let state = self.writer.try_take().ok_or(Error::Busy)?;
        Ok(self.start_batch(EngineRef::Borrowed(self), state))
    }

    /// Como [`Engine::batch`], com um lote dono de um `Arc` do motor
    /// ([`OwnedBatch`]): pode ser guardado num objeto de outra linguagem e
    /// fazer commit em outra thread.
    pub fn owned_batch(self: &Arc<Self>) -> OwnedBatch {
        let state = self.wait_for_writer();
        self.start_batch(EngineRef::Shared(Arc::clone(self)), state)
    }

    /// Como [`Engine::owned_batch`], sem esperar ([`Error::Busy`]).
    pub fn try_owned_batch(self: &Arc<Self>) -> Result<OwnedBatch> {
        let state = self.writer.try_take().ok_or(Error::Busy)?;
        Ok(self.start_batch(EngineRef::Shared(Arc::clone(self)), state))
    }

    fn wait_for_writer(&self) -> WriterState {
        let waiting = self.metrics().map(|_| Instant::now());
        let state = self.writer.take();
        if let (Some(m), Some(t)) = (self.metrics(), waiting) {
            m.record_lock_wait(t.elapsed());
        }
        state
    }

    fn start_batch<'a>(&self, engine: EngineRef<'a>, mut state: WriterState) -> IndexBatch<'a> {
        let cache = std::mem::take(&mut state.cache);
        let analyzer = std::mem::take(&mut state.analyzer);
        IndexBatch {
            engine,
            next_node_id: state.next_node_id,
            next_doc_id: state.next_doc_id,
            state,
            nodes: cache.nodes,
            generation: cache.generation + 1,
            cache_limit: self.config.writer_cache_nodes,
            dirty: Vec::new(),
            pending: FxHashMap::default(),
            ops: Vec::new(),
            last_op: FxHashMap::default(),
            repeated: false,
            labels: Vec::new(),
            external: ExternalIds::default(),
            analyzer,
        }
    }

    /// Atalho para indexar um único documento. Para muitos documentos, use [`Engine::batch`].
    pub fn index_document(&self, doc_id: DocId, text: &str) -> Result<()> {
        let mut batch = self.batch();
        batch.add_document(doc_id, text)?;
        batch.commit()
    }

    /// Atalho para remover um único documento ([`IndexBatch::delete_document`]).
    pub fn delete_document(&self, doc_id: DocId) -> Result<()> {
        let mut batch = self.batch();
        batch.delete_document(doc_id)?;
        batch.commit()
    }

    /// O documento está indexado (adicionado e não removido).
    pub fn contains(&self, doc_id: DocId) -> Result<bool> {
        Ok(self.doc_len(doc_id)?.is_some())
    }

    /// Busca full-text. Termos separados por espaço são combinados com E
    /// (o documento precisa conter todos). `termo*` casa por prefixo e
    /// `"uma frase"` exige os termos em sequência.
    /// Retorna os IDs dos documentos em ordem crescente.
    pub fn search(&self, query: &str) -> Result<Vec<DocId>> {
        self.measured(Op::Search, || self.view().search(query))
    }

    /// Os `k` documentos mais relevantes para a consulta (BM25), da maior
    /// nota para a menor; no empate, o menor ID primeiro. A consulta tem a
    /// mesma sintaxe e os mesmos resultados de [`Engine::search`].
    pub fn search_top(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        self.measured(Op::SearchTop, || Ok(self.view().search_ranked(query, k)?.hits))
    }

    /// Como [`Engine::search_top`], mais o total de documentos que casaram.
    pub fn search_ranked(&self, query: &str, k: usize) -> Result<TopHits> {
        self.measured(Op::SearchRanked, || self.view().search_ranked(query, k))
    }

    /// Documentos que contêm exatamente `term` (normalizado aqui e, com
    /// stemming, reduzido ao radical, como numa busca).
    pub fn postings(&self, term: &str) -> Result<Vec<DocId>> {
        let Some(term) = self.index_term(term) else { return Ok(Vec::new()) };
        self.measured(Op::Postings, || {
            let view = self.view();
            match view.term_node(&term)? {
                Some(node) => view.user_docs(node),
                None => Ok(Vec::new()),
            }
        })
    }

    /// Como [`Engine::postings`], com a frequência do termo em cada documento.
    pub fn term_postings(&self, term: &str) -> Result<Vec<Posting>> {
        let Some(term) = self.index_term(term) else { return Ok(Vec::new()) };
        self.measured(Op::TermPostings, || {
            let view = self.view();
            match view.term_node(&term)? {
                Some(node) => view.user_postings(node),
                None => Ok(Vec::new()),
            }
        })
    }

    /// O termo do índice para uma palavra: normalizada e, com stemming,
    /// reduzida ao radical. `None` se não houver nada indexável.
    pub fn index_term(&self, word: &str) -> Option<String> {
        let term = normalize_term(word)?;
        Some(if self.stemmer.is_on() { self.stemmer.stem(&term) } else { term })
    }

    /// Separa a consulta em partes como a busca faz (stopwords e stemming do
    /// banco); ver [`crate::query::parse_query`].
    pub fn parse_query(&self, query: &str) -> Vec<Clause> {
        parse_query_stemmed(query, &self.stopwords, &self.stemmer)
    }

    /// Autocompletar: até `limit` palavras indexadas que começam com `prefix`,
    /// das mais curtas para as mais longas e em ordem alfabética. Com
    /// stemming, as palavras indexadas são radicais.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let Some(prefix) = normalize_term(prefix) else { return Ok(Vec::new()) };
        self.measured(Op::Suggest, || {
            let words = self.view().words_with_prefix(&prefix, limit)?;
            Ok(words.into_iter().map(|(word, _)| word).collect())
        })
    }

    /// Compacta o banco inteiro: une os pedaços pendentes das listas, tira
    /// delas as versões mortas dos documentos substituídos e removidos, e
    /// tira da Trie as palavras que ficaram sem documento. Num índice grande
    /// leva minutos; buscas e lotes continuam durante a compactação. No fim,
    /// pega a trava do escritor por um instante: espera um lote aberto (não
    /// chame com um lote aberto na mesma thread).
    pub fn compact(&self) -> Result<CompactStats> {
        self.require_writable()?;
        let _one = self.compaction.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        // As edições que chegarem durante a compactação podem ficar fora dela.
        let before = self.pending_edits.load(AtomicOrdering::Acquire);
        let (purge, cut) = self.purgeable()?;
        // Com o filtro de compactação instalado, o padrão do RocksDB regrava o
        // último nível mais uma vez. Sem mortos não há o que filtrar: pula,
        // como sem filtro. Com mortos, regrava só os arquivos que já estavam lá.
        let mut opts = rocksdb::CompactOptions::default();
        if purge.is_empty() {
            opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Skip);
        } else {
            self.purge.start(purge.clone());
            opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::ForceOptimized);
        }
        self.db.compact_range_opt::<&[u8], &[u8]>(None, None, &opts);
        let stats = if purge.is_empty() { Ok(CompactStats::default()) } else { self.finish_purge(&purge, cut) };
        let pending = self.pending_edits.fetch_sub(before, AtomicOrdering::AcqRel) - before;
        if let Some(m) = self.metrics() {
            let done = stats.as_ref().map_or(CompactStats::default(), |s| *s);
            m.record_compaction(started.elapsed(), done.purged, done.emptied_words);
            m.set_edit_backlog(self.dead_versions() as u64, pending);
        }
        stats
    }

    /// Documentos substituídos ou removidos desde a última compactação (ou
    /// desde o `open`): até a compactação, as listas deles têm pedaços que
    /// toda leitura une de novo. Ver [`Maintenance`].
    pub fn pending_edits(&self) -> u64 {
        self.pending_edits.load(AtomicOrdering::Acquire)
    }

    /// Rótulo gravado com [`IndexBatch::add_labeled`] ou [`IndexBatch::set_label`].
    pub fn label(&self, doc_id: DocId) -> Result<Option<String>> {
        Ok(self
            .db
            .get_pinned(Key::new(DOC, doc_id))?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Os rótulos de vários documentos, na ordem de `docs` (`None` para quem
    /// não tem), num único `MultiGet`: mostrar os 10 primeiros resultados de
    /// uma busca custa uma leitura em lote, não 10.
    pub fn labels(&self, docs: &[DocId]) -> Result<Vec<Option<String>>> {
        self.multi_get_strings(DOC, docs)
    }

    /// Percorre os rótulos de todos os documentos (`f(documento, rótulo)`),
    /// em ordem de chave. Lê só as chaves `doc:`: serve para achar um
    /// documento pelo rótulo (o `index --sync` da CLI acha assim os arquivos
    /// já indexados).
    pub fn for_each_label(&self, mut f: impl FnMut(DocId, &str)) -> Result<()> {
        let mut opts = rocksdb::ReadOptions::default();
        opts.set_iterate_upper_bound(b"doc;".to_vec()); // ';' vem logo depois de ':'
        let mode = rocksdb::IteratorMode::From(DOC, rocksdb::Direction::Forward);
        for item in self.db.iterator_opt(mode, opts) {
            let (key, value) = item?;
            let doc = key
                .strip_prefix(DOC)
                .and_then(|d| std::str::from_utf8(d).ok())
                .and_then(|d| d.parse().ok())
                .ok_or_else(|| Error::Corrupt(format!("chave de rótulo inválida: {}", String::from_utf8_lossy(&key))))?;
            f(doc, &String::from_utf8_lossy(&value));
        }
        Ok(())
    }

    /// Tamanho do documento (tokens indexados, sem stopwords), se ele existe.
    pub fn doc_len(&self, doc_id: DocId) -> Result<Option<u32>> {
        let view = self.view();
        let id = view.version_of(doc_id)?;
        let block = view.norm_block(id / NORM_BLOCK_DOCS)?;
        Ok(block.and_then(|b| b.get((id % NORM_BLOCK_DOCS) as u16)))
    }

    /// Total de documentos e soma dos tamanhos.
    pub fn stats(&self) -> Result<IndexStats> {
        self.view().stats()
    }

    /// O banco grava posições (busca por frase disponível).
    pub fn has_positions(&self) -> bool {
        self.positions
    }

    /// Uma busca faz várias leituras (um `Get` por caractere, mais as listas).
    /// O snapshot garante que todas vejam o mesmo momento do índice, mesmo que
    /// um commit aconteça no meio.
    ///
    /// O relógio é lido antes do snapshot (ver `EpochCache`) e depois. Se não
    /// mudou e nenhum commit estava sendo gravado, o snapshot é exatamente o
    /// daquele commit: é o que diz quais mortos a busca filtra (ver
    /// `versions`). Senão, a busca lê `meta:epoch` do snapshot quando precisar.
    fn view(&self) -> View<'_> {
        let before = self.clock.load(AtomicOrdering::SeqCst);
        let snap = self.db.snapshot();
        let after = self.clock.load(AtomicOrdering::SeqCst);
        // Depois do snapshot: um commit que mata publica os mortos antes de gravar.
        let deaths = self.deaths.current();
        let exact = (before == after && before & 1 == 0).then_some(before >> 1);
        View::new(snap, before >> 1, exact, deaths, self)
    }

    /// Número do último commit gravado.
    fn epoch(&self) -> u64 {
        self.clock.load(AtomicOrdering::Acquire) >> 1
    }

    /// Grava `wb` como o commit `next` (o seguinte ao último). Quem chama já
    /// marcou nos caches o que muda (ver `EpochCache`) e publicou os mortos.
    /// Só o escritor chama (com a trava dele), um commit por vez.
    fn write_commit(&self, mut wb: WriteBatch, next: u64) -> Result<()> {
        wb.put(META_EPOCH, varint(next));
        self.clock.store(((next - 1) << 1) | 1, AtomicOrdering::SeqCst);
        let written = self.db.write(wb);
        let done = if written.is_ok() { next } else { next - 1 };
        self.clock.store(done << 1, AtomicOrdering::SeqCst);
        Ok(written?)
    }

    /// Stopwords deste banco (os idiomas gravados nele).
    pub fn stopwords(&self) -> &Stopwords {
        &self.stopwords
    }

    /// Idiomas das stopwords deste banco.
    pub fn languages(&self) -> &[Language] {
        self.stopwords.languages()
    }

    /// Stemming deste banco (o gravado nele).
    pub fn stemmer(&self) -> &Stemmer {
        &self.stemmer
    }

    /// Stemmer das flexões na consulta deste banco (o gravado nele).
    pub fn inflector(&self) -> &Stemmer {
        &self.inflector
    }
}

impl Drop for Engine {
    /// A amostradora para antes do banco fechar (e grava um último snapshot).
    fn drop(&mut self) {
        drop(self.sampler.take());
    }
}
