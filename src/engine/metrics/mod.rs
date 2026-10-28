//! Métricas de custo desprezível ([`Metrics`]): no caminho quente, só somas
//! atômicas relaxadas num shard da própria thread. A thread amostradora
//! (`sampler`) soma os shards e grava o [`snapshot::Snapshot`] que o monitor lê.
//! Ver `docs/plano-monitoramento.md`.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use crate::engine::writer::CommitStats;


pub mod snapshot;
#[cfg(test)]
mod tests;

use self::snapshot::{COMMIT_BOUNDS, CacheCounters, LAYOUT, OpCounters, QUERY_BOUNDS, Snapshot, Writer};
use crate::engine::meta::FORMAT_VERSION;

/// Os pontos de entrada da API que as consultas contam (`op`). Público só
/// para o microbench (`internals`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    SearchTop,
    SearchRanked,
    Suggest,
    Postings,
    TermPostings,
}

impl Op {
    const ALL: [Op; OPS] = [Op::Search, Op::SearchTop, Op::SearchRanked, Op::Suggest, Op::Postings, Op::TermPostings];

    fn name(self) -> &'static str {
        match self {
            Op::Search => "search",
            Op::SearchTop => "search_top",
            Op::SearchRanked => "search_ranked",
            Op::Suggest => "suggest",
            Op::Postings => "postings",
            Op::TermPostings => "term_postings",
        }
    }
}

const OPS: usize = 6;
/// Os baldes com limite, mais o `+Inf`.
const BUCKETS: usize = QUERY_BOUNDS.len() + 1;

/// Limites dos baldes em nanossegundos (o registro compara inteiros).
const QUERY_BOUNDS_NANOS: [u64; BUCKETS - 1] = nanos(QUERY_BOUNDS);
const COMMIT_BOUNDS_NANOS: [u64; BUCKETS - 1] = nanos(COMMIT_BOUNDS);

const fn nanos(bounds: [f64; BUCKETS - 1]) -> [u64; BUCKETS - 1] {
    let mut out = [0; BUCKETS - 1];
    let mut i = 0;
    while i < out.len() {
        out[i] = (bounds[i] * 1e9) as u64;
        i += 1;
    }
    out
}

#[inline]
fn bucket(bounds: &[u64; BUCKETS - 1], nanos: u64) -> usize {
    bounds.partition_point(|&b| b < nanos)
}

/// Os contadores das consultas de um grupo de threads. Uma linha de cache
/// por shard: contadores compartilhados fariam a linha pular entre os
/// núcleos a cada consulta, justamente onde a leitura escala.
#[repr(align(128))]
#[derive(Default)]
struct Shard {
    count: [AtomicU64; OPS],
    errors: [AtomicU64; OPS],
    buckets: [[AtomicU64; BUCKETS]; OPS],
    nanos: [AtomicU64; OPS],
    reads: [AtomicU64; OPS],
    /// Acertos e faltas: nós, depois blocos de `norm:`.
    cache: [AtomicU64; 4],
}

/// Indexação, edição, remoção e compactação: uma soma por commit (há um
/// escritor por vez), sem shards.
#[derive(Default)]
struct WriterCounters {
    commits: AtomicU64,
    commit_errors: AtomicU64,
    commit_buckets: [AtomicU64; BUCKETS],
    commit_nanos: AtomicU64,
    phase_nanos: [AtomicU64; 7],
    indexed: AtomicU64,
    replaced: AtomicU64,
    deleted: AtomicU64,
    killed: AtomicU64,
    batch_bytes: AtomicU64,
    lock_wait_nanos: AtomicU64,
    cache_nodes: AtomicU64,
    documents: AtomicU64,
    dead_versions: AtomicU64,
    pending_edits: AtomicU64,
    compactions: AtomicU64,
    compaction_nanos: AtomicU64,
    purged: AtomicU64,
    emptied_words: AtomicU64,
}

/// As métricas de um banco aberto com `Config::metrics`. Público só para o
/// microbench (`internals`).
pub struct Metrics {
    shards: Box<[Shard]>,
    /// `shards.len() - 1` (potência de 2).
    mask: usize,
    writer: WriterCounters,
}

/// Próximo número de thread; cada thread guarda o seu na primeira consulta.
static NEXT_THREAD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static THREAD: Cell<usize> = const { Cell::new(usize::MAX) };
}

#[inline]
fn thread_slot() -> usize {
    THREAD.with(|t| {
        let slot = t.get();
        if slot != usize::MAX {
            return slot;
        }
        let slot = NEXT_THREAD.fetch_add(1, Relaxed);
        t.set(slot);
        slot
    })
}

/// Acertos e faltas dos caches de época de uma busca. Ficam na própria busca
/// (`View`) e vão para o shard uma vez, no fim dela.
#[derive(Default)]
pub(crate) struct CacheTally {
    pub(crate) node_hits: Cell<u32>,
    pub(crate) node_misses: Cell<u32>,
    pub(crate) norm_hits: Cell<u32>,
    pub(crate) norm_misses: Cell<u32>,
}

impl CacheTally {
    /// Soma 1 em `cell`. Sem a feature `metrics`, não faz nada.
    #[inline(always)]
    pub(crate) fn add(cell: &Cell<u32>) {
        if cfg!(feature = "metrics") {
            cell.set(cell.get().wrapping_add(1));
        }
    }
}

impl Metrics {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let n = threads.next_power_of_two().clamp(1, 64);
        Self { shards: (0..n).map(|_| Shard::default()).collect(), mask: n - 1, writer: WriterCounters::default() }
    }

    #[inline]
    fn shard(&self) -> &Shard {
        &self.shards[thread_slot() & self.mask]
    }

    /// Uma consulta de `op` que levou `elapsed` e fez `reads` `Get`s.
    #[inline]
    pub fn record_query(&self, op: Op, elapsed: Duration, reads: u64, failed: bool) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let s = self.shard();
        let i = op as usize;
        s.count[i].fetch_add(1, Relaxed);
        s.buckets[i][bucket(&QUERY_BOUNDS_NANOS, nanos)].fetch_add(1, Relaxed);
        s.nanos[i].fetch_add(nanos, Relaxed);
        s.reads[i].fetch_add(reads, Relaxed);
        if failed {
            s.errors[i].fetch_add(1, Relaxed);
        }
    }

    /// Os acertos e faltas de uma busca nos caches de época.
    #[inline]
    pub(crate) fn record_cache(&self, tally: &CacheTally) {
        let s = self.shard();
        let counts = [&tally.node_hits, &tally.node_misses, &tally.norm_hits, &tally.norm_misses];
        for (slot, count) in s.cache.iter().zip(counts) {
            if count.get() > 0 {
                slot.fetch_add(u64::from(count.get()), Relaxed);
            }
        }
    }

    /// Um commit gravado. `documents`: documentos no banco depois dele.
    pub fn record_commit(&self, stats: &CommitStats, documents: u64) {
        let w = &self.writer;
        let nanos = (stats.total_secs * 1e9) as u64;
        w.commits.fetch_add(1, Relaxed);
        w.commit_buckets[bucket(&COMMIT_BOUNDS_NANOS, nanos)].fetch_add(1, Relaxed);
        w.commit_nanos.fetch_add(nanos, Relaxed);
        let phases = [
            stats.sort_secs,
            stats.trie_secs,
            stats.encode_secs,
            stats.documents_secs,
            stats.nodes_secs,
            stats.write_secs,
            stats.evict_secs,
        ];
        for (slot, secs) in w.phase_nanos.iter().zip(phases) {
            slot.fetch_add((secs * 1e9) as u64, Relaxed);
        }
        w.indexed.fetch_add(stats.docs as u64, Relaxed);
        w.replaced.fetch_add(stats.replaced as u64, Relaxed);
        w.deleted.fetch_add(stats.deleted as u64, Relaxed);
        w.killed.fetch_add(stats.killed as u64, Relaxed);
        w.batch_bytes.fetch_add(stats.batch_bytes as u64, Relaxed);
        w.cache_nodes.store(stats.cache_nodes as u64, Relaxed);
        w.documents.store(documents, Relaxed);
    }

    pub(crate) fn record_commit_error(&self) {
        self.writer.commit_errors.fetch_add(1, Relaxed);
    }

    /// Espera pela trava do escritor em `Engine::batch`.
    pub(crate) fn record_lock_wait(&self, waited: Duration) {
        self.writer.lock_wait_nanos.fetch_add(u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX), Relaxed);
    }

    /// Uma compactação que tirou `purged` versões mortas e `emptied` palavras.
    pub(crate) fn record_compaction(&self, took: Duration, purged: usize, emptied: usize) {
        let w = &self.writer;
        w.compactions.fetch_add(1, Relaxed);
        w.compaction_nanos.fetch_add(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX), Relaxed);
        w.purged.fetch_add(purged as u64, Relaxed);
        w.emptied_words.fetch_add(emptied as u64, Relaxed);
    }

    /// Versões mortas ainda nas listas e edições desde a última compactação.
    pub(crate) fn set_edit_backlog(&self, dead_versions: u64, pending_edits: u64) {
        self.writer.dead_versions.store(dead_versions, Relaxed);
        self.writer.pending_edits.store(pending_edits, Relaxed);
    }

    /// Documentos no banco (no `open`, antes do primeiro commit).
    pub(crate) fn set_documents(&self, documents: u64) {
        self.writer.documents.store(documents, Relaxed);
    }

    /// Um snapshot novo, só com quem grava e os contadores de agora (sem as
    /// propriedades do RocksDB).
    pub(crate) fn snapshot(&self, db: &str, opened_unix: f64) -> Snapshot {
        let mut s = Snapshot {
            layout: LAYOUT,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: FORMAT_VERSION,
            db: db.to_string(),
            opened_unix,
            ..Snapshot::default()
        };
        self.fill(&mut s);
        s
    }

    /// Põe os contadores de agora em `s`.
    pub(crate) fn fill(&self, s: &mut Snapshot) {
        (s.queries, s.cache, s.writer) = self.read();
    }

    /// Soma os shards. Fora do caminho quente: a amostradora chama a cada
    /// segundo.
    fn read(&self) -> (Vec<OpCounters>, CacheCounters, Writer) {
        let mut ops: Vec<OpCounters> = Op::ALL
            .iter()
            .map(|op| OpCounters { op: op.name().to_string(), buckets: vec![0; BUCKETS], ..OpCounters::default() })
            .collect();
        let mut cache = [0u64; 4];
        for s in self.shards.iter() {
            for (i, out) in ops.iter_mut().enumerate() {
                out.count += s.count[i].load(Relaxed);
                out.errors += s.errors[i].load(Relaxed);
                out.sum_nanos += s.nanos[i].load(Relaxed);
                out.reads += s.reads[i].load(Relaxed);
                for (b, slot) in out.buckets.iter_mut().zip(&s.buckets[i]) {
                    *b += slot.load(Relaxed);
                }
            }
            for (sum, slot) in cache.iter_mut().zip(&s.cache) {
                *sum += slot.load(Relaxed);
            }
        }
        let [node_hits, node_misses, norm_hits, norm_misses] = cache;
        let w = &self.writer;
        let load = |a: &AtomicU64| a.load(Relaxed);
        let writer = Writer {
            commits: load(&w.commits),
            commit_errors: load(&w.commit_errors),
            commit_buckets: w.commit_buckets.iter().map(load).collect(),
            commit_nanos: load(&w.commit_nanos),
            phase_nanos: w.phase_nanos.iter().map(load).collect(),
            indexed_documents: load(&w.indexed),
            replaced_documents: load(&w.replaced),
            deleted_documents: load(&w.deleted),
            killed_versions: load(&w.killed),
            batch_bytes: load(&w.batch_bytes),
            lock_wait_nanos: load(&w.lock_wait_nanos),
            cache_nodes: load(&w.cache_nodes),
            documents: load(&w.documents),
            dead_versions: load(&w.dead_versions),
            pending_edits: load(&w.pending_edits),
            compactions: load(&w.compactions),
            compaction_nanos: load(&w.compaction_nanos),
            purged_versions: load(&w.purged),
            emptied_words: load(&w.emptied_words),
        };
        (ops, CacheCounters { node_hits, node_misses, norm_hits, norm_misses }, writer)
    }
}
