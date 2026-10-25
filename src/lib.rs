//! fire-flower: motor de busca full-text embutido.
//!
//! Os termos ficam numa Trie persistida no RocksDB (um registro por nó), o que
//! dá busca exata, por prefixo (`comp*`) e autocompletar sem servidor externo.
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
//! assert_eq!(engine.suggest("es", 5)?, ["estudante"]);
//! # Ok::<(), fire_flower::Error>(())
//! ```

mod codec;
mod engine;
pub mod stopwords;
pub mod text;

pub use engine::{Config, Engine, IndexBatch};
pub use stopwords::{Language, Stopwords};

/// Peças internas expostas só para os microbenchmarks (`benches/micro.rs`).
/// Não fazem parte da API estável.
#[doc(hidden)]
pub mod internals {
    pub use crate::codec::{TrieNode, decode_postings, encode_postings};
    pub use crate::engine::{intersect, reads_on_this_thread, union_encoded};
}

pub type DocId = u64;
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Db(rocksdb::Error),
    /// Um valor gravado não pôde ser decodificado, ou a Trie aponta para um nó inexistente.
    Corrupt(String),
    /// A config não combina com o banco (ex.: idiomas das stopwords diferentes).
    Config(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "erro do RocksDB: {e}"),
            Error::Corrupt(msg) => write!(f, "índice corrompido: {msg}"),
            Error::Config(msg) => write!(f, "config incompatível: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            Error::Corrupt(_) | Error::Config(_) => None,
        }
    }
}

impl From<rocksdb::Error> for Error {
    fn from(e: rocksdb::Error) -> Self {
        Error::Db(e)
    }
}
