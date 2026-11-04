//! A thread amostradora: soma os shards, lê as propriedades baratas do
//! RocksDB e grava o snapshot. Roda no processo dono do banco, fora do
//! caminho quente.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rocksdb::DB;

use crate::engine::metrics::Metrics;
use crate::engine::metrics::snapshot::{self, RocksdbProps, Snapshot};

/// De quantos em quantos snapshots as propriedades do RocksDB são relidas
/// (com o intervalo padrão de 1 s, a cada 5 s).
const PROPS_EVERY: u64 = 5;

/// A thread amostradora de um banco. Para (gravando um último snapshot) ao
/// ser descartada.
pub(crate) struct Sampler {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// O que a amostradora grava sobre o banco, além dos contadores.
pub(crate) struct Target {
    pub(crate) db: Arc<DB>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) file: PathBuf,
    pub(crate) db_path: String,
    pub(crate) opened_unix: f64,
    pub(crate) interval: Duration,
}

impl Sampler {
    /// Grava o primeiro snapshot já (o monitor acha o pid logo) e começa a
    /// thread. Um erro ao gravar o primeiro volta para o `open`.
    pub(crate) fn spawn(target: Target) -> std::io::Result<Self> {
        let mut state = State::new(target);
        state.sample()?;
        let (stop, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("fire-flower-metrics".into())
            .spawn(move || state.run(&stopped))?;
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    target: Target,
    snapshot: Snapshot,
    ticks: u64,
}

impl State {
    fn new(target: Target) -> Self {
        let mut snapshot = target.metrics.snapshot(&target.db_path, target.opened_unix);
        snapshot.interval_secs = target.interval.as_secs_f64();
        Self { target, snapshot, ticks: 0 }
    }

    fn run(mut self, stopped: &Receiver<()>) {
        loop {
            let stop = !matches!(stopped.recv_timeout(self.target.interval), Err(RecvTimeoutError::Timeout));
            // Um erro de disco não derruba o motor: o próximo tenta de novo, e
            // o monitor vê o snapshot envelhecer.
            let _ = self.sample();
            if stop {
                return;
            }
        }
    }

    fn sample(&mut self) -> std::io::Result<()> {
        let s = &mut self.snapshot;
        self.target.metrics.fill(s);
        if self.ticks.is_multiple_of(PROPS_EVERY) {
            s.rocksdb = Some(read_props(&self.target.db));
        }
        self.ticks += 1;
        s.sequence += 1;
        s.sampler_cpu_secs = thread_cpu_secs();
        s.written_unix = unix_now();
        snapshot::write(&self.target.file, s)
    }
}

/// As propriedades da lista do plano: nenhuma depende de estatísticas.
fn read_props(db: &DB) -> RocksdbProps {
    let int = |name: &str| db.property_int_value(name).ok().flatten().unwrap_or(0);
    // `num-files-at-level<N>` só existe como texto.
    let files = |level: usize| {
        db.property_value(&format!("rocksdb.num-files-at-level{level}"))
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    RocksdbProps {
        read_unix: unix_now(),
        sst_files: (0..7).map(files).collect(),
        live_sst_bytes: int("rocksdb.live-sst-files-size"),
        total_sst_bytes: int("rocksdb.total-sst-files-size"),
        pending_compaction_bytes: int("rocksdb.estimate-pending-compaction-bytes"),
        running_compactions: int("rocksdb.num-running-compactions"),
        running_flushes: int("rocksdb.num-running-flushes"),
        memtable_flush_pending: int("rocksdb.mem-table-flush-pending"),
        write_stopped: int("rocksdb.is-write-stopped"),
        delayed_write_rate: int("rocksdb.actual-delayed-write-rate"),
        background_errors: int("rocksdb.background-errors"),
        memtables_bytes: int("rocksdb.cur-size-all-mem-tables"),
        table_readers_bytes: int("rocksdb.estimate-table-readers-mem"),
        block_cache_bytes: int("rocksdb.block-cache-usage"),
        block_cache_pinned_bytes: int("rocksdb.block-cache-pinned-usage"),
        block_cache_capacity_bytes: int("rocksdb.block-cache-capacity"),
        snapshots: int("rocksdb.num-snapshots"),
        oldest_snapshot_unix: int("rocksdb.oldest-snapshot-time"),
    }
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// CPU (usuário + sistema) desta thread, pelo `/proc/thread-self/stat` (só Linux).
fn thread_cpu_secs() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    // Os campos depois do nome (que pode ter espaços e parênteses).
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime e stime são os campos 14 e 15 do arquivo (11 e 12 depois do nome).
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    Some(ticks as f64 / clock_ticks())
}

/// `sysconf(_SC_CLK_TCK)`: 100 em praticamente todo Linux.
fn clock_ticks() -> f64 {
    100.0
}
