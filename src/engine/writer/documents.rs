//! O que o commit grava por documento: a versão de cada um (`ver:` e os
//! mortos, ver `versions`), os tamanhos (`norm:`) e as estatísticas, e os
//! rótulos.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use rocksdb::WriteBatch;

use crate::{DocId, Error, IndexBatch, IndexStats, Result, codec};
use crate::codec::{DocLens, NORM_BLOCK_DOCS, NormBlock};
use crate::engine::keys::{DOC, Key, META_DEAD, NORM, VER};
use crate::engine::meta::varint;
use crate::engine::versions::{FIRST_VERSION, first_high, next_high};
use crate::engine::view::Source;
use crate::engine::writer::CommitStats;

/// Os blocos de `norm:` que o lote toca, como estavam no banco.
pub(super) type Norms = BTreeMap<u64, NormBlock>;

/// A versão de cada documento do lote, decidida antes da Trie.
pub(super) struct Versions {
    /// ID interno da versão nova de cada operação (índice em `ops`), para as
    /// finais que adicionam.
    pub(super) internal: Vec<u64>,
    /// A versão viva que cada operação final substitui ou remove.
    old: Vec<Option<u64>>,
    /// Alguma versão nova tem ID diferente do documento (as entradas do lote
    /// precisam do ID interno).
    pub(super) remapped: bool,
    /// Versões que morrem neste commit, em ordem.
    pub(super) kills: Vec<u64>,
    /// Documentos cuja última versão mudou (`ver:`): (documento, ID interno).
    pub(super) changed: Vec<(DocId, u64)>,
    /// Versões novas da faixa alta.
    new_versions: usize,
}

impl IndexBatch<'_> {
    /// O bloco de `norm:` de `id`, lido do banco na primeira vez.
    fn norm_block<'n>(&self, norms: &'n mut Norms, id: u64) -> Result<&'n mut NormBlock> {
        Ok(match norms.entry(id / NORM_BLOCK_DOCS) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let block = match self.engine.db.get_bytes(Key::new(NORM, *e.key()).as_ref())? {
                    Some(v) => NormBlock::decode(&v)?,
                    None => NormBlock::default(),
                };
                e.insert(block)
            }
        })
    }

    /// Decide a versão de cada operação final (`finals`, em ordem):
    ///
    /// - um documento que nunca existiu fica com o próprio ID (a carga);
    /// - um que existe, ou que já existiu (`ver:`), ganha a versão seguinte
    ///   na faixa alta, e a versão viva dele, se houver, morre;
    /// - remover mata a versão viva.
    ///
    /// Quem existe vem dos blocos de `norm:` (lidos aqui, guardados em
    /// `norms`): numa carga nova com IDs crescentes, um ou dois blocos por lote.
    pub(super) fn plan_versions(&self, finals: &[usize], norms: &mut Norms) -> Result<Versions> {
        let mut plan = Versions {
            internal: vec![u64::MAX; self.ops.len()],
            old: vec![None; self.ops.len()],
            remapped: false,
            kills: Vec::new(),
            changed: Vec::new(),
            new_versions: 0,
        };
        for &i in finals {
            let op = self.ops[i];
            let latest = self.state.latest.get(&op.doc).copied();
            let current = latest.unwrap_or(op.doc);
            let alive = self.norm_block(norms, current)?.get_lens((current % NORM_BLOCK_DOCS) as u16).is_some();
            if alive {
                plan.old[i] = Some(current);
                plan.kills.push(current);
            }
            if op.lens.is_none() {
                if alive && latest.is_none() {
                    // A primeira versão morreu: a próxima ganha um ID novo.
                    plan.changed.push((op.doc, current));
                }
                continue;
            }
            if alive || latest.is_some() {
                let id = match latest {
                    Some(last) if last >= FIRST_VERSION => next_high(last).ok_or_else(|| {
                        Error::Config(format!("documento {} editado vezes demais (8,4 milhões de versões)", op.doc))
                    })?,
                    _ => first_high(op.doc),
                };
                plan.internal[i] = id;
                plan.remapped = true;
                plan.changed.push((op.doc, id));
                plan.new_versions += 1;
                self.norm_block(norms, id)?;
            } else {
                plan.internal[i] = op.doc;
            }
        }
        plan.kills.sort_unstable();
        Ok(plan)
    }

    /// Grava `ver:` e os mortos.
    pub(super) fn write_versions(&self, wb: &mut WriteBatch, plan: &Versions, stats: &mut CommitStats) {
        for &(doc, id) in &plan.changed {
            wb.put(Key::new(VER, doc), varint(id));
        }
        if !plan.kills.is_empty() {
            wb.merge(META_DEAD, codec::encode_node_set(&plan.kills));
        }
        stats.versions = plan.new_versions;
        stats.killed = plan.kills.len();
    }

    /// Atualiza os blocos de `norm:` tocados pelo lote e as estatísticas: a
    /// versão que morre sai do bloco, a nova entra. Um documento substituído
    /// troca de tamanho e não conta de novo; um removido desconta. Devolve as
    /// estatísticas novas e os blocos gravados.
    pub(super) fn write_lengths(
        &self,
        wb: &mut WriteBatch,
        finals: &[usize],
        plan: &Versions,
        norms: &mut Norms,
        stats: &mut CommitStats,
    ) -> Result<(IndexStats, Vec<u64>)> {
        let mut index = self.state.stats;
        for &i in finals {
            let op = self.ops[i];
            let old = match plan.old[i] {
                Some(id) => self.norm_block(norms, id)?.remove((id % NORM_BLOCK_DOCS) as u16),
                None => None,
            };
            let Some((len, title_len, title_span)) = op.lens else {
                if let Some(old) = old {
                    stats.deleted += 1;
                    index.doc_count -= 1;
                    index.total_len = index.total_len.saturating_sub(u64::from(old.len));
                    index.total_title_len = index.total_title_len.saturating_sub(u64::from(old.title_len));
                }
                continue;
            };
            // Título com mais de 65.535 tokens: a contagem para no máximo.
            let small = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            let (title_len, title_span) = (small(title_len), small(title_span));
            let id = plan.internal[i];
            self.norm_block(norms, id)?.set((id % NORM_BLOCK_DOCS) as u16, DocLens { len, title_len, title_span });
            match old {
                Some(old) => {
                    stats.replaced += 1;
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

    /// Rótulos do lote; o de um documento cuja última operação é remover sai
    /// junto com ele.
    pub(super) fn write_labels(&self, wb: &mut WriteBatch, finals: &[usize]) {
        for (doc, label) in &self.labels {
            let removed = self.last_op.get(doc).is_some_and(|&op| self.ops[op as usize].lens.is_none());
            if !removed {
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
