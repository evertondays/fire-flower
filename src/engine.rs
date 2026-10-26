//! Camadas de índice (Trie) e armazenamento (RocksDB).
//!
//! Espaços de chave (formato v3; a codificação está em `codec.rs`):
//! - `node:<id>`    -> [`TrieNode`] serializado (`node:0` é a raiz)
//! - `post:<id>`    -> (documento, tf, tf no título) do termo que termina no nó `<id>`
//! - `pos:<id>`     -> posições do termo em cada documento (só com posições);
//!   o título vem primeiro, e o texto começa `TITLE_GAP` posições depois
//! - `norm:<bloco>` -> tamanho de cada documento (e do título) com `id ÷ 1024 = bloco`
//! - `doc:<id>`     -> rótulo opcional do documento (ex.: caminho do arquivo)
//! - `meta:*`       -> formato, contadores de ID, estatísticas e idiomas

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Instant;

use rocksdb::{BlockBasedIndexType, BlockBasedOptions, Cache, DB, DBCompressionType, MergeOperands, Options, WriteBatch};
use rustc_hash::FxHashMap;

use crate::codec::{self, DocLens, NORM_BLOCK_DOCS, NormBlock, PositionsCursor, TrieNode};
use crate::stopwords::{self, Language, Stopwords};
use crate::text::{Analyzer, Clause, normalize_term, parse_query};
use crate::{DocId, Error, Hit, IndexStats, Posting, Result, TopHits};

pub const ROOT_ID: u64 = 0;
/// Versão do formato gravado. Bancos de outra versão precisam ser recarregados.
pub const FORMAT_VERSION: u64 = 3;
/// Nome do merge operator, gravado pelo RocksDB no `OPTIONS` de cada abertura.
/// Muda com o formato: é por ele que um banco antigo é reconhecido antes de
/// ser aberto para escrita.
const MERGE_OPERATOR: &str = "fire_flower_v3";

const META_FORMAT: &[u8] = b"meta:format";
/// `1` se o banco grava posições (`pos:`), `0` se não.
const META_POSITIONS: &[u8] = b"meta:positions";
const META_NEXT_NODE: &[u8] = b"meta:next_node_id";
const META_NEXT_DOC: &[u8] = b"meta:next_doc_id";
const META_DOC_COUNT: &[u8] = b"meta:doc_count";
const META_TOTAL_LEN: &[u8] = b"meta:total_len";
const META_TOTAL_TITLE_LEN: &[u8] = b"meta:total_title_len";
/// Códigos dos idiomas das stopwords, separados por vírgula ("pt,en").
const META_LANGUAGES: &[u8] = b"meta:languages";

const NODE: &[u8] = b"node:";
const POST: &[u8] = b"post:";
const POS: &[u8] = b"pos:";
const NORM: &[u8] = b"norm:";
const DOC: &[u8] = b"doc:";

/// Chave `<prefixo><número>` montada na pilha: o laço do commit monta uma
/// por termo, e `format!` alocaria uma `String` a cada vez.
struct Key {
    buf: [u8; 32],
    len: usize,
}

impl Key {
    fn new(prefix: &[u8], mut id: u64) -> Self {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[19 - n] = b'0' + (id % 10) as u8;
            n += 1;
            id /= 10;
            if id == 0 {
                break;
            }
        }
        let mut buf = [0u8; 32];
        buf[..prefix.len()].copy_from_slice(prefix);
        buf[prefix.len()..prefix.len() + n].copy_from_slice(&digits[20 - n..]);
        Self { buf, len: prefix.len() + n }
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Memória do block cache. Mantém a raiz e os primeiros níveis da Trie,
    /// que todas as buscas atravessam, sempre em RAM.
    pub block_cache_bytes: usize,
    /// Bits por chave do filtro de Bloom. 10 ≈ 1% de falsos positivos.
    pub bloom_bits_per_key: f64,
    /// Quantos termos uma busca por prefixo (`comp*`) expande no máximo.
    /// Termos além disso são ignorados; prefixos curtos podem casar milhares.
    pub max_prefix_terms: usize,
    /// Liga as estatísticas internas do RocksDB ([`Engine::rocksdb_stats`]).
    /// Custa um pouco de CPU; use em medições, não em produção.
    pub statistics: bool,
    /// Quantos nós da Trie o escritor mantém em memória entre um lote e
    /// outro (~130 bytes cada; o cache só cresce até o tamanho da Trie).
    /// Sem isso, todo lote relê do banco os nós dos termos que aparecem.
    /// Passando do limite, ficam os usados mais recentemente. 0 desliga.
    ///
    /// 3 milhões (~400 MB) cobrem 40% da Trie da Wikipédia inteira (7,3 M
    /// nós): a carga cai de 139 s para 115 s com lote de 10 mil, e as
    /// leituras do escritor, de 7,5 para 1,7 por artigo.
    pub writer_cache_nodes: usize,
    /// Até que profundidade da Trie as buscas guardam os nós já decodificados,
    /// compartilhados entre elas (0 = só a raiz). Toda busca começa pela raiz,
    /// que tem um filho por caractere inicial e cresce com o índice (13 KB com
    /// 100 mil artigos da Wikipédia): sem o cache, cada busca a decodificava
    /// inteira. `None` desliga.
    pub reader_cache_depth: Option<usize>,
    /// Idiomas das stopwords (palavras comuns que não entram no índice).
    ///
    /// Ficam gravados no banco quando ele é criado, porque índice e consulta
    /// precisam descartar as mesmas palavras. `None` usa o que está no banco,
    /// ou [`Language::DEFAULT`] (português e inglês) num banco novo. `Some`
    /// diferente do que está gravado faz o `open` falhar. `Some(vec![])`
    /// cria um banco sem stopwords.
    pub languages: Option<Vec<Language>>,
    /// Grava as posições de cada termo (`pos:`), que a busca por frase usa.
    ///
    /// Como os idiomas, é decidido na criação do banco: `None` usa o que
    /// está gravado (ou liga, num banco novo), e `Some` diferente do gravado
    /// faz o `open` falhar. Sem posições o banco fica menor e a carga mais
    /// rápida, mas frases dão erro.
    pub positions: Option<bool>,
    /// Filtros de Bloom e índices dos SSTs em partições de 4 KB (ligado por
    /// padrão). Desligar volta ao filtro inteiro, que num banco grande passa
    /// do tamanho de uma parte do block cache e é relido a cada `Get`; existe
    /// só para medir esse efeito (`wiki load --filtro-inteiro`).
    pub partition_filters: bool,
    /// Parâmetro `k1` do BM25: quanto a nota cresce com a frequência do termo.
    pub bm25_k1: f64,
    /// Parâmetro `b` do BM25: quanto o tamanho do texto pesa (0 a 1).
    pub bm25_b: f64,
    /// Quanto uma ocorrência no título vale em relação a uma no texto, no
    /// BM25F (ver `docs/plano-relevancia.md`). 0 ignora o título.
    pub title_weight: f64,
    /// `b` do título: quanto um título longo perde para um curto (0 a 1).
    pub title_b: f64,
}

impl Config {
    fn validate(&self) -> Result<()> {
        let in_unit = |v: f64| (0.0..=1.0).contains(&v);
        let ok = self.bm25_k1.is_finite()
            && self.bm25_k1 >= 0.0
            && in_unit(self.bm25_b)
            && in_unit(self.title_b)
            && self.title_weight.is_finite()
            && self.title_weight >= 0.0;
        if ok {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "parâmetros de ranking inválidos: k1 = {}, b = {}, peso do título = {}, b do título = {} \
                 (k1 e peso ≥ 0; b entre 0 e 1)",
                self.bm25_k1, self.bm25_b, self.title_weight, self.title_b
            )))
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block_cache_bytes: 256 << 20,
            bloom_bits_per_key: 10.0,
            max_prefix_terms: 256,
            statistics: false,
            writer_cache_nodes: 3_000_000,
            reader_cache_depth: Some(2),
            languages: None,
            positions: None,
            partition_filters: true,
            // Os padrões do Lucene e do Elasticsearch.
            bm25_k1: 1.2,
            bm25_b: 0.75,
            // Escolhidos pela grade da Fase 4 do plano de relevância (100k da
            // Wikipédia, confirmada com outra amostra): a qualidade sobe até
            // ~50 e depois quase não muda; b 0,9 mantém o limite do corte finito.
            title_weight: 50.0,
            title_b: 0.9,
        }
    }
}

/// Motor de busca. É `Sync`: compartilhe via `Arc<Engine>`. Buscas rodam em
/// paralelo entre si e com a indexação; só um [`IndexBatch`] existe por vez.
pub struct Engine {
    db: DB,
    writer: Mutex<WriterState>,
    config: Config,
    stopwords: Stopwords,
    /// O banco grava posições (`meta:positions`).
    positions: bool,
    /// Número do último commit gravado (ver [`EpochCache`]).
    epoch: AtomicU64,
    norms: EpochCache<NormBlock>,
    /// Nós rasos da Trie (até `Config::reader_cache_depth`).
    nodes: EpochCache<TrieNode>,
    /// Guardadas para ler as estatísticas depois do `open`.
    opts: Options,
}

struct WriterState {
    next_node_id: u64,
    next_doc_id: DocId,
    stats: IndexStats,
    /// Nós da Trie dos lotes anteriores. Só o escritor altera a Trie, então
    /// depois de um commit o cache é igual ao banco. O lote o pega emprestado
    /// e só o devolve no commit: um lote descartado leva o cache junto, e
    /// nada que não foi gravado sobrevive.
    cache: NodeCache,
}

#[derive(Default)]
struct NodeCache {
    nodes: FxHashMap<u64, CachedNode>,
    /// Número do último lote gravado. Cada nó guarda o lote que o usou por último.
    generation: u64,
}

/// Valores já decodificados, compartilhados pelas buscas: blocos de `norm:`
/// (o ranking precisa do tamanho de cada documento) e os nós rasos da Trie
/// (toda busca passa pela raiz). Sem cache, seria um `Get` e uma decodificação
/// por valor em toda busca.
///
/// O desafio é o snapshot: uma busca não pode usar um valor gravado depois
/// dela. Por isso cada commit tem um número (`Engine::epoch`) e cada entrada
/// guarda a partir de qual número o conteúdo em cache vale:
///
/// 1. antes de gravar, o escritor marca as chaves que vai mudar com o número
///    do commit (`epoch + 1`) e sem conteúdo ([`EpochCache::mark`]);
/// 2. grava o lote;
/// 3. publica o novo número.
///
/// A busca lê o número *antes* de criar o snapshot. Uma entrada com número
/// maior que o dela foi (ou está sendo) mudada depois: ela lê do próprio
/// snapshot e não guarda nada. Uma chave sem entrada nunca mudou desde o
/// `open`. Por isso o escritor marca **toda** chave que muda e que o cache
/// poderia guardar, esteja ela no cache ou não.
///
/// O cache não descarta entradas. Os blocos de `norm:` ocupam no máximo o
/// tamanho de todos os documentos (~6 bytes cada; 1 milhão ≈ 6 MB); os nós,
/// só os rasos.
struct EpochCache<T> {
    slots: RwLock<FxHashMap<u64, Slot<T>>>,
}

struct Slot<T> {
    /// Commit a partir do qual `value` vale.
    version: u64,
    value: Option<Arc<T>>,
}

impl<T> Default for EpochCache<T> {
    fn default() -> Self {
        Self { slots: RwLock::new(FxHashMap::default()) }
    }
}

impl<T> EpochCache<T> {
    fn get(&self, key: u64, epoch: u64) -> Option<Arc<T>> {
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        match slots.get(&key) {
            Some(slot) if slot.version <= epoch => slot.value.clone(),
            _ => None,
        }
    }

    /// Uma busca leu `key` do snapshot `epoch`; guarda se ainda for o atual.
    fn offer(&self, key: u64, epoch: u64, data: &Arc<T>) {
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        match slots.entry(key) {
            Entry::Vacant(e) => {
                e.insert(Slot { version: 0, value: Some(Arc::clone(data)) });
            }
            Entry::Occupied(mut e) => {
                let slot = e.get_mut();
                if slot.value.is_none() && slot.version <= epoch {
                    slot.value = Some(Arc::clone(data));
                }
            }
        }
    }

    /// Passo 1 do commit: `keys` vão mudar no commit `version`.
    fn mark(&self, keys: impl IntoIterator<Item = u64>, version: u64) {
        let mut keys = keys.into_iter().peekable();
        if keys.peek().is_none() {
            return;
        }
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            slots.insert(key, Slot { version, value: None });
        }
    }
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
        opts.set_merge_operator_associative(MERGE_OPERATOR, merge_operator);
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
        let db = match DB::open(&opts, path) {
            Ok(db) => db,
            // Um banco antigo com merges pendentes no WAL falha já na
            // recuperação, porque o merge operator não entende o formato antigo.
            Err(e) => return Err(stored_format_error(path).unwrap_or(Error::Db(e))),
        };
        let positions = resolve_format(&db, config.positions)?;
        let next_node_id = read_counter(&db, META_NEXT_NODE)?.unwrap_or(ROOT_ID + 1);
        let next_doc_id = read_counter(&db, META_NEXT_DOC)?.unwrap_or(1);
        let stats = IndexStats {
            doc_count: read_counter(&db, META_DOC_COUNT)?.unwrap_or(0),
            total_len: read_counter(&db, META_TOTAL_LEN)?.unwrap_or(0),
            total_title_len: read_counter(&db, META_TOTAL_TITLE_LEN)?.unwrap_or(0),
        };
        let stopwords = Stopwords::new(&resolve_languages(&db, config.languages.as_deref())?);
        let writer = Mutex::new(WriterState { next_node_id, next_doc_id, stats, cache: NodeCache::default() });
        Ok(Self {
            db,
            writer,
            config,
            stopwords,
            positions,
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
            lengths: Vec::new(),
            labels: Vec::new(),
            analyzer: Analyzer::default(),
        }
    }

    /// Atalho para indexar um único documento. Para muitos documentos, use [`Engine::batch`].
    pub fn index_document(&self, doc_id: DocId, text: &str) -> Result<()> {
        let mut batch = self.batch();
        batch.add_document(doc_id, text)?;
        batch.commit()
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

    /// Documentos que contêm exatamente `term` (normalizado aqui).
    pub fn postings(&self, term: &str) -> Result<Vec<DocId>> {
        let Some(term) = normalize_term(term) else { return Ok(Vec::new()) };
        let view = self.view();
        match view.term_node(&term)? {
            Some(node) => view.load_docs(node),
            None => Ok(Vec::new()),
        }
    }

    /// Como [`Engine::postings`], com a frequência do termo em cada documento.
    pub fn term_postings(&self, term: &str) -> Result<Vec<Posting>> {
        let Some(term) = normalize_term(term) else { return Ok(Vec::new()) };
        let view = self.view();
        match view.term_node(&term)? {
            Some(node) => view.load_postings(node),
            None => Ok(Vec::new()),
        }
    }

    /// Autocompletar: até `limit` palavras indexadas que começam com `prefix`,
    /// das mais curtas para as mais longas e em ordem alfabética.
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
}

/// Diagnóstico e medição: usados pelo harness de performance (`examples/wiki.rs`).
impl Engine {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Estatísticas internas do RocksDB, se `Config::statistics` estiver ligado.
    pub fn rocksdb_stats(&self) -> Option<String> {
        self.opts.get_statistics()
    }

    /// Uma propriedade do RocksDB, como `rocksdb.stats`,
    /// `rocksdb.estimate-num-keys` ou `rocksdb.total-sst-files-size`.
    pub fn property(&self, name: &str) -> Option<String> {
        self.db.property_value(name).ok().flatten()
    }

    /// Nós da Trie já criados (o próximo ID livre).
    pub fn trie_node_count(&self) -> Result<u64> {
        Ok(read_counter(&self.db, META_NEXT_NODE)?.unwrap_or(0))
    }

    /// ID do nó terminal de `term` (também é o ID da lista em `post:<id>`).
    pub fn term_node(&self, term: &str) -> Result<Option<u64>> {
        let Some(term) = normalize_term(term) else { return Ok(None) };
        self.view().term_node(&term)
    }

    /// IDs da lista de postagem pelo ID do nó terminal (ver [`Engine::for_each_term`]).
    pub fn postings_of_node(&self, node_id: u64) -> Result<Vec<DocId>> {
        self.view().load_docs(node_id)
    }

    /// Lista de postagem com tf, pelo ID do nó terminal.
    pub fn term_postings_of_node(&self, node_id: u64) -> Result<Vec<Posting>> {
        self.view().load_postings(node_id)
    }

    /// Posições do termo em cada documento, pelo ID do nó terminal. Vazio
    /// num banco sem posições.
    pub fn positions_of_node(&self, node_id: u64) -> Result<Vec<(DocId, Vec<u32>)>> {
        match self.view().snap.get_bytes(Key::new(POS, node_id).as_ref())? {
            Some(v) => codec::decode_positions(&v),
            None => Ok(Vec::new()),
        }
    }

    /// Percorre os tamanhos de todos os documentos (em ordem de bloco):
    /// `f(documento, tamanho inteiro, tamanho do título)`.
    pub fn for_each_doc_len(&self, mut f: impl FnMut(DocId, u32, u32)) -> Result<()> {
        let mode = rocksdb::IteratorMode::From(NORM, rocksdb::Direction::Forward);
        for item in self.db.iterator(mode) {
            let (key, value) = item?;
            let Some(block) = key.strip_prefix(NORM) else { break };
            let block: u64 = std::str::from_utf8(block)
                .ok()
                .and_then(|b| b.parse().ok())
                .ok_or_else(|| Error::Corrupt("chave norm: inválida".into()))?;
            for (offset, l) in NormBlock::decode(&value)?.iter() {
                f(block * NORM_BLOCK_DOCS + u64::from(offset), l.len, u32::from(l.title_len));
            }
        }
        Ok(())
    }

    /// Percorre todos os termos em ordem alfabética (busca em profundidade
    /// sobre um snapshot). `f` recebe o termo e o ID do nó terminal.
    pub fn for_each_term(&self, mut f: impl FnMut(&str, u64)) -> Result<()> {
        // Lê direto do snapshot: percorrer tudo não deve encher o cache das buscas.
        let view = self.view();
        let Some(root) = load_node(&view.snap, ROOT_ID)? else { return Ok(()) };
        let mut stack: Vec<(String, TrieNode)> = vec![(String::new(), root)];
        let mut ids = vec![ROOT_ID];
        while let (Some((word, node)), Some(id)) = (stack.pop(), ids.pop()) {
            if node.is_end_of_word {
                f(&word, id);
            }
            // Empilha ao contrário para visitar os filhos em ordem alfabética.
            for (&ch, &child) in node.children.iter().rev() {
                let mut next = word.clone();
                next.push(ch);
                stack.push((next, load_node(&view.snap, child)?.ok_or_else(|| missing_node(child))?));
                ids.push(child);
            }
        }
        Ok(())
    }

    /// Percorre todas as chaves e valores do banco, em ordem de chave.
    /// Valores de `post:` e `pos:` já vêm com os merges pendentes aplicados.
    pub fn for_each_raw(&self, mut f: impl FnMut(&[u8], &[u8])) -> Result<()> {
        for item in self.db.iterator(rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            f(&key, &value);
        }
        Ok(())
    }
}

type Snapshot<'a> = rocksdb::SnapshotWithThreadMode<'a, DB>;

/// Leituras de uma única busca, todas sobre o mesmo snapshot.
struct View<'a> {
    snap: Snapshot<'a>,
    /// Número do último commit visível quando o snapshot foi criado.
    epoch: u64,
    engine: &'a Engine,
}

/// Uma parte da consulta já carregada, para o ranking. `weight` é
/// idf · (k1 + 1), a parte da nota que não depende do documento.
enum Scorer {
    /// Termo exato, ou prefixo tratado como um termo só (R1 do plano de
    /// relevância): a lista `list` em `lists`.
    Term { weight: f64, list: usize, bound: Bound },
    /// Frase: entra na nota como um termo, com tf = ocorrências da frase (no
    /// texto e no título, pela posição) e idf = soma dos idfs dos termos
    /// (como o Lucene).
    Phrase { weight: f64, phrase: usize, bound: Bound },
}

/// Normalização de um campo pelo tamanho: `B(len) = 1 − b + b · len / média`.
#[derive(Clone, Copy)]
struct FieldNorm {
    base: f64,
    slope: f64,
}

impl FieldNorm {
    fn new(b: f64, avg: f64) -> Self {
        Self { base: 1.0 - b, slope: if avg > 0.0 { b / avg } else { 0.0 } }
    }

    #[inline]
    fn of(&self, len: f64) -> f64 {
        self.base + self.slope * len
    }
}

/// Os parâmetros do BM25F para uma busca (seção 1 de `plano-relevancia.md`):
///
/// ```text
/// B_f(d) = 1 − b_f + b_f · len_f(d) / média_f      (f = texto, título)
/// tf~    = tf_texto / B_texto  +  w · tf_título / B_título
/// nota   = idf · tf~ · (k1 + 1) / (tf~ + k1)
/// ```
///
/// O título multiplica o tf antes da saturação: uma ocorrência nele vale
/// `w` no texto, e um título curto vale mais que um longo. Sem título
/// (`tf_título = 0`) é exatamente o BM25 de antes.
struct Bm25 {
    k1: f64,
    n: f64,
    body: FieldNorm,
    title: FieldNorm,
    title_weight: f64,
    /// Maior `1 / B_texto` (documento de tamanho 0): para o limite superior.
    max_body: f64,
    /// Quanto uma ocorrência no título vale, no máximo, em ocorrências do
    /// texto num documento de tamanho 0: `(w / (1 − b_título)) / max_body`.
    title_ratio: f64,
}

/// Os pesos de um documento: `1 / B_texto` e `w / B_título`.
#[derive(Clone, Copy)]
struct DocNorm {
    body: f64,
    title: f64,
}

impl Bm25 {
    fn new(config: &Config, stats: IndexStats) -> Self {
        let body = FieldNorm::new(config.bm25_b, stats.avg_body_len());
        let title = FieldNorm::new(config.title_b, stats.avg_title_len());
        let inv = |x: f64| if x > 0.0 { 1.0 / x } else { f64::INFINITY };
        let max_body = inv(body.base);
        let max_title = if config.title_weight > 0.0 { config.title_weight * inv(title.base) } else { 0.0 };
        let title_ratio = if max_body.is_finite() { max_title / max_body } else { 0.0 };
        Self { k1: config.bm25_k1, n: stats.doc_count as f64, body, title, title_weight: config.title_weight, max_body, title_ratio }
    }

    fn idf(&self, df: usize) -> f64 {
        let df = df as f64;
        (1.0 + (self.n - df + 0.5) / (df + 0.5)).ln()
    }

    /// idf · (k1 + 1): constante por termo.
    fn weight(&self, idf: f64) -> f64 {
        idf * (self.k1 + 1.0)
    }

    /// Os pesos de um documento, uma vez por documento.
    #[inline]
    fn doc_norm(&self, lens: DocLens) -> DocNorm {
        let title_len = f64::from(lens.title_len);
        let body_len = f64::from(lens.len) - title_len;
        // B = 0 só com b = 1 e o campo vazio: aí o tf do campo é 0 também.
        let b_body = self.body.of(body_len);
        let b_title = self.title.of(title_len);
        DocNorm {
            body: if b_body > 0.0 { 1.0 / b_body } else { 0.0 },
            title: if b_title > 0.0 { self.title_weight / b_title } else { 0.0 },
        }
    }

    /// tf~: as ocorrências no texto e no título, cada uma normalizada pelo seu campo.
    #[inline]
    fn pseudo_tf(norm: DocNorm, tf: u32, title_tf: u32) -> f64 {
        f64::from(tf - title_tf) * norm.body + f64::from(title_tf) * norm.title
    }

    /// idf · tf~ · (k1 + 1) / (tf~ + k1).
    #[inline]
    fn score(&self, weight: f64, pseudo_tf: f64) -> f64 {
        weight * pseudo_tf / (pseudo_tf + self.k1)
    }
}

/// Maior nota possível de uma parte, pela chave (ocorrências equivalentes
/// do texto num documento de tamanho 0, arredondadas para cima): sem título
/// a chave é o próprio tf. Tabelada para as chaves comuns: conferir o limite
/// fica sem divisão. A margem relativa cobre o arredondamento de ponto
/// flutuante, então o corte continua exato.
struct Bound {
    weight: f64,
    k1: f64,
    max_body: f64,
    title_ratio: f64,
    table: [f64; BOUND_TF + 1],
}

const BOUND_TF: usize = 32;
const BOUND_MARGIN: f64 = 1.0 + 1e-12;

impl Bound {
    fn new(weight: f64, bm25: &Bm25) -> Self {
        let mut b = Self { weight, k1: bm25.k1, max_body: bm25.max_body, title_ratio: bm25.title_ratio, table: [0.0; BOUND_TF + 1] };
        b.table = std::array::from_fn(|key| b.compute(key as u32));
        b
    }

    fn compute(&self, key: u32) -> f64 {
        if !self.max_body.is_finite() {
            return self.weight * BOUND_MARGIN; // b = 1: sem limite útil (a nota nunca passa do peso)
        }
        let p = self.max_body * f64::from(key);
        self.weight * p / (p + self.k1) * BOUND_MARGIN
    }

    /// A chave de uma entrada: o tf, se não houver título; senão o tf do
    /// texto mais o do título convertido, arredondado para cima.
    #[inline]
    fn key(&self, tf: u32, title_tf: u32) -> u32 {
        if title_tf == 0 {
            return tf;
        }
        let x = f64::from(tf - title_tf) + self.title_ratio * f64::from(title_tf);
        if x.is_finite() && x < f64::from(u32::MAX - 1) { (x + 1e-9).ceil() as u32 } else { u32::MAX }
    }

    #[inline]
    fn get_key(&self, key: u32) -> f64 {
        match self.table.get(key as usize) {
            Some(&b) => b,
            None => self.compute(key),
        }
    }

    #[inline]
    fn get(&self, tf: u32, title_tf: u32) -> f64 {
        self.get_key(self.key(tf, title_tf))
    }
}

/// Nota de um documento (já posicionado em todas as listas): soma das
/// partes, na ordem da consulta.
#[inline]
fn doc_score(bm25: &Bm25, scorers: &[Scorer], lists: &[Vec<Posting>], at: &[usize], counts: &[(u32, u32)], norm: DocNorm) -> f64 {
    let mut score = 0.0;
    for scorer in scorers {
        score += match scorer {
            Scorer::Term { weight, list, .. } => {
                let p = lists[*list][at[*list]];
                bm25.score(*weight, Bm25::pseudo_tf(norm, p.tf, p.title_tf))
            }
            Scorer::Phrase { weight, phrase, .. } => {
                let (tf, title_tf) = counts[*phrase];
                bm25.score(*weight, Bm25::pseudo_tf(norm, tf, title_tf))
            }
        };
    }
    score
}

/// Limite superior de [`doc_score`]: a mesma soma, com o maior valor
/// possível de cada parte.
#[inline]
fn doc_bound(scorers: &[Scorer], lists: &[Vec<Posting>], at: &[usize], counts: &[(u32, u32)]) -> f64 {
    let mut bound = 0.0;
    for scorer in scorers {
        bound += match scorer {
            Scorer::Term { list, bound, .. } => {
                let p = lists[*list][at[*list]];
                bound.get(p.tf, p.title_tf)
            }
            Scorer::Phrase { phrase, bound, .. } => {
                let (tf, title_tf) = counts[*phrase];
                bound.get(tf, title_tf)
            }
        };
    }
    bound
}

impl View<'_> {
    fn search(&self, query: &str) -> Result<Vec<DocId>> {
        let clauses = parse_query(query, &self.engine.stopwords);
        self.check_phrases(&clauses)?;
        let mut lists: Vec<Vec<DocId>> = Vec::new();
        let mut phrases: Vec<Vec<(u32, u64)>> = Vec::new();
        for clause in &clauses {
            match clause {
                Clause::Term(t) => {
                    let list = if t.prefix { self.prefix_docs(&t.text)? } else { self.term_docs(&t.text)? };
                    if list.is_empty() {
                        return Ok(Vec::new());
                    }
                    lists.push(list);
                }
                Clause::Phrase(parts) => {
                    // Os termos da frase entram no E como termos comuns; as
                    // posições só são conferidas nos documentos que sobrarem.
                    let mut nodes = Vec::with_capacity(parts.len());
                    for part in parts {
                        let Some(node) = self.term_node(&part.text)? else { return Ok(Vec::new()) };
                        let list = self.load_docs(node)?;
                        if list.is_empty() {
                            return Ok(Vec::new());
                        }
                        lists.push(list);
                        nodes.push((part.offset, node));
                    }
                    phrases.push(nodes);
                }
            }
        }

        // Interseção começando pela lista menor: o resultado só encolhe.
        lists.sort_by_key(Vec::len);
        let mut lists = lists.into_iter();
        let Some(mut result) = lists.next() else { return Ok(Vec::new()) };
        for list in lists {
            result = intersect(&result, &list);
            if result.is_empty() {
                return Ok(result);
            }
        }
        for phrase in &phrases {
            let slices = self.load_positions(phrase)?;
            let mut matcher = PhraseMatcher::new(phrase, &slices)?;
            let mut kept = Vec::with_capacity(result.len());
            for &doc in &result {
                if matcher.count(doc, true, 0)?.0 > 0 {
                    kept.push(doc);
                }
            }
            result = kept;
            if result.is_empty() {
                break;
            }
        }
        Ok(result)
    }

    fn search_ranked(&self, query: &str, k: usize) -> Result<TopHits> {
        let clauses = parse_query(query, &self.engine.stopwords);
        self.check_phrases(&clauses)?;
        if clauses.is_empty() {
            return Ok(TopHits::default());
        }
        let stats = self.stats()?;
        let config = &self.engine.config;
        let bm25 = Bm25::new(config, stats);

        // 1. Carrega as listas. Todas entram no E; cada parte da consulta
        //    sabe quais são as suas.
        let mut lists: Vec<Vec<Posting>> = Vec::new();
        let mut scorers: Vec<Scorer> = Vec::with_capacity(clauses.len());
        let mut phrases: Vec<Vec<(u32, u64)>> = Vec::new();
        for clause in &clauses {
            match clause {
                Clause::Term(t) if !t.prefix => {
                    let list = match self.term_node(&t.text)? {
                        Some(node) => self.load_postings(node)?,
                        None => Vec::new(),
                    };
                    if list.is_empty() {
                        return Ok(TopHits::default());
                    }
                    let weight = bm25.weight(bm25.idf(list.len()));
                    scorers.push(Scorer::Term { weight, list: lists.len(), bound: Bound::new(weight, &bm25) });
                    lists.push(list);
                }
                Clause::Term(t) => {
                    // Prefixo como um termo só: um documento por entrada, com o
                    // tf somado das completações que ele contém, e o idf do
                    // conjunto. Uma completação rara não vale mais que a
                    // palavra comum (R1 do plano de relevância).
                    let list = self.prefix_postings(&t.text)?;
                    if list.is_empty() {
                        return Ok(TopHits::default());
                    }
                    let weight = bm25.weight(bm25.idf(list.len()));
                    scorers.push(Scorer::Term { weight, list: lists.len(), bound: Bound::new(weight, &bm25) });
                    lists.push(list);
                }
                Clause::Phrase(parts) => {
                    let mut idf = 0.0;
                    let mut nodes = Vec::with_capacity(parts.len());
                    for part in parts {
                        let Some(node) = self.term_node(&part.text)? else { return Ok(TopHits::default()) };
                        let list = self.load_postings(node)?;
                        if list.is_empty() {
                            return Ok(TopHits::default());
                        }
                        idf += bm25.idf(list.len());
                        nodes.push((part.offset, node));
                        lists.push(list);
                    }
                    let weight = bm25.weight(idf);
                    scorers.push(Scorer::Phrase { weight, phrase: phrases.len(), bound: Bound::new(weight, &bm25) });
                    phrases.push(nodes);
                }
            }
        }
        let slices: Vec<Vec<rocksdb::DBPinnableSlice<'_>>> =
            phrases.iter().map(|p| self.load_positions(p)).collect::<Result<_>>()?;
        let mut matchers: Vec<PhraseMatcher<'_>> =
            phrases.iter().zip(&slices).map(|(p, s)| PhraseMatcher::new(p, s)).collect::<Result<_>>()?;
        // (ocorrências da frase, quantas no título), por frase.
        let mut counts = vec![(0u32, 0u32); matchers.len()];

        if let [Scorer::Term { weight, list, bound }] = scorers.as_slice()
            && matchers.is_empty()
        {
            let hits = self.top_single_term(&lists[*list], *weight, bound, &bm25, k)?;
            return Ok(TopHits { hits, total: lists[*list].len() });
        }

        // 2. E pelas listas: a menor conduz, as outras avançam por saltos.
        let driver = (0..lists.len()).min_by_key(|&i| lists[i].len()).unwrap_or(0);
        let mut at = vec![0usize; lists.len()];
        let mut norms = NormCursor::new(self);
        let mut top = TopK::new(k);
        let mut total = 0usize;
        'docs: for i in 0..lists[driver].len() {
            let doc = lists[driver][i].doc;
            at[driver] = i;
            for (j, list) in lists.iter().enumerate() {
                if j != driver && !seek(list, &mut at[j], doc) {
                    continue 'docs;
                }
            }
            // Com frase, o tamanho vem antes: a extensão do título diz quais
            // ocorrências estão nele.
            let mut lens = None;
            if !matchers.is_empty() {
                let l = norms.lens(doc)?;
                for (m, matcher) in matchers.iter_mut().enumerate() {
                    counts[m] = matcher.count(doc, false, u32::from(l.title_span))?;
                    if counts[m].0 == 0 {
                        continue 'docs;
                    }
                }
                lens = Some(l);
            }
            total += 1;
            if k == 0 {
                continue;
            }

            // 3. Nota. Com o top-k cheio, um documento que nem com tamanho 0
            //    passaria do pior não precisa do tamanho (nem da nota exata).
            //    Empate também perde: os IDs chegam em ordem crescente.
            if let Some(worst) = top.threshold()
                && doc_bound(&scorers, &lists, &at, &counts) <= worst
            {
                continue;
            }
            let lens = match lens {
                Some(l) => l,
                None => norms.lens(doc)?,
            };
            let score = doc_score(&bm25, &scorers, &lists, &at, &counts, bm25.doc_norm(lens));
            top.push(Hit { doc, score });
        }
        Ok(TopHits { hits: top.into_sorted(), total })
    }

    /// Um termo só: a nota depende só dos tf e dos tamanhos, e o limite só
    /// da chave (`Bound::key`: o tf, sem título).
    ///
    /// Os documentos são agrupados em níveis de chave (nível 0 = chave ≥
    /// `CAP`, nível `CAP − chave` para o resto), e o limite de um nível vale
    /// para todos os níveis abaixo dele. A lista não é reordenada: cada faixa de níveis é
    /// uma passada sequencial em ordem de ID, o que mantém a leitura dos
    /// tamanhos em ordem de bloco.
    ///
    /// 1. Uma passada conta os documentos de cada nível.
    /// 2. A primeira faixa vai do maior tf até juntar `k` documentos, que
    ///    enchem o top-k e dão um pior resultado de referência.
    /// 3. A faixa seguinte vai até o primeiro nível cujo limite não alcança
    ///    esse pior; ali a busca para, porque os níveis abaixo também não
    ///    alcançam. Dentro da faixa, quem nem pelo limite passa do pior atual
    ///    não lê o tamanho.
    ///
    /// Num termo frequente, a maior parte da lista (tf baixo) fica de fora sem
    /// leitura de tamanho nem nota exata.
    fn top_single_term(&self, list: &[Posting], weight: f64, bound: &Bound, bm25: &Bm25, k: usize) -> Result<Vec<Hit>> {
        if k == 0 || list.is_empty() {
            return Ok(Vec::new());
        }
        const CAP: usize = 64;
        let level = |p: &Posting| CAP - (bound.key(p.tf, p.title_tf) as usize).min(CAP);
        // Menor chave de um nível: o limite do nível inteiro.
        let level_key = |l: usize| (CAP - l) as u32;
        let mut count = [0usize; CAP + 1];
        for p in list {
            count[level(p)] += 1;
        }

        let mut top = TopK::new(k);
        let mut norms = NormCursor::new(self);
        // Fora de ordem de nota: no empate, o ID menor ainda entra.
        let loses = |top: &TopK, b: f64, doc: DocId| top.worst().is_some_and(|w| b < w.score || (b == w.score && doc > w.doc));
        let mut lo = 0;
        loop {
            // Níveis vazios no começo da faixa renderiam uma passada à toa.
            while lo <= CAP && count[lo] == 0 {
                lo += 1;
            }
            if lo > CAP {
                break;
            }
            // Fim da faixa: até juntar k documentos (top-k ainda não cheio) ou
            // até o primeiro nível que não alcança o pior guardado.
            let mut hi = lo;
            let mut gathered = 0;
            while hi <= CAP {
                if count[hi] > 0 {
                    if top.worst().is_some() {
                        if hi > 0 && loses(&top, bound.get_key(level_key(hi)), 0) {
                            break;
                        }
                    } else if gathered >= k {
                        break;
                    }
                    gathered += count[hi];
                }
                hi += 1;
            }
            if hi == lo {
                break; // o nível `lo` já não alcança o pior
            }
            for p in list {
                let l = level(p);
                if l < lo || l >= hi || loses(&top, bound.get(p.tf, p.title_tf), p.doc) {
                    continue;
                }
                let norm = bm25.doc_norm(norms.lens(p.doc)?);
                top.push(Hit { doc: p.doc, score: bm25.score(weight, Bm25::pseudo_tf(norm, p.tf, p.title_tf)) });
            }
            lo = hi;
        }
        Ok(top.into_sorted())
    }

    fn check_phrases(&self, clauses: &[Clause]) -> Result<()> {
        if !self.engine.positions && clauses.iter().any(|c| matches!(c, Clause::Phrase(_))) {
            return Err(Error::Config("banco sem posições: busca por frase indisponível".into()));
        }
        Ok(())
    }

    fn stats(&self) -> Result<IndexStats> {
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
    fn term_node(&self, term: &str) -> Result<Option<u64>> {
        Ok(match self.find_node(term)? {
            Some((id, node)) if node.is_end_of_word => Some(id),
            _ => None,
        })
    }

    fn term_docs(&self, term: &str) -> Result<Vec<DocId>> {
        match self.term_node(term)? {
            Some(node) => self.load_docs(node),
            None => Ok(Vec::new()),
        }
    }

    /// Listas das completações de `prefix` unidas por documento, com o tf
    /// somado: o prefixo como se fosse um termo só.
    fn prefix_postings(&self, prefix: &str) -> Result<Vec<Posting>> {
        let mut all: Vec<Posting> = Vec::new();
        for (_, node) in self.words_with_prefix(prefix, self.engine.config.max_prefix_terms)? {
            all.extend(self.load_postings(node)?);
        }
        all.sort_unstable_by_key(|p| p.doc);
        let mut merged: Vec<Posting> = Vec::with_capacity(all.len());
        for p in all {
            match merged.last_mut() {
                Some(last) if last.doc == p.doc => {
                    last.tf = last.tf.saturating_add(p.tf);
                    last.title_tf = last.title_tf.saturating_add(p.title_tf);
                }
                _ => merged.push(p),
            }
        }
        Ok(merged)
    }

    fn prefix_docs(&self, prefix: &str) -> Result<Vec<DocId>> {
        let mut all = Vec::new();
        for (_, node_id) in self.words_with_prefix(prefix, self.engine.config.max_prefix_terms)? {
            all.extend(self.load_docs(node_id)?);
        }
        all.sort_unstable();
        all.dedup();
        Ok(all)
    }

    /// Busca em largura a partir do nó do prefixo. Retorna (palavra, id do nó).
    fn words_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let Some((start, _)) = self.find_node(prefix)? else { return Ok(out) };
        let depth = prefix.chars().count();
        let mut queue = VecDeque::from([(prefix.to_string(), start, depth)]);
        while out.len() < limit {
            let Some((word, id, depth)) = queue.pop_front() else { break };
            let node = self.require_node(id, depth)?;
            for (&ch, &child) in &node.children {
                let mut next = word.clone();
                next.push(ch);
                queue.push_back((next, child, depth + 1));
            }
            if node.is_end_of_word {
                out.push((word, id));
            }
        }
        Ok(out)
    }

    /// Percorre a Trie caractere a caractere: um `Get` no RocksDB por nível,
    /// menos nos níveis guardados no cache de nós.
    fn find_node(&self, term: &str) -> Result<Option<(u64, Arc<TrieNode>)>> {
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

    fn load_docs(&self, node_id: u64) -> Result<Vec<DocId>> {
        match self.snap.get_bytes(Key::new(POST, node_id).as_ref())? {
            Some(v) => codec::decode_docs(&v),
            None => Ok(Vec::new()),
        }
    }

    fn load_postings(&self, node_id: u64) -> Result<Vec<Posting>> {
        match self.snap.get_bytes(Key::new(POST, node_id).as_ref())? {
            Some(v) => codec::decode_postings(&v),
            None => Ok(Vec::new()),
        }
    }

    /// As listas `pos:` dos termos de uma frase, na ordem dos termos.
    fn load_positions(&self, phrase: &[(u32, u64)]) -> Result<Vec<rocksdb::DBPinnableSlice<'_>>> {
        phrase
            .iter()
            .map(|&(_, node)| {
                self.snap
                    .get_bytes(Key::new(POS, node).as_ref())?
                    .ok_or_else(|| Error::Corrupt(format!("termo do nó {node} sem posições")))
            })
            .collect()
    }

    fn norm_block(&self, block: u64) -> Result<Option<Arc<NormBlock>>> {
        if let Some(cached) = self.engine.norms.get(block, self.epoch) {
            return Ok(Some(cached));
        }
        let Some(bytes) = self.snap.get_bytes(Key::new(NORM, block).as_ref())? else { return Ok(None) };
        let decoded = Arc::new(NormBlock::decode(&bytes)?);
        self.engine.norms.offer(block, self.epoch, &decoded);
        Ok(Some(decoded))
    }
}

/// Avança `at` até a primeira entrada com documento ≥ `target` (busca
/// exponencial a partir da posição atual) e diz se ela é `target`.
#[inline]
fn seek(list: &[Posting], at: &mut usize, target: DocId) -> bool {
    let mut lo = *at;
    if lo >= list.len() {
        return false;
    }
    if list[lo].doc >= target {
        return list[lo].doc == target;
    }
    // list[lo] < target: dobra o passo até passar do alvo.
    let mut step = 1;
    let mut hi = lo + 1;
    while hi < list.len() && list[hi].doc < target {
        lo = hi;
        step *= 2;
        hi = lo + step;
    }
    let hi = hi.min(list.len());
    let found = lo + 1 + list[lo + 1..hi].partition_point(|p| p.doc < target);
    *at = found;
    found < list.len() && list[found].doc == target
}

/// Confere uma frase documento a documento (em ordem crescente).
struct PhraseMatcher<'s> {
    offsets: Vec<u32>,
    cursors: Vec<PositionsCursor<'s>>,
    positions: Vec<Vec<u32>>,
    at: Vec<usize>,
}

impl<'s> PhraseMatcher<'s> {
    fn new(phrase: &[(u32, u64)], slices: &'s [rocksdb::DBPinnableSlice<'_>]) -> Result<Self> {
        let cursors = slices.iter().map(|s| PositionsCursor::new(s)).collect::<Result<_>>()?;
        Ok(Self {
            offsets: phrase.iter().map(|&(o, _)| o).collect(),
            cursors,
            positions: vec![Vec::new(); phrase.len()],
            at: vec![0; phrase.len()],
        })
    }

    /// Quantas vezes a frase aparece em `doc`, e quantas dessas no título
    /// (as que começam antes de `title_span`). `first_only` para na primeira.
    fn count(&mut self, doc: DocId, first_only: bool, title_span: u32) -> Result<(u32, u32)> {
        for (cursor, positions) in self.cursors.iter_mut().zip(&mut self.positions) {
            if !cursor.seek(doc, positions)? {
                return Ok((0, 0));
            }
        }
        Ok(count_phrase(&self.offsets, &self.positions, &mut self.at, first_only, title_span))
    }
}

/// Quantos `p` existem com o termo `i` na posição `p + offsets[i]`, para todo
/// `i`, e quantos desses `p` ficam antes de `title_span` (no título). As listas
/// estão em ordem crescente, então basta um ponteiro por termo.
fn count_phrase(offsets: &[u32], positions: &[Vec<u32>], at: &mut [usize], first_only: bool, title_span: u32) -> (u32, u32) {
    at.fill(0);
    let (mut count, mut in_title) = (0, 0);
    'anchors: for &p0 in &positions[0] {
        let Some(start) = p0.checked_sub(offsets[0]) else { continue };
        for i in 1..positions.len() {
            let want = u64::from(start) + u64::from(offsets[i]);
            let list = &positions[i];
            while at[i] < list.len() && u64::from(list[at[i]]) < want {
                at[i] += 1;
            }
            match list.get(at[i]) {
                // As próximas âncoras são maiores: nenhuma casa mais.
                None => break 'anchors,
                Some(&p) if u64::from(p) != want => continue 'anchors,
                Some(_) => {}
            }
        }
        count += 1;
        in_title += u32::from(start < title_span);
        if first_only {
            break;
        }
    }
    (count, in_title)
}

/// Tamanho dos documentos de uma busca, um bloco de `norm:` por vez. Rápido
/// com IDs crescentes, mas aceita qualquer ordem. Os blocos já usados ficam
/// num mapa local: voltar a um bloco não passa de novo pela trava do cache
/// compartilhado.
struct NormCursor<'v, 'a> {
    view: &'v View<'a>,
    block_id: u64,
    block: Option<Arc<NormBlock>>,
    seen: FxHashMap<u64, Option<Arc<NormBlock>>>,
}

impl<'v, 'a> NormCursor<'v, 'a> {
    fn new(view: &'v View<'a>) -> Self {
        Self { view, block_id: u64::MAX, block: None, seen: FxHashMap::default() }
    }

    #[inline]
    fn lens(&mut self, doc: DocId) -> Result<DocLens> {
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

/// Os `k` melhores resultados vistos até agora.
struct TopK {
    k: usize,
    /// O topo do heap é o pior dos guardados.
    heap: BinaryHeap<Worst>,
}

/// Ordem de "pior": nota menor; no empate, ID maior.
struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.score.total_cmp(&self.0.score).then(self.0.doc.cmp(&other.0.doc))
    }
}

impl TopK {
    fn new(k: usize) -> Self {
        Self { k, heap: BinaryHeap::with_capacity(k.min(1 << 16) + 1) }
    }

    /// Nota do pior guardado, quando já há `k`: só entra quem passar dela.
    #[inline]
    fn threshold(&self) -> Option<f64> {
        self.worst().map(|w| w.score)
    }

    /// O pior guardado, quando já há `k`.
    #[inline]
    fn worst(&self) -> Option<Hit> {
        if self.heap.len() < self.k { None } else { self.heap.peek().map(|w| w.0) }
    }

    #[inline]
    fn push(&mut self, hit: Hit) {
        if self.heap.len() < self.k {
            self.heap.push(Worst(hit));
        } else if let Some(mut worst) = self.heap.peek_mut()
            && Worst(hit) < *worst
        {
            *worst = Worst(hit);
        }
    }

    fn into_sorted(self) -> Vec<Hit> {
        // `into_sorted_vec` é crescente na ordem de "pior": o melhor primeiro.
        self.heap.into_sorted_vec().into_iter().map(|w| w.0).collect()
    }
}

/// Lote de indexação. Acumula as mudanças em memória e grava tudo de uma vez,
/// atomicamente, em [`IndexBatch::commit`]. Se for descartado sem commit,
/// nada é gravado. Buscas concorrentes nunca veem um lote pela metade.
///
/// O lote é invertido: `add_document` só junta termo → documentos (com tf e
/// posições), e a Trie é percorrida no commit, uma vez por termo distinto do
/// lote (e não uma vez por documento que contém o termo).
pub struct IndexBatch<'a> {
    engine: &'a Engine,
    state: MutexGuard<'a, WriterState>,
    next_node_id: u64,
    next_doc_id: DocId,
    /// Cache de nós emprestado do `WriterState`, mais os nós lidos ou criados
    /// neste lote. Nós criados aqui ainda não existem no banco, então as
    /// travessias seguintes precisam enxergá-los por este mapa.
    nodes: FxHashMap<u64, CachedNode>,
    generation: u64,
    cache_limit: usize,
    /// Nós alterados neste lote, na ordem em que mudaram.
    dirty: Vec<u64>,
    /// Termo -> documentos deste lote que o contêm.
    pending: FxHashMap<String, TermAcc>,
    /// (documento, tamanho inteiro, tamanho do título, extensão do título),
    /// na ordem em que foram adicionados.
    lengths: Vec<(DocId, u32, u32, u32)>,
    labels: Vec<(DocId, String)>,
    analyzer: Analyzer,
}

/// O que o lote juntou de um termo.
#[derive(Default)]
struct TermAcc {
    /// (documento, tf, tf no título), na ordem em que os documentos foram adicionados.
    docs: Vec<(DocId, u32, u32)>,
    /// `tf` posições por entrada de `docs`, em sequência (vazio sem posições).
    positions: Vec<u32>,
}

impl TermAcc {
    /// Ordena por documento. Para o mesmo documento adicionado duas vezes no
    /// lote, a última vence (como entre lotes, no merge operator).
    fn normalize(&mut self, positions: bool) {
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

/// Caminho na Trie do último termo inserido no commit: `ids[d]` é o nó na
/// profundidade `d` (`ids[0]` é a raiz) e `chars` são os caracteres até ele.
#[derive(Default)]
struct TriePath {
    chars: Vec<char>,
    ids: Vec<u64>,
}

struct CachedNode {
    node: TrieNode,
    dirty: bool,
    /// Lote que usou o nó por último.
    used: u64,
    /// Distância até a raiz. Os nós rasos são os mais compartilhados.
    depth: u8,
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
        let (len, title_len) = self.analyzer.run_fields(title, text, &self.engine.stopwords);
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
        for (term, mut acc) in terms {
            let t = Instant::now();
            let id = self.insert_term(&term, &mut path)?;
            trie += t.elapsed();
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
        self.state.next_node_id = self.next_node_id;
        self.state.next_doc_id = self.next_doc_id;
        self.state.stats = stats_index;
        stats.total_secs = started.elapsed().as_secs_f64();
        Ok(stats)
    }

    /// Atualiza os blocos de `norm:` tocados pelo lote e as estatísticas.
    /// Um documento que já existia troca de tamanho e não conta de novo.
    /// Devolve as estatísticas novas e os blocos gravados.
    fn write_lengths(&self, wb: &mut WriteBatch) -> Result<(IndexStats, Vec<u64>)> {
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

    /// Cria (se preciso) o caminho de `term` na Trie e devolve o nó terminal.
    ///
    /// Os termos do lote chegam em ordem alfabética, e vizinhos compartilham o
    /// começo ("casa", "casaco", "casamento"): `path` guarda o caminho do
    /// termo anterior, e a descida recomeça do fim do prefixo comum em vez da
    /// raiz. Os nós desse caminho já estão no mapa do lote (o descarte só
    /// acontece depois do commit).
    fn insert_term(&mut self, term: &str, path: &mut TriePath) -> Result<u64> {
        if path.ids.is_empty() {
            path.ids.push(ROOT_ID);
        }
        let common = path.chars.iter().zip(term.chars()).take_while(|(a, b)| *a == b).count();
        path.chars.truncate(common);
        path.ids.truncate(common + 1);
        let mut id = path.ids[common];
        // Termos têm no máximo MAX_TOKEN_CHARS (64) caracteres.
        let mut depth = u8::try_from(common).unwrap_or(u8::MAX);
        for ch in term.chars().skip(common) {
            let next_free = self.next_node_id;
            let generation = self.generation;
            let parent = self.node_mut(id, depth)?;
            id = match parent.node.children.get(&ch) {
                Some(&child) => child,
                None => {
                    parent.node.children.insert(ch, next_free);
                    if !parent.dirty {
                        parent.dirty = true;
                        self.dirty.push(id);
                    }
                    self.next_node_id += 1;
                    self.dirty.push(next_free);
                    let node = CachedNode { node: TrieNode::default(), dirty: true, used: generation, depth: depth + 1 };
                    self.nodes.insert(next_free, node);
                    next_free
                }
            };
            depth = depth.saturating_add(1);
            path.chars.push(ch);
            path.ids.push(id);
        }
        let leaf = self.node_mut(id, depth)?;
        if !leaf.node.is_end_of_word {
            leaf.node.is_end_of_word = true;
            if !leaf.dirty {
                leaf.dirty = true;
                self.dirty.push(id);
            }
        }
        Ok(id)
    }

    /// Nó do cache, lendo do banco na primeira vez.
    fn node_mut(&mut self, id: u64, depth: u8) -> Result<&mut CachedNode> {
        let generation = self.generation;
        match self.nodes.entry(id) {
            Entry::Occupied(e) => {
                let cached = e.into_mut();
                cached.used = generation;
                Ok(cached)
            }
            Entry::Vacant(e) => {
                let (node, dirty) = match load_node(&self.engine.db, id)? {
                    Some(node) => (node, false),
                    // Banco novo: a raiz nasce no primeiro commit.
                    None if id == ROOT_ID => (TrieNode::default(), true),
                    None => return Err(missing_node(id)),
                };
                if dirty {
                    self.dirty.push(id);
                }
                Ok(e.insert(CachedNode { node, dirty, used: generation, depth }))
            }
        }
    }
}

/// Mantém o cache de nós dentro de `limit`. Quando passa, descarta até 80%
/// do limite (para não descartar a cada lote): ficam os nós usados mais
/// recentemente e, no lote em que o corte cai, os mais rasos, que são os mais
/// compartilhados.
///
/// Antes ficavam só os nós usados pelo último lote. Na Wikipédia isso jogava
/// fora ~75% do cache a cada vez que ele enchia (1 milhão → 240 mil nós), e as
/// leituras do escritor subiam de ~7 para ~18 por artigo logo depois.
fn evict(nodes: &mut FxHashMap<u64, CachedNode>, generation: u64, limit: usize) {
    if nodes.len() <= limit {
        return;
    }
    let target = limit - limit / 5;
    // Nós por (idade em lotes, profundidade). Idades acima de MAX_AGE contam juntas.
    const MAX_AGE: usize = 255;
    let age = |c: &CachedNode| (generation.saturating_sub(c.used) as usize).min(MAX_AGE);
    let mut per_age = vec![[0usize; 256]; MAX_AGE + 1];
    for c in nodes.values() {
        per_age[age(c)][c.depth as usize] += 1;
    }
    // Maior idade que cabe inteira no alvo; da seguinte, os mais rasos que couberem.
    let mut kept = 0usize;
    let mut cut_age = None;
    for (a, depths) in per_age.iter().enumerate() {
        let n: usize = depths.iter().sum();
        if kept + n > target {
            cut_age = Some(a);
            break;
        }
        kept += n;
    }
    let Some(cut_age) = cut_age else { return };
    let mut max_depth = None;
    for (d, &n) in per_age[cut_age].iter().enumerate() {
        if kept + n > target {
            break;
        }
        kept += n;
        max_depth = Some(d);
    }
    nodes.retain(|_, c| {
        let a = age(c);
        a < cut_age || (a == cut_age && max_depth.is_some_and(|d| c.depth as usize <= d))
    });
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
trait Source {
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

fn load_node(src: &impl Source, id: u64) -> Result<Option<TrieNode>> {
    src.get_bytes(Key::new(NODE, id).as_ref())?.map(|v| TrieNode::decode(&v)).transpose()
}

fn missing_node(id: u64) -> Error {
    Error::Corrupt(format!("nó {id} referenciado mas ausente"))
}

/// Confere a versão do formato e se o banco tem posições. Num banco novo,
/// grava as duas coisas.
fn resolve_format(db: &DB, wanted_positions: Option<bool>) -> Result<bool> {
    match read_counter(db, META_FORMAT)? {
        Some(FORMAT_VERSION) => {
            let stored = read_counter(db, META_POSITIONS)?
                .ok_or_else(|| Error::Corrupt("meta:positions ausente".into()))?
                != 0;
            match wanted_positions {
                Some(wanted) if wanted != stored => Err(Error::Config(format!(
                    "o banco foi criado {} posições, mas a config pede {}",
                    if stored { "com" } else { "sem" },
                    if wanted { "com" } else { "sem" }
                ))),
                _ => Ok(stored),
            }
        }
        Some(v) => Err(Error::Config(format!(
            "banco no formato {v}, mas este binário lê o formato {FORMAT_VERSION}; recarregue o banco"
        ))),
        None if read_counter(db, META_NEXT_NODE)?.is_some() => Err(old_format()),
        None => {
            let positions = wanted_positions.unwrap_or(true);
            let mut wb = WriteBatch::default();
            wb.put(META_FORMAT, varint(FORMAT_VERSION));
            wb.put(META_POSITIONS, varint(u64::from(positions)));
            db.write(wb)?;
            Ok(positions)
        }
    }
}

fn old_format() -> Error {
    Error::Config("formato antigo (v1, sem tf nem posições); recarregue o banco".into())
}

/// O RocksDB grava as opções da última abertura em `OPTIONS-<n>`, inclusive o
/// nome do merge operator, que muda a cada formato.
fn previous_merge_operator(path: &Path) -> Option<String> {
    let entries = std::fs::read_dir(path).ok()?;
    let (_, latest) = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u64 = name.strip_prefix("OPTIONS-")?.parse().ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)?;
    let options = std::fs::read_to_string(latest).ok()?;
    let name = options.lines().find_map(|l| l.trim().strip_prefix("merge_operator="))?;
    // Sem merge operator o RocksDB grava "nullptr" (ou nada).
    (name != "nullptr" && !name.is_empty()).then(|| name.to_string())
}

/// Lê o formato gravado sem abrir para escrita (sem merge operator, sem
/// recuperar o WAL) e monta o erro: v1 (sem `meta:format`) ou outra versão.
fn stored_format_error(path: &Path) -> Option<Error> {
    let db = DB::open_for_read_only(&Options::default(), path, false).ok()?;
    match read_counter(&db, META_FORMAT).ok()? {
        None if read_counter(&db, META_NEXT_NODE).ok()?.is_some() => Some(old_format()),
        Some(v) if v != FORMAT_VERSION => Some(Error::Config(format!(
            "banco no formato {v}, mas este binário lê o formato {FORMAT_VERSION}; recarregue o banco"
        ))),
        _ => None,
    }
}

/// Idiomas das stopwords do banco. Num banco sem a chave, grava os da config
/// (ou o padrão).
fn resolve_languages(db: &DB, wanted: Option<&[Language]>) -> Result<Vec<Language>> {
    let normalize = |langs: &[Language]| Stopwords::new(langs).languages().to_vec();
    let stored = match db.get_pinned(META_LANGUAGES)? {
        Some(v) => Some(parse_languages(&String::from_utf8_lossy(&v))?),
        None => None,
    };
    let languages = match (stored, wanted) {
        (Some(stored), Some(wanted)) if normalize(wanted) != stored => {
            return Err(Error::Config(format!(
                "o banco foi criado com stopwords de [{}], mas a config pede [{}]",
                stopwords::describe(&stored),
                stopwords::describe(&normalize(wanted))
            )));
        }
        (Some(stored), _) => stored,
        (None, wanted) => normalize(wanted.unwrap_or(Language::DEFAULT)),
    };
    if db.get_pinned(META_LANGUAGES)?.is_none() {
        let codes: Vec<&str> = languages.iter().map(|l| l.code()).collect();
        db.put(META_LANGUAGES, codes.join(","))?;
    }
    Ok(languages)
}

fn parse_languages(codes: &str) -> Result<Vec<Language>> {
    codes
        .split(',')
        .filter(|c| !c.trim().is_empty())
        .map(|c| Language::from_code(c).ok_or_else(|| Error::Corrupt(format!("idioma desconhecido no banco: {c:?}"))))
        .collect()
}

fn read_counter(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    db.get_pinned(key)?.map(|v| codec::get_varint(&v)).transpose()
}

fn varint(v: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    codec::put_varint(&mut buf, v);
    buf
}

/// Merge operator de `post:` e `pos:`: une as listas por documento, com a
/// gravação mais nova vencendo (ver `codec::merge_postings`). É associativo,
/// então o RocksDB pode combinar pedaços consecutivos em qualquer agrupamento.
/// Devolver `None` (dado corrompido) faz a leitura falhar em vez de mentir.
fn merge_operator(key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    let inputs = existing.into_iter().chain(operands.iter());
    if key.starts_with(POST) {
        codec::merge_postings(inputs)
    } else if key.starts_with(POS) {
        codec::merge_positions(inputs)
    } else {
        None
    }
}

/// Interseção de duas listas ordenadas.
pub fn intersect(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(used: u64, depth: u8) -> CachedNode {
        CachedNode { node: TrieNode::default(), dirty: false, used, depth }
    }

    #[test]
    fn evict_keeps_the_most_recent_and_shallowest() {
        let mut nodes: FxHashMap<u64, CachedNode> = FxHashMap::default();
        // Lote 10 (o atual): 40 nós; lote 9: 40; lote 3: 40, metade rasa.
        for i in 0..40 {
            nodes.insert(i, cached(10, 5));
            nodes.insert(100 + i, cached(9, 5));
            nodes.insert(200 + i, cached(3, if i < 20 { 1 } else { 8 }));
        }
        evict(&mut nodes, 10, 200); // abaixo do limite: nada sai
        assert_eq!(nodes.len(), 120);
        // Limite 100 → alvo 80: ficam os lotes 10 e 9 inteiros.
        evict(&mut nodes, 10, 100);
        assert_eq!(nodes.len(), 80);
        assert!(nodes.keys().all(|&k| k < 140));
        // Alvo 60 (limite 75): o lote 10 inteiro e 20 do lote 9, os mais rasos.
        for i in 0..20 {
            nodes.get_mut(&(100 + i)).unwrap().depth = 2;
        }
        evict(&mut nodes, 10, 75);
        assert_eq!(nodes.len(), 60);
        assert!((0..40).all(|k| nodes.contains_key(&k)));
        assert!((100..120).all(|k| nodes.contains_key(&k)));
        // O lote atual sozinho passa do alvo: ficam os mais rasos dele.
        evict(&mut nodes, 10, 10);
        assert!(nodes.len() <= 8);
    }

    #[test]
    fn keys_without_allocation() {
        assert_eq!(Key::new(POST, 0).as_ref(), b"post:0");
        assert_eq!(Key::new(NORM, 1234567890).as_ref(), b"norm:1234567890");
        assert_eq!(Key::new(NODE, u64::MAX).as_ref(), format!("node:{}", u64::MAX).as_bytes());
    }

    #[test]
    fn seek_gallops_and_stops_at_first_not_less() {
        let list: Vec<Posting> = [1u64, 3, 5, 7, 9, 11, 100, 101].iter().map(|&doc| Posting::new(doc, 1)).collect();
        let mut at = 0;
        assert!(seek(&list, &mut at, 1));
        assert!(!seek(&list, &mut at, 4));
        assert_eq!(list[at].doc, 5);
        assert!(seek(&list, &mut at, 100));
        assert!(seek(&list, &mut at, 100), "parado no alvo");
        assert!(!seek(&list, &mut at, 102));
        assert!(!seek(&list, &mut at, 103));
        assert!(!seek(&[], &mut 0, 1));
    }

    #[test]
    fn phrase_counting() {
        let mut at = vec![0; 3];
        // casa@0 papel@2: "casa X papel" em 0 e 10; casa em 5 sem papel em 7.
        let pos = vec![vec![0, 5, 10], vec![2, 12]];
        assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 0), (2, 0));
        assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], true, 0), (1, 0));
        // Título com extensão 3: a ocorrência que começa em 0 está nele, a de 10 não.
        assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 3), (2, 1));
        // Termo repetido: novo@0 novo@2 em "novo de novo de novo" (0, 2, 4).
        let pos = vec![vec![0, 2, 4], vec![0, 2, 4]];
        assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 0), (2, 0));
        // Ordem trocada não casa.
        let pos = vec![vec![3], vec![2]];
        assert_eq!(count_phrase(&[0, 1], &pos, &mut at[..2], false, 0), (0, 0));
        // Três termos.
        let pos = vec![vec![1, 8], vec![2, 9], vec![4, 20]];
        assert_eq!(count_phrase(&[0, 1, 3], &pos, &mut at, false, 0), (1, 0));
    }

    #[test]
    fn top_k_orders_by_score_then_id() {
        let mut top = TopK::new(3);
        for (doc, score) in [(1, 0.5), (2, 2.0), (3, 0.5), (4, 1.0), (5, 0.5), (6, 2.0)] {
            top.push(Hit { doc, score });
        }
        let got: Vec<(DocId, f64)> = top.into_sorted().into_iter().map(|h| (h.doc, h.score)).collect();
        assert_eq!(got, [(2, 2.0), (6, 2.0), (4, 1.0)]);
        let mut empty = TopK::new(0);
        empty.push(Hit { doc: 1, score: 1.0 });
        assert!(empty.into_sorted().is_empty());
    }

    #[test]
    fn epoch_cache_respects_snapshots() {
        let cache = EpochCache::<NormBlock>::default();
        let a = Arc::new(NormBlock::default());
        let mut b = NormBlock::default();
        b.set(1, DocLens { len: 10, title_len: 0, title_span: 0 });
        let b = Arc::new(b);
        // Nunca mudou: qualquer busca usa.
        cache.offer(7, 0, &a);
        assert!(cache.get(7, 0).is_some());
        // O escritor vai mudar o bloco 7 no commit 1.
        cache.mark([7], 1);
        assert!(cache.get(7, 0).is_none(), "busca antiga relê do próprio snapshot");
        cache.offer(7, 0, &a);
        assert!(cache.get(7, 1).is_none(), "busca antiga não guarda o bloco velho");
        // Commit 1 publicado: uma busca nova guarda o que leu.
        cache.offer(7, 1, &b);
        assert_eq!(cache.get(7, 1).unwrap().get(1), Some(10));
        assert!(cache.get(7, 0).is_none());
        // Uma chave nunca marcada, oferecida por uma busca antiga, vale para todas.
        cache.offer(8, 0, &a);
        assert!(cache.get(8, 5).is_some());
    }
}
