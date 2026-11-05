//! Correção de erros de digitação (`Config::typos`).

use fire_flower::{Config, Correction, Engine, TypoMode};

fn typos() -> Config {
    Config { typos: TypoMode::Missing, ..Config::default() }
}

fn sample(dir: &tempfile::TempDir, config: Config) -> Engine {
    let engine = Engine::open(dir.path(), config).unwrap();
    let mut batch = engine.batch();
    batch.add_titled(1, "Revolução Francesa", "A queda da Bastilha em Paris").unwrap();
    batch.add_titled(2, "Revolução Industrial", "Máquinas a vapor na Inglaterra").unwrap();
    batch.add_titled(3, "Culinária francesa", "Queijos e vinhos").unwrap();
    batch.add_titled(4, "Pernambuco", "Estado do Nordeste").unwrap();
    batch.add_titled(5, "Recife", "Capital de Pernambuco").unwrap();
    batch.commit().unwrap();
    engine
}

fn fixed(from: &str, to: &[&str]) -> Correction {
    Correction { from: from.into(), to: to.iter().map(|s| s.to_string()).collect() }
}

#[test]
fn off_by_default_a_typo_finds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, Config::default());
    assert!(engine.search("revolucao fracesa").unwrap().is_empty());
    assert_eq!(engine.search_ranked("revolucao fracesa", 10).unwrap().total, 0);
}

#[test]
fn one_edit_of_each_kind_is_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, typos());
    assert_eq!(engine.search("revolucao fracesa").unwrap(), [1], "letra faltando");
    assert_eq!(engine.search("pernambuuco").unwrap(), [4, 5], "letra sobrando");
    assert_eq!(engine.search("pernanbuco").unwrap(), [4, 5], "letra trocada");
    assert_eq!(engine.search("pernambcuo").unwrap(), [4, 5], "vizinhas invertidas");
    let top = engine.search_ranked("revolucao fracesa", 10).unwrap();
    assert_eq!(top.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), [1]);
    assert_eq!(top.corrections, [fixed("fracesa", &["francesa"])]);
    // A palavra certa não é corrigida.
    assert!(engine.search_ranked("revolucao francesa", 10).unwrap().corrections.is_empty());
}

#[test]
fn what_is_not_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, typos());
    assert!(engine.search("oernambuco").unwrap().is_empty(), "a primeira letra fica");
    assert!(engine.search("pernambxcuo").unwrap().is_empty(), "dois erros");
    // `paris` está num documento só (df 1 < typo_min_df): não vale como correção.
    assert!(engine.search("pariz").unwrap().is_empty());
    // Palavras de menos de 5 letras não são corrigidas (`queda` → `qeda`).
    assert!(engine.search("qeda").unwrap().is_empty());
    // Nem dentro de frase, nem prefixo.
    assert!(engine.search("\"revolucao fracesa\"").unwrap().is_empty());
    assert!(engine.search("fracesa*").unwrap().is_empty());
}

#[test]
fn a_rare_word_keeps_its_documents_and_gains_the_corrections() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, typos());
    let mut batch = engine.batch();
    batch.add_titled(6, "Fracesa", "uma palavra rara").unwrap();
    batch.commit().unwrap();
    // `fracesa` existe, mas com df 1: soma `francesa`, e o próprio documento
    // (palavra exata, peso 1) fica na frente dos corrigidos (peso γ), todos
    // com a palavra no título.
    assert_eq!(engine.search("fracesa").unwrap(), [1, 3, 6]);
    let top = engine.search_ranked("fracesa", 10).unwrap();
    assert_eq!(top.hits[0].doc, 6);
    assert_eq!(top.total, 3);
    assert_eq!(top.corrections, [fixed("fracesa", &["francesa"])]);
    // Com df 2 ela já existe de verdade: sem correção.
    let mut batch = engine.batch();
    batch.add_document(7, "fracesa de novo").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.search("fracesa").unwrap(), [6, 7]);
}

#[test]
fn corrections_by_df_up_to_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { max_typo_terms: 2, ..typos() };
    let engine = Engine::open(dir.path(), config).unwrap();
    let mut batch = engine.batch();
    for (id, text) in [(1, "casaco"), (2, "casaco"), (3, "casaco casado"), (4, "casado"), (5, "casalo"), (6, "casalo")] {
        batch.add_document(id, text).unwrap();
    }
    batch.commit().unwrap();
    // `casaxo` está a um erro das três; ficam as duas de maior df (casaco 3;
    // casado e casalo 2, empate em ordem alfabética).
    let top = engine.search_ranked("casaxo", 10).unwrap();
    assert_eq!(top.corrections, [fixed("casaxo", &["casaco", "casado"])]);
    assert_eq!(engine.search("casaxo").unwrap(), [1, 2, 3, 4]);
    assert_eq!(top.total, 4);
}

#[test]
fn ranked_and_unranked_agree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, typos());
    for q in ["revolucao fracesa", "pernanbuco", "revoluçao", "fracesa queijos", "pernambuco recife"] {
        let mut ranked: Vec<u64> = engine.search_ranked(q, 100).unwrap().hits.iter().map(|h| h.doc).collect();
        ranked.sort_unstable();
        assert_eq!(ranked, engine.search(q).unwrap(), "{q}");
    }
}

#[test]
fn removed_corrections_find_nothing_before_and_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let engine = sample(&dir, typos());
    engine.delete_document(1).unwrap();
    engine.delete_document(3).unwrap();
    // Antes da compactação, `francesa` ainda está na Trie com df 2 (as
    // versões mortas contam), mas não tem documento vivo.
    assert!(engine.search("fracesa").unwrap().is_empty());
    assert_eq!(engine.search_ranked("fracesa", 10).unwrap().total, 0);
    engine.compact().unwrap();
    assert!(engine.search("fracesa").unwrap().is_empty());
    assert!(engine.search_ranked("fracesa", 10).unwrap().corrections.is_empty());
}
