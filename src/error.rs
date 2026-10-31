//! O erro da biblioteca.

/// O `Result` da biblioteca.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Erros da biblioteca. Novas variantes podem aparecer (`#[non_exhaustive]`):
/// quem precisa decidir pelo tipo usa [`Error::code`], que não muda entre
/// versões nem com o idioma da mensagem.
#[derive(Debug)]
#[non_exhaustive]
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
    /// Outro lote está aberto ([`Engine::try_batch`](crate::Engine::try_batch)).
    Busy,
    /// O banco foi fechado (usado pelos bindings, onde um handle fechado
    /// ainda pode ser chamado).
    Closed,
    /// O ID externo já pertence a outro documento
    /// ([`IndexBatch::set_external_id`](crate::IndexBatch::set_external_id)).
    Duplicate(String),
    /// ID externo vazio ou longo demais (mais de
    /// [`MAX_EXTERNAL_ID_BYTES`](crate::MAX_EXTERNAL_ID_BYTES) bytes).
    InvalidId(String),
}

impl Error {
    /// Código estável do erro, para os bindings e para quem decide pelo tipo:
    /// `FF_DB`, `FF_CORRUPT`, `FF_CONFIG`, `FF_OPEN`, `FF_READ_ONLY`,
    /// `FF_BUSY`, `FF_CLOSED`, `FF_DUPLICATE` ou `FF_INVALID_ID`.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Db(_) => "FF_DB",
            Error::Corrupt(_) => "FF_CORRUPT",
            Error::Config(_) => "FF_CONFIG",
            Error::Open(_) => "FF_OPEN",
            Error::ReadOnly => "FF_READ_ONLY",
            Error::Busy => "FF_BUSY",
            Error::Closed => "FF_CLOSED",
            Error::Duplicate(_) => "FF_DUPLICATE",
            Error::InvalidId(_) => "FF_INVALID_ID",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "erro do RocksDB: {e}"),
            Error::Corrupt(msg) => write!(f, "índice corrompido: {msg}"),
            Error::Config(msg) => write!(f, "config incompatível: {msg}"),
            Error::Open(msg) => write!(f, "o banco não abriu: {msg}"),
            Error::ReadOnly => write!(f, "banco aberto só para leitura: abra com Engine::open para gravar"),
            Error::Busy => write!(f, "outro lote está aberto: faça commit dele ou tente de novo"),
            Error::Closed => write!(f, "o banco foi fechado"),
            Error::Duplicate(msg) => write!(f, "ID externo repetido: {msg}"),
            Error::InvalidId(msg) => write!(f, "ID externo inválido: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rocksdb::Error> for Error {
    fn from(e: rocksdb::Error) -> Self {
        Error::Db(e)
    }
}
