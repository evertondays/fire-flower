//! Métricas (`Config::metrics`): consultas, commits, edições, remoções e compactação.

use std::path::Path;

use fire_flower::{Config, Engine};
use fire_flower::metrics::Snapshot;

use crate::sample;

fn with_metrics(dir: &tempfile::TempDir) -> Engine {
    Engine::open(dir.path(), Config { metrics: true, ..Config::default() }).unwrap()
}

fn op(s: &Snapshot, name: &str) -> fire_flower::metrics::OpCounters {
    s.queries.iter().find(|q| q.op == name).cloned().unwrap()
}

#[test]
fn off_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::open(&dir);
    sample(&engine);
    assert!(engine.metrics_snapshot().is_none());
}

#[test]
fn queries_are_counted_by_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let engine = with_metrics(&dir);
    sample(&engine);
    engine.search("roupa").unwrap();
    engine.search("roupa rei").unwrap();
    engine.search_top("roupa", 10).unwrap();
    engine.search_ranked("café", 10).unwrap();
    engine.suggest("ro", 5).unwrap();
    engine.postings("roupa").unwrap();
    engine.term_postings("roupa").unwrap();
    let s = engine.metrics_snapshot().unwrap();
    assert_eq!(op(&s, "search").count, 2);
    // `search_top` não conta também como `search_ranked`.
    assert_eq!(op(&s, "search_top").count, 1);
    assert_eq!(op(&s, "search_ranked").count, 1);
    assert_eq!(op(&s, "suggest").count, 1);
    assert_eq!(op(&s, "postings").count, 1);
    assert_eq!(op(&s, "term_postings").count, 1);
    for q in &s.queries {
        assert_eq!(q.buckets.iter().sum::<u64>(), q.count, "{}", q.op);
        assert!(q.count == 0 || (q.reads > 0 && q.sum_nanos > 0), "{}", q.op);
        assert_eq!(q.errors, 0);
    }
    assert!(s.cache.node_hits + s.cache.node_misses > 0);
    assert!(s.cache.norm_hits + s.cache.norm_misses > 0, "o ranking lê os tamanhos");
}

#[test]
fn failed_queries_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { metrics: true, positions: Some(false), ..Config::default() };
    let engine = Engine::open(dir.path(), config).unwrap();
    sample(&engine);
    assert!(engine.search("\"roupa nova\"").is_err(), "frase sem posições");
    let s = engine.metrics_snapshot().unwrap();
    assert_eq!((op(&s, "search").count, op(&s, "search").errors), (1, 1));
}

#[test]
fn commits_edits_deletions_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let engine = with_metrics(&dir);
    sample(&engine);
    let s = engine.metrics_snapshot().unwrap();
    assert_eq!((s.writer.commits, s.writer.indexed_documents, s.writer.documents), (1, 4, 4));
    assert_eq!(s.writer.commit_buckets.iter().sum::<u64>(), 1);
    assert!(s.writer.commit_nanos > 0 && s.writer.batch_bytes > 0 && s.writer.cache_nodes > 0);
    assert_eq!(s.writer.phase_nanos.len(), fire_flower::metrics::COMMIT_PHASES.len());

    let mut batch = engine.batch();
    batch.add_document(2, "a camisa nova").unwrap();
    batch.delete_document(3).unwrap();
    batch.delete_document(99).unwrap(); // não existe: não conta
    batch.commit().unwrap();
    let w = engine.metrics_snapshot().unwrap().writer;
    assert_eq!((w.commits, w.indexed_documents, w.replaced_documents, w.deleted_documents), (2, 5, 1, 1));
    assert_eq!((w.killed_versions, w.dead_versions, w.pending_edits, w.documents), (2, 2, 2, 3));

    // Um commit que falha conta como erro, não como commit.
    let reader = Engine::open_read_only(dir.path(), Config { metrics: true, ..Config::reader() }).unwrap();
    let mut batch = reader.batch();
    batch.add_document(7, "nada").unwrap();
    assert!(batch.commit().is_err());
    let w = reader.metrics_snapshot().unwrap().writer;
    assert_eq!((w.commits, w.commit_errors, w.documents, w.dead_versions), (0, 1, 3, 2));

    let stats = engine.compact().unwrap();
    let w = engine.metrics_snapshot().unwrap().writer;
    assert_eq!((w.compactions, w.purged_versions, w.emptied_words), (1, 2, stats.emptied_words as u64));
    assert!(w.emptied_words > 0, "\"roupa\" de 2 e \"programação\" de 3 sumiram");
    assert_eq!((w.dead_versions, w.pending_edits), (0, 0));
    assert!(w.compaction_nanos > 0);
}

#[test]
fn reopening_starts_the_gauges_from_the_database() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = crate::open(&dir);
        sample(&engine);
        engine.delete_document(1).unwrap();
    }
    let engine = with_metrics(&dir);
    let w = engine.metrics_snapshot().unwrap().writer;
    assert_eq!((w.documents, w.dead_versions, w.commits), (3, 1, 0));
}

#[test]
fn waiting_for_the_writer_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let engine = std::sync::Arc::new(with_metrics(&dir));
    let batch = engine.batch();
    let other = {
        let engine = std::sync::Arc::clone(&engine);
        std::thread::spawn(move || engine.index_document(1, "espera").unwrap())
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(batch);
    other.join().unwrap();
    let w = engine.metrics_snapshot().unwrap().writer;
    assert!(w.lock_wait_nanos >= 40_000_000, "{}", w.lock_wait_nanos);
}

fn fast() -> Config {
    Config { metrics: true, metrics_interval: std::time::Duration::from_millis(20), ..Config::default() }
}

/// Espera a amostradora gravar um snapshot que satisfaça `ok`.
fn wait_for(path: &Path, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
    for _ in 0..500 {
        if let Ok(s) = fire_flower::metrics::read(path)
            && ok(&s)
        {
            return s;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("a amostradora não gravou o snapshot esperado em {}", path.display());
}

#[test]
fn the_sampler_writes_the_snapshot_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = fire_flower::metrics::default_path(dir.path());
    let engine = Engine::open(dir.path(), fast()).unwrap();
    // O primeiro snapshot sai no `open`: o monitor acha o pid logo.
    let first = fire_flower::metrics::read(&file).unwrap();
    assert_eq!((first.layout, first.pid), (fire_flower::metrics::LAYOUT, std::process::id()));
    assert_eq!(first.db, dir.path().display().to_string());
    assert_eq!(first.format_version, fire_flower::FORMAT_VERSION);
    assert!(first.rocksdb.is_some(), "as propriedades entram no primeiro");

    sample(&engine);
    engine.search("roupa").unwrap();
    let later = wait_for(&file, |s| s.writer.commits == 1 && s.queries.iter().any(|q| q.count == 1));
    assert!(later.sequence > first.sequence && later.written_unix >= first.written_unix);
    assert_eq!(later.writer.documents, 4);
    let props = wait_for(&file, |s| s.rocksdb.as_ref().is_some_and(|p| p.memtables_bytes > 0)).rocksdb.unwrap();
    assert_eq!(props.sst_files.len(), 7);
    assert!(props.block_cache_capacity_bytes > 0);
    if cfg!(target_os = "linux") {
        assert!(later.sampler_cpu_secs.is_some());
    }

    // Ao fechar, um último snapshot com tudo; e o banco reabre na hora.
    engine.delete_document(1).unwrap();
    drop(engine);
    let last = fire_flower::metrics::read(&file).unwrap();
    assert_eq!((last.writer.deleted_documents, last.writer.dead_versions), (1, 1));
    let engine = crate::open(&dir);
    assert_eq!(engine.search("roupa").unwrap(), [2]);
}

#[test]
fn readers_write_metrics_only_to_their_own_file() {
    let dir = tempfile::tempdir().unwrap();
    sample(&crate::open(&dir));
    let reader = Engine::open_read_only(dir.path(), Config { metrics: true, ..Config::reader() }).unwrap();
    assert!(!fire_flower::metrics::default_path(dir.path()).exists(), "a pasta é de quem grava");
    assert!(reader.metrics_snapshot().is_some());
    drop(reader);

    let own = dir.path().join("leitor.json");
    let config = Config { metrics: true, metrics_file: Some(own.clone()), ..Config::reader() };
    let reader = Engine::open_read_only(dir.path(), config).unwrap();
    reader.search("roupa").unwrap();
    drop(reader);
    let s = fire_flower::metrics::read(&own).unwrap();
    assert_eq!(s.queries.iter().map(|q| q.count).sum::<u64>(), 1);
}
