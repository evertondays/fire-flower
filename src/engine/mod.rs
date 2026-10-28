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
//! - `stem:<radical>` -> nós das palavras com esse radical (só com flexões na consulta)
//! - `meta:*`       -> formato, contadores de ID, estatísticas, idiomas, stemming e mortos
//!
//! Em `post:`, `pos:` e `norm:` o documento é o ID interno da versão (ver
//! `versions`); a API recebe e devolve o ID do usuário.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rocksdb::{BlockBasedIndexType, BlockBasedOptions, Cache, DB, DBCompressionType, Options, WriteBatch};
use rustc_hash::FxHashMap;

use crate::{DocId, Error, Hit, IndexStats, Posting, Result, TopHits};
use crate::analysis::{Analyzer, normalize_term};
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::{Language, Stopwords};
use crate::codec::{NORM_BLOCK_DOCS, NormBlock, TrieNode};
use crate::engine::cache::EpochCache;
use crate::engine::keys::{
    DOC, Key, MERGE_OPERATOR, META_DOC_COUNT, META_EPOCH, META_INFLECTIONS, META_NEXT_DOC, META_NEXT_NODE,
    META_STEMMING, META_TOTAL_LEN, META_TOTAL_TITLE_LEN, ROOT_ID,
};
use crate::engine::metrics::{Metrics, Op};
use crate::engine::meta::{
    previous_merge_operator, read_counter, resolve_format, resolve_languages, resolve_stemming, stored_format_error,
    varint,
};
use crate::engine::purge::{FilterFactory, Purge};
use crate::engine::versions::Deaths;
use crate::engine::view::View;
use crate::engine::writer::{NodeCache, WriterState};
use crate::query::{Clause, parse_query_stemmed};

mod cache;
mod config;
mod conjunction;
mod diagnostics;
mod empty_words;
mod maintenance;
mod groups;
mod keys;
mod meta;
pub(crate) mod metrics;
mod phrase;
mod prefix;
mod purge;
mod ranking;
mod search;
mod versions;
mod view;
mod writer;
#[cfg(test)]
mod tests;


pub use self::config::{Config, InflectionMode};
pub use self::empty_words::CompactStats;
pub use self::maintenance::{Maintenance, MaintenanceOptions};
pub use self::conjunction::intersect;
pub use self::meta::FORMAT_VERSION;
pub use self::view::reads_on_this_thread;
pub use self::writer::{CommitStats, IndexBatch};

/// Motor de busca. É `Sync`: compartilhe via `Arc<Engine>`. Buscas rodam em
/// paralelo entre si e com a indexação; só um [`IndexBatch`] existe por vez.
pub struct Engine {
    db: DB,
    writer: Mutex<WriterState>,
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
    /// A pasta do banco e quando ele foi aberto (Unix, s), para as métricas.
    path: PathBuf,
    opened_unix: f64,
}

impl Engine {
    /// Abre (ou cria, com [`Config::create_if_missing`]) o banco para leitura
    /// e escrita. Só um processo por vez abre um banco assim: o RocksDB trava
    /// a pasta (`LOCK`).
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        Self::open_with(path.as_ref(), config, false)
    }

    /// Abre um banco que já existe só para leitura, ao lado do processo que o
    /// grava (que continua com o `LOCK`). Não cria nem altera arquivos e não
    /// roda compactação. A visão é a do momento da abertura: o que for gravado
    /// depois só aparece abrindo de novo. Lotes e [`Engine::compact`] dão erro.
    pub fn open_read_only(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        Self::open_with(path.as_ref(), config, true)
    }

    fn open_with(path: &Path, config: Config, read_only: bool) -> Result<Self> {
        config.validate()?;
        // Antes do RocksDB: ele cria a pasta mesmo sem `create_if_missing`.
        if (read_only || !config.create_if_missing) && !path.join("CURRENT").is_file() {
            return Err(not_a_database(path));
        }
        let mut table = BlockBasedOptions::default();
        table.set_bloom_filter(config.bloom_bits_per_key, false);
        table.set_block_cache(&Cache::new_lru_cache(config.block_cache_bytes));
        table.set_cache_index_and_filter_blocks(true);
        table.set_pin_l0_filter_and_index_blocks_in_cache(true);
        // Filtro e índice em partições de 4 KB, com um índice de topo fixo no
        // cache. O cache LRU se divide em até 64 partes (4 MB com 256 MB), e um
        // bloco maior que uma parte não fica nele. Com filtro inteiro, um SST de
        // chaves `node:` gerado pela compactação passa de 6 MB de filtro na
        // Wikipédia inteira: cada `Get` relia o filtro todo e a carga caía de
        // ~5.000 para ~200 artigos/s (teste `bloom_filters_larger_than_...`).
        if config.partition_filters {
            table.set_partition_filters(true);
            table.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
            table.set_metadata_block_size(4096);
            table.set_pin_top_level_index_and_filter(true);
        }

        let purge = Arc::new(Purge::default());
        let mut opts = Options::default();
        opts.create_if_missing(config.create_if_missing && !read_only);
        opts.set_block_based_table_factory(&table);
        opts.set_compression_type(DBCompressionType::Lz4);
        let (full, partial) = (Arc::clone(&purge), Arc::clone(&purge));
        opts.set_merge_operator(
            MERGE_OPERATOR,
            move |key: &[u8], existing: Option<&[u8]>, operands: &rocksdb::MergeOperands| {
                purge::merge(&full, key, existing, operands)
            },
            move |key: &[u8], existing: Option<&[u8]>, operands: &rocksdb::MergeOperands| {
                purge::merge(&partial, key, existing, operands)
            },
        );
        if !read_only {
            // Só leitura: sem compactação nem threads extras do RocksDB.
            opts.set_compaction_filter_factory(FilterFactory { purge: Arc::clone(&purge) });
            let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
            opts.increase_parallelism(cpus as i32);
        }
        if config.statistics {
            opts.enable_statistics();
            opts.set_statistics_level(rocksdb::statistics::StatsLevel::ExceptTimers);
        }

        // Antes de abrir para escrita: a recuperação do WAL de um banco de
        // outro formato rodaria o merge operator novo sobre listas antigas.
        if let Some(name) = previous_merge_operator(path)
            && name != MERGE_OPERATOR
        {
            return Err(stored_format_error(path).unwrap_or_else(|| {
                Error::Config(format!("banco de outro formato (merge operator {name}); recarregue o banco"))
            }));
        }
        // Com a família padrão explícita, `cf_handle("default")` existe (o
        // `MultiGet` em lote do crate precisa dela). Para o RocksDB é o mesmo
        // banco; o descritor leva as mesmas opções (merge operator, cache…).
        let default_cf = rocksdb::ColumnFamilyDescriptor::new(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, opts.clone());
        let opened = if read_only {
            DB::open_cf_descriptors_read_only(&opts, path, [default_cf], false)
        } else {
            DB::open_cf_descriptors(&opts, path, [default_cf])
        };
        let db = match opened {
            Ok(db) => db,
            // Um banco antigo com merges pendentes no WAL falha já na
            // recuperação, porque o merge operator não entende o formato antigo.
            Err(e) => return Err(stored_format_error(path).unwrap_or_else(|| open_error(path, e))),
        };
        let writable = !read_only;
        let positions = resolve_format(&db, config.positions, writable)?;
        let next_node_id = read_counter(&db, META_NEXT_NODE)?.unwrap_or(ROOT_ID + 1);
        let next_doc_id = read_counter(&db, META_NEXT_DOC)?.unwrap_or(1);
        let stats = IndexStats {
            doc_count: read_counter(&db, META_DOC_COUNT)?.unwrap_or(0),
            total_len: read_counter(&db, META_TOTAL_LEN)?.unwrap_or(0),
            total_title_len: read_counter(&db, META_TOTAL_TITLE_LEN)?.unwrap_or(0),
        };
        let stopwords = Stopwords::new(&resolve_languages(&db, config.languages.as_deref(), writable)?);
        let stemmer = Stemmer::new(resolve_stemming(&db, META_STEMMING, config.stemming, writable)?);
        let inflector = Stemmer::new(resolve_stemming(&db, META_INFLECTIONS, config.inflections, writable)?);
        if stemmer.is_on() && inflector.is_on() {
            return Err(Error::Config(format!(
                "stemming ({}) e flexões na consulta ({}) não combinam: escolha um",
                stemmer.stemming(),
                inflector.stemming()
            )));
        }
        let deaths = Deaths::default();
        let loaded = versions::load(&db, &deaths)?;
        let metrics = if config.metrics {
            if !cfg!(feature = "metrics") {
                return Err(Error::Config("Config::metrics pede a feature `metrics`, desligada nesta compilação".into()));
            }
            let m = Metrics::new();
            m.set_documents(stats.doc_count);
            m.set_edit_backlog(loaded.dead.len() as u64, 0);
            Some(Arc::new(m))
        } else {
            None
        };
        // Os números de commit continuam os do último `open`: `meta:epoch`
        // gravado por um commit tem que bater com o relógio (ver `view`).
        let epoch = read_counter(&db, META_EPOCH)?.unwrap_or(0);
        let writer = Mutex::new(WriterState {
            next_node_id,
            next_doc_id,
            stats,
            cache: NodeCache::default(),
            analyzer: Analyzer::default(),
            latest: loaded.latest,
        });
        Ok(Self {
            db,
            writer,
            config,
            stopwords,
            stemmer,
            inflector,
            positions,
            clock: AtomicU64::new(epoch << 1),
            norms: EpochCache::default(),
            nodes: EpochCache::default(),
            deaths,
            purge,
            // Mortos de antes do `open`: todo snapshot de agora já os vê.
            dead: Mutex::new(loaded.dead.into_iter().map(|id| (0, id)).collect()),
            compaction: Mutex::new(()),
            pending_edits: AtomicU64::new(0),
            opts,
            read_only,
            metrics,
            path: path.to_path_buf(),
            opened_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()),
        })
    }

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
        // Os contadores só mudam após um commit bem-sucedido, então o estado
        // continua válido mesmo se outra thread entrou em pânico com o lock.
        let waiting = self.metrics().map(|_| Instant::now());
        let mut state = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(m), Some(t)) = (self.metrics(), waiting) {
            m.record_lock_wait(t.elapsed());
        }
        let cache = std::mem::take(&mut state.cache);
        let analyzer = std::mem::take(&mut state.analyzer);
        IndexBatch {
            engine: self,
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

/// O erro do RocksDB ao abrir, com uma mensagem melhor para os casos comuns:
/// a pasta não é um banco, ou outro processo o tem aberto para escrita.
fn open_error(path: &Path, e: rocksdb::Error) -> Error {
    let text = e.to_string();
    if text.contains("does not exist") || text.contains("No such file") {
        not_a_database(path)
    } else if text.contains("lock") && text.contains("LOCK") {
        Error::Open(format!(
            "{} está aberto para escrita por outro processo; para só ler, use Engine::open_read_only",
            path.display()
        ))
    } else {
        Error::Db(e)
    }
}

fn not_a_database(path: &Path) -> Error {
    Error::Open(format!("{} não é um banco do fire-flower (pasta vazia ou inexistente)", path.display()))
}
