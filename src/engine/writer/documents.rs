//! O que o commit grava por documento: tamanhos (`norm:`) e estatísticas,
//! termos (`fwd:`) e lápides dos que saíram, e rótulos.

use std::collections::BTreeMap;

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{DocId, Error, IndexBatch, IndexStats, Result, codec};
use crate::codec::{DocLens, NORM_BLOCK_DOCS, NormBlock};
use crate::engine::keys::{DOC, FWD, Key, NORM, POS, POST};
use crate::engine::view::Source;
use crate::engine::writer::CommitStats;

/// Os blocos de `norm:` que o lote toca, como estavam no banco.
pub(super) type Norms = BTreeMap<u64, NormBlock>;

/// Os termos (nós terminais, em ordem) dos documentos do lote que já existiam.
pub(super) struct OldTerms {
    terms: FxHashMap<DocId, Vec<u64>>,
    pub(super) reads: u64,
}

/// O documento está no bloco de `norm:` (já foi indexado e não removido).
fn exists(norms: &Norms, doc: DocId) -> bool {
    norms.get(&(doc / NORM_BLOCK_DOCS)).is_some_and(|b| b.get_lens((doc % NORM_BLOCK_DOCS) as u16).is_some())
}

/// Os nós em `old` que não estão em `new` (as duas listas em ordem).
fn removed<'a>(old: &'a [u64], new: &'a [u64]) -> impl Iterator<Item = u64> + 'a {
    let mut j = 0;
    old.iter().copied().filter(move |&node| {
        while j < new.len() && new[j] < node {
            j += 1;
        }
        new.get(j) != Some(&node)
    })
}

impl IndexBatch<'_> {
    /// Lê os blocos de `norm:` dos documentos das operações `finals`. É por
    /// eles que o commit sabe quem já existia: numa carga nova, com IDs
    /// crescentes, são um ou dois blocos por lote e nenhuma leitura a mais.
    pub(super) fn read_norms(&self, finals: &[usize]) -> Result<Norms> {
        let mut norms = Norms::new();
        for &i in finals {
            let block_id = self.ops[i].doc / NORM_BLOCK_DOCS;
            if norms.contains_key(&block_id) {
                continue;
            }
            let block = match self.engine.db.get_bytes(Key::new(NORM, block_id).as_ref())? {
                Some(v) => NormBlock::decode(&v)?,
                None => NormBlock::default(),
            };
            norms.insert(block_id, block);
        }
        Ok(norms)
    }

    /// Os termos antigos (`fwd:`) dos documentos que já existiam, num
    /// `MultiGet` só. Vazio num banco sem `fwd:` ou numa carga nova.
    pub(super) fn read_old_terms(&self, finals: &[usize], norms: &Norms) -> Result<OldTerms> {
        let mut old = OldTerms { terms: FxHashMap::default(), reads: 0 };
        if !self.engine.edits {
            return Ok(old);
        }
        let docs: Vec<DocId> = finals.iter().map(|&i| self.ops[i].doc).filter(|&d| exists(norms, d)).collect();
        if docs.is_empty() {
            return Ok(old);
        }
        let db = &self.engine.db;
        let cf = db
            .cf_handle(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
            .ok_or_else(|| Error::Corrupt("família de colunas padrão ausente".into()))?;
        let keys: Vec<Key> = docs.iter().map(|&d| Key::new(FWD, d)).collect();
        old.reads = keys.len() as u64;
        for (doc, value) in docs.into_iter().zip(db.batched_multi_get_cf(cf, keys.iter().map(|k| k.as_ref()), false)) {
            let value = value?.ok_or_else(|| Error::Corrupt(format!("documento {doc} sem fwd:")))?;
            old.terms.insert(doc, codec::decode_node_set(&value)?);
        }
        Ok(old)
    }

    /// Grava os termos novos (`fwd:`) de cada documento e as lápides dos
    /// termos que saíram: de um documento substituído, os antigos que o texto
    /// novo não tem; de um removido, todos. Termos que continuam não precisam
    /// de lápide, porque a entrada nova vence a antiga no merge.
    pub(super) fn write_documents(
        &self,
        wb: &mut WriteBatch,
        finals: &[usize],
        old: &OldTerms,
        new_terms: &mut [Vec<u64>],
        stats: &mut CommitStats,
    ) -> Result<()> {
        if !self.engine.edits {
            return Ok(());
        }
        // Termo -> documentos que saíram dele.
        let mut gone: FxHashMap<u64, Vec<DocId>> = FxHashMap::default();
        for &i in finals {
            let op = self.ops[i];
            let terms = &mut new_terms[i];
            terms.sort_unstable();
            if op.lens.is_some() {
                wb.put(Key::new(FWD, op.doc), codec::encode_node_set(terms));
            }
            let Some(before) = old.terms.get(&op.doc) else { continue };
            match op.lens {
                Some(_) => stats.replaced += 1,
                None => {
                    wb.delete(Key::new(FWD, op.doc));
                    stats.deleted += 1;
                }
            }
            for node in removed(before, terms) {
                gone.entry(node).or_default().push(op.doc);
            }
        }
        let mut buf = Vec::new();
        for (node, mut docs) in gone {
            docs.sort_unstable();
            stats.tombstones += docs.len();
            buf.clear();
            codec::encode_tombstones_into(&mut buf, &docs);
            wb.merge(Key::new(POST, node), &buf);
            if self.engine.positions {
                wb.merge(Key::new(POS, node), &buf);
            }
        }
        Ok(())
    }

    /// Atualiza os blocos de `norm:` tocados pelo lote e as estatísticas. Um
    /// documento que já existia troca de tamanho e não conta de novo; um
    /// removido sai do bloco e desconta. Devolve as estatísticas novas e os
    /// blocos gravados. `norms` fica como o banco depois do commit.
    pub(super) fn write_lengths(
        &self,
        wb: &mut WriteBatch,
        finals: &[usize],
        norms: &mut Norms,
    ) -> Result<(IndexStats, Vec<u64>)> {
        let mut index = self.state.stats;
        for &i in finals {
            let op = self.ops[i];
            let block = norms.get_mut(&(op.doc / NORM_BLOCK_DOCS)).expect("read_norms leu todo bloco do lote");
            let offset = (op.doc % NORM_BLOCK_DOCS) as u16;
            let Some((len, title_len, title_span)) = op.lens else {
                if let Some(old) = block.remove(offset) {
                    index.doc_count -= 1;
                    index.total_len = index.total_len.saturating_sub(u64::from(old.len));
                    index.total_title_len = index.total_title_len.saturating_sub(u64::from(old.title_len));
                }
                continue;
            };
            // Título com mais de 65.535 tokens: a contagem para no máximo.
            let small = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            let (title_len, title_span) = (small(title_len), small(title_span));
            match block.set(offset, DocLens { len, title_len, title_span }) {
                Some(old) => {
                    index.total_len = (index.total_len + u64::from(len)).saturating_sub(u64::from(old.len));
                    index.total_title_len =
                        (index.total_title_len + u64::from(title_len)).saturating_sub(u64::from(old.title_len));
                }
                None => {
                    index.doc_count += 1;
                    index.total_len += u64::from(len);
                    index.total_title_len += u64::from(title_len);
                }
            }
        }
        let mut blocks = Vec::with_capacity(norms.len());
        for (&block_id, block) in norms.iter() {
            if block.is_empty() {
                wb.delete(Key::new(NORM, block_id));
            } else {
                wb.put(Key::new(NORM, block_id), block.encode());
            }
            blocks.push(block_id);
        }
        Ok((index, blocks))
    }

    /// Rótulos do lote; o de um documento removido sai junto com ele. `norms`
    /// já é o estado depois do commit.
    pub(super) fn write_labels(&self, wb: &mut WriteBatch, finals: &[usize], norms: &Norms) {
        for (doc, label) in &self.labels {
            if exists(norms, *doc) || !self.last_op.contains_key(doc) {
                wb.put(Key::new(DOC, *doc), label.as_bytes());
            }
        }
        for &i in finals {
            let op = self.ops[i];
            if op.lens.is_none() {
                wb.delete(Key::new(DOC, op.doc));
            }
        }
    }
}
