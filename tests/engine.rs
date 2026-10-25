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

/// Fase 5 do plano: uma busca nunca vê metade de um lote. Cada lote tem um
/// marcador próprio; quem lê tem que ver o lote inteiro ou nada dele.
#[test]
fn search_never_sees_half_a_batch() {
    const BATCHES: u64 = 60;
    const PER_BATCH: u64 = 50;
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));

    let writer = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            for b in 0..BATCHES {
                let mut batch = engine.batch();
                for i in 0..PER_BATCH {
                    batch.add_document(b * PER_BATCH + i, &format!("comum lote{b}")).unwrap();
                }
                batch.commit().unwrap();
            }
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let mut checks = 0;
                loop {
                    let done = engine.search("comum").unwrap().len() as u64 == BATCHES * PER_BATCH;
                    for b in 0..BATCHES {
                        let n = engine.search(&format!("lote{b} comum")).unwrap().len() as u64;
                        assert!(n == 0 || n == PER_BATCH, "lote {b} visto pela metade: {n}");
                        checks += 1;
                    }
                    if done {
                        return checks;
                    }
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        assert!(r.join().unwrap() > 0);
    }
}

#[test]
fn diagnostics_walk_terms_and_raw_keys() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Config { statistics: true, ..Config::default() }).unwrap();
    sample(&engine);
    let mut terms = Vec::new();
    engine.for_each_term(|t, node| terms.push((t.to_string(), node))).unwrap();
    let words: Vec<&str> = terms.iter().map(|(t, _)| t.as_str()).collect();
    assert!(words.windows(2).all(|w| w[0] < w[1]), "ordem alfabética");
    assert!(words.contains(&"roupa") && words.contains(&"programacao"));
    let (_, node) = terms.iter().find(|(t, _)| t == "roupa").unwrap();
    assert_eq!(engine.postings_of_node(*node).unwrap(), [1, 2]);
    assert_eq!(engine.term_node("Roupa").unwrap(), Some(*node));

    let mut post_keys = 0;
    engine.for_each_raw(|k, _| post_keys += k.starts_with(b"post:") as usize).unwrap();
    assert_eq!(post_keys, terms.len());
    assert!(engine.trie_node_count().unwrap() > terms.len() as u64);
    assert!(engine.rocksdb_stats().unwrap().contains("rocksdb.block.cache"));
    assert!(engine.property("rocksdb.estimate-num-keys").is_some());
}

#[test]
fn stopwords_are_not_indexed_nor_required() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    engine.index_document(5, "The Lord of the Rings, em dezembro").unwrap();
    // Stopword sozinha não acha nada: ela não está no índice.
    assert!(engine.search("do").unwrap().is_empty());
    assert!(engine.postings("the").unwrap().is_empty());
    assert!(engine.search("o que é").unwrap().is_empty());
    // No meio da consulta, é ignorada em vez de zerar o resultado.
    assert_eq!(engine.search("roupa do rei").unwrap(), [1]);
    assert_eq!(engine.search("the lord of the rings").unwrap(), [5]);
    // Como prefixo continua valendo.
    assert_eq!(engine.search("de*").unwrap(), [5]);
    assert_eq!(engine.suggest("d", 10).unwrap(), ["dezembro"]);
}

/// Mesmo conteúdo com o cache de nós desligado, minúsculo e padrão.
#[test]
fn node_cache_limit_does_not_change_results() {
    let digests: Vec<Vec<(String, Vec<u64>)>> = [0usize, 3, 1_000_000]
        .into_iter()
        .map(|limit| {
            let dir = tempfile::tempdir().unwrap();
            let engine = Engine::open(dir.path(), Config { writer_cache_nodes: limit, ..Config::default() }).unwrap();
            for chunk in 0..6u64 {
                let mut batch = engine.batch();
                for i in 0..50u64 {
                    let id = chunk * 50 + i;
                    batch.add_document(id, &format!("comum termo{id} prefixo{} lote{chunk}", id % 7)).unwrap();
                }
                batch.commit().unwrap();
            }
            let mut terms = Vec::new();
            engine.for_each_term(|t, node| terms.push((t.to_string(), node))).unwrap();
            terms.into_iter().map(|(t, node)| (t, engine.postings_of_node(node).unwrap())).collect()
        })
        .collect();
    assert_eq!(digests[0].len(), 300 + 7 + 6 + 1);
    assert_eq!(digests[0], digests[1]);
    assert_eq!(digests[0], digests[2]);
}

/// Um lote descartado não pode deixar nós que não foram gravados no cache.
#[test]
fn dropped_batch_after_cached_batches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(1, "casa").unwrap();
    {
        let mut batch = engine.batch();
        batch.add_document(2, "casamento casarao").unwrap();
        // Sem commit: com o lote invertido a Trie nem foi tocada, mas o
        // cache emprestado também tem que ser descartado.
    }
    engine.index_document(3, "casulo").unwrap();
    assert_eq!(engine.suggest("cas", 10).unwrap(), ["casa", "casulo"]);
    drop(engine);
    let engine = open(&dir);
    assert_eq!(engine.suggest("cas", 10).unwrap(), ["casa", "casulo"]);
    assert_eq!(engine.search("casulo").unwrap(), [3]);
}

#[test]
fn languages_are_stored_on_creation_and_checked_on_open() {
    use fire_flower::{Error, Language};
    let dir = tempfile::tempdir().unwrap();
    let english = Config { languages: Some(vec![Language::English]), ..Config::default() };
    {
        let engine = Engine::open(dir.path(), english.clone()).unwrap();
        assert_eq!(engine.languages(), [Language::English]);
        engine.index_document(1, "the house de papel").unwrap();
    }
    // Sem pedir nada, vale o que está gravado: "de" é termo, "the" não.
    let engine = open(&dir);
    assert_eq!(engine.languages(), [Language::English]);
    assert_eq!(engine.search("de").unwrap(), [1]);
    assert!(engine.postings("the").unwrap().is_empty());
    assert_eq!(engine.search("the house").unwrap(), [1]);
    drop(engine);
    // Pedir o mesmo funciona; pedir outro idioma é erro, não uma busca errada.
    drop(Engine::open(dir.path(), english).unwrap());
    let pt = Config { languages: Some(vec![Language::Portuguese]), ..Config::default() };
    assert!(matches!(Engine::open(dir.path(), pt), Err(Error::Config(_))));
}

#[test]
fn database_without_stopwords() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Config { languages: Some(vec![]), ..Config::default() }).unwrap();
    assert!(engine.languages().is_empty());
    engine.index_document(1, "o que é").unwrap();
    assert_eq!(engine.search("o que").unwrap(), [1]);
}
