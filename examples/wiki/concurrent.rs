//! `wiki concurrent`: buscas enquanto o índice é escrito.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fire_flower::{Config, Engine};
use rocksdb::perf::{PerfContext, PerfStatsLevel, set_perf_stats};
use serde_json::{Value, json};

use crate::AnyResult;
use crate::bench::{Sample, read_query_sets, run_query, summarize};
use crate::corpus::{Corpus, stem};
use crate::env::{save_result, write_metrics};

#[allow(clippy::too_many_arguments)]
pub(crate) fn concurrent(db: &Path, corpus_path: &Path, skip: u64, take: Option<u64>, queries_path: &Path, threads: usize, batch_size: usize, baseline_secs: u64, label: Option<String>) -> AnyResult {
    let set = read_query_sets(&[queries_path.to_path_buf()])?.pop().ok_or("conjunto de consultas vazio")?;
    let engine = Arc::new(Engine::open(db, Config { statistics: true, ..Config::default() })?);

    // Leitores rodam até `stop`; cada um devolve suas amostras.
    let readers = |stop: Arc<AtomicBool>| -> Vec<std::thread::JoinHandle<Vec<Sample>>> {
        (0..threads)
            .map(|t| {
                let engine = Arc::clone(&engine);
                let stop = Arc::clone(&stop);
                let set = set.clone();
                std::thread::spawn(move || {
                    set_perf_stats(PerfStatsLevel::EnableCount);
                    let mut ctx = PerfContext::default();
                    let mut out = Vec::new();
                    let mut i = t;
                    while !stop.load(Ordering::Relaxed) {
                        let q = &set.queries[i % set.queries.len()];
                        if let Ok(s) = run_query(&engine, &set, q, &mut ctx) {
                            out.push(s);
                        }
                        i += threads;
                    }
                    out
                })
            })
            .collect()
    };
    let collect = |handles: Vec<std::thread::JoinHandle<Vec<Sample>>>| -> Vec<Sample> {
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    };

    // A. Só leitura, para a linha de base.
    eprintln!("fase A: {threads} leitores sem escrita por {baseline_secs}s");
    let stop = Arc::new(AtomicBool::new(false));
    let handles = readers(Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(baseline_secs));
    stop.store(true, Ordering::Relaxed);
    let alone = collect(handles);
    let before = write_metrics(&engine);

    // B. Leitores + indexação.
    eprintln!("fase B: {threads} leitores + indexação a partir da linha {}", skip + 1);
    let stop = Arc::new(AtomicBool::new(false));
    let handles = readers(Arc::clone(&stop));
    let started = Instant::now();
    let (mut docs, mut bytes) = (0u64, 0u64);
    let mut batch = engine.batch();
    for doc in Corpus::open(corpus_path, skip, take, false)? {
        let doc = doc?;
        batch.add_titled(doc.id, &doc.title, &doc.text)?;
        batch.set_label(doc.id, &doc.title);
        docs += 1;
        bytes += doc.bytes as u64;
        if batch.len() >= batch_size {
            batch.commit()?;
            batch = engine.batch();
        }
        if docs.is_multiple_of(10_000) {
            eprintln!("{docs:>8} docs  {:.0} docs/s", docs as f64 / started.elapsed().as_secs_f64());
        }
    }
    batch.commit()?;
    let write_secs = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let during = collect(handles);
    let after = write_metrics(&engine);

    let a = summarize(&set.name, "sem escrita", &alone, baseline_secs as f64, threads);
    let b = summarize(&set.name, "com escrita", &during, write_secs, threads);
    for (name, s) in [("sem escrita", &a), ("com escrita", &b)] {
        println!(
            "{name:<12} p50 {:>7.0} µs  p99 {:>8.0} µs  máx {:>9.0} µs  {:>8.0} qps",
            s["p50_us"].as_f64().unwrap_or(0.0), s["p99_us"].as_f64().unwrap_or(0.0), s["max_us"].as_f64().unwrap_or(0.0), s["qps"].as_f64().unwrap_or(0.0)
        );
    }
    let stall = |v: &Value| v["stall_micros"].as_u64().unwrap_or(0);
    println!("indexação: {docs} docs em {write_secs:.1}s ({:.0} docs/s) com {threads} leitores; stalls: {} ms", docs as f64 / write_secs, stall(&after).saturating_sub(stall(&before)) / 1000);
    save_result(
        "concurrent",
        &label.unwrap_or_else(|| format!("{}-t{threads}", stem(db))),
        json!({ "db": db, "corpus": corpus_path, "skip": skip, "take": take, "threads": threads, "batch": batch_size,
                "leitura_sem_escrita": a, "leitura_com_escrita": b,
                "indexacao": { "docs": docs, "bytes": bytes, "segundos": write_secs, "docs_por_s": docs as f64 / write_secs },
                "escrita_antes": before, "escrita_depois": after }),
    )?;
    Ok(())
}
