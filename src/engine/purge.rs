//! Limpeza dos mortos nas listas: o merge operator e o filtro de compactação
//! tiram as versões mortas enquanto [`Engine::compact`](crate::Engine::compact)
//! roda.
//!
//! Tirar um morto de uma lista muda o que um snapshot mais antigo que a morte
//! veria (para ele, a versão ainda está viva). Por isso a compactação só
//! limpa os mortos que todo snapshot aberto já vê como mortos (pelo número
//! de sequência do RocksDB), e os snapshots abertos depois são mais novos.

use std::ffi::CStr;
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use rocksdb::MergeOperands;
use rocksdb::compaction_filter::{CompactionFilter, Decision};
use rocksdb::compaction_filter_factory::{CompactionFilterContext, CompactionFilterFactory};

use crate::codec;
use crate::codec::ListKind;
use crate::engine::keys::{META_DEAD, POS, POST, STEM};

/// Os mortos sendo tirados das listas agora (vazio fora da compactação).
#[derive(Default)]
pub(super) struct Purge {
    active: AtomicBool,
    ids: RwLock<Arc<Vec<u64>>>,
    /// Nós cujo `post:` ficou vazio numa limpeza: palavras que podem ter
    /// ficado sem documento.
    emptied: Mutex<Vec<u64>>,
}

impl Purge {
    /// Os IDs a tirar, se uma limpeza está em andamento.
    fn current(&self) -> Option<Arc<Vec<u64>>> {
        if !self.active.load(AtomicOrdering::Acquire) {
            return None;
        }
        Some(self.ids.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Começa a tirar `ids` (em ordem) em todo merge e compactação.
    pub(super) fn start(&self, ids: Vec<u64>) {
        *self.ids.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ids);
        self.active.store(true, AtomicOrdering::Release);
    }

    /// Os nós que a limpeza esvaziou até agora, em ordem (e os esquece).
    pub(super) fn emptied(&self) -> Vec<u64> {
        let mut nodes = std::mem::take(&mut *self.emptied.lock().unwrap_or_else(|e| e.into_inner()));
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    /// Para a limpeza: os merges e compactações seguintes não tiram nada.
    pub(super) fn finish(&self) {
        self.active.store(false, AtomicOrdering::Release);
        *self.ids.write().unwrap_or_else(|e| e.into_inner()) = Arc::default();
        self.emptied.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn note_emptied(&self, key: &[u8]) {
        let node = key.strip_prefix(POST).and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| d.parse().ok());
        if let Some(node) = node {
            self.emptied.lock().unwrap_or_else(|e| e.into_inner()).push(node);
        }
    }
}

/// O tipo de lista da chave, se for uma.
fn list_kind(key: &[u8]) -> Option<ListKind> {
    if key.starts_with(POST) {
        Some(ListKind::Postings)
    } else if key.starts_with(POS) {
        Some(ListKind::Positions)
    } else {
        None
    }
}

/// Merge operator de `post:` e `pos:` (ver `codec::merge_lists`), de `stem:`
/// e de `meta:dead` (conjuntos de nós). O mesmo para merges completos e
/// parciais: tirar um morto vale em qualquer um. Devolver `None` (dado
/// corrompido) faz a leitura falhar em vez de mentir.
pub(super) fn merge(purge: &Purge, key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    let inputs = existing.into_iter().chain(operands.iter());
    if let Some(kind) = list_kind(key) {
        let Some(ids) = purge.current() else { return codec::merge_lists(inputs, kind, &[]) };
        let out = codec::merge_lists(inputs, kind, &ids)?;
        // Num merge parcial pode haver um valor mais antigo: é só um candidato,
        // conferido depois da compactação.
        if kind == ListKind::Postings && matches!(codec::list_len(&out), Ok(0)) {
            purge.note_emptied(key);
        }
        Some(out)
    } else if key.starts_with(STEM) || key == META_DEAD {
        codec::merge_node_sets(inputs)
    } else {
        None
    }
}

/// Cria um filtro por compactação, com os mortos do momento.
pub(super) struct FilterFactory {
    pub(super) purge: Arc<Purge>,
}

const FILTER_NAME: &CStr = c"fire_flower_purge";

impl CompactionFilterFactory for FilterFactory {
    type Filter = Filter;

    fn create(&mut self, _context: CompactionFilterContext) -> Filter {
        Filter { ids: self.purge.current(), purge: Arc::clone(&self.purge), buf: Vec::new() }
    }

    fn name(&self) -> &CStr {
        FILTER_NAME
    }
}

/// Tira os mortos dos valores já unidos (os que não passam pelo merge).
pub(super) struct Filter {
    ids: Option<Arc<Vec<u64>>>,
    purge: Arc<Purge>,
    /// A lista nova do último valor trocado.
    buf: Vec<u8>,
}

impl CompactionFilter for Filter {
    fn filter(&mut self, _level: u32, key: &[u8], value: &[u8]) -> Decision {
        let Some(ids) = &self.ids else { return Decision::Keep };
        let Some(kind) = list_kind(key) else { return Decision::Keep };
        // Dado corrompido fica como está: a leitura acusa.
        let Ok(Some(list)) = codec::purge_list(value, kind, ids) else { return Decision::Keep };
        if matches!(codec::list_len(&list), Ok(0)) {
            if kind == ListKind::Postings {
                self.purge.note_emptied(key);
            }
            return Decision::Remove;
        }
        self.buf = list;
        // SAFETY: o crate pede `&'static`, mas a API C do RocksDB copia o valor
        // novo antes de chamar o filtro de novo (`new_value->assign` em
        // `c.cc`), e o filtro é de uma compactação só (`create`): `buf` vive
        // até lá.
        Decision::Change(unsafe { std::mem::transmute::<&[u8], &'static [u8]>(self.buf.as_slice()) })
    }

    fn name(&self) -> &CStr {
        FILTER_NAME
    }
}
