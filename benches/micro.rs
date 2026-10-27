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
use fire_flower::{Config, Engine, Posting, Stopwords};
use fire_flower::analysis::{Analyzer, tokenize};
use fire_flower::internals::{
    decode_docs, decode_positions, decode_postings, encode_postings, intersect, merge_positions, merge_postings,
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
    // Lápide de um documento do meio: remover ou editar um documento antigo.
    let mut tombstone = Vec::new();
    fire_flower::internals::encode_tombstones_into(&mut tombstone, &[area[area.len() / 2].doc]);

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
    g.bench_function("merge_area_tombstone", |b| {
        b.iter(|| merge_postings([black_box(area_bytes.as_slice()), tombstone.as_slice()].into_iter()).unwrap())
    });
    g.bench_function("merge_area_positions_tombstone", |b| {
        b.iter(|| merge_positions([black_box(area_pos_bytes.as_slice()), tombstone.as_slice()].into_iter()).unwrap())
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

criterion_group!(benches, bench_tokenize, bench_postings, bench_search, bench_index);
criterion_main!(benches);
