//! Ranking: BM25 e BM25F conferidos à mão, empates e o top-k.

use fire_flower::{Config, Engine, Error, Hit, Posting};

use crate::{B, K1, bm25, close, open, p, ranked, titled};

#[test]
pub(crate) fn bm25_matches_hand_computation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_document(1, "gato gato cachorro").unwrap();
    batch.add_document(2, "gato").unwrap();
    batch.add_document(3, "cachorro peixe peixe peixe").unwrap();
    batch.commit().unwrap();
    let (n, avg) = (3.0, 8.0 / 3.0);

    // Documento curto com o termo uma vez ganha do longo com duas.
    let got = ranked(&engine, "gato");
    let (s1, s2) = (bm25(n, 2.0, 2.0, 3.0, avg), bm25(n, 2.0, 1.0, 1.0, avg));
    assert!(s2 > s1);
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [2, 1]);
    assert!(close(got[0].1, s2) && close(got[1].1, s1), "{got:?} vs {s2} {s1}");

    // E de dois termos: soma das notas, só onde os dois aparecem.
    let got = ranked(&engine, "cachorro gato");
    assert_eq!(got.len(), 1);
    assert!(close(got[0].1, bm25(n, 2.0, 1.0, 3.0, avg) + bm25(n, 2.0, 2.0, 3.0, avg)));

    // search_ranked dá o total e search_top com k pequeno corta.
    let top = engine.search_ranked("gato", 1).unwrap();
    assert_eq!(top.total, 2);
    assert_eq!(top.hits.len(), 1);
    assert_eq!(top.hits[0].doc, 2);
    assert!(engine.search_top("gato", 0).unwrap().is_empty());
    assert_eq!(engine.search_ranked("gato", 0).unwrap().total, 2);
    assert!(engine.search_top("inexistente", 10).unwrap().is_empty());
    assert!(engine.search_top("", 10).unwrap().is_empty());
    assert!(engine.search_top("o que", 10).unwrap().is_empty());
}

#[test]
pub(crate) fn ties_break_by_id_and_prefix_is_one_term() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    for id in [30, 10, 20] {
        batch.add_document(id, "igual").unwrap();
    }
    // R1: o prefixo é um termo só, com idf do conjunto e tf somado.
    batch.add_document(40, "carro carroca").unwrap();
    batch.add_document(41, "carro xis").unwrap();
    batch.add_document(42, "carroca xis").unwrap();
    batch.commit().unwrap();

    let got = ranked(&engine, "igual");
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [10, 20, 30]);
    assert!(got.windows(2).all(|w| w[0].1 == w[1].1));

    // "carr*" casa 3 documentos: df = 3, e o 40 tem as duas completações (tf 2).
    let (n, avg) = (6.0, 9.0 / 6.0);
    let prefix_score = |tf: f64, len: f64| bm25(n, 3.0, tf, len, avg);
    let got: std::collections::HashMap<u64, f64> = ranked(&engine, "carr*").into_iter().collect();
    assert_eq!(got.len(), 3);
    assert!(close(got[&40], prefix_score(2.0, 2.0)), "tf somado");
    assert!(close(got[&41], prefix_score(1.0, 2.0)));
    assert!(close(got[&42], prefix_score(1.0, 2.0)));
    assert!(got[&40] > got[&41]);
    let got = ranked(&engine, "carr* xis");
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [41, 42]);
}

/// O caso "nortel": uma completação rara não pode valer mais que a palavra
/// que se está digitando. Com o máximo por completação (a regra antiga), o
/// documento com "nortel" (df 1) ganhava dos que têm "norte" (df alto).
#[test]
pub(crate) fn rare_completion_does_not_beat_the_common_word() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_document(1, "coreia nortel empresa").unwrap();
    for id in 2..12 {
        batch.add_document(id, "coreia norte").unwrap();
    }
    for id in 12..40 {
        batch.add_document(id, &format!("norte outro{id}")).unwrap();
    }
    batch.commit().unwrap();
    let got = ranked(&engine, "coreia norte*");
    assert_eq!(got.len(), 11);
    assert_ne!(got[0].0, 1, "{got:?}");
    assert_eq!(got.last().unwrap().0, 1, "documento só com a completação rara vem por último: {got:?}");
    // Com só "norte" e "nortel" no índice, `norte*` tem o mesmo df de `norte` mais 1.
    let exact = ranked(&engine, "coreia norte");
    assert_eq!(exact.iter().map(|g| g.0).collect::<Vec<_>>(), got[..10].iter().map(|g| g.0).collect::<Vec<_>>());
}

/// Os cortes do top-k (limite superior e níveis de tf) não podem mudar o
/// resultado: com k pequeno tem que dar o começo do ranking completo, inclusive
/// nos empates (ID menor primeiro), que o termo único visita fora de ordem.
#[test]
pub(crate) fn top_k_pruning_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let mut rng = 99u64;
    let mut next = |n: u64| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (rng >> 33) % n
    };
    for id in (0..3000u64).rev() {
        // Muitos empates: só 4 tamanhos e tf de 1 a 5 (alguns com tf > 64).
        let tf = if id % 97 == 0 { 70 } else { 1 + next(5) as usize };
        let filler = ["x"; 3][..next(4) as usize].join(" ");
        batch.add_document(id * 7, &format!("{} {filler} outro", "alvo ".repeat(tf))).unwrap();
    }
    for id in 0..40u64 {
        batch.add_document(100_000 + id, "igual").unwrap();
    }
    batch.commit().unwrap();
    for q in ["alvo", "igual", "alvo outro", "alv*", "x alvo"] {
        let all = engine.search_ranked(q, usize::MAX).unwrap();
        assert!(all.hits.windows(2).all(|w| w[0].score > w[1].score || (w[0].score == w[1].score && w[0].doc < w[1].doc)), "{q}");
        for k in [1, 2, 5, 10, 33, 100, 2999] {
            let top = engine.search_ranked(q, k).unwrap();
            assert_eq!(top.total, all.total, "{q} k={k}");
            assert_eq!(top.hits, all.hits[..k.min(all.hits.len())], "{q} k={k}");
        }
    }
    let ties = engine.search_top("igual", 5).unwrap();
    assert_eq!(ties.iter().map(|h| h.doc).collect::<Vec<_>>(), [100_000, 100_001, 100_002, 100_003, 100_004]);
}

/// O ranking devolve exatamente os documentos da busca sem ranking.
#[test]
pub(crate) fn ranked_and_unranked_agree_on_matches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let words = ["sol", "lua", "mar", "rio", "casa", "papel", "de", "a", "monte", "vale"];
    let mut rng = 12345u64;
    let mut next = |n: usize| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (rng >> 33) as usize % n
    };
    for chunk in 0..4u64 {
        let mut batch = engine.batch();
        for i in 0..150u64 {
            let len = 1 + next(12);
            let text: Vec<&str> = (0..len).map(|_| words[next(words.len())]).collect();
            batch.add_document(chunk * 1000 + i * 3, &text.join(" ")).unwrap();
        }
        batch.commit().unwrap();
    }
    for q in ["sol", "sol lua", "m*", "\"casa de papel\"", "\"sol lua\" mar", "ri* \"a casa\"", "vale \"monte vale\"", "inexistente", "\"lua sol\" s*"] {
        let docs = engine.search(q).unwrap();
        let top = engine.search_ranked(q, usize::MAX).unwrap();
        assert_eq!(top.total, docs.len(), "{q}");
        let mut got: Vec<u64> = top.hits.iter().map(|h| h.doc).collect();
        assert!(top.hits.windows(2).all(|w| w[0].score > w[1].score || (w[0].score == w[1].score && w[0].doc < w[1].doc)), "{q}");
        got.sort_unstable();
        assert_eq!(got, docs, "{q}");
        let first: Vec<Hit> = engine.search_top(q, 5).unwrap();
        assert_eq!(first, top.hits[..top.hits.len().min(5)], "{q}");
    }
}

/// Formato v3: o título é um campo. As palavras dele são buscáveis, o tf do
/// título vem à parte na lista e os tamanhos do título ficam guardados.
#[test]
pub(crate) fn title_is_a_field() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_titled(1, "Coreia do Norte", "A Coreia do Norte fica na Ásia. Coreia, Coreia.").unwrap();
    batch.add_titled(2, "Bandeira", "A bandeira da Coreia do Norte.").unwrap();
    batch.add_document(3, "norte sem título").unwrap();
    batch.commit().unwrap();

    assert_eq!(engine.term_postings("coreia").unwrap(), [Posting { doc: 1, tf: 4, title_tf: 1 }, p(2, 1)]);
    assert_eq!(engine.term_postings("bandeira").unwrap(), [Posting { doc: 2, tf: 2, title_tf: 1 }]);
    assert_eq!(engine.term_postings("norte").unwrap(), [Posting { doc: 1, tf: 2, title_tf: 1 }, p(2, 1), p(3, 1)]);
    // Termo só no título também casa.
    assert_eq!(engine.search("bandeira").unwrap(), [2]);

    // Tamanhos: o inteiro (título + texto) e o do título, sem stopwords.
    let mut lens = Vec::new();
    engine.for_each_doc_len(|d, l, t| lens.push((d, l, t))).unwrap();
    assert_eq!(lens, [(1, 8, 2), (2, 4, 1), (3, 2, 0)]);
    let stats = engine.stats().unwrap();
    assert_eq!((stats.doc_count, stats.total_len, stats.total_title_len), (3, 14, 3));

    // Reindexar troca o título e as estatísticas.
    engine.index_document(2, "outro texto").unwrap();
    assert_eq!(engine.stats().unwrap().total_title_len, 2);

    // add_labeled_titled grava o rótulo e o título.
    let mut batch = engine.batch();
    let id = batch.add_labeled_titled("docs/readme.md", "readme", "instruções").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.label(id).unwrap().as_deref(), Some("docs/readme.md"));
    assert_eq!(engine.term_postings("readme").unwrap(), [Posting { doc: id, tf: 1, title_tf: 1 }]);
}

/// BM25F (plano de relevância, R4): tf~ = tf_texto / B_texto + w · tf_título / B_título.
pub(crate) fn bm25f(n: f64, df: f64, (tf_body, tf_title): (f64, f64), (body_len, title_len): (f64, f64), (avg_body, avg_title): (f64, f64), (w, bt): (f64, f64)) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    let b_body = 1.0 - B + B * body_len / avg_body;
    let b_title = 1.0 - bt + bt * title_len / avg_title;
    let ptf = tf_body / b_body + w * tf_title / b_title;
    idf * ptf * (K1 + 1.0) / (ptf + K1)
}

#[test]
pub(crate) fn bm25f_matches_hand_computation() {
    let dir = tempfile::tempdir().unwrap();
    let (w, bt) = (3.0, 0.5);
    let config = Config { title_weight: w, title_b: bt, ..Config::default() };
    let engine = titled(&dir, config, &[(1, "Coreia", "coreia fica asia"), (2, "Bandeira coreia", "verde"), (3, "Outro", "coreia coreia texto longo aqui")]);
    // Tamanhos (texto, título): 1 → (3, 1); 2 → (1, 2); 3 → (5, 1). Médias: texto 3, título 4/3.
    let (n, avg) = (3.0, (3.0, 4.0 / 3.0));
    let got: std::collections::HashMap<u64, f64> = ranked(&engine, "coreia").into_iter().collect();
    let want = [
        (1, bm25f(n, 3.0, (1.0, 1.0), (3.0, 1.0), avg, (w, bt))),
        (2, bm25f(n, 3.0, (0.0, 1.0), (1.0, 2.0), avg, (w, bt))),
        (3, bm25f(n, 3.0, (2.0, 0.0), (5.0, 1.0), avg, (w, bt))),
    ];
    for (doc, score) in want {
        assert!(close(got[&doc], score), "doc {doc}: {} vs {score}", got[&doc]);
    }
    // Termo só no título também pontua; "verde" só no texto do 2.
    assert!(close(ranked(&engine, "verde")[0].1, bm25f(n, 1.0, (1.0, 0.0), (1.0, 2.0), avg, (w, bt))));
}

/// O caso "Coreia do Norte": o artigo cujo título é a consulta vem antes dos
/// derivados ("Bandeira da…", "Demografia da…"), mesmo sendo mais longo; e
/// título curto vence título longo.
#[test]
pub(crate) fn title_match_ranks_first() {
    let dir = tempfile::tempdir().unwrap();
    let long_text = "a coreia do norte é um país da ásia com capital em pyongyang ".repeat(30);
    let engine = titled(
        &dir,
        Config::default(),
        &[
            (1, "Bandeira da Coreia do Norte", "a bandeira da coreia do norte tem estrela"),
            (2, "Coreia do Norte", &long_text),
            (3, "Demografia da Coreia do Norte", "população da coreia do norte"),
            (4, "Rio", "coreia norte coreia norte"),
        ],
    );
    let got = ranked(&engine, "coreia do norte");
    assert_eq!(got[0].0, 2, "{got:?}");
    assert_eq!(ranked(&engine, "coreia do nor*")[0].0, 2, "digitando também");
    assert_eq!(ranked(&engine, "\"coreia do norte\"")[0].0, 2, "frase também");
}

#[test]
pub(crate) fn invalid_ranking_config_is_rejected() {
    for config in [
        Config { title_weight: -1.0, ..Config::default() },
        Config { title_weight: f64::NAN, ..Config::default() },
        Config { title_b: 1.5, ..Config::default() },
        Config { bm25_b: -0.1, ..Config::default() },
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(Engine::open(dir.path(), config), Err(Error::Config(_))));
    }
    // b = 1 é válido (sem limite útil para o corte, mas exato).
    let dir = tempfile::tempdir().unwrap();
    let engine = titled(&dir, Config { bm25_b: 1.0, title_b: 1.0, ..Config::default() }, &[(1, "a", "x"), (2, "", "x x")]);
    assert_eq!(engine.search_top("x", 10).unwrap().len(), 2);
}

/// O corte do top-k com títulos (chave com o tf do título convertido) não
/// pode mudar o resultado: com k pequeno, o começo do ranking completo.
#[test]
pub(crate) fn top_k_pruning_is_exact_with_titles() {
    for (w, bt) in [(3.0, 0.5), (8.0, 0.0), (1.0, 0.75), (0.0, 0.5)] {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path(), Config { title_weight: w, title_b: bt, ..Config::default() }).unwrap();
        let mut batch = engine.batch();
        let mut rng = 7u64;
        let mut next = |n: u64| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 33) % n
        };
        for id in 0..2000u64 {
            let title = ["", "alvo", "alvo alvo outro", "outro", "x y z alvo"][next(5) as usize];
            let tf = 1 + next(6) as usize;
            let filler = ["x"; 4][..next(5) as usize].join(" ");
            batch.add_titled(id * 3, title, &format!("{} {filler} outro casa de papel", "alvo ".repeat(tf))).unwrap();
        }
        batch.commit().unwrap();
        for q in ["alvo", "alvo outro", "alv*", "x alvo", "\"casa de papel\"", "\"casa de papel\" alvo"] {
            let all = engine.search_ranked(q, usize::MAX).unwrap();
            for k in [1, 3, 10, 50, 500] {
                let top = engine.search_ranked(q, k).unwrap();
                assert_eq!(top.total, all.total, "{q} k={k} w={w}");
                assert_eq!(top.hits, all.hits[..k.min(all.hits.len())], "{q} k={k} w={w} b={bt}");
            }
        }
    }
}
