//! Testes do parser de consultas.

use crate::query::parse_query_stemmed;
use crate::analysis::stem::Stemmer;
use crate::query::parse_query;
use crate::analysis::stopwords::Stopwords;
use crate::query::PhraseTerm;
use crate::query::QueryTerm;
use crate::query::Clause;

fn term(text: &str, prefix: bool) -> Clause {
    Clause::Term(QueryTerm { text: text.into(), prefix, stem: None })
}

fn phrase(parts: &[(&str, u32)]) -> Clause {
    Clause::Phrase(parts.iter().map(|&(t, o)| PhraseTerm { text: t.into(), offset: o }).collect())
}

#[test]
fn parse_query_skips_exact_stopwords_but_keeps_prefixes() {
    let sw = Stopwords::default();
    assert_eq!(parse_query("Casa de Papel", &sw), [term("casa", false), term("papel", false)]);
    assert_eq!(parse_query("de* e-mail", &sw), [term("de", true), term("mail", false)]);
    assert!(parse_query("o que é", &sw).is_empty());
}

#[test]
fn parse_query_phrases() {
    let sw = Stopwords::default();
    let q = |s: &str| parse_query(s, &sw);
    assert_eq!(q("\"casa de papel\""), [phrase(&[("casa", 0), ("papel", 2)])]);
    assert_eq!(
        q("brasil \"copa do mundo\" 2014"),
        [term("brasil", false), phrase(&[("copa", 0), ("mundo", 2)]), term("2014", false)]
    );
    // Um termo só é termo comum; só stopwords some.
    assert_eq!(q("\"papel\""), [term("papel", false)]);
    assert_eq!(q("\"o papel\" rei"), [term("papel", false), term("rei", false)]);
    assert!(q("\"to be or not to be\"").is_empty());
    // Stopword no começo não conta: a distância é até o primeiro termo.
    assert_eq!(q("\"a casa de papel\""), [phrase(&[("casa", 0), ("papel", 2)])]);
    // Aspas sem fechamento vão até o fim; prefixo dentro da frase não existe.
    assert_eq!(q("rei \"revolução franc*"), [term("rei", false), phrase(&[("revolucao", 0), ("franc", 1)])]);
    // Aspas grudadas em palavras também separam.
    assert_eq!(q("rei\"de novo de novo\"fim*"), [term("rei", false), phrase(&[("novo", 0), ("novo", 2)]), term("fim", true)]);
    assert!(q("\"\" \"").is_empty());
    // Clause::terms serve para o destaque.
    assert_eq!(q("\"casa de papel\"")[0].terms().len(), 2);
}

#[test]
fn stemmed_queries_reduce_words_but_not_prefixes() {
    use crate::analysis::stem::Stemming;
    let sw = Stopwords::default();
    let stemmer = Stemmer::new(Stemming::Snowball(crate::analysis::stopwords::Language::Portuguese));
    let q = |s: &str| parse_query_stemmed(s, &sw, &stemmer);
    assert_eq!(q("Corrida das alunas"), [term("corr", false), term("alun", false)]);
    assert_eq!(q("\"informações do país\""), [phrase(&[("inform", 0), ("pais", 2)])]);
    assert_eq!(q("\"o país\""), [term("pais", false)]);
    // O prefixo fica como foi digitado; o radical vai à parte quando é outro.
    let stem = |text: &str, stem: Option<&str>| {
        Clause::Term(QueryTerm { text: text.into(), prefix: true, stem: stem.map(String::from) })
    };
    assert_eq!(q("brasileiro* bras* de*"), [stem("brasileiro", Some("brasileir")), stem("bras", None), stem("de", None)]);
    // Sem stemming, igual ao `parse_query`.
    assert_eq!(parse_query_stemmed("Corrida brasileiro*", &sw, &Stemmer::default()), parse_query("Corrida brasileiro*", &sw));
}
