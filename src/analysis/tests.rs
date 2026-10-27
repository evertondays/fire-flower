//! Testes da análise de texto.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, is_combining_mark};

use crate::Language;
use crate::analysis::{
    Analysis, Analyzer, MAX_TOKEN_CHARS, analyze, for_each_term, for_each_token, index_terms,
    normalize_term, strip_html, tokenize,
};
use crate::analysis::stem::Stemmer;
use crate::analysis::stopwords::Stopwords;

/// A tokenização original: NFKD da string inteira.
fn reference_tokenize(text: &str) -> Vec<String> {
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

/// NFKD por caractere só muda a ordem das marcas combinantes (classe de
/// combinação ≠ 0). Isso não altera os tokens se nenhum caractere com
/// classe ≠ 0 for, ao mesmo tempo, alfanumérico e não-marca.
#[test]
fn per_char_nfkd_matches_string_nfkd() {
    for code in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(code) else { continue };
        if canonical_combining_class(c) != 0 {
            assert!(is_combining_mark(c) || !c.is_alphanumeric(), "U+{code:04X}");
        }
    }
    let samples = [
        "Ação, CORAÇÃO! São Paulo... pinguim/über ﬁm １２ e-mail",
        "Ελληνικά Ἀθῆναι, Москва́, 東京都, 한국어 낱말, ﷺ ㎏ Ⅻ ½",
        "a\u{0301}\u{0327}b c\u{0327}\u{0301}d Å Ǻ ǻ ẞ ß İ ŉ Ǆ ǅ ǆ ﬀ",
        "हिन्दी कि़ ক্ষ ภาษาไทย עִבְרִית العَرَبِيَّة",
    ];
    for s in samples {
        assert_eq!(tokenize(s), reference_tokenize(s), "{s}");
    }
    // Cada caractere entre letras, sozinho: pega decomposições e separadores.
    for code in (0..=0x10FFFFu32).step_by(7) {
        let Some(c) = char::from_u32(code) else { continue };
        let s = format!("x{c}y{c}\u{0301}z");
        assert_eq!(tokenize(&s), reference_tokenize(&s), "U+{code:04X}");
    }
}

#[test]
fn positions_count_stopwords_but_not_huge_tokens() {
    let mut got = Vec::new();
    let huge = "x".repeat(MAX_TOKEN_CHARS + 1);
    for_each_token(&format!("Casa de {huge} Papel, casa"), |t, p| got.push((t.to_string(), p)));
    assert_eq!(got, [("casa".into(), 0), ("de".into(), 1), ("papel".into(), 2), ("casa".into(), 3)]);
}

#[test]
fn analyze_groups_positions_and_counts_length() {
    let sw = Stopwords::default();
    let a = analyze("O rato roeu a roupa do Rei de Roma, o rato", &sw);
    assert_eq!(a.len, 6, "stopwords não contam no tamanho");
    assert_eq!(
        a.terms,
        [
            ("rato".to_string(), vec![1, 10]),
            ("rei".into(), vec![6]),
            ("roeu".into(), vec![2]),
            ("roma".into(), vec![8]),
            ("roupa".into(), vec![4]),
        ]
    );
    assert_eq!(analyze("de, da, do e o", &sw), Analysis { terms: vec![], len: 0 });
    assert_eq!(analyze("", &sw), Analysis { terms: vec![], len: 0 });
}

#[test]
fn analyzer_reuses_buffers_between_documents() {
    let sw = Stopwords::none();
    let mut analyzer = Analyzer::default();
    assert_eq!(analyzer.run("a b a c a", &sw), 5);
    let first: Vec<(String, Vec<u32>)> = analyzer.terms().map(|(t, p)| (t.into(), p.to_vec())).collect();
    assert_eq!(first, [("a".to_string(), vec![0, 2, 4]), ("b".into(), vec![1]), ("c".into(), vec![3])]);
    assert_eq!(analyzer.run("z", &sw), 1);
    let second: Vec<(String, Vec<u32>)> = analyzer.terms().map(|(t, p)| (t.into(), p.to_vec())).collect();
    assert_eq!(second, [("z".to_string(), vec![0])]);
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
fn analyzer_stems_terms_after_dropping_stopwords() {
    use crate::analysis::stem::Stemming;
    let sw = Stopwords::default();
    let stemmer = Stemmer::new(Stemming::Snowball(crate::analysis::stopwords::Language::Portuguese));
    let mut analyzer = Analyzer::default();
    let (len, title_len) = analyzer.run_fields("Corridas", "Os alunos correndo e a aluna", &sw, &stemmer);
    assert_eq!((len, title_len), (4, 1));
    let mut terms: Vec<(String, Vec<u32>)> = analyzer.terms().map(|(t, p)| (t.into(), p.to_vec())).collect();
    terms.sort();
    assert_eq!(terms, [("alun".to_string(), vec![102, 106]), ("corr".into(), vec![0, 103])]);
    let mut seen = Vec::new();
    for_each_term("Os alunos correndo", &sw, &stemmer, |t, p| seen.push((t.to_string(), p)));
    assert_eq!(seen, [("alun".to_string(), 1), ("corr".into(), 2)]);
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
