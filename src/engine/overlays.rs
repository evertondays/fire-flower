//! Nós com remendo de edição ainda não incorporado na lista base.
//!
//! O conjunto só existe para a leitura não pagar um `Get` a mais nos termos
//! que ninguém editou. Vazio (o caso comum, e o banco depois da compactação):
//! um load atômico e a busca segue como sem edição.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{Engine, Result, codec};
use crate::engine::keys::{Key, META_OVERLAYS, POS, POST, POV, PSV};

struct Slot {
    /// Commit a partir do qual o remendo já foi incorporado (`u64::MAX`: ainda vale).
    until: u64,
    /// Entradas na lista base (`post:`), sem o remendo.
    base_len: u64,
    /// Variação do df causada pelo remendo.
    df_delta: i64,
}

#[derive(Default)]
pub(crate) struct Overlays {
    nodes: RwLock<FxHashMap<u64, Slot>>,
    /// Quantos ainda valem.
    active: AtomicUsize,
    /// Commit da última incorporação. Snapshots anteriores ainda aplicam o remendo.
    resolved_at: AtomicU64,
}

impl Overlays {
    /// Algum remendo pode valer para o snapshot `epoch`? Sem edições, não.
    #[inline]
    pub(super) fn might(&self, epoch: u64) -> bool {
        self.active.load(AtomicOrdering::Acquire) > 0 || epoch < self.resolved_at.load(AtomicOrdering::Acquire)
    }

    /// O snapshot `epoch` aplica o remendo de `node`.
    pub(super) fn applies(&self, node: u64, epoch: u64) -> bool {
        if !self.might(epoch) {
            return false;
        }
        let nodes = self.nodes.read().unwrap_or_else(|e| e.into_inner());
        nodes.get(&node).is_some_and(|slot| epoch < slot.until)
    }

    /// df com o remendo, se este snapshot aplica um. A busca de autocompletar
    /// usa isto para não ler a lista inteira só para saber se ela esvaziou.
    pub(super) fn live(&self, node: u64, epoch: u64) -> Option<i64> {
        if !self.might(epoch) {
            return None;
        }
        let nodes = self.nodes.read().unwrap_or_else(|e| e.into_inner());
        let slot = nodes.get(&node)?;
        (epoch < slot.until).then_some(slot.base_len as i64 + slot.df_delta)
    }

    /// O nó já tem remendo neste processo (commit ainda não incorporado).
    pub(super) fn is_active(&self, node: u64) -> bool {
        let nodes = self.nodes.read().unwrap_or_else(|e| e.into_inner());
        nodes.get(&node).is_some_and(|slot| slot.until == u64::MAX)
    }

    /// Grava o df absoluto. Antes do commit, ou ao reabrir.
    pub(super) fn observe(&self, node: u64, base_len: u64, df_delta: i64) {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        nodes.insert(node, Slot { until: u64::MAX, base_len, df_delta });
        self.recount(&nodes);
    }

    /// Soma um lote a um nó que já está ativo.
    pub(super) fn bump(&self, node: u64, base_add: u64, delta_add: i64) {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = nodes.get_mut(&node)
            && slot.until == u64::MAX
        {
            slot.base_len += base_add;
            slot.df_delta += delta_add;
        }
    }

    /// Nós cujo remendo ainda vale (para incorporar na compactação).
    pub(super) fn active_ids(&self) -> Vec<u64> {
        let nodes = self.nodes.read().unwrap_or_else(|e| e.into_inner());
        let mut ids: Vec<u64> = nodes.iter().filter(|(_, slot)| slot.until == u64::MAX).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    }

    /// Depois do commit `epoch` que incorporou os remendos.
    pub(super) fn resolve_all(&self, epoch: u64) {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        for slot in nodes.values_mut() {
            if slot.until == u64::MAX {
                slot.until = epoch;
            }
        }
        self.resolved_at.store(epoch, AtomicOrdering::Release);
        self.recount(&nodes);
    }

    fn recount(&self, nodes: &FxHashMap<u64, Slot>) {
        let n = nodes.values().filter(|slot| slot.until == u64::MAX).count();
        self.active.store(n, AtomicOrdering::Release);
    }
}

impl Engine {
    /// Grava cada lista com o remendo aplicado (`put`, que esconde os merges
    /// antigos) e apaga o remendo. A leitura seguinte não une mais nada.
    pub(super) fn fold_overlays(&self) -> Result<()> {
        let ids = self.overlays.active_ids();
        if ids.is_empty() {
            return Ok(());
        }
        let mut wb = WriteBatch::default();
        for &id in &ids {
            fold_post(&self.db, &mut wb, id)?;
            if self.positions {
                fold_pos(&self.db, &mut wb, id)?;
            } else {
                wb.delete(Key::new(PSV, id));
            }
        }
        wb.delete(META_OVERLAYS);
        let next = self.epoch.load(AtomicOrdering::Acquire) + 1;
        self.db.write(wb)?;
        self.overlays.resolve_all(next);
        self.epoch.store(next, AtomicOrdering::Release);
        Ok(())
    }
}

fn fold_post(db: &rocksdb::DB, wb: &mut WriteBatch, id: u64) -> Result<()> {
    let base = db.get(Key::new(POST, id).as_ref())?;
    let overlay = db.get(Key::new(POV, id).as_ref())?;
    if base.is_some() || overlay.is_some() {
        let edits = match &overlay {
            Some(v) => codec::decode_post_edits(v)?.1,
            None => Vec::new(),
        };
        let posts = codec::apply_postings(base.as_deref(), &edits)?;
        if posts.is_empty() {
            wb.delete(Key::new(POST, id));
        } else {
            wb.put(Key::new(POST, id), codec::encode_postings(&posts));
        }
    }
    wb.delete(Key::new(POV, id));
    Ok(())
}

fn fold_pos(db: &rocksdb::DB, wb: &mut WriteBatch, id: u64) -> Result<()> {
    let base = db.get(Key::new(POS, id).as_ref())?;
    let overlay = db.get(Key::new(PSV, id).as_ref())?;
    if base.is_none() && overlay.is_none() {
        return Ok(());
    }
    let edits = match &overlay {
        Some(v) => codec::decode_pos_edits(v)?,
        None => Vec::new(),
    };
    let list = codec::apply_positions(base.as_deref(), &edits)?;
    if list.is_empty() {
        wb.delete(Key::new(POS, id));
    } else {
        let entries: Vec<(u64, u32, u32)> = list.iter().map(|(doc, p)| (*doc, p.len() as u32, 0)).collect();
        let flat: Vec<u32> = list.iter().flat_map(|(_, p)| p.iter().copied()).collect();
        let mut buf = Vec::new();
        codec::encode_positions_into(&mut buf, &entries, &flat);
        wb.put(Key::new(POS, id), buf);
    }
    wb.delete(Key::new(PSV, id));
    Ok(())
}
