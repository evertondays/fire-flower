//! Abrir o banco: para leitura e escrita ou só leitura, as opções do
//! RocksDB, o formato gravado e as mensagens de erro dos casos comuns.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

use rocksdb::{BlockBasedIndexType, BlockBasedOptions, Cache, DB, DBCompressionType, Options};

use crate::{Engine, Error, IndexStats, Result};
use crate::analysis::Analyzer;
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::Stopwords;
use crate::engine::cache::EpochCache;
use crate::engine::config::Config;
use crate::engine::external;
use crate::engine::keys::{
    MERGE_OPERATOR, META_DOC_COUNT, META_EPOCH, META_INFLECTIONS, META_NEXT_DOC, META_NEXT_NODE, META_STEMMING,
    META_TOTAL_LEN, META_TOTAL_TITLE_LEN, ROOT_ID,
};
use crate::engine::meta::{
    previous_merge_operator, read_counter, resolve_format, resolve_languages, resolve_stemming, stored_format_error,
};
use crate::engine::metrics;
use crate::engine::metrics::Metrics;
use crate::engine::metrics::sampler::{Sampler, Target};
use crate::engine::purge::{self, FilterFactory, Purge};
use crate::engine::versions::{self, Deaths};
use crate::engine::writer::{NodeCache, WriterSlot, WriterState};

impl Engine {
    /// Abre (ou cria, com [`Config::create_if_missing`]) o banco para leitura
    /// e escrita. Só um processo por vez abre um banco assim: o RocksDB trava
    /// a pasta (`LOCK`).
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        Self::open_with(path.as_ref(), config, false, None)
    }

    /// Abre um banco que já existe só para leitura, ao lado do processo que o
    /// grava (que continua com o `LOCK`). Não cria nem altera arquivos e não
    /// roda compactação. A visão é a do momento da abertura: o que for gravado
    /// depois só aparece abrindo de novo. Lotes e [`Engine::compact`] dão erro.
    pub fn open_read_only(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        Self::open_with(path.as_ref(), config, true, None)
    }

    /// Como [`Engine::open_read_only`], com um block cache de quem chama: o
    /// `Reader` passa o mesmo a cada reabertura, e a instância nova já nasce
    /// com os blocos quentes.
    pub(super) fn open_read_only_with_cache(path: &Path, config: Config, cache: &Cache) -> Result<Self> {
        Self::open_with(path, config, true, Some(cache))
    }

    fn open_with(path: &Path, config: Config, read_only: bool, cache: Option<&Cache>) -> Result<Self> {
        config.validate()?;
        // Antes do RocksDB: ele cria a pasta mesmo sem `create_if_missing`.
        if (read_only || !config.create_if_missing) && !path.join("CURRENT").is_file() {
            return Err(not_a_database(path));
        }
        let mut table = BlockBasedOptions::default();
        table.set_bloom_filter(config.bloom_bits_per_key, false);
        match cache {
            Some(cache) => table.set_block_cache(cache),
            None => table.set_block_cache(&Cache::new_lru_cache(config.block_cache_bytes)),
        }
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
        if let (false, Some(bytes)) = (read_only, config.max_wal_bytes) {
            // Ver `Config::max_wal_bytes`. Com uma column family só, o WAL
            // acompanha a memtable (`max_total_wal_size` só vale com várias):
            // memtables menores, flushes mais cedo, WAL menor.
            opts.set_write_buffer_size(bytes as usize);
        }
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
        let open = || {
            let default_cf = rocksdb::ColumnFamilyDescriptor::new(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, opts.clone());
            if read_only {
                DB::open_cf_descriptors_read_only(&opts, path, [default_cf], false)
            } else {
                DB::open_cf_descriptors(&opts, path, [default_cf])
            }
        };
        let mut opened = open();
        // Só leitura, ao lado de quem grava: um WAL ou SST listado pode sumir
        // no meio da abertura (o outro processo terminou um flush ou uma
        // compactação). É passageiro; a próxima tentativa vê os arquivos novos.
        for _ in 0..READ_ONLY_RETRIES {
            match &opened {
                Err(e) if read_only && is_missing_file(e) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    opened = open();
                }
                _ => break,
            }
        }
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
                let msg = "Config::metrics pede a feature `metrics`, desligada nesta compilação";
                return Err(Error::Config(msg.into()));
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
        let writer = WriterSlot::new(WriterState {
            next_node_id,
            next_doc_id,
            stats,
            cache: NodeCache::default(),
            analyzer: Analyzer::default(),
            latest: loaded.latest,
            has_external_ids: external::any_external_ids(&db)?,
        });
        let db = Arc::new(db);
        let opened_unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let sampler = match &metrics {
            // Só leitura: a pasta é de quem grava; só com um arquivo próprio.
            Some(m) if !read_only || config.metrics_file.is_some() => {
                let file = config.metrics_file.clone().unwrap_or_else(|| metrics::snapshot::default_path(path));
                let target = Target {
                    db: Arc::clone(&db),
                    metrics: Arc::clone(m),
                    file,
                    db_path: path.display().to_string(),
                    opened_unix,
                    interval: config.metrics_interval,
                };
                Some(Sampler::spawn(target).map_err(|e| Error::Config(format!("snapshot das métricas: {e}")))?)
            }
            _ => None,
        };
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
            sampler,
            path: path.to_path_buf(),
            opened_unix,
        })
    }
}

/// O erro do RocksDB ao abrir, com uma mensagem melhor para os casos comuns:
/// a pasta não é um banco, ou outro processo o tem aberto para escrita.
fn open_error(path: &Path, e: rocksdb::Error) -> Error {
    let text = e.to_string();
    if !path.join("CURRENT").is_file() {
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

/// Quantas vezes `open_read_only` tenta de novo quando um arquivo some no
/// meio da abertura.
const READ_ONLY_RETRIES: usize = 5;

fn is_missing_file(e: &rocksdb::Error) -> bool {
    let text = e.to_string();
    text.contains("No such file") || text.contains("does not exist")
}

fn not_a_database(path: &Path) -> Error {
    Error::Open(format!("{} não é um banco do fire-flower (pasta vazia ou inexistente)", path.display()))
}
