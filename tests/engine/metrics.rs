//! Métricas (`Config::metrics`): consultas, commits, edições, remoções e compactação.

use fire_flower::{Config, Engine};

use crate::sample;

fn with_metrics(dir: &tempfile::TempDir) -> Engine {
    Engine::open(dir.path(), Config { metrics: true, ..Config::default() }).unwrap()
}

fn op(s: &fire_flower::metrics::Snapshot, name: &str) -> fire_flower::metrics::OpCounters {
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
