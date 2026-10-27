//! Consulta → partes: termos, prefixos (`termo*`) e frases (`"..."`), com a
//! mesma normalização da indexação.

use crate::analysis::for_each_term;
use crate::analysis::tokenize;
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::Stopwords;

#[cfg(test)]
mod tests;

/// Um termo da consulta, já normalizado. `prefix` vale para `termo*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    /// O termo como está no índice (o radical, com stemming). Num prefixo, o
    /// texto digitado, normalizado: os radicais que começam com ele casam.
    pub text: String,
    pub prefix: bool,
    /// Só em prefixos com stemming: o radical do texto digitado, quando é
    /// outro (`brasileiro*` → `brasileir`). A busca casa também esse termo
    /// exato, senão a palavra inteira, digitada sem espaço, não acharia nada
    /// (nenhum radical começa com `brasileiro`).
    pub stem: Option<String>,
}

/// Termo de uma frase e sua distância até o primeiro termo dela.
/// `"casa de papel"` vira `casa@0, papel@2`: a stopword fica como buraco.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseTerm {
    pub text: String,
    pub offset: u32,
}

/// Uma parte da consulta. Todas são obrigatórias (E).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    /// Termo exato ou prefixo (`termo*`).
    Term(QueryTerm),
    /// Frase entre aspas, com dois ou mais termos.
    Phrase(Vec<PhraseTerm>),
}

impl Clause {
    /// Os termos da parte (para destacar no texto). Termos de frase são exatos.
    pub fn terms(&self) -> Vec<QueryTerm> {
        match self {
            Clause::Term(t) => vec![t.clone()],
            Clause::Phrase(parts) => {
                parts.iter().map(|p| QueryTerm { text: p.text.clone(), prefix: false, stem: None }).collect()
            }
        }
    }
}

/// Separa a consulta em partes, com a mesma regra da indexação.
///
/// - Palavras soltas viram termos; `termo*` casa por prefixo.
/// - Texto entre aspas vira uma frase: os termos têm que aparecer nessa
///   ordem e a essa distância. Aspas sem fechamento vão até o fim.
/// - Stopwords exatas são ignoradas: elas não estão no índice, então exigi-las
///   zeraria o resultado ("casa de papel" busca `casa` E `papel`). Um prefixo
///   que é stopword continua valendo (`de*` acha "dezembro"). Numa frase, a
///   stopword só conta a distância.
/// - Uma frase com um termo só (`"papel"`, `"o papel"`) é um termo comum; uma
///   frase só de stopwords some.
pub fn parse_query(query: &str, stopwords: &Stopwords) -> Vec<Clause> {
    parse_query_stemmed(query, stopwords, &Stemmer::default())
}

/// Como [`parse_query`], com os termos reduzidos ao radical como na
/// indexação. Prefixos não são reduzidos (ver [`QueryTerm::stem`]).
pub fn parse_query_stemmed(query: &str, stopwords: &Stopwords, stemmer: &Stemmer) -> Vec<Clause> {
    let mut clauses = Vec::new();
    let mut rest = query;
    while let Some(open) = rest.find('"') {
        parse_words(&rest[..open], stopwords, stemmer, &mut clauses);
        let inside = &rest[open + 1..];
        let (phrase, after) = match inside.find('"') {
            Some(close) => (&inside[..close], &inside[close + 1..]),
            None => (inside, ""),
        };
        parse_phrase(phrase, stopwords, stemmer, &mut clauses);
        rest = after;
    }
    parse_words(rest, stopwords, stemmer, &mut clauses);
    clauses
}

fn parse_words(text: &str, stopwords: &Stopwords, stemmer: &Stemmer, out: &mut Vec<Clause>) {
    for piece in text.split_whitespace() {
        let (piece, prefix) = match piece.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (piece, false),
        };
        // "e-mail" vira ["e", "mail"], exatamente como na indexação.
        let tokens = tokenize(piece);
        let last = tokens.len().saturating_sub(1);
        for (i, text) in tokens.into_iter().enumerate() {
            if prefix && i == last {
                let stem = Some(stemmer.stem(&text)).filter(|s| stemmer.is_on() && *s != text);
                out.push(Clause::Term(QueryTerm { text, prefix: true, stem }));
            } else if !stopwords.contains(&text) {
                out.push(Clause::Term(QueryTerm { text: stemmer.stem(&text), prefix: false, stem: None }));
            }
        }
    }
}

fn parse_phrase(text: &str, stopwords: &Stopwords, stemmer: &Stemmer, out: &mut Vec<Clause>) {
    let mut parts: Vec<PhraseTerm> = Vec::new();
    let mut first = None;
    for_each_term(text, stopwords, stemmer, |t, position| {
        let base = *first.get_or_insert(position);
        parts.push(PhraseTerm { text: t.to_string(), offset: position - base });
    });
    match parts.len() {
        0 => {}
        1 => out.push(Clause::Term(QueryTerm { text: parts.pop().unwrap().text, prefix: false, stem: None })),
        _ => out.push(Clause::Phrase(parts)),
    }
}
