//! Edição e remoção: substituir um ID, remover, a regra do lote e bancos
//! sem `fwd:`.

use std::collections::BTreeMap;

use fire_flower::{Config, Engine, IndexStats};

use crate::{open, p, positions};

#[test]
pub(crate) fn replacing_a_document_drops_the_terms_that_left() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_titled(1, "Casa", "casa azul com jardim").unwrap();
    batch.add_document(2, "casa verde").unwrap();
    batch.set_label(1, "um.txt");
    batch.commit().unwrap();
    engine.index_document(1, "jardim jardim de pedra").unwrap();

    assert!(engine.search("azul").unwrap().is_empty(), "termo que saiu do texto");
    assert_eq!(engine.search("casa").unwrap(), [2], "o título também foi substituído");
    assert_eq!(engine.search("pedra").unwrap(), [1]);
    assert_eq!(engine.term_postings("jardim").unwrap(), [p(1, 2)]);
    assert_eq!(positions(&engine, "jardim"), [(1, vec![0, 1])]);
    assert_eq!(engine.doc_len(1).unwrap(), Some(3));
    assert_eq!(engine.stats().unwrap(), IndexStats { doc_count: 2, total_len: 5, total_title_len: 0 });
    assert_eq!(engine.label(1).unwrap().as_deref(), Some("um.txt"), "o rótulo continua");
    // `fwd:` tem exatamente os termos do texto novo.
    let mut want: Vec<u64> = ["jardim", "pedra"].iter().map(|t| engine.term_node(t).unwrap().unwrap()).collect();
    want.sort_unstable();
    assert_eq!(engine.document_nodes(1).unwrap(), Some(want));
}

#[test]
pub(crate) fn deleting_removes_the_document_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_titled(1, "Roma", "o rato roeu a roupa do rei de roma").unwrap();
    batch.add_document(2, "a roupa nova do rei").unwrap();
    batch.set_label(1, "roma.txt");
    batch.commit().unwrap();
    let before = engine.stats().unwrap();
    engine.delete_document(1).unwrap();

    for q in ["roma", "rato", "roupa rei", "\"roupa do rei\"", "ro*"] {
        assert!(!engine.search(q).unwrap().contains(&1), "{q}");
    }
    assert_eq!(engine.search("roupa").unwrap(), [2]);
    assert_eq!(engine.search_ranked("rei", 10).unwrap().total, 1);
    assert!(!engine.contains(1).unwrap());
    assert!(engine.contains(2).unwrap());
    assert_eq!(engine.label(1).unwrap(), None);
    assert_eq!(engine.document_nodes(1).unwrap(), None);
    let after = engine.stats().unwrap();
    assert_eq!(after.doc_count, before.doc_count - 1);
    assert_eq!(after.total_len, engine.doc_len(2).unwrap().unwrap() as u64);
    assert_eq!(after.total_title_len, 0);

    // Remover o que não existe não muda nada.
    engine.delete_document(1).unwrap();
    engine.delete_document(99).unwrap();
    assert_eq!(engine.stats().unwrap(), after);
    // O ID pode voltar.
    engine.index_document(1, "roma de novo").unwrap();
    assert_eq!(engine.search("roma").unwrap(), [1]);
    assert_eq!(engine.stats().unwrap().doc_count, 2);
}

#[test]
pub(crate) fn the_last_operation_in_a_batch_wins() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    engine.index_document(5, "antigo velho").unwrap();
    let mut batch = engine.batch();
    batch.add_document(1, "fantasma").unwrap();
    batch.delete_document(1).unwrap(); // adicionar e remover: nada
    batch.add_document(2, "primeiro texto").unwrap();
    batch.add_document(2, "segundo texto").unwrap(); // o segundo texto inteiro
    batch.delete_document(5).unwrap();
    batch.add_document(5, "novo").unwrap(); // remover e adicionar: substitui
    batch.set_label(1, "sumiu.txt");
    batch.commit().unwrap();

    assert!(engine.search("fantasma").unwrap().is_empty());
    assert!(engine.suggest("fant", 5).unwrap().is_empty(), "a palavra nem entrou na Trie");
    assert!(!engine.contains(1).unwrap());
    assert_eq!(engine.label(1).unwrap(), None);
    assert!(engine.search("primeiro").unwrap().is_empty());
    assert_eq!(engine.search("texto segundo").unwrap(), [2]);
    assert!(engine.search("antigo").unwrap().is_empty());
    assert_eq!(engine.search("novo").unwrap(), [5]);
    assert_eq!(engine.stats().unwrap().doc_count, 2);
}

#[test]
pub(crate) fn edits_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        engine.index_document(1, "alfa beta").unwrap();
        engine.index_document(2, "alfa gama").unwrap();
    }
    {
        let engine = open(&dir);
        engine.index_document(1, "delta").unwrap();
        engine.delete_document(2).unwrap();
    }
    let engine = open(&dir);
    assert!(engine.search("alfa").unwrap().is_empty());
    assert_eq!(engine.search("delta").unwrap(), [1]);
    engine.delete_document(1).unwrap();
    assert!(engine.search("delta").unwrap().is_empty());
    assert_eq!(engine.stats().unwrap(), IndexStats::default());
    engine.compact();
    assert!(engine.search("delta").unwrap().is_empty());
}

#[test]
pub(crate) fn database_without_edits_keeps_the_old_reindexing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Config { edits: Some(false), ..Config::default() }).unwrap();
    assert!(!engine.has_edits());
    engine.index_document(1, "eco azul").unwrap();
    engine.index_document(1, "eco").unwrap();
    assert_eq!(engine.search("azul").unwrap(), [1], "sem fwd:, o termo que saiu fica");
    assert!(matches!(engine.delete_document(1), Err(fire_flower::Error::Config(_))));
    drop(engine);
    assert!(Engine::open(dir.path(), Config { edits: Some(true), ..Config::default() }).is_err());
    assert!(!Engine::open(dir.path(), Config::default()).unwrap().has_edits());
}

/// Gerador pequeno e determinístico.
fn rng(seed: u64) -> impl FnMut(u64) -> u64 {
    let mut x = seed;
    move |n| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n.max(1)
    }
}

/// O conteúdo lógico de um banco: cada termo com (doc, tf, tf no título) e
/// posições, os tamanhos e as estatísticas. Termos sem documento (a Trie não
/// encolhe) ficam de fora.
type Content = (BTreeMap<String, (Vec<fire_flower::Posting>, Vec<(u64, Vec<u32>)>)>, Vec<(u64, u32, u32)>, IndexStats);

fn content(engine: &Engine) -> Content {
    let mut nodes = Vec::new();
    engine.for_each_term(|t, node| nodes.push((t.to_string(), node))).unwrap();
    let mut terms = BTreeMap::new();
    for (term, node) in nodes {
        let postings = engine.term_postings_of_node(node).unwrap();
        if !postings.is_empty() {
            terms.insert(term, (postings, engine.positions_of_node(node).unwrap()));
        }
    }
    let mut lens = Vec::new();
    engine.for_each_doc_len(|doc, len, title| lens.push((doc, len, title))).unwrap();
    (terms, lens, engine.stats().unwrap())
}

/// Operações aleatórias (adicionar, substituir, remover) em lotes de
/// tamanhos aleatórios, com compactação no meio, dão o mesmo conteúdo que
/// carregar direto o estado final.
#[test]
pub(crate) fn edits_match_a_fresh_load() {
    let words = ["sol", "lua", "mar", "rio", "monte", "vale", "pedra", "areia", "vento", "chuva"];
    for seed in 1..=4u64 {
        let mut next = rng(seed * 7919);
        let dir = tempfile::tempdir().unwrap();
        let engine = open(&dir);
        let mut state: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for round in 0..12 {
            let mut batch = engine.batch();
            for _ in 0..1 + next(400) {
                let doc = next(600);
                if next(4) == 0 {
                    batch.delete_document(doc).unwrap();
                    state.remove(&doc);
                } else {
                    let text = |next: &mut dyn FnMut(u64) -> u64, n: u64| {
                        (0..n).map(|_| words[next(words.len() as u64) as usize]).collect::<Vec<_>>().join(" ")
                    };
                    let title = if next(3) == 0 { let n = 1 + next(2); text(&mut next, n) } else { String::new() };
                    let n = 1 + next(12);
                    let body = text(&mut next, n);
                    batch.add_titled(doc, &title, &body).unwrap();
                    state.insert(doc, (title, body));
                }
            }
            batch.commit().unwrap();
            if round == 6 {
                engine.compact();
            }
        }
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = open(&fresh_dir);
        let mut batch = fresh.batch();
        for (doc, (title, body)) in &state {
            batch.add_titled(*doc, title, body).unwrap();
        }
        batch.commit().unwrap();
        assert!(content(&engine) == content(&fresh), "seed {seed}: antes de compactar");
        for q in ["sol lua", "\"mar rio\"", "pedra*", "vento"] {
            let (a, b) = (engine.search_ranked(q, 50).unwrap(), fresh.search_ranked(q, 50).unwrap());
            assert_eq!(a.total, b.total, "seed {seed}: {q}");
            let docs = |t: &fire_flower::TopHits| t.hits.iter().map(|h| h.doc).collect::<Vec<_>>();
            assert_eq!(docs(&a), docs(&b), "seed {seed}: {q}");
        }
        engine.compact();
        assert!(content(&engine) == content(&fresh), "seed {seed}: depois de compactar");
    }
}

/// Uma palavra cujo último documento saiu some do autocompletar e do prefixo
/// na hora (sem ocupar vaga do limite de termos), continua fora depois de
/// reabrir, e a limpeza tira o fim de palavra dela na Trie.
#[test]
pub(crate) fn words_without_documents_leave_suggestions() {
    let dir = tempfile::tempdir().unwrap();
    let config = || Config { max_prefix_terms: 2, ..Config::default() };
    {
        let engine = Engine::open(dir.path(), config()).unwrap();
        let mut batch = engine.batch();
        batch.add_document(1, "palco palha").unwrap();
        batch.add_document(2, "palma palha").unwrap();
        batch.add_document(3, "palito").unwrap();
        batch.commit().unwrap();
        assert_eq!(engine.suggest("pal", 10).unwrap(), ["palco", "palha", "palma", "palito"]);
        engine.delete_document(1).unwrap();
        engine.index_document(3, "outro").unwrap();
        assert_eq!(engine.suggest("pal", 10).unwrap(), ["palha", "palma"], "palco e palito ficaram sem documento");
        // As duas vagas do prefixo vão para palavras com documento.
        assert_eq!(engine.search("pal*").unwrap(), [2]);
    }
    let engine = Engine::open(dir.path(), config()).unwrap();
    assert_eq!(engine.suggest("pal", 10).unwrap(), ["palha", "palma"], "o conjunto sobrevive ao open");
    assert!(engine.term_node("palco").unwrap().is_some(), "antes da limpeza o nó ainda é palavra");
    assert_eq!(engine.collect_empty_words().unwrap(), 2);
    assert_eq!(engine.collect_empty_words().unwrap(), 0, "nada mais a limpar");
    assert_eq!(engine.term_node("palco").unwrap(), None);
    assert_eq!(engine.suggest("pal", 10).unwrap(), ["palha", "palma"]);
    // A palavra pode voltar.
    engine.index_document(4, "palco").unwrap();
    assert_eq!(engine.suggest("palc", 10).unwrap(), ["palco"]);
    assert_eq!(engine.search("palco").unwrap(), [4]);
    drop(engine);
    let engine = Engine::open(dir.path(), config()).unwrap();
    assert_eq!(engine.suggest("pal", 10).unwrap(), ["palco", "palha", "palma"]);
}

/// A manutenção compacta sozinha quando as edições passam do limite e limpa
/// as palavras que ficaram sem documento.
#[test]
pub(crate) fn maintenance_compacts_after_enough_edits() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use fire_flower::{Maintenance, MaintenanceOptions};

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(open(&dir));
    let mut batch = engine.batch();
    for id in 0..20 {
        batch.add_document(id, &format!("comum unico{id}")).unwrap();
    }
    batch.commit().unwrap();
    let options = MaintenanceOptions { edits_before_compact: Some(5), poll: Duration::from_millis(5) };
    let maintenance = Maintenance::spawn(Arc::clone(&engine), options);
    for id in 0..4 {
        engine.delete_document(id).unwrap();
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(engine.pending_edits(), 4, "abaixo do limite, nada acontece");
    assert!(engine.term_node("unico0").unwrap().is_some());
    engine.delete_document(4).unwrap();
    let started = Instant::now();
    while engine.pending_edits() > 0 {
        assert!(started.elapsed() < Duration::from_secs(10), "a manutenção não compactou");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A limpeza vem logo depois da compactação, na mesma rodada.
    while engine.term_node("unico4").unwrap().is_some() {
        assert!(started.elapsed() < Duration::from_secs(10), "a manutenção não limpou as palavras");
        std::thread::sleep(Duration::from_millis(5));
    }
    maintenance.stop().unwrap();
    assert_eq!(engine.search("comum").unwrap(), (5..20).collect::<Vec<_>>());
    assert_eq!(engine.suggest("unico", 50).unwrap().len(), 15);
}
