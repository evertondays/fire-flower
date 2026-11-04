//! Palavras que podem ter ficado sem documento depois de uma remoção: o
//! autocompletar e o prefixo as pulam, e a limpeza tira o fim de palavra
//! delas na Trie ([`Engine::collect_empty_words`]).

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use rocksdb::WriteBatch;
use rustc_hash::FxHashMap;

use crate::{Engine, Result};
use crate::engine::keys::{Key, META_MAYBE_EMPTY, NODE};
use crate::engine::view::View;

/// A Trie não encolhe: uma palavra cujo último documento saiu continua com o
/// fim de palavra marcado. Todo nó que recebe uma lápide entra aqui (e em
/// `meta:maybe_empty`, para sobreviver a um `open`), e as buscas conferem a
/// lista dele antes de sugeri-lo. Sem remoções o conjunto fica vazio e a
/// busca não paga nada.
///
/// A limpeza resolve os nós e guarda o número do commit dela: uma busca com
/// snapshot anterior ainda os confere (para ela, o nó pode estar vazio e
/// ainda com o fim de palavra). Os resolvidos saem do mapa na limpeza
/// seguinte; uma busca que dure uma limpeza inteira pode, no pior caso,
/// sugerir uma palavra que acabou de ficar vazia.
#[derive(Default)]
pub(crate) struct MaybeEmpty {
    /// Nó → commit a partir do qual ele deixou de ser suspeito (`u64::MAX`: ainda é).
    nodes: RwLock<FxHashMap<u64, u64>>,
    /// Quantos ainda são suspeitos.
    pending: AtomicUsize,
    /// O commit da última limpeza: buscas de snapshots anteriores ainda
    /// conferem os nós que ela resolveu.
    resolved_at: AtomicU64,
}

impl MaybeEmpty {
    /// Uma busca do snapshot `epoch` precisa conferir alguma coisa? No caso
    /// comum (sem remoções, ou já limpo) a resposta sai sem pegar trava.
    #[inline]
    pub(super) fn applies_to(&self, epoch: u64) -> bool {
        self.pending.load(AtomicOrdering::Acquire) > 0 || epoch < self.resolved_at.load(AtomicOrdering::Acquire)
    }

    /// Uma busca do snapshot `epoch` precisa conferir a lista de `node`.
    pub(super) fn is_suspect(&self, node: u64, epoch: u64) -> bool {
        let nodes = self.nodes.read().unwrap_or_else(|e| e.into_inner());
        nodes.get(&node).is_some_and(|&until| epoch < until)
    }

    /// Antes do commit que grava as lápides (como `EpochCache::mark`).
    pub(super) fn add(&self, ids: impl IntoIterator<Item = u64>) {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            nodes.insert(id, u64::MAX);
        }
        self.count_pending(&nodes);
    }

    fn count_pending(&self, nodes: &FxHashMap<u64, u64>) {
        let n = nodes.values().filter(|&&until| until == u64::MAX).count();
        self.pending.store(n, AtomicOrdering::Release);
    }

    /// Os nós ainda suspeitos (para a limpeza), em ordem. Tira os que uma
    /// limpeza anterior já resolveu.
    fn pending(&self) -> Vec<u64> {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        nodes.retain(|_, until| *until == u64::MAX);
        let mut ids: Vec<u64> = nodes.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Depois do commit `epoch` da limpeza: os nós dela deixam de ser
    /// suspeitos para buscas desse commit em diante.
    fn resolve(&self, ids: &[u64], epoch: u64) {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            if let Some(until) = nodes.get_mut(id) {
                *until = epoch;
            }
        }
        self.resolved_at.store(epoch, AtomicOrdering::Release);
        self.count_pending(&nodes);
    }
}

impl View<'_> {
    /// A palavra do nó `node` não tem documento neste snapshot (só confere os
    /// suspeitos de [`MaybeEmpty`]; o resto tem documento).
    pub(super) fn is_empty_word(&self, node: u64) -> Result<bool> {
        let maybe = &self.engine.maybe_empty;
        if !maybe.applies_to(self.epoch) || !maybe.is_suspect(node, self.epoch) {
            return Ok(false);
        }
        // O df em memória evita reler a lista (no pior caso, regravá-la) só
        // para saber se a palavra esvaziou.
        if let Some(live) = self.engine.overlays.live(node, self.epoch) {
            return Ok(live == 0);
        }
        match self.post_source(node)? {
            Some(src) => Ok(src.len()? == 0),
            None => Ok(true),
        }
    }
}

impl Engine {
    /// Limpeza das palavras que ficaram sem documento: confere a lista de
    /// cada nó que recebeu lápide desde a última limpeza e, se estiver vazia,
    /// tira o fim de palavra dele na Trie. Até lá as buscas conferem essas
    /// listas uma a uma no autocompletar e no prefixo; depois, não pagam nada.
    ///
    /// Pega a trava do escritor (espera um lote aberto). Barata depois de uma
    /// compactação, quando as listas já não têm pedaços pendentes. Devolve
    /// quantas palavras saíram da Trie.
    pub fn collect_empty_words(&self) -> Result<usize> {
        let mut batch = self.batch();
        let ids = self.maybe_empty.pending();
        if ids.is_empty() {
            batch.keep_cache();
            return Ok(0);
        }
        let mut empty = Vec::new();
        for &id in &ids {
            // Com remendo, a lista base ainda tem o documento: o df é o da base
            // mais o delta, não o cabeçalho cru.
            let is_empty = match self.view().post_source(id)? {
                Some(src) => src.len()? == 0,
                None => true,
            };
            if is_empty {
                empty.push(id);
            }
        }
        let mut wb = WriteBatch::default();
        for &id in &empty {
            let node = batch.clear_end_of_word(id)?;
            wb.put(Key::new(NODE, id), node);
        }
        wb.delete(META_MAYBE_EMPTY);
        // Como no commit: marca os nós no cache das buscas, grava, publica.
        let next = self.epoch.load(AtomicOrdering::Acquire) + 1;
        self.nodes.mark(empty.iter().copied(), next);
        self.db.write(wb)?;
        self.epoch.store(next, AtomicOrdering::Release);
        self.maybe_empty.resolve(&ids, next);
        batch.keep_cache();
        Ok(empty.len())
    }
}
