//! Stopwords por idioma: palavras tão comuns que não entram no índice.
//!
//! As listas ficam em `src/analysis/stopwords/<código>.txt`, uma palavra por linha, e
//! vão embutidas no binário. Para adicionar um idioma:
//! 1. crie `src/analysis/stopwords/<código>.txt`;
//! 2. adicione a variante em [`Language`] e o código em `code` e `list`.
//!
//! Os idiomas de um banco são gravados na criação (ver
//! [`Config::languages`](crate::Config::languages)): índice e consulta
//! precisam descartar as mesmas palavras.

use rustc_hash::FxHashSet;

use crate::analysis::tokenize;

/// Idioma das stopwords de um banco.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Language {
    Portuguese,
    English,
}

impl Language {
    /// Todos os idiomas com lista de stopwords.
    pub const ALL: &'static [Language] = &[Language::Portuguese, Language::English];

    /// Idiomas de um banco novo quando a config não diz nada.
    pub const DEFAULT: &'static [Language] = &[Language::Portuguese, Language::English];

    /// Código ISO 639-1 ("pt", "en"), como é gravado no banco.
    pub fn code(self) -> &'static str {
        match self {
            Language::Portuguese => "pt",
            Language::English => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.code().eq_ignore_ascii_case(code.trim()))
    }

    /// Conteúdo de `src/analysis/stopwords/<código>.txt`.
    fn list(self) -> &'static str {
        match self {
            Language::Portuguese => include_str!("pt.txt"),
            Language::English => include_str!("en.txt"),
        }
    }

    /// Palavras da lista, como estão no arquivo (sem comentários nem linhas vazias).
    pub fn words(self) -> impl Iterator<Item = &'static str> {
        self.list()
            .lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter(|w| !w.is_empty())
    }
}

/// Lista de idiomas legível: "pt, en" ou "none".
pub fn describe(languages: &[Language]) -> String {
    if languages.is_empty() {
        return "none".to_string();
    }
    languages.iter().map(|l| l.code()).collect::<Vec<_>>().join(", ")
}

/// Conjunto de stopwords já normalizadas de um ou mais idiomas.
///
/// Usa FxHash: é consultado uma vez por token indexado, e o SipHash da std
/// (resistente a ataques, mas lento em chaves curtas) pesa nessa escala.
#[derive(Debug, Clone)]
pub struct Stopwords {
    languages: Vec<Language>,
    set: FxHashSet<String>,
}

impl Stopwords {
    pub fn new(languages: &[Language]) -> Self {
        let mut languages = languages.to_vec();
        languages.sort_unstable();
        languages.dedup();
        // A mesma normalização da indexação: "não" vira "nao".
        let set = languages.iter().flat_map(|l| l.words()).flat_map(tokenize).collect();
        Self { languages, set }
    }

    /// Sem stopwords: tudo é indexado.
    pub fn none() -> Self {
        Self::new(&[])
    }

    /// Idiomas em ordem fixa (a de [`Language`]), sem repetição.
    pub fn languages(&self) -> &[Language] {
        &self.languages
    }

    /// `term` (já normalizado) é stopword de algum dos idiomas.
    pub fn contains(&self, term: &str) -> bool {
        self.set.contains(term)
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

impl Default for Stopwords {
    fn default() -> Self {
        Self::new(Language::DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_word_is_one_normalized_token() {
        for &lang in Language::ALL {
            let n = lang.words().count();
            assert!(n > 100, "{lang:?}: só {n} palavras");
            for w in lang.words() {
                assert_eq!(tokenize(w).len(), 1, "{lang:?}: {w:?} vira {:?}", tokenize(w));
            }
        }
    }

    #[test]
    fn languages_are_separate() {
        let pt = Stopwords::new(&[Language::Portuguese]);
        let en = Stopwords::new(&[Language::English]);
        let both = Stopwords::default();
        assert!(pt.contains("nao") && !pt.contains("the"));
        assert!(en.contains("the") && !en.contains("nao"));
        assert!(both.contains("nao") && both.contains("the"));
        assert!(Stopwords::none().is_empty());
        // Nomes comuns que ficam fora das listas.
        for w in ["sao", "era", "i", "brasil"] {
            assert!(!both.contains(w), "{w}");
        }
    }

    #[test]
    fn codes_round_trip() {
        for &lang in Language::ALL {
            assert_eq!(Language::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Language::from_code(" PT "), Some(Language::Portuguese));
        assert_eq!(Language::from_code("xx"), None);
        let s = Stopwords::new(&[Language::English, Language::Portuguese, Language::English]);
        assert_eq!(s.languages(), [Language::Portuguese, Language::English]);
        assert_eq!(describe(s.languages()), "pt, en");
    }
}
