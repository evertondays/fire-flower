//! IDs externos: a chave do usuário de cada documento (link, número, UUID),
//! devolvida com os resultados, única e buscável.

use fire_flower::{Config, Engine, Error, MAX_EXTERNAL_ID_BYTES};

use crate::open;

fn ids_of(engine: &Engine, query: &str) -> Vec<Option<String>> {
    let docs: Vec<u64> = engine.search_top(query, 10).unwrap().into_iter().map(|h| h.doc).collect();
    engine.external_ids(&docs).unwrap()
}

#[test]
fn search_results_come_back_with_the_external_id() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let a = batch.add_with_external_id("https://pt.wikipedia.org/wiki/Brasil", "Brasil", "país da américa do sul").unwrap();
    let b = batch.add_with_external_id("42", "Argentina", "país vizinho do brasil").unwrap();
    batch.add_document(100, "brasil sem ID externo").unwrap();
    batch.commit().unwrap();

    assert_ne!(a, b);
    assert_eq!(engine.external_id(a).unwrap().as_deref(), Some("https://pt.wikipedia.org/wiki/Brasil"));
    assert_eq!(engine.doc_by_external_id("42").unwrap(), Some(b));
    assert_eq!(engine.doc_by_external_id("43").unwrap(), None);
    let mut got = ids_of(&engine, "brasil");
    got.sort();
    assert_eq!(got, [None, Some("42".into()), Some("https://pt.wikipedia.org/wiki/Brasil".into())]);
    assert!(engine.external_ids(&[]).unwrap().is_empty());
    assert_eq!(engine.external_ids(&[b, 999, a]).unwrap(), [Some("42".into()), None, Some("https://pt.wikipedia.org/wiki/Brasil".into())]);
}

#[test]
fn adding_with_an_existing_external_id_replaces_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let doc = batch.add_with_external_id("uuid-1", "", "texto antigo azul").unwrap();
    batch.commit().unwrap();

    let mut batch = engine.batch();
    assert_eq!(batch.add_with_external_id("uuid-1", "", "texto novo verde").unwrap(), doc, "mesmo DocId");
    batch.commit().unwrap();
    assert!(engine.search("azul").unwrap().is_empty());
    assert_eq!(engine.search("verde").unwrap(), [doc]);
    assert_eq!(engine.stats().unwrap().doc_count, 1);

    // No mesmo lote: duas vezes o mesmo ID é o mesmo documento, e o último texto vence.
    let mut batch = engine.batch();
    let x = batch.add_with_external_id("uuid-2", "", "primeiro").unwrap();
    assert_eq!(batch.add_with_external_id("uuid-2", "", "segundo").unwrap(), x);
    batch.commit().unwrap();
    assert!(engine.search("primeiro").unwrap().is_empty());
    assert_eq!(engine.search("segundo").unwrap(), [x]);
}

#[test]
fn an_external_id_belongs_to_one_document() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_document(1, "um").unwrap();
    batch.add_document(2, "dois").unwrap();
    batch.set_external_id(1, "a").unwrap();
    let err = batch.set_external_id(2, "a").unwrap_err();
    assert!(matches!(&err, Error::Duplicate(m) if m.contains("documento 1")), "{err}");
    assert_eq!(err.code(), "FF_DUPLICATE");
    batch.set_external_id(1, "a").unwrap(); // o mesmo dono: nada muda
    batch.commit().unwrap();

    // Também contra o que já está gravado.
    let mut batch = engine.batch();
    assert!(matches!(batch.set_external_id(2, "a"), Err(Error::Duplicate(_))));
    // Trocar o ID de um documento libera o anterior.
    batch.set_external_id(1, "b").unwrap();
    batch.set_external_id(2, "a").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id("a").unwrap(), Some(2));
    assert_eq!(engine.doc_by_external_id("b").unwrap(), Some(1));
    assert_eq!(engine.external_ids(&[1, 2]).unwrap(), [Some("b".into()), Some("a".into())]);
}

#[test]
fn deleting_releases_the_external_id() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let a = batch.add_with_external_id("a", "", "alfa").unwrap();
    let b = batch.add_with_external_id("b", "", "beta").unwrap();
    batch.commit().unwrap();

    // Pelo ID externo e pelo DocId.
    let mut batch = engine.batch();
    assert_eq!(batch.delete_by_external_id("a").unwrap(), Some(a));
    assert_eq!(batch.delete_by_external_id("nenhum").unwrap(), None);
    batch.delete_document(b).unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id("a").unwrap(), None);
    assert_eq!(engine.doc_by_external_id("b").unwrap(), None);
    assert_eq!(engine.external_ids(&[a, b]).unwrap(), [None, None]);
    assert!(engine.search("alfa").unwrap().is_empty());

    // O ID livre pode ir para outro documento, que é novo.
    let mut batch = engine.batch();
    let c = batch.add_with_external_id("a", "", "gama").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id("a").unwrap(), Some(c));
}

#[test]
fn the_last_operation_of_the_batch_decides() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    let a = batch.add_with_external_id("a", "", "alfa").unwrap();
    batch.add_document(7, "sete").unwrap();
    batch.commit().unwrap();

    // Remover e readicionar pelo mesmo ID no lote: a remoção libera o ID na
    // hora, então o readicionado é um documento novo (o removido some).
    let mut batch = engine.batch();
    batch.delete_by_external_id("a").unwrap();
    let again = batch.add_with_external_id("a", "", "alfa de novo").unwrap();
    batch.commit().unwrap();
    assert_ne!(again, a);
    assert_eq!(engine.doc_by_external_id("a").unwrap(), Some(again));
    assert_eq!(engine.search("alfa").unwrap(), [again]);
    assert_eq!(engine.external_id(a).unwrap(), None);

    // Remover o dono e dar o ID a outro documento no mesmo lote.
    let mut batch = engine.batch();
    batch.delete_document(again).unwrap();
    batch.set_external_id(7, "a").unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id("a").unwrap(), Some(7));
    assert_eq!(engine.external_id(again).unwrap(), None);

    // Dar um ID e remover o documento no mesmo lote: o ID sai junto.
    let mut batch = engine.batch();
    batch.set_external_id(7, "z").unwrap();
    batch.delete_document(7).unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id("z").unwrap(), None);
    assert_eq!(engine.doc_by_external_id("a").unwrap(), None, "o anterior também saiu com o documento");

    // Um lote descartado não grava nada.
    let mut batch = engine.batch();
    batch.add_with_external_id("descartado", "", "x").unwrap();
    drop(batch);
    assert_eq!(engine.doc_by_external_id("descartado").unwrap(), None);
}

#[test]
fn replacing_and_compacting_keep_the_external_id() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_titled(5, "Cinco", "texto um").unwrap();
    batch.set_external_id(5, "cinco").unwrap();
    batch.commit().unwrap();
    // Substituir pelo DocId (sem falar do ID externo) mantém o ID.
    engine.index_document(5, "texto dois").unwrap();
    engine.compact().unwrap();
    assert_eq!(engine.external_id(5).unwrap().as_deref(), Some("cinco"));
    assert_eq!(engine.doc_by_external_id("cinco").unwrap(), Some(5));
    assert_eq!(ids_of(&engine, "dois"), [Some("cinco".into())]);
}

#[test]
fn invalid_external_ids_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    batch.add_document(1, "um").unwrap();
    let long = "x".repeat(MAX_EXTERNAL_ID_BYTES + 1);
    for bad in ["", long.as_str()] {
        assert!(matches!(batch.set_external_id(1, bad), Err(Error::InvalidId(_))), "{}", bad.len());
        assert!(matches!(batch.add_with_external_id(bad, "", "t"), Err(Error::InvalidId(_))));
        assert!(matches!(batch.delete_by_external_id(bad), Err(Error::InvalidId(_))));
        assert_eq!(engine.doc_by_external_id(bad).unwrap(), None);
    }
    assert_eq!(Error::InvalidId(String::new()).code(), "FF_INVALID_ID");
    // No limite, e com caracteres não ASCII, vale.
    let max = "é".repeat(MAX_EXTERNAL_ID_BYTES / 2);
    batch.set_external_id(1, &max).unwrap();
    batch.commit().unwrap();
    assert_eq!(engine.doc_by_external_id(&max).unwrap(), Some(1));
}

#[test]
fn external_ids_persist_and_are_read_only_safe() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = open(&dir);
        let mut batch = engine.batch();
        batch.add_with_external_id("doc-1", "", "persistente").unwrap();
        batch.commit().unwrap();
    }
    // Reaberto: o motor sabe que o banco tem IDs externos, e remover pelo
    // DocId ainda libera o ID.
    let engine = open(&dir);
    let doc = engine.doc_by_external_id("doc-1").unwrap().unwrap();
    {
        let reader = Engine::open_read_only(dir.path(), Config::default()).unwrap();
        assert_eq!(reader.doc_by_external_id("doc-1").unwrap(), Some(doc));
        assert_eq!(reader.external_ids(&[doc]).unwrap(), [Some("doc-1".into())]);
    }
    engine.delete_document(doc).unwrap();
    assert_eq!(engine.doc_by_external_id("doc-1").unwrap(), None);
}

#[test]
fn new_documents_never_take_a_reserved_doc_id() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);
    let mut batch = engine.batch();
    // ID externo num DocId ainda sem texto: o próximo automático pula ele.
    batch.set_external_id(1, "reservado").unwrap();
    let doc = batch.add_with_external_id("outro", "", "texto").unwrap();
    batch.commit().unwrap();
    assert_ne!(doc, 1);
    assert_eq!(engine.doc_by_external_id("reservado").unwrap(), Some(1));
    assert_eq!(engine.doc_by_external_id("outro").unwrap(), Some(doc));
}
