//! IDs externos: a chave que o usuário dá a cada documento (link, número,
//! UUID…), devolvida junto com os resultados e única no banco.
//!
//! Dois espaços de chave, gravados pelo lote (`writer/external.rs`):
//! - `ext:<doc>` → o ID externo do documento;
//! - `xid:<ID externo>` → o documento dono do ID (o índice reverso, que acha
//!   o documento para substituir ou remover pelo ID do usuário).
//!
//! O documento é o `DocId` do usuário, não o ID interno da versão: editar não
//! mexe nos IDs externos.

use rocksdb::DB;

use crate::{DocId, Engine, Error, Result, codec};
use crate::engine::keys::{EXT, Key, XID, xid_key};

/// Tamanho máximo de um ID externo, em bytes (UTF-8). Ele vira parte de uma
/// chave do RocksDB: um link longo cabe, um texto inteiro não.
pub const MAX_EXTERNAL_ID_BYTES: usize = 512;

/// [`Error::InvalidId`] para um ID vazio ou longo demais.
pub(super) fn validate(external_id: &str) -> Result<()> {
    if external_id.is_empty() {
        return Err(Error::InvalidId("o ID externo não pode ser vazio".into()));
    }
    if external_id.len() > MAX_EXTERNAL_ID_BYTES {
        return Err(Error::InvalidId(format!(
            "{} bytes; o máximo é {MAX_EXTERNAL_ID_BYTES}",
            external_id.len()
        )));
    }
    Ok(())
}

/// O banco já tem algum ID externo. Sem nenhum, o commit não precisa ler
/// `ext:` dos documentos removidos (ver `write_external_ids`).
pub(super) fn any_external_ids(db: &DB) -> Result<bool> {
    let mode = rocksdb::IteratorMode::From(XID, rocksdb::Direction::Forward);
    match db.iterator(mode).next() {
        Some(item) => Ok(item?.0.starts_with(XID)),
        None => Ok(false),
    }
}

impl Engine {
    /// O ID externo do documento, se ele tiver um
    /// ([`IndexBatch::set_external_id`](crate::IndexBatch::set_external_id)).
    pub fn external_id(&self, doc_id: DocId) -> Result<Option<String>> {
        Ok(self.db.get_pinned(Key::new(EXT, doc_id))?.map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Os IDs externos de vários documentos, na ordem de `docs` (`None` para
    /// quem não tem), num único `MultiGet`: é como os resultados de uma busca
    /// voltam com o ID do usuário.
    pub fn external_ids(&self, docs: &[DocId]) -> Result<Vec<Option<String>>> {
        self.multi_get_strings(EXT, docs)
    }

    /// O documento dono do ID externo, se houver.
    pub fn doc_by_external_id(&self, external_id: &str) -> Result<Option<DocId>> {
        if validate(external_id).is_err() {
            return Ok(None); // nenhum documento pode ter esse ID
        }
        self.db.get_pinned(xid_key(external_id))?.map(|v| codec::get_varint(&v)).transpose()
    }

    /// Os valores de `<prefixo><doc>` de vários documentos, como texto, num
    /// único `MultiGet`.
    pub(super) fn multi_get_strings(&self, prefix: &[u8], docs: &[DocId]) -> Result<Vec<Option<String>>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let cf = self
            .db
            .cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
            .ok_or_else(|| Error::Corrupt("família de colunas padrão ausente".into()))?;
        let keys: Vec<Key> = docs.iter().map(|&doc| Key::new(prefix, doc)).collect();
        self.db
            .batched_multi_get_cf(cf, keys.iter().map(|k| k.as_ref()), false)
            .into_iter()
            .map(|value| Ok(value?.map(|v| String::from_utf8_lossy(&v).into_owned())))
            .collect()
    }
}
