//! Testes de integração do motor, por tema; aqui ficam as funções
//! auxiliares que vários temas usam.

use fire_flower::{Config, Engine, Posting};

mod concurrency;
mod edit_model;
mod editing;
mod indexing;
mod phrases;
mod read_only;
mod ranking;
mod search;
mod stemming;
mod storage;

pub(crate) fn open(dir: &tempfile::TempDir) -> Engine {
    Engine::open(dir.path(), Config::default()).unwrap()
}

pub(crate) fn sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_document(1, "O rato roeu a roupa do rei de Roma").unwrap();
    batch.add_document(2, "A roupa nova do imperador").unwrap();
    batch.add_document(3, "Programação em Rust: rápida e segura").unwrap();
    batch.add_document(4, "Rotina de programador: café, código, café").unwrap();
    batch.commit().unwrap();
}

pub(crate) fn p(doc: u64, tf: u32) -> Posting {
    Posting::new(doc, tf)
}

pub(crate) fn positions(engine: &Engine, term: &str) -> Vec<(u64, Vec<u32>)> {
    engine.positions_of_node(engine.term_node(term).unwrap().unwrap()).unwrap()
}

pub(crate) const K1: f64 = 1.2;

pub(crate) const B: f64 = 0.75;

pub(crate) fn bm25(n: f64, df: f64, tf: f64, len: f64, avg: f64) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg))
}

pub(crate) fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * b.abs().max(1.0)
}

pub(crate) fn ranked(engine: &Engine, q: &str) -> Vec<(u64, f64)> {
    engine.search_top(q, 100).unwrap().into_iter().map(|h| (h.doc, h.score)).collect()
}

pub(crate) fn titled(dir: &tempfile::TempDir, config: Config, docs: &[(u64, &str, &str)]) -> Engine {
    let engine = Engine::open(dir.path(), config).unwrap();
    let mut batch = engine.batch();
    for (id, title, text) in docs {
        batch.add_titled(*id, title, text).unwrap();
    }
    batch.commit().unwrap();
    engine
}
