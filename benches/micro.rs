//! Microbenchmarks com dados reais (Fase 4 do plano de performance).
//!
//! Precisa do banco de 100k já carregado e do corpus de 1k:
//!   FF_BENCH_DB      (padrão: ~/bench-db/ptwiki-100k)
//!   FF_BENCH_CORPUS  (padrão: ~/datasets/ptwiki/ptwiki-1000.jsonl)
//!
//!   cargo bench --bench micro -- --save-baseline main
//!   cargo bench --bench micro -- --baseline main      # depois de uma mudança

use std::hint::black_box;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use fire_flower::{CommitStats, Config, Engine, Posting, Stopwords};
use fire_flower::analysis::{Analyzer, tokenize};
use fire_flower::internals::{
    Metrics, Op, decode_docs, decode_docs_except, decode_positions, decode_postings, encode_postings, intersect,
    merge_postings,
};

fn env_path(var: &str, default: &str) -> PathBuf {
    let raw = std::env::var(var).unwrap_or_else(|_| default.to_string());
    match raw.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(raw),
    }
}

/// `title + "\n" + text` dos artigos do corpus de 1k.
fn articles() -> Vec<String> {
    let path = env_path("FF_BENCH_CORPUS", "~/datasets/ptwiki/ptwiki-1000.jsonl");
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    std::io::BufReader::new(file)
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(&l.unwrap()).unwrap();
            format!("{}\n{}", v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or(""))
        })
        .collect()
}

fn open_db() -> Option<Engine> {
    let path = env_path("FF_BENCH_DB", "~/bench-db/ptwiki-100k");
    if !path.exists() {
        eprintln!("pulando benches de banco: {} não existe (carregue a fatia de 100k)", path.display());
        return None;
    }
    match Engine::open(&path, Config::default()) {
        Ok(engine) => Some(engine),
        Err(e) => {
            eprintln!("pulando benches de banco: {}: {e}", path.display());
            None
        }
    }
}

fn bench_tokenize(c: &mut Criterion) {
    let docs = articles();
    let bytes: usize = docs.iter().map(String::len).sum();
    let stopwords = Stopwords::default();
    let mut g = c.benchmark_group("text");
    g.throughput(Throughput::Bytes(bytes as u64));
    g.sample_size(20);
    g.bench_function("tokenize_1000_articles", |b| {
        b.iter(|| docs.iter().map(|d| tokenize(black_box(d)).len()).sum::<usize>())
    });
    // O que a indexação faz por documento: tokens, stopwords e posições por termo.
    g.bench_function("analyzer_1000_articles", |b| {
        let mut analyzer = Analyzer::default();
        b.iter(|| docs.iter().map(|d| analyzer.run(black_box(d), &stopwords) as usize).sum::<usize>())
    });
    g.finish();
}

fn bench_postings(c: &mut Criterion) {
    let Some(engine) = open_db() else { return };
    // Os termos mais frequentes que não são stopwords ("de" e "em" não são mais indexados).
    let area = engine.term_postings("area").unwrap();
    let population = engine.postings("populacao").unwrap();
    let rare = engine.postings("astrofisica").unwrap();
    let area_docs: Vec<u64> = area.iter().map(|p| p.doc).collect();
    let area_bytes = encode_postings(&area);
    let area_pos = engine.positions_of_node(engine.term_node("area").unwrap().unwrap()).unwrap();
    let mut area_pos_bytes = Vec::new();
    {
        // Mesmo formato do lote: (doc, tf, tf no título) + posições em sequência.
        let pairs: Vec<(u64, u32, u32)> = area_pos.iter().map(|(d, ps)| (*d, ps.len() as u32, 0)).collect();
        let flat: Vec<u32> = area_pos.iter().flat_map(|(_, ps)| ps.iter().copied()).collect();
        fire_flower::internals::encode_positions_into(&mut area_pos_bytes, &pairs, &flat);
    }
    // Pedaços pequenos como os que um lote grava via merge.
    let max = area.last().map_or(0, |p| p.doc);
    let pieces: Vec<Vec<u8>> = (1..=8u64)
        .map(|i| encode_postings(&[Posting::new(max + i * 10, 1), Posting::new(max + i * 10 + 1, 2)]))
        .collect();
    // Pedaço que sobrepõe a lista: reindexar um documento antigo (remendo).
    let overlap = encode_postings(&[Posting::new(area[area.len() / 2].doc, 3)]);
    // Versões mortas (1% da lista): a busca as pula ao decodificar.
    let dead_ids: Vec<u64> = area.iter().step_by(100).map(|p| p.doc).collect();
    let dead = fire_flower::internals::DeadBits::new(&dead_ids);

    let mut g = c.benchmark_group("lists");
    g.bench_function(format!("decode_area_{}_postings", area.len()), |b| {
        b.iter(|| decode_postings(black_box(&area_bytes)).unwrap())
    });
    g.bench_function(format!("decode_docs_area_{}", area.len()), |b| b.iter(|| decode_docs(black_box(&area_bytes)).unwrap()));
    g.bench_function(format!("decode_positions_area_{}_bytes", area_pos_bytes.len()), |b| {
        b.iter(|| decode_positions(black_box(&area_pos_bytes)).unwrap())
    });
    g.bench_function("merge_area_over_8_chunks", |b| {
        b.iter(|| merge_postings(std::iter::once(black_box(area_bytes.as_slice())).chain(pieces.iter().map(Vec::as_slice))).unwrap())
    });
    g.bench_function("merge_area_overlapping", |b| {
        b.iter(|| merge_postings([black_box(area_bytes.as_slice()), overlap.as_slice()].into_iter()).unwrap())
    });
    g.bench_function(format!("decode_docs_area_{}_dead_1pct", area.len()), |b| {
        b.iter(|| decode_docs_except(black_box(&area_bytes), &dead).unwrap())
    });
    g.bench_function(format!("intersect_rare{}_x_area{}", rare.len(), area.len()), |b| {
        b.iter(|| intersect(black_box(&rare), black_box(&area_docs)))
    });
    g.bench_function(format!("intersect_population{}_x_area{}", population.len(), area.len()), |b| {
        b.iter(|| intersect(black_box(&population), black_box(&area_docs)))
    });
    g.finish();
}

fn bench_search(c: &mut Criterion) {
    let Some(engine) = open_db() else { return };
    let mut g = c.benchmark_group("search_100k");
    for (name, q) in [
        ("rare", "astrofisica"),
        ("title", "revolução francesa"),
        ("frequent", "brasil"),
        ("prefix", "port*"),
        ("missing", "constituicaox"),
        ("phrase", "\"revolução francesa\""),
    ] {
        g.bench_function(name, |b| b.iter(|| engine.search(black_box(q)).unwrap().len()));
    }
    for (name, q) in [("top10_frequent", "brasil"), ("top10_title", "revolução francesa"), ("top10_prefix", "port*")] {
        g.bench_function(name, |b| b.iter(|| engine.search_top(black_box(q), 10).unwrap().len()));
    }
    g.bench_function("suggest_por", |b| b.iter(|| engine.suggest(black_box("por"), 10).unwrap()));
    g.finish();
}

fn bench_index(c: &mut Criterion) {
    let docs = articles();
    let bytes: usize = docs.iter().map(String::len).sum();
    let mut g = c.benchmark_group("indexing");
    g.throughput(Throughput::Bytes(bytes as u64));
    g.sample_size(10);
    g.bench_function("1000_artigos_banco_vazio", |b| {
        b.iter_batched(
            || tempfile::tempdir().unwrap(),
            |dir| {
                let engine = Engine::open(dir.path(), Config::default()).unwrap();
                let mut batch = engine.batch();
                for (i, d) in docs.iter().enumerate() {
                    batch.add_document(i as u64 + 1, d).unwrap();
                }
                batch.commit().unwrap();
                dir
            },
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

/// O custo das métricas (`docs/plano-monitoramento.md`): meta de ≤ 50 ns por
/// consulta, com 1 e com 12 threads registrando ao mesmo tempo.
fn bench_metrics(c: &mut Criterion) {
    let mut g = c.benchmark_group("metrics");
    let m = Arc::new(Metrics::new());
    g.bench_function("record_query", |b| {
        b.iter(|| m.record_query(black_box(Op::Search), black_box(Duration::from_nanos(12_345)), 7, false))
    });
    // Tempo por registro em cada thread, com 12 registrando juntas: sem
    // shards, a linha de cache dos contadores pularia entre os núcleos.
    g.bench_function("record_query_12_threads", |b| {
        b.iter_custom(|iters| {
            let start = Arc::new(Barrier::new(13));
            let threads: Vec<_> = (0..12)
                .map(|_| {
                    let (m, start) = (Arc::clone(&m), Arc::clone(&start));
                    std::thread::spawn(move || {
                        start.wait();
                        for i in 0..iters {
                            m.record_query(Op::SearchTop, Duration::from_nanos(black_box(i)), 3, false);
                        }
                    })
                })
                .collect();
            start.wait();
            let t = Instant::now();
            for th in threads {
                th.join().unwrap();
            }
            t.elapsed()
        })
    });
    let stats = CommitStats { docs: 1, replaced: 1, killed: 1, total_secs: 2e-4, ..CommitStats::default() };
    g.bench_function("record_commit", |b| b.iter(|| m.record_commit(black_box(&stats), 100_000)));
    // Uma consulta curta de verdade com as métricas desligadas e ligadas: a
    // diferença é o custo inteiro (relógio, leituras, registro, cache).
    let dir = tempfile::tempdir().unwrap();
    for (name, metrics) in [("suggest_off", false), ("suggest_on", true)] {
        if metrics && !cfg!(feature = "metrics") {
            continue;
        }
        let path = dir.path().join(name);
        let engine = Engine::open(&path, Config { metrics, ..Config::default() }).unwrap();
        let mut batch = engine.batch();
        for (i, w) in ["roupa", "rotina", "roma", "rato", "rei", "rapido"].iter().enumerate() {
            batch.add_document(i as u64 + 1, w).unwrap();
        }
        batch.commit().unwrap();
        g.bench_function(name, |b| b.iter(|| engine.suggest(black_box("ro"), 10).unwrap().len()));
    }
    g.finish();
}

criterion_group!(benches, bench_tokenize, bench_postings, bench_search, bench_index, bench_metrics);
criterion_main!(benches);
