//! Leitor em outro processo (`Reader`): reabre quando o banco muda, inclusive
//! com o escritor compactando em outro processo.

use std::process::Command;
use std::time::{Duration, Instant};

use fire_flower::{Config, Engine, Reader};

use crate::{open, sample};

#[test]
fn refresh_reopens_only_when_the_database_changed() {
    let dir = tempfile::tempdir().unwrap();
    let writer = open(&dir);
    sample(&writer);
    let reader = Reader::open(dir.path(), Config::reader(), Duration::ZERO).unwrap();
    assert_eq!(reader.engine().search("roupa").unwrap(), [1, 2]);
    assert!(!reader.refresh().unwrap(), "nada mudou");

    let before = reader.engine();
    writer.index_document(5, "roupa de festa").unwrap();
    assert!(reader.refresh().unwrap(), "o commit acrescentou ao WAL");
    assert_eq!(reader.engine().search("roupa").unwrap(), [1, 2, 5]);
    // Uma busca que pegou a instância antiga continua nela.
    assert_eq!(before.search("roupa").unwrap(), [1, 2]);
    assert!(!reader.refresh().unwrap());

    writer.delete_document(1).unwrap();
    writer.compact().unwrap();
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.engine().search("roupa").unwrap(), [2, 5]);
}

#[test]
fn refresh_waits_for_the_minimum_interval() {
    let dir = tempfile::tempdir().unwrap();
    let writer = open(&dir);
    sample(&writer);
    let reader = Reader::open(dir.path(), Config::reader(), Duration::from_millis(200)).unwrap();
    writer.index_document(5, "roupa de festa").unwrap();
    assert!(!reader.refresh().unwrap(), "antes do intervalo, nem confere");
    std::thread::sleep(Duration::from_millis(250));
    assert!(reader.refresh().unwrap());
    assert_eq!(reader.engine().search("festa").unwrap(), [5]);
    // Um pedido explícito confere sem esperar o intervalo.
    writer.index_document(6, "festa junina").unwrap();
    assert!(!reader.refresh().unwrap());
    assert!(reader.refresh_now().unwrap());
    assert_eq!(reader.engine().search("festa").unwrap(), [5, 6]);
}

const CHILD_DB: &str = "FF_TEST_WRITER_DB";
const CHILD_COMMITS: u64 = 60;

/// O escritor em outro processo: o próprio binário de teste, chamado de novo
/// só com este teste e `FF_TEST_WRITER_DB`. Sem a variável, não faz nada.
#[test]
fn writer_child_process() {
    let Some(db) = std::env::var_os(CHILD_DB) else { return };
    let engine = Engine::open(&db, Config::default()).unwrap();
    for i in 1..=CHILD_COMMITS {
        let mut batch = engine.batch();
        // Cada commit troca o documento 1 e acrescenta dois: "comum" sempre
        // acha um número ímpar de documentos.
        batch.add_document(1, &format!("alfa comum versão {i}")).unwrap();
        batch.add_document(i * 2, "beta comum").unwrap();
        batch.add_document(i * 2 + 1, "gama comum").unwrap();
        batch.commit().unwrap();
        if i % 20 == 0 {
            engine.compact().unwrap(); // apaga SSTs e versões mortas com leitores abertos
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn reads_follow_a_writer_in_another_process() {
    let dir = tempfile::tempdir().unwrap();
    open(&dir).index_document(1, "alfa comum").unwrap();
    let reader = Reader::open(dir.path(), Config::reader(), Duration::ZERO).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["reader::writer_child_process", "--exact", "--test-threads", "1"])
        .env(CHILD_DB, dir.path())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let (mut last, mut reopened) = (1, 0);
    loop {
        let done = child.try_wait().unwrap().is_some();
        if reader.refresh().unwrap() {
            reopened += 1;
        }
        let found = reader.engine().search("comum").unwrap();
        assert_eq!(found.len() % 2, 1, "um commit pela metade: {} documentos", found.len());
        assert!(found.len() >= last, "o leitor voltou no tempo");
        assert_eq!(reader.engine().search("alfa").unwrap(), [1], "uma versão só do documento 1");
        last = found.len();
        if done {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(120), "o escritor não terminou");
    }
    assert!(child.wait().unwrap().success(), "o escritor falhou");
    reader.refresh().unwrap();
    assert_eq!(reader.engine().search("comum").unwrap().len() as u64, 1 + 2 * CHILD_COMMITS);
    assert!(reopened > 1, "reabriu ao longo da escrita: {reopened}");
}
