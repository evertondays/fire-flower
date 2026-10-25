//! Camada de processamento de texto: limpeza, normalização e tokenização.
//!
//! A mesma tokenização ([`for_each_token`]) é usada na indexação e na busca,
//! garantindo que "Ação", "ACAO" e "acao" virem o mesmo termo.

use rustc_hash::FxHashMap;
use unicode_normalization::char::{decompose_compatible, is_combining_mark};

use crate::stopwords::Stopwords;

/// Tokens maiores que isso são descartados (hashes, base64, lixo binário),
/// para não criar caminhos enormes e inúteis na Trie.
pub const MAX_TOKEN_CHARS: usize = 64;

/// Chama `f(token, posição)` para cada termo normalizado do texto:
/// - remove acentos (decomposição NFKD + descarte das marcas combinantes);
/// - converte para minúsculas;
/// - trata qualquer caractere não alfanumérico (pontuação, espaço) como separador.
///
/// A posição conta todo token entregue, stopwords inclusive (quem descarta
/// stopwords é quem chama, e o buraco fica: "casa de papel" → casa@0, de@1,
/// papel@2). Tokens longos demais são descartados e não contam.
///
/// Não aloca por token: `token` é um buffer reaproveitado.
pub fn for_each_token(text: &str, mut f: impl FnMut(&str, u32)) {
    let mut token = String::new();
    let mut chars = 0usize;
    let mut position = 0u32;

    let mut flush = |token: &mut String, chars: &mut usize| {
        if *chars > 0 {
            if *chars <= MAX_TOKEN_CHARS {
                f(token, position);
                position += 1;
            }
            token.clear();
            *chars = 0;
        }
    };

    for c in text.chars() {
        if c.is_ascii() {
            // ASCII não se decompõe nem tem acento: o caminho comum, sem tabelas.
            if c.is_ascii_alphanumeric() {
                token.push(c.to_ascii_lowercase());
                chars += 1;
            } else {
                flush(&mut token, &mut chars);
            }
            continue;
        }
        // NFKD caractere a caractere. A forma normalizada da string inteira
        // só difere disso na ordem das marcas combinantes, que são descartadas
        // (o teste `per_char_nfkd_matches_string_nfkd` confere).
        decompose_compatible(c, |d| {
            if is_combining_mark(d) {
                return;
            }
            if d.is_alphanumeric() {
                for lower in d.to_lowercase() {
                    if !is_combining_mark(lower) {
                        token.push(lower);
                        chars += 1;
                    }
                }
            } else {
                flush(&mut token, &mut chars);
            }
        });
    }
    flush(&mut token, &mut chars);
}

/// Divide o texto em termos normalizados (ver [`for_each_token`]).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for_each_token(text, |t, _| tokens.push(t.to_string()));
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

/// Um documento analisado: cada termo indexado com suas posições, e o
/// tamanho do documento (tokens indexados, sem stopwords).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// Termos em ordem alfabética; posições em ordem crescente.
    pub terms: Vec<(String, Vec<u32>)>,
    pub len: u32,
}

/// Analisa `text` como a indexação faz. Para muitos documentos, o
/// [`Analyzer`] reaproveita a memória entre um e outro.
pub fn analyze(text: &str, stopwords: &Stopwords) -> Analysis {
    let mut analyzer = Analyzer::default();
    let len = analyzer.run(text, stopwords);
    let mut terms: Vec<(String, Vec<u32>)> = analyzer.terms().map(|(t, p)| (t.to_string(), p.to_vec())).collect();
    terms.sort_unstable();
    Analysis { terms, len }
}

/// Agrupa os tokens de um documento por termo, guardando as posições.
///
/// Feito para o laço quente da indexação: os buffers são reaproveitados
/// entre documentos e nenhum token vira `String` (o texto dos termos fica
/// num buffer só).
#[derive(Default)]
pub struct Analyzer {
    /// Texto dos tokens indexados, um atrás do outro.
    text: String,
    /// (início, fim, posição) de cada token indexado.
    tokens: Vec<(u32, u32, u32)>,
    /// Grupo (termo) de cada token.
    group_of: Vec<u32>,
    /// (primeiro token, quantas ocorrências) de cada termo distinto.
    groups: Vec<(u32, u32)>,
    /// Onde começam as posições de cada grupo em `positions`.
    starts: Vec<u32>,
    /// Posições agrupadas por termo, cada grupo em ordem crescente.
    positions: Vec<u32>,
}

impl Analyzer {
    /// Analisa `text` e devolve o tamanho do documento (tokens sem stopwords).
    /// Os termos ficam disponíveis em [`Analyzer::terms`] até a próxima chamada.
    pub fn run(&mut self, text: &str, stopwords: &Stopwords) -> u32 {
        let Self { text: buf, tokens, group_of, groups, starts, positions } = self;
        buf.clear();
        tokens.clear();
        group_of.clear();
        groups.clear();
        for_each_token(text, |t, p| {
            if !stopwords.contains(t) {
                let start = buf.len() as u32;
                buf.push_str(t);
                tokens.push((start, buf.len() as u32, p));
            }
        });

        // Termo -> grupo. O mapa empresta `buf`, então nasce a cada documento
        // (uma alocação só, já no tamanho certo).
        let mut ids: FxHashMap<&str, u32> = FxHashMap::with_capacity_and_hasher(tokens.len(), Default::default());
        for (i, &(s, e, _)) in tokens.iter().enumerate() {
            let next = groups.len() as u32;
            let g = *ids.entry(&buf[s as usize..e as usize]).or_insert(next);
            if g == next {
                groups.push((i as u32, 0));
            }
            groups[g as usize].1 += 1;
            group_of.push(g);
        }

        // Ordenação por contagem: as posições de cada termo ficam juntas e,
        // como os tokens vêm em ordem, crescentes dentro do grupo.
        starts.clear();
        let mut acc = 0u32;
        for &(_, count) in groups.iter() {
            starts.push(acc);
            acc += count;
        }
        positions.clear();
        positions.resize(tokens.len(), 0);
        let mut cursor = starts.clone();
        for (&(_, _, p), &g) in tokens.iter().zip(group_of.iter()) {
            positions[cursor[g as usize] as usize] = p;
            cursor[g as usize] += 1;
        }
        tokens.len() as u32
    }

    /// Termos distintos do último documento, na ordem da primeira ocorrência.
    pub fn terms(&self) -> impl Iterator<Item = (&str, &[u32])> + '_ {
        self.groups.iter().zip(self.starts.iter()).map(|(&(first, count), &start)| {
            let (s, e, _) = self.tokens[first as usize];
            (&self.text[s as usize..e as usize], &self.positions[start as usize..(start + count) as usize])
        })
    }
}

/// Um termo da consulta, já normalizado. `prefix` vale para `termo*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub text: String,
    pub prefix: bool,
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
                parts.iter().map(|p| QueryTerm { text: p.text.clone(), prefix: false }).collect()
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
    let mut clauses = Vec::new();
    let mut rest = query;
    while let Some(open) = rest.find('"') {
        parse_words(&rest[..open], stopwords, &mut clauses);
        let inside = &rest[open + 1..];
        let (phrase, after) = match inside.find('"') {
            Some(close) => (&inside[..close], &inside[close + 1..]),
            None => (inside, ""),
        };
        parse_phrase(phrase, stopwords, &mut clauses);
        rest = after;
    }
    parse_words(rest, stopwords, &mut clauses);
    clauses
}

fn parse_words(text: &str, stopwords: &Stopwords, out: &mut Vec<Clause>) {
    for piece in text.split_whitespace() {
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
                out.push(Clause::Term(QueryTerm { text, prefix }));
            }
        }
    }
}

fn parse_phrase(text: &str, stopwords: &Stopwords, out: &mut Vec<Clause>) {
    let mut parts: Vec<PhraseTerm> = Vec::new();
    let mut first = None;
    for_each_token(text, |t, position| {
        if stopwords.contains(t) {
            return;
        }
        let base = *first.get_or_insert(position);
        parts.push(PhraseTerm { text: t.to_string(), offset: position - base });
    });
    match parts.len() {
        0 => {}
        1 => out.push(Clause::Term(QueryTerm { text: parts.pop().unwrap().text, prefix: false })),
        _ => out.push(Clause::Phrase(parts)),
    }
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
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::canonical_combining_class;

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

    fn term(text: &str, prefix: bool) -> Clause {
        Clause::Term(QueryTerm { text: text.into(), prefix })
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
