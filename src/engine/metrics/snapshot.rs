//! O arquivo que o motor grava e o monitor lê ([`Snapshot`]): os contadores
//! somados, as propriedades do RocksDB e quem gravou.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Versão do layout do arquivo. Muda quando um campo muda de sentido; campos
/// novos entram com valor padrão.
pub const LAYOUT: u32 = 1;

/// Nome do arquivo dentro da pasta do banco, quando `Config::metrics_file` é
/// `None`.
pub const FILE_NAME: &str = "fire-flower.metrics.json";

/// Limites superiores (segundos) dos baldes do histograma das consultas; o
/// último balde (`+Inf`) fica implícito.
pub const QUERY_BOUNDS: [f64; 16] =
    [5e-6, 1e-5, 2.5e-5, 5e-5, 1e-4, 2.5e-4, 5e-4, 1e-3, 2.5e-3, 5e-3, 1e-2, 2.5e-2, 5e-2, 0.1, 0.25, 1.0];

/// Limites superiores (segundos) dos baldes do histograma dos commits.
pub const COMMIT_BOUNDS: [f64; 16] =
    [1e-3, 2.5e-3, 5e-3, 1e-2, 2.5e-2, 5e-2, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0];

/// Fases do commit, na ordem de [`Writer::phase_nanos`].
pub const COMMIT_PHASES: [&str; 7] = ["sort", "trie", "encode", "documents", "nodes", "write", "evict"];

/// Uma leitura das métricas de um banco, gravada pela thread amostradora.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub layout: u32,
    /// Processo que gravou (o dono do banco).
    pub pid: u32,
    /// Versão do fire-flower e do formato do banco.
    pub version: String,
    pub format_version: u64,
    /// Pasta do banco.
    pub db: String,
    /// Quando o banco foi aberto e quando este snapshot foi gravado (Unix, s).
    pub opened_unix: f64,
    pub written_unix: f64,
    /// Cresce a cada snapshot gravado.
    pub sequence: u64,
    /// De quanto em quanto tempo a amostradora grava (s).
    pub interval_secs: f64,
    /// Uma entrada por ponto de entrada da API (`op`).
    pub queries: Vec<OpCounters>,
    pub cache: CacheCounters,
    pub writer: Writer,
    /// Propriedades do RocksDB (lidas a cada 5 s); `None` antes da primeira.
    pub rocksdb: Option<RocksdbProps>,
    /// CPU gasta pela própria thread amostradora (s), se o sistema disser.
    pub sampler_cpu_secs: Option<f64>,
}

/// Contadores de um ponto de entrada da API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpCounters {
    pub op: String,
    pub count: u64,
    pub errors: u64,
    /// Consultas por balde de [`QUERY_BOUNDS`], mais o `+Inf` (não acumulado).
    pub buckets: Vec<u64>,
    pub sum_nanos: u64,
    /// `Get`s do motor nessas consultas.
    pub reads: u64,
}

/// Consultas aos caches de época (nós rasos da Trie e blocos de `norm:`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheCounters {
    pub node_hits: u64,
    pub node_misses: u64,
    pub norm_hits: u64,
    pub norm_misses: u64,
}

/// Indexação, edição, remoção e compactação.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Writer {
    pub commits: u64,
    pub commit_errors: u64,
    /// Commits por balde de [`COMMIT_BOUNDS`], mais o `+Inf` (não acumulado).
    pub commit_buckets: Vec<u64>,
    pub commit_nanos: u64,
    /// Tempo por fase, na ordem de [`COMMIT_PHASES`].
    pub phase_nanos: Vec<u64>,
    /// Documentos adicionados ou substituídos.
    pub indexed_documents: u64,
    pub replaced_documents: u64,
    pub deleted_documents: u64,
    /// Versões que morreram (substituídas ou removidas).
    pub killed_versions: u64,
    pub batch_bytes: u64,
    /// Espera pela trava do escritor em `Engine::batch`.
    pub lock_wait_nanos: u64,
    /// Nós no cache do escritor, documentos no banco, versões mortas nas
    /// listas e edições desde a última compactação (os quatro são valores
    /// do momento, não somas).
    pub cache_nodes: u64,
    pub documents: u64,
    pub dead_versions: u64,
    pub pending_edits: u64,
    pub compactions: u64,
    pub compaction_nanos: u64,
    pub purged_versions: u64,
    pub emptied_words: u64,
}

/// Propriedades baratas do RocksDB (sem estatísticas).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RocksdbProps {
    /// Quando foram lidas (Unix, s).
    pub read_unix: f64,
    /// Arquivos SST por nível (0 a 6).
    pub sst_files: Vec<u64>,
    pub live_sst_bytes: u64,
    pub total_sst_bytes: u64,
    pub pending_compaction_bytes: u64,
    pub running_compactions: u64,
    pub running_flushes: u64,
    pub memtable_flush_pending: u64,
    pub write_stopped: u64,
    pub delayed_write_rate: u64,
    pub background_errors: u64,
    pub memtables_bytes: u64,
    pub table_readers_bytes: u64,
    pub block_cache_bytes: u64,
    pub block_cache_pinned_bytes: u64,
    pub block_cache_capacity_bytes: u64,
    pub snapshots: u64,
    /// Quando o snapshot do RocksDB mais antigo foi criado (Unix, s; 0 = nenhum).
    pub oldest_snapshot_unix: u64,
}

/// Onde o motor grava o snapshot de um banco, por padrão.
pub fn default_path(db: &Path) -> PathBuf {
    db.join(FILE_NAME)
}

/// Lê um snapshot gravado pelo motor.
pub fn read(path: &Path) -> std::io::Result<Snapshot> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
