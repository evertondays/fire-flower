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

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use fire_flower::internals::{decode_postings, encode_postings, intersect, union_encoded};
use fire_flower::text::tokenize;
use fire_flower::{Config, Engine};

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
    Some(Engine::open(&path, Config::default()).expect("abrir banco de benchmark"))
}

fn bench_tokenize(c: &mut Criterion) {
    let docs = articles();
    let bytes: usize = docs.iter().map(String::len).sum();
    let mut g = c.benchmark_group("texto");
    g.throughput(Throughput::Bytes(bytes as u64));
    g.sample_size(20);
    g.bench_function("tokenize_1000_artigos", |b| {
        b.iter(|| docs.iter().map(|d| tokenize(black_box(d)).len()).sum::<usize>())
    });
    g.finish();
}

fn bench_postings(c: &mut Criterion) {
    let Some(engine) = open_db() else { return };
    let de = engine.postings("de").unwrap();
    let em = engine.postings("em").unwrap();
    let rare = engine.postings("astrofisica").unwrap();
    let de_bytes = encode_postings(&de);
    // Pedaços pequenos como os que um lote grava via merge.
    let max = de.last().copied().unwrap_or(0);
    let pieces: Vec<Vec<u8>> = (1..=8u64).map(|i| encode_postings(&[max + i * 10, max + i * 10 + 1])).collect();

    let mut g = c.benchmark_group("listas");
    g.bench_function(format!("decode_de_{}_ids", de.len()), |b| b.iter(|| decode_postings(black_box(&de_bytes)).unwrap()));
    g.bench_function("merge_de_mais_8_pedacos", |b| {
        b.iter(|| union_encoded(Some(black_box(&de_bytes)), pieces.iter().map(Vec::as_slice)).unwrap())
    });
    g.bench_function(format!("intersect_raro{}_x_de{}", rare.len(), de.len()), |b| {
        b.iter(|| intersect(black_box(&rare), black_box(&de)))
    });
    g.bench_function(format!("intersect_em{}_x_de{}", em.len(), de.len()), |b| {
        b.iter(|| intersect(black_box(&em), black_box(&de)))
    });
    g.finish();
}

fn bench_search(c: &mut Criterion) {
    let Some(engine) = open_db() else { return };
    let mut g = c.benchmark_group("busca_100k");
    for (name, q) in [
        ("raro", "astrofisica"),
        ("titulo", "revolução francesa"),
        ("frequente", "de"),
        ("prefixo", "port*"),
        ("inexistente", "constituicaox"),
    ] {
        g.bench_function(name, |b| b.iter(|| engine.search(black_box(q)).unwrap().len()));
    }
    g.bench_function("suggest_por", |b| b.iter(|| engine.suggest(black_box("por"), 10).unwrap()));
    g.finish();
}

fn bench_index(c: &mut Criterion) {
    let docs = articles();
    let bytes: usize = docs.iter().map(String::len).sum();
    let mut g = c.benchmark_group("indexacao");
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

criterion_group!(benches, bench_tokenize, bench_postings, bench_search, bench_index);
criterion_main!(benches);
