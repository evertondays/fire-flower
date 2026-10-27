//! Espaços de chave no RocksDB e as chaves numéricas (`node:<id>`, `post:<id>`…).

pub const ROOT_ID: u64 = 0;

/// Nome do merge operator, gravado pelo RocksDB no `OPTIONS` de cada abertura.
/// Muda com o formato: é por ele que um banco antigo é reconhecido antes de
/// ser aberto para escrita.
pub(crate) const MERGE_OPERATOR: &str = "fire_flower_v5";

pub(crate) const META_FORMAT: &[u8] = b"meta:format";

/// `1` se o banco grava posições (`pos:`), `0` se não.
pub(crate) const META_POSITIONS: &[u8] = b"meta:positions";

pub(crate) const META_NEXT_NODE: &[u8] = b"meta:next_node_id";

pub(crate) const META_NEXT_DOC: &[u8] = b"meta:next_doc_id";

pub(crate) const META_DOC_COUNT: &[u8] = b"meta:doc_count";

pub(crate) const META_TOTAL_LEN: &[u8] = b"meta:total_len";

pub(crate) const META_TOTAL_TITLE_LEN: &[u8] = b"meta:total_title_len";

/// Códigos dos idiomas das stopwords, separados por vírgula ("pt,en").
pub(crate) const META_LANGUAGES: &[u8] = b"meta:languages";

/// Stemming do banco ("nenhum", "pt", "en"; ver [`Stemming::code`]).
pub(crate) const META_STEMMING: &[u8] = b"meta:stemming";

/// Stemmer das flexões na consulta (`Config::inflections`), no mesmo código.
pub(crate) const META_INFLECTIONS: &[u8] = b"meta:inflections";

pub(crate) const STEM: &[u8] = b"stem:";

pub(crate) const NODE: &[u8] = b"node:";

pub(crate) const POST: &[u8] = b"post:";

pub(crate) const POS: &[u8] = b"pos:";

pub(crate) const NORM: &[u8] = b"norm:";

pub(crate) const DOC: &[u8] = b"doc:";

/// Chave `<prefixo><número>` montada na pilha: o laço do commit monta uma
/// por termo, e `format!` alocaria uma `String` a cada vez.
pub(crate) struct Key {
    pub(crate) buf: [u8; 32],
    pub(crate) len: usize,
}

impl Key {
    pub(crate) fn new(prefix: &[u8], mut id: u64) -> Self {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[19 - n] = b'0' + (id % 10) as u8;
            n += 1;
            id /= 10;
            if id == 0 {
                break;
            }
        }
        let mut buf = [0u8; 32];
        buf[..prefix.len()].copy_from_slice(prefix);
        buf[prefix.len()..prefix.len() + n].copy_from_slice(&digits[20 - n..]);
        Self { buf, len: prefix.len() + n }
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// `stem:<radical>`.
pub(crate) fn stem_key(stem: &str) -> Vec<u8> {
    [STEM, stem.as_bytes()].concat()
}
