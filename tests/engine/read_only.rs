//! Abrir só para leitura ao lado de quem grava, e nunca criar banco por engano.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fire_flower::{Config, Engine, Error};

use crate::{open, sample};

#[test]
fn reader_opens_while_the_writer_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let writer = open(&dir);
    sample(&writer);
    // Um segundo `open` para escrita falha no LOCK, com uma dica.
    let Err(Error::Open(msg)) = Engine::open(dir.path(), Config::default()) else { panic!("abriu duas vezes") };
    assert!(msg.contains("open_read_only"), "{msg}");

    let reader = Engine::open_read_only(dir.path(), Config::reader()).unwrap();
    assert!(reader.is_read_only());
    assert_eq!(reader.search("roupa").unwrap(), [1, 2]);
    // A visão é a da abertura: o que foi gravado depois só aparece reabrindo.
    writer.index_document(5, "roupa de festa").unwrap();
    assert_eq!(reader.search("roupa").unwrap(), [1, 2]);
    let reader = Engine::open_read_only(dir.path(), Config::reader()).unwrap();
    assert_eq!(reader.search("roupa").unwrap(), [1, 2, 5]);
}

#[test]
fn readers_follow_edits_and_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let writer = open(&dir);
    sample(&writer);
    writer.index_document(2, "a camisa nova do imperador").unwrap();
    writer.delete_document(1).unwrap();
    let reader = Engine::open_read_only(dir.path(), Config::reader()).unwrap();
    assert!(reader.search("roupa").unwrap().is_empty());
    assert_eq!(reader.search("camisa").unwrap(), [2]);
    assert_eq!(reader.stats().unwrap().doc_count, 3);
    assert!(!reader.contains(1).unwrap());
}

#[test]
fn readers_never_write() {
    let dir = tempfile::tempdir().unwrap();
    sample(&open(&dir));
    let reader = Engine::open_read_only(dir.path(), Config::reader()).unwrap();
    let mut batch = reader.batch();
    batch.add_document(9, "nada").unwrap();
    assert!(matches!(batch.commit(), Err(Error::ReadOnly)));
    assert!(matches!(reader.compact(), Err(Error::ReadOnly)));
    assert!(matches!(reader.delete_document(1), Err(Error::ReadOnly)));
    assert_eq!(reader.search("rato").unwrap(), [1]);
}

#[test]
fn a_wrong_path_does_not_create_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nao-existe");
    let Err(Error::Open(msg)) = Engine::open_read_only(&missing, Config::reader()) else { panic!("abriu") };
    assert!(msg.contains("não é um banco"), "{msg}");
    let config = Config { create_if_missing: false, ..Config::default() };
    assert!(Engine::open(&missing, config).is_err());
    assert!(!missing.exists(), "nenhum dos dois cria a pasta");
}

/// Uma indexação rodando com leitores abrindo e buscando ao mesmo tempo
/// (a TUI e o `search` da CLI): nenhum erro de LOCK, e cada leitor vê um
/// commit inteiro.
#[test]
fn readers_open_and_search_during_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Arc::new(open(&dir));
    writer.index_document(1, "alfa comum").unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let indexer = {
        let (writer, done) = (Arc::clone(&writer), Arc::clone(&done));
        std::thread::spawn(move || {
            for i in 2..200u64 {
                let mut batch = writer.batch();
                // Cada commit troca o 1 e acrescenta dois: "comum" sempre acha um número ímpar.
                batch.add_document(1, &format!("alfa comum versão {i}")).unwrap();
                batch.add_document(i * 2, "beta comum").unwrap();
                batch.add_document(i * 2 + 1, "gama comum").unwrap();
                batch.commit().unwrap();
            }
            done.store(true, Ordering::Release);
        })
    };
    let mut opened = 0;
    while !done.load(Ordering::Acquire) || opened < 3 {
        let reader = Engine::open_read_only(dir.path(), Config::reader()).unwrap();
        let found = reader.search("comum").unwrap();
        assert_eq!(found.len() % 2, 1, "um commit pela metade: {found:?}");
        assert_eq!(reader.search("alfa").unwrap(), [1]);
        opened += 1;
    }
    indexer.join().unwrap();
    assert!(opened >= 3);
}
