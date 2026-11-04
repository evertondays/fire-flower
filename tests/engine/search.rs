//! Busca: exata, E, prefixo, autocompletar, stopwords e listas com saltos.

use fire_flower::Engine;

use crate::{open, sample};

#[test]
pub(crate) fn empty_index_finds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    assert!(engine.search("qualquer").unwrap().is_empty());
    assert!(engine.suggest("q", 10).unwrap().is_empty());
}

#[test]
pub(crate) fn exact_search_is_normalized() {
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
pub(crate) fn multi_term_is_and() {
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
pub(crate) fn prefix_search() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.search("program*").unwrap(), [3, 4]);
    assert_eq!(engine.search("ro*").unwrap(), [1, 2, 4]); // rato? não: roeu, roupa, roma, rotina
    assert_eq!(engine.search("program* cafe").unwrap(), [4]);
}

#[test]
pub(crate) fn suggest_shortest_first() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    sample(&engine);
    assert_eq!(engine.suggest("ro", 10).unwrap(), ["roeu", "roma", "roupa", "rotina"]);
    assert_eq!(engine.suggest("RO", 2).unwrap(), ["roeu", "roma"]);
    assert_eq!(engine.suggest("programa", 10).unwrap(), ["programacao", "programador"]);
    assert_eq!(engine.suggest("rei", 10).unwrap(), ["rei"]);
}

#[test]
pub(crate) fn stopwords_are_not_indexed_nor_required() {
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

/// Formato v4: listas com tabela de saltos. E, frases e ranking com listas
/// que atravessam muitos blocos de 128 documentos, montadas em vários lotes
/// (merge rápido), com reindexação (merge completo) e depois de compactar,
/// têm que dar o mesmo que um cálculo ingênuo.
#[test]
pub(crate) fn skip_lists_give_the_same_results() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let words = ["sol", "lua", "mar", "rio", "monte", "vale", "pedra", "areia"];
    let mut rng = 2024u64;
    let mut next = |n: u64| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (rng >> 33) % n
    };
    let mut docs: std::collections::BTreeMap<u64, Vec<&str>> = std::collections::BTreeMap::new();
    let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<&str> {
        // Palavras comuns em quase todo documento; "rara" de vez em quando.
        let mut text: Vec<&str> = (0..2 + next(8)).map(|_| words[next(words.len() as u64) as usize]).collect();
        if next(40) == 0 {
            text.push("rara");
        }
        text
    };
    for chunk in 0..4u64 {
        let mut batch = engine.batch();
        for i in 0..900u64 {
            let id = chunk * 900 + i;
            let text = make(&mut next);
            batch.add_document(id * 3, &text.join(" ")).unwrap();
            docs.insert(id * 3, text);
        }
        batch.commit().unwrap();
    }
    // Reindexa documentos espalhados: os pedaços se sobrepõem às listas antigas.
    let mut batch = engine.batch();
    for id in (0..3600u64).step_by(37) {
        let text = make(&mut next);
        batch.add_document(id * 3, &text.join(" ")).unwrap();
        // O texto novo substitui o antigo: termos que saíram não acham mais o documento.
        docs.insert(id * 3, text);
    }
    batch.commit().unwrap();

    let has = |d: &Vec<&str>, w: &str| d.contains(&w);
    let check = |engine: &Engine, when: &str| {
        for q in ["sol lua", "rara sol", "rara mar pedra", "vale areia monte", "sol", "rara"] {
            let terms: Vec<&str> = q.split(' ').collect();
            let want: Vec<u64> = docs.iter().filter(|(_, d)| terms.iter().all(|t| has(d, t))).map(|(&id, _)| id).collect();
            assert_eq!(engine.search(q).unwrap(), want, "{when}: {q}");
            let top = engine.search_ranked(q, 20).unwrap();
            assert_eq!(top.total, want.len(), "{when}: {q} (ranqueado)");
            let all = engine.search_ranked(q, usize::MAX).unwrap();
            assert_eq!(top.hits, all.hits[..top.hits.len()], "{when}: {q} (top 20 = começo do ranking)");
        }
        // Frases: conferidas contra a busca sem frase (subconjunto) e contra
        // a contagem do ranking.
        for q in ["\"sol lua\"", "\"mar rio\" sol", "rara \"pedra areia\""] {
            let found = engine.search(q).unwrap();
            let loose = engine.search(&q.replace('"', "")).unwrap();
            assert!(found.iter().all(|d| loose.binary_search(d).is_ok()), "{when}: {q}");
            assert!(!found.is_empty() || q.contains("rara"), "{when}: {q} vazio");
            assert_eq!(engine.search_ranked(q, 5).unwrap().total, found.len(), "{when}: {q}");
        }
    };
    check(&engine, "antes de compactar");
    let before: Vec<Vec<u64>> = ["\"sol lua\"", "\"mar rio\" sol"].iter().map(|q| engine.search(q).unwrap()).collect();
    engine.compact();
    check(&engine, "depois de compactar");
    let after: Vec<Vec<u64>> = ["\"sol lua\"", "\"mar rio\" sol"].iter().map(|q| engine.search(q).unwrap()).collect();
    assert_eq!(before, after);
}
