//! O começo e o fim da limpeza de uma compactação: quais mortos ela pode
//! tirar das listas e, depois, as palavras que ficaram sem documento.

use rocksdb::WriteBatch;

use crate::{Engine, Error, Result, codec};
use crate::engine::keys::{Key, META_DEAD, NODE, POST};

/// O que uma compactação limpou ([`Engine::compact`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactStats {
    /// Versões mortas (de documentos substituídos ou removidos) tiradas das listas.
    pub purged: usize,
    /// Palavras que ficaram sem documento e saíram da Trie (e do autocompletar).
    pub emptied_words: usize,
}

impl Engine {
    /// Os mortos que esta compactação pode tirar das listas, em ordem, e
    /// quantos do começo de `dead` eles são.
    ///
    /// Só os que todo snapshot aberto já vê como mortos: para um snapshot
    /// anterior à morte, a versão ainda está viva, e tirá-la das listas a
    /// faria sumir para ele. Snapshots abertos depois desta leitura são mais
    /// novos. Uma busca longa só adia a limpeza de algumas mortes.
    pub(super) fn purgeable(&self) -> Result<(Vec<u64>, usize)> {
        let dead = self.dead.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = self.db.property_int_value("rocksdb.oldest-snapshot-sequence")?.unwrap_or(0);
        // 0: nenhum snapshot aberto.
        let limit = if oldest == 0 { u64::MAX } else { oldest };
        let cut = dead.partition_point(|&(seq, _)| seq <= limit);
        let mut ids: Vec<u64> = dead[..cut].iter().map(|&(_, id)| id).collect();
        ids.sort_unstable();
        Ok((ids, cut))
    }

    /// Depois de `compact_range`: as listas já não têm `purged`. Grava os
    /// mortos que sobraram, tira da Trie as palavras que a limpeza esvaziou e
    /// para a limpeza. Pega a trava do escritor (espera um lote aberto).
    pub(super) fn finish_purge(&self, purged: &[u64], cut: usize) -> Result<CompactStats> {
        let mut batch = self.batch();
        let result = (|| {
            let mut wb = WriteBatch::default();
            let mut remaining: Vec<u64> = {
                let dead = self.dead.lock().unwrap_or_else(|e| e.into_inner());
                dead[cut..].iter().map(|&(_, id)| id).collect()
            };
            remaining.sort_unstable();
            if remaining.is_empty() {
                wb.delete(META_DEAD);
            } else {
                wb.put(META_DEAD, codec::encode_node_set(&remaining));
            }
            let empty = self.empty_words(&self.purge.emptied())?;
            for &id in &empty {
                let node = batch.clear_end_of_word(id)?;
                wb.put(Key::new(NODE, id), node);
            }
            // Como no commit: marca os nós no cache das buscas, grava, publica.
            let next = self.epoch() + 1;
            self.nodes.mark(empty.iter().copied(), next);
            self.write_commit(wb, next)?;
            self.dead.lock().unwrap_or_else(|e| e.into_inner()).drain(..cut);
            Ok(CompactStats { purged: purged.len(), emptied_words: empty.len() })
        })();
        // Gravado ou não, a compactação acabou: as listas já estão limpas, e
        // uma busca que deixe de filtrar `purged` não os encontra mais.
        if result.is_ok() {
            self.deaths.remove(purged);
        }
        self.purge.finish();
        batch.keep_cache();
        result
    }

    /// Dos nós candidatos (em ordem), os que estão com `post:` vazio: a
    /// palavra não tem mais documento.
    fn empty_words(&self, candidates: &[u64]) -> Result<Vec<u64>> {
        let db = &self.db;
        let cf = db
            .cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
            .ok_or_else(|| Error::Corrupt("família de colunas padrão ausente".into()))?;
        let mut empty = Vec::new();
        for chunk in candidates.chunks(1024) {
            let keys: Vec<Key> = chunk.iter().map(|&id| Key::new(POST, id)).collect();
            for (&id, value) in chunk.iter().zip(db.batched_multi_get_cf(cf, keys.iter().map(|k| k.as_ref()), false)) {
                let is_empty = match value? {
                    Some(v) => codec::list_len(&v)? == 0,
                    None => true,
                };
                if is_empty {
                    empty.push(id);
                }
            }
        }
        Ok(empty)
    }
}
