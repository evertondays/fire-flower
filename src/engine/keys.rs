//! Espaços de chave no RocksDB e as chaves numéricas (`node:<id>`, `post:<id>`…).

pub const ROOT_ID: u64 = 0;

/// Nome do merge operator, gravado pelo RocksDB no `OPTIONS` de cada abertura.
/// Muda com o formato: é por ele que um banco antigo é reconhecido antes de
/// ser aberto para escrita.
pub(super) const MERGE_OPERATOR: &str = "fire_flower_v7";

pub(super) const META_FORMAT: &[u8] = b"meta:format";

/// `1` se o banco grava posições (`pos:`), `0` se não.
pub(super) const META_POSITIONS: &[u8] = b"meta:positions";

pub(super) const META_NEXT_NODE: &[u8] = b"meta:next_node_id";

/// Versões mortas ainda nas listas (ver `versions`), unidas pelo merge.
pub(super) const META_DEAD: &[u8] = b"meta:dead";

/// Número do último commit (ver `Engine::view`).
pub(super) const META_EPOCH: &[u8] = b"meta:epoch";

pub(super) const META_NEXT_DOC: &[u8] = b"meta:next_doc_id";

pub(super) const META_DOC_COUNT: &[u8] = b"meta:doc_count";

pub(super) const META_TOTAL_LEN: &[u8] = b"meta:total_len";

pub(super) const META_TOTAL_TITLE_LEN: &[u8] = b"meta:total_title_len";

/// Códigos dos idiomas das stopwords, separados por vírgula ("pt,en").
pub(super) const META_LANGUAGES: &[u8] = b"meta:languages";

/// Stemming do banco ("none", "pt", "en"; ver [`Stemming::code`]).
pub(super) const META_STEMMING: &[u8] = b"meta:stemming";

/// Stemmer das flexões na consulta (`Config::inflections`), no mesmo código.
pub(super) const META_INFLECTIONS: &[u8] = b"meta:inflections";

pub(super) const STEM: &[u8] = b"stem:";

pub(super) const NODE: &[u8] = b"node:";

pub(super) const POST: &[u8] = b"post:";

pub(super) const POS: &[u8] = b"pos:";

pub(super) const NORM: &[u8] = b"norm:";

pub(super) const DOC: &[u8] = b"doc:";

/// `ver:<doc>`: o ID interno da última versão do documento, se ele já mudou.
pub(super) const VER: &[u8] = b"ver:";

/// `ext:<doc>`: o ID externo do documento (ver `external`).
pub(super) const EXT: &[u8] = b"ext:";

/// `xid:<ID externo>`: o documento dono do ID externo (o índice reverso).
pub(super) const XID: &[u8] = b"xid:";

/// Chave `<prefixo><número>` montada na pilha: o laço do commit monta uma
/// por termo, e `format!` alocaria uma `String` a cada vez.
pub(super) struct Key {
    buf: [u8; 32],
    len: usize,
}

impl Key {
    pub(super) fn new(prefix: &[u8], mut id: u64) -> Self {
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
pub(super) fn stem_key(stem: &str) -> Vec<u8> {
    [STEM, stem.as_bytes()].concat()
}

/// `xid:<ID externo>`.
pub(super) fn xid_key(external_id: &str) -> Vec<u8> {
    [XID, external_id.as_bytes()].concat()
}
