//! O lote e o escritor: try_batch, lotes donos do motor (OwnedBatch), rótulos
//! em lote e códigos de erro (o que os bindings usam).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fire_flower::{Error, OwnedBatch};

use crate::{open, sample};

#[test]
fn try_batch_does_not_wait() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let batch = engine.batch();
    assert!(matches!(engine.try_batch(), Err(Error::Busy)));
    drop(batch);
    let mut batch = engine.try_batch().unwrap();
    batch.add_document(1, "livre").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.search("livre").unwrap(), [1]);
}

#[test]
fn owned_batches_commit_on_another_thread() {
    fn assert_send_static<T: Send + 'static>(_: &T) {}
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    let mut batch: OwnedBatch = engine.owned_batch();
    assert_send_static(&batch);
    batch.add_document(1, "outra thread").unwrap();
    assert!(matches!(engine.try_owned_batch(), Err(Error::Busy)));
    std::thread::spawn(move || batch.commit().unwrap()).join().unwrap();
    assert_eq!(engine.search("thread").unwrap(), [1]);
    // O estado voltou: o próximo lote abre sem esperar.
    let mut batch = engine.try_owned_batch().unwrap();
    batch.delete_document(1).unwrap();
    batch.commit().unwrap();
    assert!(engine.search("thread").unwrap().is_empty());
}

#[test]
fn a_waiting_batch_wakes_when_the_owned_one_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    let mut held = engine.owned_batch();
    held.add_document(1, "primeiro").unwrap();
    let opened = Arc::new(AtomicBool::new(false));
    let waiter = {
        let (engine, opened) = (Arc::clone(&engine), Arc::clone(&opened));
        std::thread::spawn(move || {
            let mut batch = engine.batch();
            opened.store(true, Ordering::SeqCst);
            batch.add_document(2, "segundo").unwrap();
            batch.commit().unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!opened.load(Ordering::SeqCst), "o segundo lote espera o primeiro");
    held.commit().unwrap();
    waiter.join().unwrap();
    assert_eq!(engine.search("primeiro").unwrap(), [1]);
    assert_eq!(engine.search("segundo").unwrap(), [2]);
}

#[test]
fn a_panic_with_an_open_batch_releases_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    sample(&engine);
    let panicked = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            let mut batch = engine.batch();
            batch.add_document(9, "perdido").unwrap();
            panic!("o lote cai junto");
        })
        .join()
    };
    assert!(panicked.is_err());
    let mut batch = engine.try_batch().expect("o estado voltou no unwind");
    batch.add_document(10, "posterior").unwrap();
    batch.commit().unwrap();
    assert!(engine.search("perdido").unwrap().is_empty());
    assert_eq!(engine.search("posterior").unwrap(), [10]);
    assert_eq!(engine.search("roupa").unwrap(), [1, 2], "o banco de antes continua igual");
}

#[test]
fn labels_in_one_read() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let a = batch.add_labeled("a.txt", "um").unwrap();
    let b = batch.add_labeled("b.txt", "dois").unwrap();
    batch.add_document(50, "sem rótulo").unwrap();
    batch.commit().unwrap();
    let got = engine.labels(&[b, 50, a, 999, b]).unwrap();
    assert_eq!(got, [Some("b.txt".into()), None, Some("a.txt".into()), None, Some("b.txt".into())]);
    assert!(engine.labels(&[]).unwrap().is_empty());
}

#[test]
fn error_codes_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nada");
    let open_error = fire_flower::Engine::open_read_only(&missing, fire_flower::Config::reader()).err().unwrap();
    assert_eq!(open_error.code(), "FF_OPEN");
    assert_eq!(Error::Busy.code(), "FF_BUSY");
    assert_eq!(Error::ReadOnly.code(), "FF_READ_ONLY");
    assert_eq!(Error::Closed.code(), "FF_CLOSED");
    assert_eq!(Error::Config(String::new()).code(), "FF_CONFIG");
    assert_eq!(Error::Corrupt(String::new()).code(), "FF_CORRUPT");
}

#[test]
fn a_dropped_batch_after_a_commit_keeps_the_next_one_working() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    let mut batch = engine.batch();
    batch.add_document(9, "perdido").unwrap();
    drop(batch);
    let mut batch = engine.batch();
    batch.add_document(10, "posterior").unwrap();
    batch.commit().unwrap();
    assert!(engine.search("perdido").unwrap().is_empty());
    assert_eq!(engine.search("posterior").unwrap(), [10]);
}

