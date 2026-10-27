//! `wiki bench`: latência e vazão dos conjuntos de consulta.

use std::path::{Path, PathBuf};
use std::time::Instant;

use fire_flower::{Config, Engine};
use rocksdb::perf::{PerfContext, PerfStatsLevel, set_perf_stats};
use rustc_hash::FxHashMap as HashMap;
use serde_json::{Value, json};

use crate::AnyResult;
use crate::corpus::stem;
use crate::env::{config_json, save_result};

/// Como um conjunto é medido, pelo prefixo do nome do arquivo.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `search`: todos os IDs em ordem.
    Search,
    /// `s-*`: `suggest` com limite 10.
    Suggest,
    /// `rank-*`: `search_ranked` com os 10 melhores (BM25).
    Top(usize),
}

impl Mode {
    pub(crate) fn of(set_name: &str) -> Self {
        if set_name.starts_with("s-") {
            Mode::Suggest
        } else if set_name.starts_with("rank-") {
            Mode::Top(10)
        } else {
            Mode::Search
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Search => "search",
            Mode::Suggest => "suggest",
            Mode::Top(_) => "top10",
        }
    }
}

#[derive(Clone)]
pub(crate) struct QuerySet {
    pub(crate) name: String,
    pub(crate) mode: Mode,
    pub(crate) queries: Vec<String>,
}

pub(crate) fn read_query_sets(paths: &[PathBuf]) -> AnyResult<Vec<QuerySet>> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "txt")).collect();
            entries.sort();
            files.extend(entries);
        } else {
            files.push(p.clone());
        }
    }
    files
        .into_iter()
        .map(|f| {
            let name = stem(&f);
            let queries = std::fs::read_to_string(&f)?.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
            Ok(QuerySet { mode: Mode::of(&name), name, queries })
        })
        .collect()
}

#[derive(Default, Clone)]
pub(crate) struct Sample {
    pub(crate) nanos: u64,
    pub(crate) results: usize,
    pub(crate) gets: u64,
    pub(crate) cache_hits: u64,
    pub(crate) block_reads: u64,
    pub(crate) bloom_useful: u64,
    pub(crate) merges: u64,
    pub(crate) merge_nanos: u64,
}

/// Contadores do PerfContext lidos pelo nome. O enum `PerfMetric` do crate
/// rocksdb 0.23 está desalinhado com o RocksDB 10.4 embutido (lê o contador
/// errado), então usamos o relatório em texto ("nome = valor, ...").
pub(crate) fn perf_counters(ctx: &PerfContext) -> HashMap<String, u64> {
    ctx.report(true)
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

pub(crate) fn run_query(engine: &Engine, set: &QuerySet, q: &str, ctx: &mut PerfContext) -> AnyResult<Sample> {
    ctx.reset();
    let reads_before = fire_flower::internals::reads_on_this_thread();
    let t = Instant::now();
    // Ranqueado: "resultados" é o total que casou, comparável com o `search`.
    let results = match set.mode {
        Mode::Search => engine.search(q)?.len(),
        Mode::Suggest => engine.suggest(q, 10)?.len(),
        Mode::Top(k) => engine.search_ranked(q, k)?.total,
    };
    let nanos = t.elapsed().as_nanos() as u64;
    let gets = fire_flower::internals::reads_on_this_thread() - reads_before;
    let c = perf_counters(ctx);
    let get = |k: &str| c.get(k).copied().unwrap_or(0);
    Ok(Sample {
        nanos,
        results,
        gets,
        cache_hits: get("block_cache_hit_count"),
        block_reads: get("block_read_count"),
        bloom_useful: get("bloom_sst_miss_count"),
        merges: get("internal_merge_point_lookup_count"),
        merge_nanos: get("merge_operator_time_nanos"),
    })
}

/// Roda o conjunto em `threads` threads (consultas divididas entre elas).
pub(crate) fn run_set(engine: &Engine, set: &QuerySet, threads: usize, perf_level: PerfStatsLevel) -> AnyResult<(Vec<Sample>, f64)> {
    let started = Instant::now();
    let samples = std::thread::scope(|scope| -> AnyResult<Vec<Sample>> {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || -> Result<Vec<Sample>, String> {
                    set_perf_stats(perf_level); // o PerfContext é por thread
                    let mut ctx = PerfContext::default();
                    set.queries
                        .iter()
                        .skip(t)
                        .step_by(threads)
                        .map(|q| run_query(engine, set, q, &mut ctx).map_err(|e| e.to_string()))
                        .collect()
                })
            })
            .collect();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().map_err(|_| "thread de busca entrou em pânico")??);
        }
        Ok(all)
    })?;
    Ok((samples, started.elapsed().as_secs_f64()))
}

pub(crate) fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx] as f64 / 1000.0
}

pub(crate) fn summarize(name: &str, mode: &str, samples: &[Sample], wall: f64, threads: usize) -> Value {
    let mut lat: Vec<u64> = samples.iter().map(|s| s.nanos).collect();
    lat.sort_unstable();
    let n = samples.len().max(1) as f64;
    let avg = |f: fn(&Sample) -> u64| samples.iter().map(f).sum::<u64>() as f64 / n;
    json!({
        "set": name, "mode": mode, "queries": samples.len(), "threads": threads,
        "p50_us": percentile(&lat, 0.50), "p95_us": percentile(&lat, 0.95),
        "p99_us": percentile(&lat, 0.99), "max_us": percentile(&lat, 1.0),
        "mean_us": avg(|s| s.nanos) / 1000.0, "qps": samples.len() as f64 / wall,
        "mean_results": samples.iter().map(|s| s.results).sum::<usize>() as f64 / n,
        "gets_per_query": avg(|s| s.gets), "cache_hits_per_query": avg(|s| s.cache_hits),
        "block_reads_per_query": avg(|s| s.block_reads), "bloom_util_per_query": avg(|s| s.bloom_useful),
        "merges_per_query": avg(|s| s.merges), "merge_us_per_query": avg(|s| s.merge_nanos) / 1000.0,
    })
}

pub(crate) fn drop_os_caches() -> AnyResult {
    let ok = std::process::Command::new("sudo")
        .args(["-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err("--cold precisa esvaziar o cache do sistema com `sudo -n`. Rode `sudo -v` antes, ou meça sem --cold.".into())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench(db: &Path, paths: &[PathBuf], threads: usize, passes: usize, cold: bool, perf_time: bool, config: Config, label: Option<String>) -> AnyResult {
    let sets = read_query_sets(paths)?;
    // O enum do crate também está deslocado: `EnableTime` corresponde a medir tempos no RocksDB 10.
    let level = if perf_time { PerfStatsLevel::EnableTime } else { PerfStatsLevel::EnableCount };
    let mut engine = if cold { None } else { Some(Engine::open(db, config.clone())?) };
    let mut results = Vec::new();

    println!("| conjunto | p50 µs | p95 µs | p99 µs | máx µs | qps | resultados | Gets | block reads |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for set in &sets {
        if cold {
            drop(engine.take()); // fecha antes de esvaziar o cache
            drop_os_caches()?;
            engine = Some(Engine::open(db, config.clone())?);
        } else {
            run_set(engine.as_ref().unwrap(), set, threads, level)?; // aquecimento, descartado
        }
        let mut samples = Vec::new();
        let mut wall = 0.0;
        for _ in 0..if cold { 1 } else { passes } {
            let (s, w) = run_set(engine.as_ref().unwrap(), set, threads, level)?;
            samples.extend(s);
            wall += w;
        }
        let summary = summarize(&set.name, set.mode.name(), &samples, wall, threads);
        let f = |k: &str| summary[k].as_f64().unwrap_or(0.0);
        println!(
            "| {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.2} |",
            set.name, f("p50_us"), f("p95_us"), f("p99_us"), f("max_us"), f("qps"), f("mean_results"), f("gets_per_query"), f("block_reads_per_query")
        );
        results.push(summary);
    }
    let label = label.unwrap_or_else(|| {
        let db_name = stem(db);
        format!("{db_name}-t{threads}{}", if cold { "-frio" } else { "" })
    });
    save_result(
        "bench",
        &label,
        json!({ "db": db, "threads": threads, "passes": passes, "cold": cold, "perf_time": perf_time,
                "config": config_json(&config), "sets": results }),
    )?;
    Ok(())
}
