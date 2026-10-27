//! Camadas de índice (Trie) e armazenamento (RocksDB).
//!
//! Espaços de chave (formato v6; a codificação está em `src/codec/`):
//! - `node:<id>`    -> [`TrieNode`] serializado (`node:0` é a raiz)
//! - `post:<id>`    -> (documento, tf, tf no título) do termo que termina no nó `<id>`
//! - `pos:<id>`     -> posições do termo em cada documento (só com posições);
//!   o título vem primeiro, e o texto começa `TITLE_GAP` posições depois
//! - `norm:<bloco>` -> tamanho de cada documento (e do título) com `id ÷ 1024 = bloco`
//! - `doc:<id>`     -> rótulo opcional do documento (ex.: caminho do arquivo)
//! - `stem:<radical>` -> nós das palavras com esse radical (só com flexões na consulta)
//! - `meta:*`       -> formato, contadores de ID, estatísticas, idiomas e stemming

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use rocksdb::{BlockBasedIndexType, BlockBasedOptions, Cache, DB, DBCompressionType, Options};
use rustc_hash::FxHashMap;

use crate::{DocId, Error, Hit, IndexStats, Posting, Result, TopHits};
use crate::analysis::{Analyzer, normalize_term};
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::{Language, Stopwords};
use crate::codec::{NORM_BLOCK_DOCS, NormBlock, TrieNode};
use crate::engine::cache::EpochCache;
use crate::engine::keys::{
    DOC, Key, MERGE_OPERATOR, META_DOC_COUNT, META_INFLECTIONS, META_NEXT_DOC, META_NEXT_NODE,
    META_STEMMING, META_TOTAL_LEN, META_TOTAL_TITLE_LEN, ROOT_ID,
};
use crate::engine::meta::{
    Features, full_merge, partial_merge, previous_merge_operator, read_counter, resolve_format, resolve_languages,
    resolve_stemming, stored_format_error,
};
use crate::engine::view::View;
use crate::engine::writer::{NodeCache, WriterState};
use crate::query::{Clause, parse_query_stemmed};

mod cache;
mod config;
mod conjunction;
mod diagnostics;
mod groups;
mod keys;
mod meta;
mod phrase;
mod prefix;
mod ranking;
mod search;
mod view;
mod writer;
#[cfg(test)]
mod tests;


pub use self::config::{Config, InflectionMode};
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
    /// O banco grava os termos de cada documento (`meta:edits`, `fwd:`).
    edits: bool,
    /// Número do último commit gravado (ver [`EpochCache`]).
    epoch: AtomicU64,
    norms: EpochCache<NormBlock>,
    /// Nós rasos da Trie (até `Config::reader_cache_depth`).
    nodes: EpochCache<TrieNode>,
    /// Guardadas para ler as estatísticas depois do `open`.
    opts: Options,
}

impl Engine {
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let path = path.as_ref();
        config.validate()?;
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

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_block_based_table_factory(&table);
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.set_merge_operator(MERGE_OPERATOR, full_merge, partial_merge);
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        opts.increase_parallelism(cpus as i32);
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
        let db = match DB::open_cf_descriptors(&opts, path, [default_cf]) {
            Ok(db) => db,
            // Um banco antigo com merges pendentes no WAL falha já na
            // recuperação, porque o merge operator não entende o formato antigo.
            Err(e) => return Err(stored_format_error(path).unwrap_or(Error::Db(e))),
        };
        let Features { positions, edits } = resolve_format(&db, config.positions, config.edits)?;
        let next_node_id = read_counter(&db, META_NEXT_NODE)?.unwrap_or(ROOT_ID + 1);
        let next_doc_id = read_counter(&db, META_NEXT_DOC)?.unwrap_or(1);
        let stats = IndexStats {
            doc_count: read_counter(&db, META_DOC_COUNT)?.unwrap_or(0),
            total_len: read_counter(&db, META_TOTAL_LEN)?.unwrap_or(0),
            total_title_len: read_counter(&db, META_TOTAL_TITLE_LEN)?.unwrap_or(0),
        };
        let stopwords = Stopwords::new(&resolve_languages(&db, config.languages.as_deref())?);
        let stemmer = Stemmer::new(resolve_stemming(&db, META_STEMMING, config.stemming)?);
        let inflector = Stemmer::new(resolve_stemming(&db, META_INFLECTIONS, config.inflections)?);
        if stemmer.is_on() && inflector.is_on() {
            return Err(Error::Config(format!(
                "stemming ({}) e flexões na consulta ({}) não combinam: escolha um",
                stemmer.stemming(),
                inflector.stemming()
            )));
        }
        let writer = Mutex::new(WriterState {
            next_node_id,
            next_doc_id,
            stats,
            cache: NodeCache::default(),
            analyzer: Analyzer::default(),
        });
        Ok(Self {
            db,
            writer,
            config,
            stopwords,
            stemmer,
            inflector,
            positions,
            edits,
            epoch: AtomicU64::new(0),
            norms: EpochCache::default(),
            nodes: EpochCache::default(),
            opts,
        })
    }

    /// Inicia um lote de indexação. Bloqueia se outro lote estiver aberto
    /// (inclusive na mesma thread: não chame `index_document` com um lote aberto).
    pub fn batch(&self) -> IndexBatch<'_> {
        // Os contadores só mudam após um commit bem-sucedido, então o estado
        // continua válido mesmo se outra thread entrou em pânico com o lock.
        let mut state = self.writer.lock().unwrap_or_else(|e| e.into_inner());
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
        self.view().search(query)
    }

    /// Os `k` documentos mais relevantes para a consulta (BM25), da maior
    /// nota para a menor; no empate, o menor ID primeiro. A consulta tem a
    /// mesma sintaxe e os mesmos resultados de [`Engine::search`].
    pub fn search_top(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        Ok(self.search_ranked(query, k)?.hits)
    }

    /// Como [`Engine::search_top`], mais o total de documentos que casaram.
    pub fn search_ranked(&self, query: &str, k: usize) -> Result<TopHits> {
        self.view().search_ranked(query, k)
    }

    /// Documentos que contêm exatamente `term` (normalizado aqui e, com
    /// stemming, reduzido ao radical, como numa busca).
    pub fn postings(&self, term: &str) -> Result<Vec<DocId>> {
        let Some(term) = self.index_term(term) else { return Ok(Vec::new()) };
        let view = self.view();
        match view.term_node(&term)? {
            Some(node) => view.load_docs(node),
            None => Ok(Vec::new()),
        }
    }

    /// Como [`Engine::postings`], com a frequência do termo em cada documento.
    pub fn term_postings(&self, term: &str) -> Result<Vec<Posting>> {
        let Some(term) = self.index_term(term) else { return Ok(Vec::new()) };
        let view = self.view();
        match view.term_node(&term)? {
            Some(node) => view.load_postings(node),
            None => Ok(Vec::new()),
        }
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
        let words = self.view().words_with_prefix(&prefix, limit)?;
        Ok(words.into_iter().map(|(word, _)| word).collect())
    }

    /// Compacta o banco inteiro: une os pedaços pendentes das listas de
    /// postagem e descarta versões antigas. Num índice grande leva minutos.
    pub fn compact(&self) {
        self.db.compact_range::<&[u8], &[u8]>(None, None);
    }

    /// Rótulo gravado com [`IndexBatch::add_labeled`] ou [`IndexBatch::set_label`].
    pub fn label(&self, doc_id: DocId) -> Result<Option<String>> {
        Ok(self
            .db
            .get_pinned(Key::new(DOC, doc_id))?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Tamanho do documento (tokens indexados, sem stopwords), se ele existe.
    pub fn doc_len(&self, doc_id: DocId) -> Result<Option<u32>> {
        let view = self.view();
        let block = view.norm_block(doc_id / NORM_BLOCK_DOCS)?;
        Ok(block.and_then(|b| b.get((doc_id % NORM_BLOCK_DOCS) as u16)))
    }

    /// Total de documentos e soma dos tamanhos.
    pub fn stats(&self) -> Result<IndexStats> {
        self.view().stats()
    }

    /// O banco grava posições (busca por frase disponível).
    pub fn has_positions(&self) -> bool {
        self.positions
    }

    /// O banco grava os termos de cada documento: dá para substituir e
    /// remover documentos ([`Config::edits`]).
    pub fn has_edits(&self) -> bool {
        self.edits
    }

    /// Uma busca faz várias leituras (um `Get` por caractere, mais as listas).
    /// O snapshot garante que todas vejam o mesmo momento do índice, mesmo que
    /// um commit aconteça no meio.
    fn view(&self) -> View<'_> {
        // Antes do snapshot: ver `EpochCache`.
        let epoch = self.epoch.load(AtomicOrdering::Acquire);
        View { snap: self.db.snapshot(), epoch, engine: self }
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
