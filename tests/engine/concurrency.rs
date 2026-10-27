//! Buscas em paralelo com a indexação: cada uma vê um momento só do índice.

use std::sync::Arc;

use fire_flower::{Config, Engine};

use crate::open;

#[test]
pub(crate) fn readers_run_while_indexing() {
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

/// Fase 5 do plano: uma busca nunca vê metade de um lote. Cada lote tem um
/// marcador próprio; quem lê tem que ver o lote inteiro ou nada dele.
#[test]
pub(crate) fn search_never_sees_half_a_batch() {
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

/// Nenhuma busca ranqueada pode ver um documento sem o tamanho dele, nem um
/// tamanho de outro momento: o cache de `norm:` respeita os snapshots.
#[test]
pub(crate) fn ranking_while_indexing_sees_consistent_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    engine.index_document(0, "sempre").unwrap();
    let writer = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            // Mesmo bloco de norm: (IDs < 1024), e reindexando com outro tamanho.
            for id in 1..=300u64 {
                engine.index_document(id, &"sempre ".repeat(1 + (id % 5) as usize)).unwrap();
                if id % 3 == 0 {
                    engine.index_document(id / 2, "sempre agora").unwrap();
                }
            }
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let mut rounds = 0;
                loop {
                    let done = engine.search("sempre").unwrap().len() == 301;
                    let top = engine.search_ranked("sempre", 5).unwrap();
                    assert!(top.total >= 1);
                    assert!(top.hits.iter().all(|h| h.score.is_finite() && h.score > 0.0));
                    rounds += 1;
                    if done {
                        return rounds;
                    }
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        assert!(r.join().unwrap() > 0);
    }
    assert_eq!(engine.stats().unwrap().doc_count, 301);
}

/// O cache de nós rasos das buscas não pode esconder termos de commits
/// novos: cada commit marca os nós rasos que mudou, e a busca seguinte relê.
#[test]
pub(crate) fn reader_node_cache_sees_new_commits() {
    for depth in [None, Some(0), Some(2), Some(64)] {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path(), Config { reader_cache_depth: depth, ..Config::default() }).unwrap();
        engine.index_document(1, "abc").unwrap();
        // Enche o cache com a raiz e os nós de "a", "ab"; "abd", "x" e "ax" ainda não existem.
        assert_eq!(engine.search("abc").unwrap(), [1]);
        assert!(engine.search("abd").unwrap().is_empty());
        assert!(engine.search("x").unwrap().is_empty());
        assert!(engine.suggest("a", 10).unwrap().contains(&"abc".to_string()));

        // Filho novo na raiz (profundidade 1), em "a" (2) e em "ab" (3).
        let mut batch = engine.batch();
        batch.add_document(2, "x ax abd").unwrap();
        batch.commit().unwrap();
        for q in ["x", "ax", "abd"] {
            assert_eq!(engine.search(q).unwrap(), [2], "{q} com cache até {depth:?}");
        }
        assert_eq!(engine.search("abc").unwrap(), [1]);
        assert_eq!(engine.suggest("a", 10).unwrap(), ["ax", "abc", "abd"], "{depth:?}");
        assert_eq!(engine.search_top("ab*", 10).unwrap().len(), 2);

        // Reaberto: o cache começa vazio e o banco tem tudo.
        drop(engine);
        let engine = Engine::open(dir.path(), Config { reader_cache_depth: depth, ..Config::default() }).unwrap();
        assert_eq!(engine.search("ax").unwrap(), [2]);
    }
}

/// Buscas em paralelo com commits que criam nós rasos: um termo, depois de
/// aparecer para uma thread, não some mais, e no fim todos aparecem.
#[test]
pub(crate) fn reader_node_cache_under_concurrent_commits() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    engine.index_document(1, "base").unwrap();
    // Termos de duas letras novas: cada um cria nós na raiz e no nível 1.
    let terms: Vec<String> = (0..200u32)
        .map(|i| {
            let a = char::from_u32(0x4e00 + i).unwrap(); // ideogramas: um filho novo na raiz por termo
            format!("{a}{a}")
        })
        .collect();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let engine = Arc::clone(&engine);
            let terms = terms.clone();
            std::thread::spawn(move || {
                let mut seen = vec![false; terms.len()];
                for _ in 0..50 {
                    for (i, t) in terms.iter().enumerate() {
                        let found = !engine.search(t).unwrap().is_empty();
                        assert!(found || !seen[i], "{t} sumiu depois de aparecer");
                        seen[i] |= found;
                    }
                }
            })
        })
        .collect();
    for (i, t) in terms.iter().enumerate() {
        engine.index_document(10 + i as u64, t).unwrap();
    }
    for r in readers {
        r.join().unwrap();
    }
    for (i, t) in terms.iter().enumerate() {
        assert_eq!(engine.search(t).unwrap(), [10 + i as u64]);
    }
}
