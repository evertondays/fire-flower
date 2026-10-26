use std::sync::Arc;

use fire_flower::text::Clause;
use fire_flower::{Config, Engine, Language, Stemming};

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

// ====================================================================
// Formato v2: tf, posições, tamanhos e estatísticas.

use fire_flower::{Error, Hit, IndexStats, Posting};

fn p(doc: u64, tf: u32) -> Posting {
    Posting::new(doc, tf)
}

fn positions(engine: &Engine, term: &str) -> Vec<(u64, Vec<u32>)> {
    engine.positions_of_node(engine.term_node(term).unwrap().unwrap()).unwrap()
}

#[test]
fn stores_tf_positions_lengths_and_stats() {
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
fn reindexing_replaces_tf_positions_and_length() {
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

#[test]
fn database_without_positions() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), Config { positions: Some(false), ..Config::default() }).unwrap();
        assert!(!engine.has_positions());
        engine.index_document(1, "casa de papel").unwrap();
        engine.index_document(2, "papel de casa").unwrap();
        let mut pos_keys = 0;
        engine.for_each_raw(|k, _| pos_keys += k.starts_with(b"pos:") as usize).unwrap();
        assert_eq!(pos_keys, 0);
    }
    // Sem pedir nada vale o que está gravado; pedir posições é erro.
    let engine = open(&dir);
    assert!(!engine.has_positions());
    assert_eq!(engine.search("casa papel").unwrap(), [1, 2]);
    assert_eq!(engine.search_top("casa", 10).unwrap().len(), 2);
    assert_eq!(engine.term_postings("casa").unwrap(), [p(1, 1), p(2, 1)]);
    assert!(matches!(engine.search("\"casa de papel\""), Err(Error::Config(_))));
    assert!(matches!(engine.search_top("\"casa de papel\"", 5), Err(Error::Config(_))));
    // Uma "frase" de um termo só é termo comum e funciona.
    assert_eq!(engine.search("\"o papel\"").unwrap(), [1, 2]);
    drop(engine);
    let with = Config { positions: Some(true), ..Config::default() };
    assert!(matches!(Engine::open(dir.path(), with), Err(Error::Config(_))));
}

#[test]
fn old_format_is_rejected_with_a_clear_error() {
    // Banco v1: contadores sem `meta:format`.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.put(b"post:4", [1u8, 7]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v1 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato antigo")), "{err}");

    // v1 com merges pendentes no WAL: a recuperação roda o merge operator.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_merge_operator_associative("fire_flower_postings_union", |_, existing, ops| {
            let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
            for op in ops {
                out.extend_from_slice(op);
            }
            Some(out)
        });
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.merge(b"post:4", [1u8, 7]).unwrap();
        db.merge(b"post:4", [1u8, 9]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v1 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato antigo")), "{err}");
    // E o banco antigo não foi tocado: o WAL com os merges continua lá e o
    // merge operator antigo ainda lê a lista como era.
    let mut opts = rocksdb::Options::default();
    opts.set_merge_operator_associative("fire_flower_postings_union", |_, existing, ops| {
        let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
        for op in ops {
            out.extend_from_slice(op);
        }
        Some(out)
    });
    let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
    assert_eq!(db.get(b"post:4").unwrap().unwrap(), [1u8, 7, 1, 9]);
}

// ====================================================================
// BM25

const K1: f64 = 1.2;
const B: f64 = 0.75;

fn bm25(n: f64, df: f64, tf: f64, len: f64, avg: f64) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * b.abs().max(1.0)
}

fn ranked(engine: &Engine, q: &str) -> Vec<(u64, f64)> {
    engine.search_top(q, 100).unwrap().into_iter().map(|h| (h.doc, h.score)).collect()
}

#[test]
fn bm25_matches_hand_computation() {
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
fn ties_break_by_id_and_prefix_is_one_term() {
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
    let carr = |tf: f64, len: f64| bm25(n, 3.0, tf, len, avg);
    let got: std::collections::HashMap<u64, f64> = ranked(&engine, "carr*").into_iter().collect();
    assert_eq!(got.len(), 3);
    assert!(close(got[&40], carr(2.0, 2.0)), "tf somado");
    assert!(close(got[&41], carr(1.0, 2.0)));
    assert!(close(got[&42], carr(1.0, 2.0)));
    assert!(got[&40] > got[&41]);
    let got = ranked(&engine, "carr* xis");
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [41, 42]);
}

/// O caso "nortel": uma completação rara não pode valer mais que a palavra
/// que se está digitando. Com o máximo por completação (a regra antiga), o
/// documento com "nortel" (df 1) ganhava dos que têm "norte" (df alto).
#[test]
fn rare_completion_does_not_beat_the_common_word() {
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
fn top_k_pruning_is_exact() {
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
fn ranked_and_unranked_agree_on_matches() {
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

// ====================================================================
// Frases

fn phrase_sample(engine: &Engine) {
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
fn phrase_search() {
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
fn phrase_ranking_counts_occurrences() {
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

/// Nenhuma busca ranqueada pode ver um documento sem o tamanho dele, nem um
/// tamanho de outro momento: o cache de `norm:` respeita os snapshots.
#[test]
fn ranking_while_indexing_sees_consistent_lengths() {
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
fn reader_node_cache_sees_new_commits() {
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
fn reader_node_cache_under_concurrent_commits() {
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

// ====================================================================
// Filtro de Bloom maior que uma parte do block cache.
//
// O block cache LRU se divide em até 64 partes de pelo menos 512 KB; um bloco
// maior que uma parte não fica no cache. Se o filtro de um SST for um bloco
// só, toda leitura que passa por esse SST relê o filtro inteiro. Na Wikipédia
// inteira (cache de 256 MB, partes de 4 MB) a compactação gera SSTs de chaves
// `node:` com filtros de ~6 MB, e a carga cai de ~5.000 para ~200 artigos/s
// no momento em que o primeiro aparece. Aqui o mesmo mecanismo, em escala
// pequena: cache de 1 MB (2 partes de 512 KB) e um SST com filtro de ~1 MB.

/// Contador do RocksDB pelo nome, a partir do texto das estatísticas.
fn ticker(engine: &Engine, name: &str) -> u64 {
    let stats = engine.rocksdb_stats().expect("estatísticas ligadas");
    stats
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(" COUNT : ")?.trim().parse().ok())
        .unwrap_or_else(|| panic!("contador {name} ausente"))
}

/// Bytes de filtro de Bloom que o RocksDB carregou no cache, por `Get`, em
/// buscas sobre um SST com ~1 milhão de chaves e um cache de 1 MB.
fn filter_bytes_per_get(partition_filters: bool) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { block_cache_bytes: 1 << 20, statistics: true, partition_filters, ..Config::default() };
    // Palavras aleatórias de 9 letras: quase sem prefixo comum, ~6 nós cada.
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let words: Vec<String> = (0..150_000)
        .map(|_| {
            (0..9)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    (b'a' + (rng % 26) as u8) as char
                })
                .collect()
        })
        .collect();
    {
        let engine = Engine::open(dir.path(), config.clone()).unwrap();
        let mut batch = engine.batch();
        for (i, chunk) in words.chunks(20).enumerate() {
            batch.add_document(i as u64, &chunk.join(" ")).unwrap();
        }
        batch.commit().unwrap();
        // Um SST só, com ~1 milhão de chaves: filtro de ~1 MB se for inteiro.
        engine.compact();
    }
    let engine = Engine::open(dir.path(), config).unwrap();
    let before = (ticker(&engine, "rocksdb.block.cache.filter.bytes.insert"), engine_reads());
    for w in words.iter().step_by(71).take(2000) {
        assert!(!engine.search(w).unwrap().is_empty(), "{w}");
    }
    let filter_bytes = ticker(&engine, "rocksdb.block.cache.filter.bytes.insert") - before.0;
    filter_bytes / (engine_reads() - before.1).max(1)
}

#[test]
fn bloom_filters_larger_than_a_cache_shard_are_not_reread_on_every_get() {
    // O cenário reproduz o problema com o filtro inteiro (~1 MB relido por Get)...
    let whole = filter_bytes_per_get(false);
    assert!(whole > 256 << 10, "o cenário não reproduziu o problema: {whole} bytes de filtro por Get");
    // ...e com partições de 4 KB o filtro fica no cache.
    let partitioned = filter_bytes_per_get(true);
    assert!(partitioned < 4096, "{partitioned} bytes de filtro carregados por Get (antes: {whole})");
}

fn engine_reads() -> u64 {
    fire_flower::internals::reads_on_this_thread()
}

/// Formato v3: o título é um campo. As palavras dele são buscáveis, o tf do
/// título vem à parte na lista e os tamanhos do título ficam guardados.
#[test]
fn title_is_a_field() {
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

/// Posições: o título em 0…T − 1, o texto a partir de T + TITLE_GAP. Uma
/// frase não casa atravessando o fim do título e o começo do texto.
#[test]
fn phrase_does_not_cross_from_title_to_text() {
    use fire_flower::text::TITLE_GAP;
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

/// Banco v2 (formato anterior ao título) com merges pendentes no WAL: tem que
/// ser recusado com a mensagem de recarregar, sem rodar o merge operator novo
/// e sem alterar o banco.
#[test]
fn v2_database_is_rejected_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let concat = |_: &[u8], existing: Option<&[u8]>, ops: &rocksdb::MergeOperands| {
        let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
        for op in ops {
            out.extend_from_slice(op);
        }
        Some(out)
    };
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_merge_operator_associative("fire_flower_v2", concat);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:format", [2u8]).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.merge(b"post:4", [1u8, 7, 1]).unwrap();
        db.merge(b"post:4", [1u8, 9, 1]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v2 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato 2") && m.contains("recarregue")), "{err}");
    // O merge operator antigo ainda lê a lista como era: nada foi tocado.
    let mut opts = rocksdb::Options::default();
    opts.set_merge_operator_associative("fire_flower_v2", concat);
    let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
    assert_eq!(db.get(b"post:4").unwrap().unwrap(), [1u8, 7, 1, 1, 9, 1]);
}

/// BM25F (plano de relevância, R4): tf~ = tf_texto / B_texto + w · tf_título / B_título.
fn bm25f(n: f64, df: f64, (tf_body, tf_title): (f64, f64), (body_len, title_len): (f64, f64), (avg_body, avg_title): (f64, f64), (w, bt): (f64, f64)) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    let b_body = 1.0 - B + B * body_len / avg_body;
    let b_title = 1.0 - bt + bt * title_len / avg_title;
    let ptf = tf_body / b_body + w * tf_title / b_title;
    idf * ptf * (K1 + 1.0) / (ptf + K1)
}

fn titled(dir: &tempfile::TempDir, config: Config, docs: &[(u64, &str, &str)]) -> Engine {
    let engine = Engine::open(dir.path(), config).unwrap();
    let mut batch = engine.batch();
    for (id, title, text) in docs {
        batch.add_titled(*id, title, text).unwrap();
    }
    batch.commit().unwrap();
    engine
}

#[test]
fn bm25f_matches_hand_computation() {
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
fn title_match_ranks_first() {
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

/// Frase no título: a ocorrência conta como título pela posição.
#[test]
fn phrase_in_title_counts_as_title() {
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

#[test]
fn invalid_ranking_config_is_rejected() {
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
fn top_k_pruning_is_exact_with_titles() {
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

/// Formato v4: listas com tabela de saltos. E, frases e ranking com listas
/// que atravessam muitos blocos de 128 documentos, montadas em vários lotes
/// (merge rápido), com reindexação (merge completo) e depois de compactar,
/// têm que dar o mesmo que um cálculo ingênuo.
#[test]
fn skip_lists_give_the_same_results() {
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
        // Termos que saíram do texto continuam apontando para o documento.
        let old = docs.remove(&(id * 3)).unwrap();
        let mut merged: Vec<&str> = text.clone();
        merged.extend(old.iter().filter(|w| !text.contains(w)));
        docs.insert(id * 3, merged);
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

fn stemmed() -> Config {
    Config { stemming: Some(Stemming::Snowball(Language::Portuguese)), ..Config::default() }
}

fn stem_sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_titled(1, "Praia", "O aluno estava correndo na praia").unwrap();
    batch.add_titled(2, "Corridas", "Os alunos brasileiros e as informações").unwrap();
    batch.add_titled(3, "Revolução Francesa", "Os franceses e a informação, em 1789").unwrap();
    batch.commit().unwrap();
}

#[test]
fn stemming_matches_inflections() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), stemmed()).unwrap();
    stem_sample(&engine);
    assert_eq!(engine.search("correr praias").unwrap(), [1]);
    assert_eq!(engine.search("corrida").unwrap(), [1, 2]);
    assert_eq!(engine.search("aluno").unwrap(), [1, 2]);
    assert_eq!(engine.search("INFORMAÇÕES").unwrap(), [2, 3]);
    assert_eq!(engine.search("francês").unwrap(), [3]);
    assert_eq!(engine.search("1789").unwrap(), [3]);
    // Continua insensível a acentos: o radical sai do termo normalizado.
    assert_eq!(engine.search("informacoes").unwrap(), [2, 3]);
    assert_eq!(engine.search("REVOLUCAO francesas").unwrap(), [3]);
    // Frases casam pelos radicais, com as stopwords como buraco.
    assert_eq!(engine.search("\"correu na praia\"").unwrap(), [1]);
    assert_eq!(engine.search("\"revoluções francesas\"").unwrap(), [3]);
    assert!(engine.search("\"praia correndo\"").unwrap().is_empty());
    // Stopwords continuam fora, conferidas na palavra inteira.
    assert!(engine.postings("os").unwrap().is_empty());
    // As APIs por palavra reduzem como a busca.
    assert_eq!(engine.postings("correndo").unwrap(), engine.postings("corridas").unwrap());
    assert_eq!(engine.index_term("Informações").as_deref(), Some("inform"));
    assert_eq!(engine.stemmer().stemming(), Stemming::Snowball(Language::Portuguese));
    // O ranking também: o título "Corridas" pesa para "corrida".
    assert_eq!(engine.search_top("corrida", 1).unwrap()[0].doc, 2);
}

#[test]
fn stemming_prefixes_match_stems_and_the_typed_word() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), stemmed()).unwrap();
    stem_sample(&engine);
    // Os radicais que começam com o prefixo.
    assert_eq!(engine.search("bras*").unwrap(), [2]);
    assert_eq!(engine.search("franc*").unwrap(), [3]);
    // A palavra inteira digitada como prefixo: nenhum radical começa com
    // "brasileiro" (é `brasileir`), então vale também o radical dela.
    assert_eq!(engine.search("brasileiro*").unwrap(), [2]);
    assert_eq!(engine.search("correndo*").unwrap(), [1, 2]);
    assert_eq!(engine.search_ranked("brasileiro*", 10).unwrap().total, 1);
    let clauses = engine.parse_query("brasileiro* bras*");
    let stems: Vec<Option<&str>> = clauses
        .iter()
        .map(|c| match c {
            Clause::Term(t) => t.stem.as_deref(),
            Clause::Phrase(_) => None,
        })
        .collect();
    assert_eq!(stems, [Some("brasileir"), None]);
    // O autocompletar mostra os radicais indexados.
    assert_eq!(engine.suggest("cor", 10).unwrap(), ["corr"]);
}

#[test]
fn stemming_is_stored_on_creation_and_checked_on_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), stemmed()).unwrap();
        stem_sample(&engine);
    }
    // `None` usa o gravado.
    let engine = Engine::open(dir.path(), Config::default()).unwrap();
    assert!(engine.stemmer().is_on());
    assert_eq!(engine.search("corrida").unwrap(), [1, 2]);
    drop(engine);
    let err = Engine::open(dir.path(), Config { stemming: Some(Stemming::Off), ..Config::default() }).err().unwrap();
    assert!(err.to_string().contains("stemming"), "{err}");

    // Banco novo sem nada na config: sem stemming, e fica gravado assim.
    let plain = tempfile::tempdir().unwrap();
    {
        let engine = open(&plain);
        assert!(!engine.stemmer().is_on());
        stem_sample(&engine);
        assert!(engine.search("corrida").unwrap().is_empty());
        assert_eq!(engine.search("corridas").unwrap(), [2]);
    }
    assert!(Engine::open(plain.path(), stemmed()).is_err());
}

#[test]
fn database_from_before_stemming_is_off() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        stem_sample(&engine);
        engine.compact(); // sem merges pendentes para o RocksDB puro abaixo
    }
    // Um banco com dados e sem `meta:stemming` é de antes do stemming.
    {
        let mut opts = rocksdb::Options::default();
        opts.set_merge_operator_associative("fire_flower_v5", |_, _, _| None);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        assert_eq!(db.get(b"meta:stemming").unwrap().as_deref(), Some(&b"nenhum"[..]));
        db.delete(b"meta:stemming").unwrap();
    }
    let engine = open(&dir);
    assert!(!engine.stemmer().is_on());
    assert_eq!(engine.search("corridas").unwrap(), [2]);
    drop(engine);
    assert!(Engine::open(dir.path(), stemmed()).is_err());
}

/// O cache de radicais passa de um lote para o outro: o resultado é o
/// mesmo que indexar tudo num lote só.
#[test]
fn stem_cache_across_batches_does_not_change_results() {
    let docs: Vec<String> = (0..300u64)
        .map(|i| format!("Correndo {i} informações brasileiras {} revoluções papéis", ["praia", "praias", "corrida"][i as usize % 3]))
        .collect();
    let digest = |batch_size: usize| {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path(), stemmed()).unwrap();
        for (n, chunk) in docs.chunks(batch_size).enumerate() {
            let mut batch = engine.batch();
            for (i, d) in chunk.iter().enumerate() {
                batch.add_document((n * batch_size + i) as u64 + 1, d).unwrap();
            }
            batch.commit().unwrap();
        }
        let mut terms = Vec::new();
        engine.for_each_term(|t, node| terms.push((t.to_string(), node))).unwrap();
        terms.into_iter().map(|(t, node)| (t, engine.term_postings_of_node(node).unwrap())).collect::<Vec<_>>()
    };
    let one = digest(300);
    assert_eq!(digest(7), one);
    assert!(one.iter().any(|(t, _)| t == "corr"));
    assert!(one.iter().all(|(t, _)| t != "correndo"));
}
