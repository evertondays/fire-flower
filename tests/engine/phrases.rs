//! Busca por frase.

use fire_flower::Config;
use crate::titled;
use crate::close;
use crate::ranked;
use crate::B;
use crate::K1;
use crate::open;
use fire_flower::Engine;

pub(crate) fn phrase_sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_document(1, "A casa de papel pegou fogo").unwrap();
    batch.add_document(2, "papel de casa e casa da papelaria").unwrap();
    batch.add_document(3, "uma casa da papel, outra casa de papel").unwrap();
    batch.add_document(4, "Copa do Mundo de 2014 no Brasil").unwrap();
    batch.commit().unwrap();
    engine.index_document(5, "de novo de novo e de novo").unwrap();
    engine.index_document(6, "casa papel").unwrap();
}

#[test]
pub(crate) fn phrase_search() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    phrase_sample(&engine);
    assert_eq!(engine.search("\"casa de papel\"").unwrap(), [1, 3]);
    // Stopword no meio só conta a distância (limitação conhecida).
    assert_eq!(engine.search("\"casa da papel\"").unwrap(), [1, 3]);
    // No 3, "papel, outra casa": "outra" também é stopword, então casa.
    assert_eq!(engine.search("\"papel de casa\"").unwrap(), [2, 3]);
    assert_eq!(engine.search("\"casa papel\"").unwrap(), [6], "sem buraco: adjacentes");
    assert_eq!(engine.search("casa papel").unwrap(), [1, 2, 3, 6], "sem aspas: E comum");
    assert_eq!(engine.search("brasil \"copa do mundo\" 2014").unwrap(), [4]);
    assert!(engine.search("\"mundo do copa\"").unwrap().is_empty(), "ordem trocada");
    assert!(engine.search("\"casa de inexistente\"").unwrap().is_empty());
    assert_eq!(engine.search("\"novo de novo\"").unwrap(), [5], "termo repetido");
    assert_eq!(engine.search("\"pegou fogo\" cas*").unwrap(), [1]);
    assert_eq!(engine.search("\"casa de papel\" \"pegou fogo\"").unwrap(), [1]);
    assert_eq!(engine.search("\"papel\"").unwrap(), [1, 2, 3, 6]);
    assert!(engine.search("\"de da do\"").unwrap().is_empty(), "só stopwords");
    // Aspas sem fechamento vão até o fim.
    assert_eq!(engine.search("\"casa de papel").unwrap(), [1, 3]);
    // Persistem ao reabrir e depois de compactar.
    drop(engine);
    let engine = open(&dir);
    engine.compact();
    assert_eq!(engine.search("\"casa de papel\"").unwrap(), [1, 3]);
}

#[test]
pub(crate) fn phrase_ranking_counts_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    phrase_sample(&engine);
    let stats = engine.stats().unwrap();
    let (n, avg) = (stats.doc_count as f64, stats.avg_len());
    let idf = |df: f64| (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    let len = |d: u64| f64::from(engine.doc_len(d).unwrap().unwrap());
    let score = |idf: f64, tf: f64, d: u64| idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len(d) / avg));

    // "casa" está em 1, 2, 3, 6 e "papel" em 1, 2, 3, 6; o doc 3 tem a frase 2 vezes.
    let phrase_idf = idf(4.0) + idf(4.0);
    let got = ranked(&engine, "\"casa de papel\"");
    let want = [(1, score(phrase_idf, 1.0, 1)), (3, score(phrase_idf, 2.0, 3))];
    assert_eq!(got.len(), 2);
    for (d, s) in want {
        let g = got.iter().find(|g| g.0 == d).unwrap().1;
        assert!(close(g, s), "doc {d}: {g} vs {s}");
    }
    // Frase + termo somam.
    let got = ranked(&engine, "fogo \"casa de papel\"");
    assert_eq!(got.len(), 1);
    assert!(close(got[0].1, score(idf(1.0), 1.0, 1) + score(phrase_idf, 1.0, 1)));
}

/// Posições: o título em 0…T − 1, o texto a partir de T + TITLE_GAP. Uma
/// frase não casa atravessando o fim do título e o começo do texto.
#[test]
pub(crate) fn phrase_does_not_cross_from_title_to_text() {
    use fire_flower::analysis::TITLE_GAP;
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_titled(1, "Casa de", "papel azul").unwrap();
    batch.add_titled(2, "Outro", "casa de papel").unwrap();
    batch.commit().unwrap();
    let node = engine.term_node("papel").unwrap().unwrap();
    // Título "Casa de" tem 2 tokens: o texto começa em 2 + TITLE_GAP.
    assert_eq!(engine.positions_of_node(node).unwrap(), [(1, vec![2 + TITLE_GAP]), (2, vec![1 + TITLE_GAP + 2])]);
    assert_eq!(engine.search("\"casa de papel\"").unwrap(), [2]);
    assert_eq!(engine.search("casa papel").unwrap(), [1, 2], "o E continua valendo entre os campos");
}

/// Frase no título: a ocorrência conta como título pela posição.
#[test]
pub(crate) fn phrase_in_title_counts_as_title() {
    let dir = tempfile::tempdir().unwrap();
    let engine = titled(&dir, Config::default(), &[(1, "Casa de papel", "outra coisa aqui"), (2, "Série", "casa de papel outra")]);
    let got = ranked(&engine, "\"casa de papel\"");
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [1, 2]);
    assert!(got[0].1 > got[1].1);
    // Sem peso no título as duas ocorrências valem como texto.
    drop(engine);
    let engine = Engine::open(dir.path(), Config { title_weight: 0.0, ..Config::default() }).unwrap();
    let got: std::collections::HashMap<u64, f64> = ranked(&engine, "\"casa de papel\"").into_iter().collect();
    assert_eq!(got[&1], 0.0, "com peso 0 a frase só no título não pontua");
}
