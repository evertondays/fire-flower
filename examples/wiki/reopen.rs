//! `wiki reopen`: quanto o `Reader` leva para reabrir enquanto o WAL cresce.
//! Grava lotes num banco e, depois de cada commit, reabre o leitor
//! (`refresh_now`), anotando o tempo e o tamanho do WAL naquele momento.

use std::path::Path;
use std::time::{Duration, Instant};

use fire_flower::{Config, Engine, Reader};
use serde_json::json;

use crate::AnyResult;
use crate::corpus::Corpus;
use crate::env::{mb, save_result};

/// Bytes dos WAL (`*.log`) da pasta.
fn wal_bytes(db: &Path) -> u64 {
    std::fs::read_dir(db)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".log"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub(crate) fn reopen(
    db: &Path,
    corpus: &Path,
    take: u64,
    batch: usize,
    max_wal_bytes: Option<u64>,
    label: Option<String>,
) -> AnyResult {
    let writer = Engine::open(db, Config { max_wal_bytes, ..Config::default() })?;
    writer.index_document(0, "início")?;
    let reader = Reader::open(db, Config::reader(), Duration::ZERO)?;
    let mut docs = Corpus::open(corpus, 0, Some(take), false)?.peekable();
    let mut rounds = Vec::new();
    println!("| documentos | WAL MB | reabrir ms | 1ª busca µs | 2ª busca µs |");
    println!("|---|---|---|---|---|");
    let mut indexed = 0u64;
    while docs.peek().is_some() {
        let mut b = writer.batch();
        for doc in docs.by_ref().take(batch) {
            let doc = doc?;
            b.add_titled(doc.id, &doc.title, &doc.text)?;
            indexed += 1;
        }
        b.commit()?;
        let wal = wal_bytes(db);
        let t = Instant::now();
        reader.refresh_now()?;
        let reopen_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        reader.engine().search_top("brasil", 10)?;
        let first_us = t.elapsed().as_secs_f64() * 1e6;
        let t = Instant::now();
        reader.engine().search_top("brasil", 10)?;
        let second_us = t.elapsed().as_secs_f64() * 1e6;
        println!("| {indexed} | {:.1} | {reopen_ms:.1} | {first_us:.0} | {second_us:.0} |", mb(wal));
        rounds.push(json!({ "docs": indexed, "wal_bytes": wal, "reopen_ms": reopen_ms,
                            "first_search_us": first_us, "second_search_us": second_us }));
    }
    let data = json!({ "db": db, "batch": batch, "max_wal_bytes": max_wal_bytes, "rounds": rounds });
    save_result("reopen", &label.unwrap_or_else(|| "reopen".into()), data)?;
    Ok(())
}
