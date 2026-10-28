//! O erro da biblioteca.

/// O `Result` da biblioteca.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Db(rocksdb::Error),
    /// Um valor gravado não pôde ser decodificado, ou a Trie aponta para um nó inexistente.
    Corrupt(String),
    /// A config não combina com o banco (ex.: idiomas das stopwords diferentes).
    Config(String),
    /// O banco não abriu: a pasta não é um banco, ou outro processo o tem
    /// aberto para escrita.
    Open(String),
    /// Gravação (lote ou compactação) num banco aberto só para leitura.
    ReadOnly,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "erro do RocksDB: {e}"),
            Error::Corrupt(msg) => write!(f, "índice corrompido: {msg}"),
            Error::Config(msg) => write!(f, "config incompatível: {msg}"),
            Error::Open(msg) => write!(f, "o banco não abriu: {msg}"),
            Error::ReadOnly => write!(f, "banco aberto só para leitura: abra com Engine::open para gravar"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            Error::Corrupt(_) | Error::Config(_) | Error::Open(_) | Error::ReadOnly => None,
        }
    }
}

impl From<rocksdb::Error> for Error {
    fn from(e: rocksdb::Error) -> Self {
        Error::Db(e)
    }
}
