//! IDs externos no lote: a chave do usuário (link, número, UUID) de cada
//! documento, única e buscável (`ext:<doc>` e o reverso `xid:<ID>`).

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{DocId, Error, IndexBatch, Result, codec};
use crate::engine::external::validate;
use crate::engine::keys::{EXT, Key, xid_key};
use crate::engine::meta::varint;
use crate::engine::versions::MAX_DOC;
use crate::engine::view::Source;

/// O que o lote mudou nos IDs externos, por cima do que está no banco.
#[derive(Default)]
pub(in crate::engine) struct ExternalIds {
    /// ID externo → documento dono (`None`: o ID ficou livre neste lote).
    by_id: FxHashMap<String, Option<DocId>>,
    /// Documento → ID externo (`None`: o documento ficou sem).
    by_doc: FxHashMap<DocId, Option<String>>,
}

impl ExternalIds {
    pub(super) fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.by_doc.is_empty()
    }
}

impl IndexBatch<'_> {
    /// Indexa um documento pelo ID externo: se o ID já tem documento, ele é
    /// substituído (mesmo `DocId`); senão, um documento novo ganha um
    /// `DocId` automático e o ID. Devolve o `DocId`.
    ///
    /// Se o dono do ID foi removido antes neste mesmo lote, o ID está livre e
    /// o documento é novo (outro `DocId`).
    pub fn add_with_external_id(&mut self, external_id: &str, title: &str, text: &str) -> Result<DocId> {
        validate(external_id)?;
        let doc = match self.external_owner(external_id)? {
            Some(doc) => doc,
            None => self.next_doc_id,
        };
        self.add_titled(doc, title, text)?;
        self.set_external_id(doc, external_id)?;
        Ok(doc)
    }

    /// Dá ao documento `doc_id` o ID externo `external_id` (o anterior dele,
    /// se houver, fica livre). O ID não pode pertencer a outro documento:
    /// [`Error::Duplicate`]. Como o rótulo, vale para qualquer `DocId`; se a
    /// última operação do documento no lote for uma remoção, o ID sai junto.
    pub fn set_external_id(&mut self, doc_id: DocId, external_id: &str) -> Result<()> {
        validate(external_id)?;
        if doc_id > MAX_DOC {
            return Err(Error::Config(format!("ID de documento {doc_id} grande demais: vão até 2^40 − 1")));
        }
        if let Some(owner) = self.external_owner(external_id)?
            && owner != doc_id
        {
            return Err(Error::Duplicate(format!("{external_id:?} já pertence ao documento {owner}")));
        }
        match self.external_of(doc_id)? {
            Some(old) if old == external_id => return Ok(()),
            Some(old) => {
                self.external.by_id.insert(old, None);
            }
            None => {}
        }
        self.external.by_id.insert(external_id.to_string(), Some(doc_id));
        self.external.by_doc.insert(doc_id, Some(external_id.to_string()));
        // Um DocId ainda não indexado não pode ser dado depois a outro
        // documento por `add_with_external_id`.
        self.next_doc_id = self.next_doc_id.max(doc_id.saturating_add(1));
        Ok(())
    }

    /// Remove o documento dono do ID externo (como
    /// [`IndexBatch::delete_document`]). Devolve o `DocId` dele, ou `None` se
    /// o ID não tem documento.
    pub fn delete_by_external_id(&mut self, external_id: &str) -> Result<Option<DocId>> {
        validate(external_id)?;
        let Some(doc) = self.external_owner(external_id)? else { return Ok(None) };
        self.delete_document(doc)?;
        Ok(Some(doc))
    }

    /// O documento dono do ID neste ponto do lote. Um dono que acabou de ser
    /// removido no lote (última operação) libera o ID: quem o pega agora fica
    /// com ele, e o removido perde o dele.
    fn external_owner(&mut self, external_id: &str) -> Result<Option<DocId>> {
        let owner = match self.external.by_id.get(external_id) {
            Some(owner) => *owner,
            None if !self.state.has_external_ids => None,
            None => match self.engine.db.get_bytes(&xid_key(external_id))? {
                Some(v) => Some(codec::get_varint(&v)?),
                None => None,
            },
        };
        match owner {
            Some(doc) if self.is_deleted_in_batch(doc) => {
                self.external.by_doc.insert(doc, None);
                self.external.by_id.insert(external_id.to_string(), None);
                Ok(None)
            }
            owner => Ok(owner),
        }
    }

    /// O ID externo do documento neste ponto do lote.
    fn external_of(&self, doc: DocId) -> Result<Option<String>> {
        if let Some(id) = self.external.by_doc.get(&doc) {
            return Ok(id.clone());
        }
        if !self.state.has_external_ids {
            return Ok(None);
        }
        stored_external_id(self, doc)
    }

    /// A última operação do documento no lote é uma remoção.
    fn is_deleted_in_batch(&self, doc: DocId) -> bool {
        self.last_op.get(&doc).is_some_and(|&op| self.ops[op as usize].lens.is_none())
    }

    /// Grava as mudanças do lote em `ext:` e `xid:`. Um documento cuja
    /// operação final (`finals`) é remover perde o ID externo, como o rótulo.
    /// Num banco sem IDs externos, não lê nem grava nada.
    pub(super) fn write_external_ids(&mut self, wb: &mut WriteBatch, finals: &[usize]) -> Result<()> {
        for &i in finals {
            let op = self.ops[i];
            if op.lens.is_some() {
                continue;
            }
            if let Some(id) = self.external_of(op.doc)? {
                self.external.by_doc.insert(op.doc, None);
                // Só libera o ID se ele ainda for deste documento.
                if self.external.by_id.get(&id).is_none_or(|owner| *owner == Some(op.doc)) {
                    self.external.by_id.insert(id, None);
                }
            }
        }
        if self.external.is_empty() {
            return Ok(());
        }
        // Daqui em diante o banco tem (ou teve) IDs externos. Marcar antes do
        // commit gravar é conservador: um commit que falha só faz o próximo
        // lote procurar à toa.
        self.state.has_external_ids = true;
        for (doc, id) in &self.external.by_doc {
            match id {
                Some(id) => wb.put(Key::new(EXT, *doc), id.as_bytes()),
                None => wb.delete(Key::new(EXT, *doc)),
            }
        }
        for (id, owner) in &self.external.by_id {
            match owner {
                Some(doc) => wb.put(xid_key(id), varint(*doc)),
                None => wb.delete(xid_key(id)),
            }
        }
        Ok(())
    }
}

/// O ID externo gravado no banco para `doc`.
fn stored_external_id(batch: &IndexBatch<'_>, doc: DocId) -> Result<Option<String>> {
    Ok(batch.engine.db.get_bytes(Key::new(EXT, doc).as_ref())?.map(|v| String::from_utf8_lossy(&v).into_owned()))
}
