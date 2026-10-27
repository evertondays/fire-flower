//! Texto → termos: limpeza, normalização, tokenização, stopwords e stemming.
//!
//! A mesma tokenização ([`for_each_token`]) é usada na indexação e na busca,
//! garantindo que "Ação", "ACAO" e "acao" virem o mesmo termo. Com stemming
//! (ver [`stem`]), o termo é o radical do token.

pub mod stem;
pub mod stopwords;

mod analyzer;
mod html;
mod tokenizer;
#[cfg(test)]
mod tests;

pub use self::analyzer::{Analysis, Analyzer, TITLE_GAP, analyze, for_each_term, index_terms};
pub use self::html::strip_html;
pub use self::tokenizer::{MAX_TOKEN_CHARS, for_each_token, normalize_term, tokenize};
