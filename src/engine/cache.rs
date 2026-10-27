//! Cache de valores decodificados compartilhado pelas buscas, consistente
//! com os snapshots ([`EpochCache`]).

use std::collections::hash_map::Entry;
use std::sync::Arc;
use rustc_hash::FxHashMap;
use std::sync::RwLock;

/// Valores já decodificados, compartilhados pelas buscas: blocos de `norm:`
/// (o ranking precisa do tamanho de cada documento) e os nós rasos da Trie
/// (toda busca passa pela raiz). Sem cache, seria um `Get` e uma decodificação
/// por valor em toda busca.
///
/// O desafio é o snapshot: uma busca não pode usar um valor gravado depois
/// dela. Por isso cada commit tem um número (`Engine::epoch`) e cada entrada
/// guarda a partir de qual número o conteúdo em cache vale:
///
/// 1. antes de gravar, o escritor marca as chaves que vai mudar com o número
///    do commit (`epoch + 1`) e sem conteúdo ([`EpochCache::mark`]);
/// 2. grava o lote;
/// 3. publica o novo número.
///
/// A busca lê o número *antes* de criar o snapshot. Uma entrada com número
/// maior que o dela foi (ou está sendo) mudada depois: ela lê do próprio
/// snapshot e não guarda nada. Uma chave sem entrada nunca mudou desde o
/// `open`. Por isso o escritor marca **toda** chave que muda e que o cache
/// poderia guardar, esteja ela no cache ou não.
///
/// O cache não descarta entradas. Os blocos de `norm:` ocupam no máximo o
/// tamanho de todos os documentos (~6 bytes cada; 1 milhão ≈ 6 MB); os nós,
/// só os rasos.
pub(crate) struct EpochCache<T> {
    pub(crate) slots: RwLock<FxHashMap<u64, Slot<T>>>,
}

pub(crate) struct Slot<T> {
    /// Commit a partir do qual `value` vale.
    pub(crate) version: u64,
    pub(crate) value: Option<Arc<T>>,
}

impl<T> Default for EpochCache<T> {
    fn default() -> Self {
        Self { slots: RwLock::new(FxHashMap::default()) }
    }
}

impl<T> EpochCache<T> {
    pub(crate) fn get(&self, key: u64, epoch: u64) -> Option<Arc<T>> {
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        match slots.get(&key) {
            Some(slot) if slot.version <= epoch => slot.value.clone(),
            _ => None,
        }
    }

    /// Uma busca leu `key` do snapshot `epoch`; guarda se ainda for o atual.
    pub(crate) fn offer(&self, key: u64, epoch: u64, data: &Arc<T>) {
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        match slots.entry(key) {
            Entry::Vacant(e) => {
                e.insert(Slot { version: 0, value: Some(Arc::clone(data)) });
            }
            Entry::Occupied(mut e) => {
                let slot = e.get_mut();
                if slot.value.is_none() && slot.version <= epoch {
                    slot.value = Some(Arc::clone(data));
                }
            }
        }
    }

    /// Passo 1 do commit: `keys` vão mudar no commit `version`.
    pub(crate) fn mark(&self, keys: impl IntoIterator<Item = u64>, version: u64) {
        let mut keys = keys.into_iter().peekable();
        if keys.peek().is_none() {
            return;
        }
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            slots.insert(key, Slot { version, value: None });
        }
    }
}
