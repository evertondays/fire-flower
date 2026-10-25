//! Camada de processamento de texto: limpeza, normalização e tokenização.
//!
//! A mesma função [`tokenize`] é usada na indexação e na busca, garantindo
//! que "Ação", "ACAO" e "acao" virem o mesmo termo.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::stopwords::Stopwords;

/// Tokens maiores que isso são descartados (hashes, base64, lixo binário),
/// para não criar caminhos enormes e inúteis na Trie.
pub const MAX_TOKEN_CHARS: usize = 64;

/// Divide o texto em termos normalizados:
/// - remove acentos (decomposição NFKD + descarte das marcas combinantes);
/// - converte para minúsculas;
/// - trata qualquer caractere não alfanumérico (pontuação, espaço) como separador.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut len = 0usize;

    let mut flush = |current: &mut String, len: &mut usize| {
        if *len > 0 && *len <= MAX_TOKEN_CHARS {
            tokens.push(std::mem::take(current));
        } else {
            current.clear();
        }
        *len = 0;
    };

    for c in text.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        if c.is_alphanumeric() {
            for lower in c.to_lowercase() {
                if !is_combining_mark(lower) {
                    current.push(lower);
                    len += 1;
                }
            }
        } else {
            flush(&mut current, &mut len);
        }
    }
    flush(&mut current, &mut len);
    tokens
}

/// Termos que a indexação grava para `text`: tokenizados, sem stopwords,
/// ordenados e sem repetição.
pub fn index_terms(text: &str, stopwords: &Stopwords) -> Vec<String> {
    let mut terms = tokenize(text);
    terms.retain(|t| !stopwords.contains(t));
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Um termo da consulta, já normalizado. `prefix` vale para `termo*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub text: String,
    pub prefix: bool,
}

/// Separa a consulta em termos, com a mesma regra da indexação.
///
/// Stopwords exatas são ignoradas: elas não estão no índice, então exigi-las
/// zeraria o resultado ("casa de papel" busca `casa` E `papel`). Um prefixo
/// que é stopword continua valendo (`de*` acha "dezembro").
pub fn parse_query(query: &str, stopwords: &Stopwords) -> Vec<QueryTerm> {
    let mut terms = Vec::new();
    for piece in query.split_whitespace() {
        let (piece, prefix) = match piece.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (piece, false),
        };
        // "e-mail" vira ["e", "mail"], exatamente como na indexação.
        let tokens = tokenize(piece);
        let last = tokens.len().saturating_sub(1);
        for (i, text) in tokens.into_iter().enumerate() {
            let prefix = prefix && i == last;
            if prefix || !stopwords.contains(&text) {
                terms.push(QueryTerm { text, prefix });
            }
        }
    }
    terms
}

/// Normaliza um único termo (ex.: prefixo do autocompletar).
/// Retorna `None` se o texto não contiver nenhum caractere indexável.
pub fn normalize_term(text: &str) -> Option<String> {
    let mut tokens = tokenize(text);
    match tokens.len() {
        0 => None,
        1 => tokens.pop(),
        // "e-mail" vira "email": para prefixos, juntamos as partes.
        _ => Some(tokens.concat()),
    }
}

/// Remove tags HTML e decodifica as entidades mais comuns.
/// Conteúdo de `<script>` e `<style>` é descartado por inteiro.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('>') else {
            // '<' sem fechamento: trata como texto.
            out.push_str(rest);
            return decode_entities(&out);
        };
        let tag = rest[1..end].trim_start().to_ascii_lowercase();
        rest = &rest[end + 1..];

        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if (name == "script" || name == "style") && !tag.ends_with('/') {
            let closing = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&closing) {
                Some(pos) => rest[pos..].find('>').map_or("", |e| &rest[pos + e + 1..]),
                None => "",
            };
        }
        // Tags viram separadores, senão "<p>a</p><p>b</p>" viraria "ab".
        out.push(' ');
    }
    out.push_str(rest);
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stopwords::Language;

    #[test]
    fn removes_accents_case_and_punctuation() {
        assert_eq!(
            tokenize("Ação, CORAÇÃO! São Paulo... pinguim/über"),
            ["acao", "coracao", "sao", "paulo", "pinguim", "uber"]
        );
    }

    #[test]
    fn keeps_numbers_and_drops_huge_tokens() {
        let huge = "x".repeat(MAX_TOKEN_CHARS + 1);
        assert_eq!(tokenize(&format!("rust 2024 {huge} fim")), ["rust", "2024", "fim"]);
    }

    #[test]
    fn folds_compatibility_chars() {
        // Ligadura "ﬁ" e dígitos de largura total.
        assert_eq!(tokenize("ﬁm １２"), ["fim", "12"]);
    }

    #[test]
    fn normalize_term_joins_parts() {
        assert_eq!(normalize_term("E-mail").as_deref(), Some("email"));
        assert_eq!(normalize_term("  !! "), None);
    }

    #[test]
    fn index_terms_drop_stopwords_sort_and_dedup() {
        let sw = Stopwords::default();
        assert_eq!(index_terms("O rato roeu a roupa do Rei de Roma, o rato", &sw), ["rato", "rei", "roeu", "roma", "roupa"]);
        assert_eq!(index_terms("The Lord of the Rings", &sw), ["lord", "rings"]);
        assert!(index_terms("de, da, do e o", &sw).is_empty());
        // Só inglês: as palavras do português são indexadas.
        let en = Stopwords::new(&[Language::English]);
        assert_eq!(index_terms("the casa de papel", &en), ["casa", "de", "papel"]);
        assert_eq!(index_terms("o rato", &Stopwords::none()), ["o", "rato"]);
    }

    #[test]
    fn parse_query_skips_exact_stopwords_but_keeps_prefixes() {
        let sw = Stopwords::default();
        let q = |s: &str| parse_query(s, &sw).into_iter().map(|t| (t.text, t.prefix)).collect::<Vec<_>>();
        assert_eq!(q("Casa de Papel"), [("casa".into(), false), ("papel".into(), false)]);
        assert_eq!(q("de* e-mail"), [("de".into(), true), ("mail".into(), false)]);
        assert!(q("o que é").is_empty());
    }

    #[test]
    fn strips_html() {
        let html = "<html><head><style>p{color:red}</style><script>var x=1;</script></head>\
                    <body><p>Olá&nbsp;mundo</p><p>Tom &amp; Jerry</p></body></html>";
        assert_eq!(tokenize(&strip_html(html)), ["ola", "mundo", "tom", "jerry"]);
    }

    #[test]
    fn unclosed_tag_is_text() {
        assert_eq!(tokenize(&strip_html("a < b")), ["a", "b"]);
    }
}
