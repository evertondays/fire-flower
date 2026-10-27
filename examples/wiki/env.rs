//! Ambiente da medição: commit, máquina, memória, caches do sistema e onde gravar os resultados.

use crate::AnyResult;
use fire_flower::Engine;
use rustc_hash::FxHashMap as HashMap;
use fire_flower::Config;
use serde_json::Value;
use std::path::Path;
use std::time::UNIX_EPOCH;
use std::time::SystemTime;
use std::path::PathBuf;
use serde_json::json;

pub(crate) fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Repositório dos resultados: `FF_BENCH_DIR` ou `../fire-flower-bench`.
pub(crate) fn bench_dir() -> PathBuf {
    match std::env::var_os("FF_BENCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => project_dir().parent().map_or_else(project_dir, PathBuf::from).join("fire-flower-bench"),
    }
}

pub(crate) fn git_commit() -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(project_dir())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let hash = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "desconhecido".into());
    // bench/ fica de fora: o perf.py regrava consultas, baseline e relatório no meio da rodada.
    let dirty = run(&["status", "--porcelain", "--untracked-files=no", "--", ".", ":!bench"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{hash}-modificado") } else { hash }
}

/// Data e hora UTC, sem dependências: "2026-10-25T18-04-11".
pub(crate) fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Algoritmo de Howard Hinnant: dias desde 1970 -> data civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// (RSS atual, pico de RSS) em bytes.
pub(crate) fn rss() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

pub(crate) fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
        })
        .unwrap_or(0)
}

pub(crate) fn machine() -> Value {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem = |name: &str| {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    json!({
        "nucleos": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "ram_total": mem("MemTotal:"),
        "ram_livre": mem("MemAvailable:"),
        "swap_usado": mem("SwapTotal:").saturating_sub(mem("SwapFree:")),
        "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
    })
}

/// Swap em uso no sistema inteiro, em bytes.
pub(crate) fn swap_used() -> u64 {
    machine()["swap_usado"].as_u64().unwrap_or(0)
}

pub(crate) fn config_json(config: &Config) -> Value {
    json!({
        "block_cache_bytes": config.block_cache_bytes,
        "bloom_bits_per_key": config.bloom_bits_per_key,
        "max_prefix_terms": config.max_prefix_terms,
        "statistics": config.statistics,
        "writer_cache_nodes": config.writer_cache_nodes,
        "reader_cache_depth": config.reader_cache_depth,
        "languages": config.languages.as_ref().map(|l| l.iter().map(|l| l.code()).collect::<Vec<_>>()),
        "positions": config.positions,
        "partition_filters": config.partition_filters,
        "bm25_k1": config.bm25_k1,
        "bm25_b": config.bm25_b,
        "inflection_weight": config.inflection_weight,
        "inflection_mode": match config.inflection_mode {
            fire_flower::InflectionMode::Always => json!("sempre"),
            fire_flower::InflectionMode::IfFewer(n) => json!(n),
        },
    })
}

/// Grava o resultado em `results/` do repositório de resultados e devolve o caminho.
pub(crate) fn save_result(kind: &str, label: &str, mut value: Value) -> AnyResult<PathBuf> {
    let dir = bench_dir().join("results");
    std::fs::create_dir_all(&dir)?;
    let commit = git_commit();
    let ts = timestamp();
    if let Value::Object(map) = &mut value {
        map.insert("tipo".into(), json!(kind));
        map.insert("rotulo".into(), json!(label));
        map.insert("commit".into(), json!(commit));
        map.insert("quando".into(), json!(ts));
        map.insert("maquina".into(), machine());
    }
    let path = dir.join(format!("{ts}-{commit}-{kind}-{label}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&value)? + "\n")?;
    eprintln!("resultado: {}", path.display());
    Ok(path)
}

/// Contadores do RocksDB lidos do texto de estatísticas ("nome COUNT : valor").
pub(crate) fn tickers(stats: &str) -> HashMap<String, u64> {
    stats
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(" COUNT : ")?;
            Some((name.trim().to_string(), rest.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

/// Write amplification e stalls a partir dos contadores.
pub(crate) fn write_metrics(engine: &Engine) -> Value {
    let Some(stats) = engine.rocksdb_stats() else { return Value::Null };
    let t = tickers(&stats);
    let get = |k: &str| t.get(k).copied().unwrap_or(0);
    let user = get("rocksdb.bytes.written");
    let flush = get("rocksdb.flush.write.bytes");
    let compact = get("rocksdb.compact.write.bytes");
    json!({
        "bytes_escritos_usuario": user,
        "bytes_flush": flush,
        "bytes_compactacao": compact,
        "write_amp": if user > 0 { (flush + compact) as f64 / user as f64 } else { 0.0 },
        "stall_micros": get("rocksdb.stall.micros"),
        "block_cache_hit": get("rocksdb.block.cache.hit"),
        "block_cache_miss": get("rocksdb.block.cache.miss"),
        "bloom_util": get("rocksdb.bloom.filter.useful"),
    })
}

pub(crate) fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}
