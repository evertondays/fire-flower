//! fire-flower: motor de busca full-text embutido.
//!
//! Os termos ficam numa Trie persistida no RocksDB (um registro por nó), o que
//! dá busca exata, por prefixo (`comp*`), por frase (`"casa de papel"`),
//! ranking BM25 e autocompletar sem servidor externo.
//!
//! ```
//! use fire_flower::{Config, Engine};
//!
//! # let dir = tempfile::tempdir().unwrap();
//! # let path = dir.path();
//! let engine = Engine::open(path, Config::default())?;
//! let mut batch = engine.batch();
//! batch.add_document(1, "O coração do Brasil")?;
//! batch.add_document(2, "Coração de estudante")?;
//! batch.commit()?;
//!
//! assert_eq!(engine.search("CORACAO")?, [1, 2]);
//! assert_eq!(engine.search("coração bras*")?, [1]);
//! assert_eq!(engine.search("\"coração do brasil\"")?, [1]);
//! assert_eq!(engine.suggest("es", 5)?, ["estudante"]);
//!
//! // Ranqueado por BM25: os mais relevantes primeiro.
//! let top = engine.search_top("coração", 10)?;
//! assert_eq!(top.len(), 2);
//! assert!(top[0].score >= top[1].score);
//! # Ok::<(), fire_flower::Error>(())
//! ```

pub mod analysis;
mod codec;
mod engine;
mod error;
pub mod query;

pub use engine::{CommitStats, Config, Engine, FORMAT_VERSION, IndexBatch, InflectionMode};
pub use error::{Error, Result};
pub use analysis::stem::{Stemmer, Stemming};
pub use analysis::stopwords::{Language, Stopwords};

/// Peças internas expostas só para os microbenchmarks (`benches/micro.rs`).
/// Não fazem parte da API estável.
#[doc(hidden)]
pub mod internals {
    pub use crate::codec::{
        NormBlock, PositionsCursor, PostingsCursor, TrieNode, decode_docs, decode_positions, decode_postings, encode_positions_into, encode_postings, list_len, merge_positions, merge_postings,
    };
    pub use crate::engine::{intersect, reads_on_this_thread};
}

pub type DocId = u64;

/// Um documento numa lista de postagem e quantas vezes o termo aparece nele.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    pub doc: DocId,
    /// Frequência do termo no documento inteiro, título e texto (≥ 1).
    pub tf: u32,
    /// Quantas dessas ocorrências estão no título (≤ `tf`).
    pub title_tf: u32,
}

impl Posting {
    /// Entrada sem ocorrências no título.
    pub fn new(doc: DocId, tf: u32) -> Self {
        Self { doc, tf, title_tf: 0 }
    }
}

/// Um resultado ranqueado: o documento e a nota BM25.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    pub score: f64,
}

/// Os melhores resultados e quantos documentos casaram ao todo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopHits {
    /// Nota decrescente; no empate, ID crescente.
    pub hits: Vec<Hit>,
    /// Documentos que satisfazem a consulta (não só os devolvidos).
    pub total: usize,
}

/// Estatísticas do índice usadas pelo BM25.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Documentos distintos indexados (reindexar o mesmo ID não conta de novo).
    pub doc_count: u64,
    /// Soma dos tamanhos dos documentos (tokens indexados, sem stopwords),
    /// título e texto.
    pub total_len: u64,
    /// Soma dos tamanhos dos títulos (já contados em `total_len`).
    pub total_title_len: u64,
}

impl IndexStats {
    /// Tamanho médio de um documento.
    pub fn avg_len(&self) -> f64 {
        if self.doc_count == 0 { 0.0 } else { self.total_len as f64 / self.doc_count as f64 }
    }

    /// Tamanho médio de um título.
    pub fn avg_title_len(&self) -> f64 {
        if self.doc_count == 0 { 0.0 } else { self.total_title_len as f64 / self.doc_count as f64 }
    }

    /// Tamanho médio do texto sem o título.
    pub fn avg_body_len(&self) -> f64 {
        self.avg_len() - self.avg_title_len()
    }
}
