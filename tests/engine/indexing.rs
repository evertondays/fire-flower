//! Indexação: lotes, reindexação, rótulos, cache de nós e o que fica gravado.

use fire_flower::IndexStats;
use crate::positions;
use crate::p;
use fire_flower::Config;
use fire_flower::Engine;
use crate::sample;
use crate::open;

#[test]
pub(crate) fn persists_across_reopen_and_appends() {
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
pub(crate) fn reindexing_same_doc_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(7, "eco eco eco").unwrap();
    engine.index_document(7, "eco").unwrap();
    assert_eq!(engine.search("eco").unwrap(), [7]);
}

#[test]
pub(crate) fn dropped_batch_writes_nothing() {
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
pub(crate) fn labels_and_auto_ids() {
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
pub(crate) fn many_docs_across_batches() {
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
pub(crate) fn explicit_labels_and_compaction() {
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

#[test]
pub(crate) fn diagnostics_walk_terms_and_raw_keys() {
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

    let (mut post_keys, mut pos_keys, mut norm_keys) = (0, 0, 0);
    engine
        .for_each_raw(|k, _| {
            post_keys += k.starts_with(b"post:") as usize;
            pos_keys += k.starts_with(b"pos:") as usize;
            norm_keys += k.starts_with(b"norm:") as usize;
        })
        .unwrap();
    assert_eq!(post_keys, terms.len());
    assert_eq!(pos_keys, terms.len());
    assert_eq!(norm_keys, 1, "IDs 1 a 4 cabem num bloco");
    assert!(engine.trie_node_count().unwrap() > terms.len() as u64);
    assert!(engine.rocksdb_stats().unwrap().contains("rocksdb.block.cache"));
    assert!(engine.property("rocksdb.estimate-num-keys").is_some());
}

/// Mesmo conteúdo com o cache de nós desligado, minúsculo e padrão.
#[test]
pub(crate) fn node_cache_limit_does_not_change_results() {
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
pub(crate) fn dropped_batch_after_cached_batches() {
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
pub(crate) fn stores_tf_positions_lengths_and_stats() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    assert!(engine.has_positions(), "posições ligadas por padrão");
    let mut batch = engine.batch();
    // Posições contam stopwords: casa@0 de@1 papel@2.
    batch.add_document(1, "Casa de papel, casa velha").unwrap();
    batch.add_document(2, "papel").unwrap();
    batch.commit().unwrap();
    engine.index_document(3, "o que é").unwrap(); // só stopwords: tamanho 0

    assert_eq!(engine.term_postings("casa").unwrap(), [p(1, 2)]);
    assert_eq!(engine.term_postings("papel").unwrap(), [p(1, 1), p(2, 1)]);
    assert_eq!(positions(&engine, "casa"), [(1, vec![0, 3])]);
    assert_eq!(positions(&engine, "papel"), [(1, vec![2]), (2, vec![0])]);
    assert_eq!(engine.doc_len(1).unwrap(), Some(4), "stopwords não contam");
    assert_eq!(engine.doc_len(2).unwrap(), Some(1));
    assert_eq!(engine.doc_len(3).unwrap(), Some(0));
    assert_eq!(engine.doc_len(4).unwrap(), None);
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 3, total_len: 5, total_title_len: 0 });

    let mut lens = Vec::new();
    engine.for_each_doc_len(|d, l, t| lens.push((d, l, t))).unwrap();
    assert_eq!(lens, [(1, 4, 0), (2, 1, 0), (3, 0, 0)]);

    // Persistem e seguem certos depois de reabrir e compactar.
    drop(engine);
    let engine = open(&dir);
    engine.index_document(2000, "papel papel").unwrap(); // outro bloco de norm:
    engine.compact();
    assert_eq!(engine.term_postings("papel").unwrap(), [p(1, 1), p(2, 1), p(2000, 2)]);
    assert_eq!(positions(&engine, "papel"), [(1, vec![2]), (2, vec![0]), (2000, vec![0, 1])]);
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 4, total_len: 7, total_title_len: 0 });
    assert_eq!(engine.doc_len(2000).unwrap(), Some(2));
}

/// D6: reindexar o mesmo ID troca tf, posições e tamanho, e não conta o
/// documento duas vezes, no mesmo lote ou em lotes diferentes.
#[test]
pub(crate) fn reindexing_replaces_tf_positions_and_length() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(7, "eco eco eco").unwrap();
    engine.index_document(7, "eco").unwrap();
    assert_eq!(engine.term_postings("eco").unwrap(), [p(7, 1)]);
    assert_eq!(positions(&engine, "eco"), [(7, vec![0])]);
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 1, total_len: 1, total_title_len: 0 });

    // No mesmo lote, fora de ordem: a última adição vence.
    let mut batch = engine.batch();
    batch.add_document(9, "sol sol").unwrap();
    batch.add_document(8, "sol lua").unwrap();
    batch.add_document(9, "lua sol lua").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.term_postings("sol").unwrap(), [p(8, 1), p(9, 1)]);
    assert_eq!(positions(&engine, "sol"), [(8, vec![0]), (9, vec![1])]);
    assert_eq!(positions(&engine, "lua"), [(8, vec![1]), (9, vec![0, 2])]);
    assert_eq!(engine.doc_len(9).unwrap(), Some(3));
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 3, total_len: 1 + 2 + 3, total_title_len: 0 });

    // Depois de compactar (merge completo) continua igual.
    engine.compact();
    assert_eq!(engine.term_postings("sol").unwrap(), [p(8, 1), p(9, 1)]);
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 3, total_len: 6, total_title_len: 0 });
    drop(engine);
    let engine = open(&dir);
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 3, total_len: 6, total_title_len: 0 });
}
