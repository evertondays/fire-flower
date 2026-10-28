//! Stemming na indexação e flexões na consulta.

use fire_flower::{Config, Engine, InflectionMode, Language, Stemming};
use fire_flower::query::Clause;

use crate::open;

pub(crate) fn stemmed() -> Config {
    Config { stemming: Some(Stemming::Snowball(Language::Portuguese)), ..Config::default() }
}

pub(crate) fn stem_sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_titled(1, "Praia", "O aluno estava correndo na praia").unwrap();
    batch.add_titled(2, "Corridas", "Os alunos brasileiros e as informações").unwrap();
    batch.add_titled(3, "Revolução Francesa", "Os franceses e a informação, em 1789").unwrap();
    batch.commit().unwrap();
}

#[test]
pub(crate) fn stemming_matches_inflections() {
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
pub(crate) fn stemming_prefixes_match_stems_and_the_typed_word() {
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
pub(crate) fn stemming_is_stored_on_creation_and_checked_on_open() {
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
pub(crate) fn database_from_before_stemming_is_off() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        stem_sample(&engine);
        engine.compact().unwrap(); // sem merges pendentes para o RocksDB puro abaixo
    }
    // Um banco com dados e sem `meta:stemming` é de antes do stemming.
    {
        let mut opts = rocksdb::Options::default();
        opts.set_merge_operator_associative("fire_flower_v7", |_, _, _| None);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        assert_eq!(db.get(b"meta:stemming").unwrap().as_deref(), Some(&b"none"[..]));
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
pub(crate) fn stem_cache_across_batches_does_not_change_results() {
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

pub(crate) fn inflected(mode: InflectionMode) -> Config {
    Config { inflections: Some(Stemming::Plural(Language::Portuguese)), inflection_mode: mode, ..Config::default() }
}

pub(crate) fn inflection_sample(engine: &Engine) {
    let mut batch = engine.batch();
    batch.add_titled(1, "Torre", "A torre do castelo").unwrap();
    batch.add_titled(2, "Torres", "Torres é uma cidade com torres altas").unwrap();
    batch.commit().unwrap();
    // Outro lote: o radical `eleicao` ganha palavras em dois commits.
    let mut batch = engine.batch();
    batch.add_titled(3, "Eleições", "As eleições de 2018").unwrap();
    batch.add_titled(4, "Castelos", "Castelos medievais").unwrap();
    batch.commit().unwrap();
    let mut batch = engine.batch();
    batch.add_titled(5, "Votação", "Uma eleição indireta").unwrap();
    batch.commit().unwrap();
}

#[test]
pub(crate) fn inflections_match_other_forms_but_prefer_the_exact_word() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), inflected(InflectionMode::Always)).unwrap();
    inflection_sample(&engine);
    assert_eq!(engine.search("torre").unwrap(), [1, 2]);
    assert_eq!(engine.search("castelo").unwrap(), [1, 4]);
    // Palavras de lotes diferentes com o mesmo radical.
    assert_eq!(engine.search("eleicao").unwrap(), [3, 5]);
    assert_eq!(engine.search("eleições").unwrap(), [3, 5]);
    // A palavra exata vale mais: "Torre" à frente de "Torres", e vice-versa.
    let top = |q: &str| engine.search_top(q, 10).unwrap().iter().map(|h| h.doc).collect::<Vec<_>>();
    assert_eq!(top("torre"), [1, 2]);
    assert_eq!(top("torres"), [2, 1]);
    assert_eq!(engine.search_ranked("castelos", 10).unwrap().total, 2);
    // E entre partes: as duas formas contam.
    assert_eq!(engine.search("castelo torres").unwrap(), [1]);
    // Uma palavra que só aparece flexionada ainda acha.
    assert_eq!(engine.search("votacoes").unwrap(), [5]);
    // Frases e prefixos continuam exatos; as APIs por palavra também.
    assert_eq!(engine.search("\"torres altas\"").unwrap(), [2]);
    assert!(engine.search("\"torre altas\"").unwrap().is_empty());
    assert_eq!(engine.search("torre*").unwrap(), [1, 2]);
    assert_eq!(engine.postings("torre").unwrap(), [1]);
    assert_eq!(engine.suggest("torr", 10).unwrap(), ["torre", "torres"]);
    assert!(engine.inflector().is_on());
    assert!(!engine.stemmer().is_on());
}

#[test]
pub(crate) fn inflections_only_when_the_exact_search_finds_too_little() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), inflected(InflectionMode::IfFewer(2))).unwrap();
        inflection_sample(&engine);
        // "torre" sozinha acha 1 documento (< 2): soma as flexões.
        assert_eq!(engine.search("torre").unwrap(), [1, 2]);
        assert_eq!(engine.search_ranked("torre", 10).unwrap().total, 2);
        // "torres" já acha 1 também (só o 2 tem a palavra exata)…
        assert_eq!(engine.search("torres").unwrap(), [1, 2]);
    }
    // O modo vale por busca: o mesmo banco, com limiar 1, fica exato.
    let engine = Engine::open(dir.path(), inflected(InflectionMode::IfFewer(1))).unwrap();
    assert_eq!(engine.search("torre").unwrap(), [1]);
    assert_eq!(engine.search_ranked("torre", 10).unwrap().total, 1);
    assert_eq!(engine.search("votacoes").unwrap(), [5], "nenhum exato: soma");
}

#[test]
pub(crate) fn inflections_are_stored_on_creation_and_exclude_stemming() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), inflected(InflectionMode::Always)).unwrap();
        inflection_sample(&engine);
        engine.compact().unwrap(); // o merge de `stem:` também passa pela compactação
    }
    let engine = Engine::open(dir.path(), Config::default()).unwrap();
    assert!(engine.inflector().is_on());
    assert_eq!(engine.search("eleicao").unwrap(), [3, 5]);
    drop(engine);
    let other = Config { inflections: Some(Stemming::Snowball(Language::Portuguese)), ..Config::default() };
    assert!(Engine::open(dir.path(), other).is_err());

    let both = Config { stemming: Some(Stemming::Snowball(Language::Portuguese)), ..inflected(InflectionMode::Always) };
    let err = Engine::open(tempfile::tempdir().unwrap().path(), both).err().unwrap();
    assert!(err.to_string().contains("não combinam"), "{err}");
    // Sem flexões, a mesma consulta é exata.
    let plain = tempfile::tempdir().unwrap();
    let engine = open(&plain);
    inflection_sample(&engine);
    assert_eq!(engine.search("torre").unwrap(), [1]);
    assert!(!engine.inflector().is_on());
    let bad = Config { inflection_weight: 1.5, ..Config::default() };
    assert!(Engine::open(tempfile::tempdir().unwrap().path(), bad).is_err());
}
