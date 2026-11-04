//! Edições aleatórias contra um modelo ingênuo em memória, com reabrir e
//! compactar no meio, e a compactação com buscas em paralelo.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fire_flower::{Config, Engine};

const WORDS: &[&str] = &["pato", "pano", "pardo", "pasta", "pausa", "pavio", "palco", "palha", "rio", "sol"];

type Model = BTreeMap<u64, Vec<&'static str>>;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn text(rng: &mut Rng) -> Vec<&'static str> {
    let n = 1 + rng.below(8);
    // Vocabulário enviesado: as primeiras palavras aparecem em quase tudo.
    let n_words = WORDS.len() as u64;
    (0..n).map(|_| WORDS[(rng.below(n_words) * rng.below(n_words) / n_words) as usize]).collect()
}

/// Confere listas, posições, buscas, E, frases, prefixo e estatísticas.
/// `compacted`: sem versões mortas, o autocompletar e as notas são exatos.
fn check(engine: &Engine, model: &Model, compacted: bool, ctx: &str) {
    for w in WORDS {
        let mut want = Vec::new();
        let mut want_pos = Vec::new();
        for (&doc, toks) in model {
            let pos: Vec<u32> = toks.iter().enumerate().filter(|(_, t)| *t == w).map(|(i, _)| i as u32).collect();
            if !pos.is_empty() {
                want.push((doc, pos.len() as u32));
                want_pos.push((doc, pos));
            }
        }
        let got: Vec<(u64, u32)> = engine.term_postings(w).unwrap().iter().map(|p| (p.doc, p.tf)).collect();
        assert_eq!(got, want, "{ctx}: postings de {w}");
        if let Some(node) = engine.term_node(w).unwrap().filter(|_| engine.has_positions()) {
            assert_eq!(engine.positions_of_node(node).unwrap(), want_pos, "{ctx}: posições de {w}");
        }
        let docs: Vec<u64> = want.iter().map(|(d, _)| *d).collect();
        assert_eq!(engine.search(w).unwrap(), docs, "{ctx}: search {w}");
        let ranked = engine.search_ranked(w, 1000).unwrap();
        assert_eq!(ranked.total, docs.len(), "{ctx}: total ranqueado {w}");
        let ranked: BTreeSet<u64> = ranked.hits.iter().map(|h| h.doc).collect();
        assert_eq!(ranked, docs.iter().copied().collect(), "{ctx}: ranqueado {w}");
    }
    for a in &WORDS[..4] {
        for b in &WORDS[..4] {
            if a == b {
                continue;
            }
            let and: Vec<u64> = model.iter().filter(|(_, t)| t.contains(a) && t.contains(b)).map(|(d, _)| *d).collect();
            assert_eq!(engine.search(&format!("{a} {b}")).unwrap(), and, "{ctx}: E {a} {b}");
            if !engine.has_positions() {
                continue;
            }
            let phrase: Vec<u64> =
                model.iter().filter(|(_, t)| t.windows(2).any(|w| w[0] == *a && w[1] == *b)).map(|(d, _)| *d).collect();
            assert_eq!(engine.search(&format!("\"{a} {b}\"")).unwrap(), phrase, "{ctx}: frase {a} {b}");
            let ranked: BTreeSet<u64> =
                engine.search_top(&format!("\"{a} {b}\""), 1000).unwrap().iter().map(|h| h.doc).collect();
            assert_eq!(ranked, phrase.iter().copied().collect(), "{ctx}: frase ranqueada {a} {b}");
        }
    }
    // Autocompletar: toda palavra com documento; depois da compactação, só elas.
    let live: BTreeSet<String> = WORDS
        .iter()
        .filter(|w| w.starts_with("pa") && model.values().any(|t| t.contains(w)))
        .map(|w| w.to_string())
        .collect();
    let got: BTreeSet<String> = engine.suggest("pa", 100).unwrap().into_iter().collect();
    if compacted {
        assert_eq!(got, live, "{ctx}: suggest pa");
    } else {
        assert!(got.is_superset(&live), "{ctx}: suggest pa perdeu uma palavra com documento");
    }
    let prefix: Vec<u64> =
        model.iter().filter(|(_, t)| t.iter().any(|w| w.starts_with("pa"))).map(|(d, _)| *d).collect();
    assert_eq!(engine.search("pa*").unwrap(), prefix, "{ctx}: pa*");
    assert_eq!(engine.stats().unwrap().doc_count as usize, model.len(), "{ctx}: doc_count");
    for (&doc, toks) in model {
        assert_eq!(engine.doc_len(doc).unwrap(), Some(toks.len() as u32), "{ctx}: tamanho de {doc}");
    }
}

/// Depois da compactação, as notas e a ordem (empates pelo menor ID) são as
/// de um banco carregado direto com o estado do modelo.
fn same_ranking_as_fresh(engine: &Engine, model: &Model, ctx: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { positions: Some(engine.has_positions()), ..Config::default() };
    let fresh = Engine::open(dir.path(), config).unwrap();
    let mut batch = fresh.batch();
    for (&doc, toks) in model {
        batch.add_document(doc, &toks.join(" ")).unwrap();
    }
    batch.commit().unwrap();
    let mut queries: Vec<String> = WORDS.iter().map(|w| w.to_string()).collect();
    queries.extend(["pato pano", "pa*", "rio sol pato", "pardo pa*"].map(String::from));
    if engine.has_positions() {
        queries.push("\"pato pano\"".into());
    }
    for q in &queries {
        assert_eq!(engine.search_ranked(q, 7).unwrap(), fresh.search_ranked(q, 7).unwrap(), "{ctx}: ranking de {q}");
    }
}

fn run(seed: u64, positions: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = || Config { positions: Some(positions), max_prefix_terms: 100, ..Config::default() };
    let mut engine = Engine::open(dir.path(), config()).unwrap();
    let mut rng = Rng(seed);
    let mut model = Model::new();
    let mut next = 10u64;
    let mut compacted = true;
    for round in 0..120 {
        let mut batch = engine.batch();
        let mut staged = model.clone();
        for _ in 0..1 + rng.below(6) {
            let r = rng.below(10);
            let existing: Vec<u64> = staged.keys().copied().collect();
            let doc = if r < 3 || existing.is_empty() {
                next += 1 + rng.below(3); // às vezes deixa buracos
                next
            } else if r < 7 {
                existing[rng.below(existing.len() as u64) as usize]
            } else if r < 8 {
                rng.below(next) // ID antigo, talvez nunca usado
            } else {
                let doc = existing[rng.below(existing.len() as u64) as usize];
                batch.delete_document(doc).unwrap();
                staged.remove(&doc);
                compacted = false;
                continue;
            };
            let toks = text(&mut rng);
            batch.add_document(doc, &toks.join(" ")).unwrap();
            compacted &= !model.contains_key(&doc);
            staged.insert(doc, toks);
        }
        batch.commit().unwrap();
        model = staged;
        let ctx = format!("seed {seed}, rodada {round}");
        check(&engine, &model, compacted, &ctx);
        match rng.below(12) {
            0 => {
                drop(engine);
                engine = Engine::open(dir.path(), config()).unwrap();
                check(&engine, &model, compacted, &format!("{ctx}, reaberto"));
            }
            1 => {
                engine.compact().unwrap();
                compacted = true;
                check(&engine, &model, true, &format!("{ctx}, compactado"));
                same_ranking_as_fresh(&engine, &model, &format!("{ctx}, compactado"));
            }
            _ => {}
        }
    }
}

#[test]
pub(crate) fn random_edits_match_the_model() {
    for seed in 1..=12 {
        run(seed * 0x9E37_79B9_7F4A_7C15, true);
    }
}

#[test]
pub(crate) fn random_edits_match_the_model_without_positions() {
    for seed in 1..=6 {
        run(seed * 0x2545_F491_4F6C_DD1D, false);
    }
}

/// Compactações com buscas e edições em paralelo: a limpeza dos mortos não
/// pode tirar das listas uma versão que um snapshot aberto ainda vê viva.
/// Cada edição troca "pano" por "rio" num documento par: numa busca, nenhum
/// documento tem os dois e "pato" acha sempre os dois mil.
#[test]
pub(crate) fn compaction_while_reading_and_editing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), Config::default()).unwrap());
    let mut batch = engine.batch();
    for d in 0..2000u64 {
        batch.add_document(d, if d % 2 == 0 { "pato pano" } else { "pato sol" }).unwrap();
    }
    batch.commit().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (engine, stop) = (Arc::clone(&engine), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    assert_eq!(engine.search("pato").unwrap().len(), 2000);
                    assert!(engine.search("pano rio").unwrap().is_empty(), "versão velha e nova juntas");
                    // Duas buscas: um documento que muda entre elas fica fora das
                    // duas, nunca nas duas.
                    let both = engine.search("rio").unwrap().len() + engine.search("pano").unwrap().len();
                    assert!(both <= 1000, "um documento contado duas vezes: {both}");
                    let ranked = engine.search_ranked("pato", 3).unwrap();
                    assert_eq!(ranked.total, 2000);
                    assert!(ranked.hits.iter().all(|h| h.score.is_finite() && h.score > 0.0));
                    n += 1;
                }
                n
            })
        })
        .collect();
    let compactor = {
        let (engine, stop) = (Arc::clone(&engine), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                engine.compact().unwrap();
            }
        })
    };
    for i in 0..500u64 {
        engine.index_document(i * 2, "pato rio").unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        assert!(r.join().unwrap() > 0);
    }
    // Quantos mortos cada rodada limpa depende de quais snapshots estavam abertos.
    compactor.join().unwrap();
    engine.compact().unwrap();
    assert_eq!(engine.dead_versions(), 0);
    assert_eq!(engine.search("pano").unwrap().len(), 500);
    assert_eq!(engine.search("rio").unwrap().len(), 500);
    assert_eq!(engine.search_ranked("pano", 10).unwrap().total, 500);
}

/// A compactação tira a palavra da Trie; uma busca guarda o nó no cache de
/// leitura; a palavra volta num commit depois de reabrir. A busca tem que
/// vê-la (o nó entra no cache do escritor pela limpeza, sem profundidade).
#[test]
pub(crate) fn word_returns_after_compaction_with_a_reader_in_between() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), Config::default()).unwrap();
        engine.index_document(1, "rio").unwrap();
        engine.index_document(1, "pato").unwrap();
    }
    let engine = Engine::open(dir.path(), Config::default()).unwrap();
    assert_eq!(engine.compact().unwrap().emptied_words, 1);
    assert!(engine.search("rio").unwrap().is_empty()); // guarda o caminho no cache
    assert!(engine.suggest("ri", 5).unwrap().is_empty());
    engine.index_document(2, "rio").unwrap();
    assert_eq!(engine.search("rio").unwrap(), [2]);
    assert_eq!(engine.suggest("ri", 5).unwrap(), ["rio"]);
}
