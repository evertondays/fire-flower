//! Testes das métricas: baldes, shards e somas.

use std::sync::Arc;
use std::time::Duration;

use super::*;

#[test]
fn buckets_follow_the_prometheus_le_rule() {
    // Um valor igual ao limite fica no balde dele (`le`); acima do último, no +Inf.
    assert_eq!(bucket(&QUERY_BOUNDS_NANOS, 0), 0);
    assert_eq!(bucket(&QUERY_BOUNDS_NANOS, 5_000), 0);
    assert_eq!(bucket(&QUERY_BOUNDS_NANOS, 5_001), 1);
    assert_eq!(bucket(&QUERY_BOUNDS_NANOS, 1_000_000_000), BUCKETS - 2);
    assert_eq!(bucket(&QUERY_BOUNDS_NANOS, 1_000_000_001), BUCKETS - 1);
    assert_eq!(COMMIT_BOUNDS_NANOS[0], 1_000_000);
    assert_eq!(COMMIT_BOUNDS_NANOS[BUCKETS - 2], 100_000_000_000);
}

#[test]
fn shards_are_cache_line_aligned() {
    assert_eq!(std::mem::align_of::<Shard>(), 128);
    let m = Metrics::new();
    assert!(m.shards.len().is_power_of_two());
}

#[test]
fn counts_from_many_threads_add_up() {
    let m = Arc::new(Metrics::new());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                for i in 0..1000u64 {
                    m.record_query(Op::Suggest, Duration::from_micros(i % 30), 2, i % 100 == 0);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let s = m.snapshot("db", 0.0);
    let suggest = s.queries.iter().find(|q| q.op == "suggest").unwrap();
    assert_eq!(suggest.count, 8000);
    assert_eq!(suggest.errors, 80);
    assert_eq!(suggest.reads, 16_000);
    assert_eq!(suggest.buckets.iter().sum::<u64>(), 8000);
    assert!(s.queries.iter().filter(|q| q.op != "suggest").all(|q| q.count == 0));
}
