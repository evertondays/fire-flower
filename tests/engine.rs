use std::sync::Arc;

use fire_flower::{Config, Engine};

fn open(dir: &tempfile::TempDir) -> Engine {
    Engine::open(dir.path(), Config::default()).unwrap()
}

fn sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_document(1, "O rato roeu a roupa do rei de Roma").unwrap();
    batch.add_document(2, "A roupa nova do imperador").unwrap();
    batch.add_document(3, "Programação em Rust: rápida e segura").unwrap();
    batch.add_document(4, "Rotina de programador: café, código, café").unwrap();
    batch.commit().unwrap();
}

#[test]
fn empty_index_finds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    assert!(engine.search("qualquer").unwrap().is_empty());
    assert!(engine.suggest("q", 10).unwrap().is_empty());
}

#[test]
fn exact_search_is_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.search("roupa").unwrap(), [1, 2]);
    assert_eq!(engine.search("PROGRAMAÇÃO").unwrap(), [3]);
    assert_eq!(engine.search("cafe").unwrap(), [4]);
    // Prefixo de uma palavra não é palavra: "rot" não casa "rotina".
    assert!(engine.search("rot").unwrap().is_empty());
    assert!(engine.search("inexistente").unwrap().is_empty());
}

#[test]
fn multi_term_is_and() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.search("roupa rei").unwrap(), [1]);
    assert!(engine.search("roupa rust").unwrap().is_empty());
    assert_eq!(engine.search("  roupa   ").unwrap(), [1, 2]);
    assert!(engine.search("").unwrap().is_empty());
    assert!(engine.search("!!! *").unwrap().is_empty());
}

#[test]
fn prefix_search() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.search("program*").unwrap(), [3, 4]);
    assert_eq!(engine.search("ro*").unwrap(), [1, 2, 4]); // rato? não: roeu, roupa, roma, rotina
    assert_eq!(engine.search("program* cafe").unwrap(), [4]);
}

#[test]
fn suggest_shortest_first() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.suggest("ro", 10).unwrap(), ["roeu", "roma", "roupa", "rotina"]);
    assert_eq!(engine.suggest("RO", 2).unwrap(), ["roeu", "roma"]);
    assert_eq!(engine.suggest("programa", 10).unwrap(), ["programacao", "programador"]);
    assert_eq!(engine.suggest("rei", 10).unwrap(), ["rei"]);
}

#[test]
fn persists_across_reopen_and_appends() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        sample(&engine);
    }
    let engine = open(&dir);
    assert_eq!(engine.search("roupa").unwrap(), [1, 2]);

    // Novo lote estende listas e a Trie existentes sem sobrescrever nós.
    engine.index_document(10, "roupa rosa").unwrap();
    assert_eq!(engine.search("roupa").unwrap(), [1, 2, 10]);
    assert_eq!(engine.search("rosa").unwrap(), [10]);
    assert_eq!(engine.search("roma").unwrap(), [1]);
}

#[test]
fn reindexing_same_doc_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(7, "eco eco eco").unwrap();
    engine.index_document(7, "eco").unwrap();
    assert_eq!(engine.search("eco").unwrap(), [7]);
}

#[test]
fn dropped_batch_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(1, "primeiro").unwrap();
    {
        let mut batch = engine.batch();
        batch.add_document(2, "fantasma primeiro").unwrap();
    } // descartado sem commit
    assert!(engine.search("fantasma").unwrap().is_empty());
    assert_eq!(engine.search("primeiro").unwrap(), [1]);

    // IDs de nó não vazaram: o próximo lote ainda funciona e é coerente.
    engine.index_document(3, "fantasma").unwrap();
    assert_eq!(engine.search("fantasma").unwrap(), [3]);
}

#[test]
fn labels_and_auto_ids() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        let mut batch = engine.batch();
        batch.add_document(41, "manual").unwrap();
        let a = batch.add_labeled("a.txt", "automatico").unwrap();
        assert_eq!(a, 42, "ID automático pula IDs manuais");
        batch.commit().unwrap();
    }
    let engine = open(&dir);
    let mut batch = engine.batch();
    let b = batch.add_labeled("b.txt", "automatico").unwrap();
    batch.commit().unwrap();
    assert_eq!(b, 43, "contador persiste entre aberturas");
    assert_eq!(engine.search("automatico").unwrap(), [42, 43]);
    assert_eq!(engine.label(43).unwrap().as_deref(), Some("b.txt"));
    assert_eq!(engine.label(41).unwrap(), None);
}

#[test]
fn many_docs_across_batches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    for chunk in 0..5u64 {
        let mut batch = engine.batch();
        for i in 0..400u64 {
            let id = chunk * 400 + i;
            let parity = if id % 2 == 0 { "par" } else { "impar" };
            batch.add_document(id, &format!("comum {parity} n{id}")).unwrap();
        }
        batch.commit().unwrap();
    }
    assert_eq!(engine.search("comum").unwrap().len(), 2000);
    assert_eq!(engine.search("par").unwrap().len(), 1000);
    assert_eq!(engine.search("n1999").unwrap(), [1999]);
    assert_eq!(engine.search("n199*").unwrap(), [199, 1990, 1991, 1992, 1993, 1994, 1995, 1996, 1997, 1998, 1999]);
}

#[test]
fn readers_run_while_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    engine.index_document(0, "sempre").unwrap();

    let writer = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            for id in 1..=200u64 {
                engine.index_document(id, &format!("sempre doc{id}")).unwrap();
            }
        })
    };
    let mut last = 0;
    while !writer.is_finished() {
        let found = engine.search("sempre").unwrap();
        // Lotes são atômicos: a contagem só cresce e a lista é sempre válida.
        assert!(found.len() >= last);
        assert!(found.windows(2).all(|w| w[0] < w[1]));
        last = found.len();
    }
    writer.join().unwrap();
    assert_eq!(engine.search("sempre").unwrap().len(), 201);
}

#[test]
fn explicit_labels_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    for round in 0..3u64 {
        let mut batch = engine.batch();
        batch.add_document(100 + round, "termo compartilhado").unwrap();
        batch.set_label(100 + round, &format!("Artigo {round}"));
        batch.commit().unwrap();
    }
    // Três pedaços de merge em "compartilhado"; depois da compactação, um só.
    assert_eq!(engine.search("compartilhado").unwrap(), [100, 101, 102]);
    engine.compact();
    assert_eq!(engine.search("compartilhado").unwrap(), [100, 101, 102]);
    assert_eq!(engine.label(101).unwrap().as_deref(), Some("Artigo 1"));
}
