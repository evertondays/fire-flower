//! IDs internos de versão e documentos mortos: editar só acrescenta, e a
//! busca filtra as versões que saíram.
//!
//! As listas e os blocos de `norm:` usam o **ID interno** da versão do
//! documento. Na primeira versão ele é o próprio ID do usuário (a carga não
//! muda nada). Uma substituição, ou uma readição depois de remover, ganha um
//! ID da faixa alta: `FIRST_VERSION + (versão << DOC_BITS) + documento`. As
//! entradas novas caem depois de todos os documentos sem edição. Na faixa
//! alta, as versões 0 (a primeira edição de cada documento) vêm antes das 1, e
//! assim por diante; dentro de uma versão, em ordem de documento, com deltas
//! tão pequenos quanto os da carga. Traduzir de volta é tirar a versão, e o
//! resultado em ordem de ID é a junção de poucos trechos já ordenados (um por
//! versão presente). Até a compactação, uma edição no meio de um trecho vira
//! um remendo do merge só nos blocos dela.
//!
//! A versão anterior vira **morta**: some de `norm:` no mesmo commit e
//! continua nas listas até a compactação, que a tira (ver `purge`). Um ID
//! interno nunca volta (a versão só cresce), então tirar um morto vale em
//! qualquer merge.
//!
//! - `ver:<doc>` → ID interno da última versão, para os documentos que já
//!   mudaram (o ID dela, mesmo morta, diz qual é a próxima versão).
//! - `meta:dead` → os mortos que a compactação ainda não tirou das listas.

use std::sync::{Arc, OnceLock, RwLock};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use rocksdb::DB;
use rustc_hash::FxHashMap;

use crate::{DocId, Error, Result, codec};
use crate::codec::DeadBits;
use crate::engine::keys::{META_DEAD, VER};

/// Primeiro ID interno da faixa alta.
pub(super) const FIRST_VERSION: u64 = 1 << 63;

/// Bits do documento no ID interno da faixa alta; a versão vem acima deles
/// (até 2^23 − 1 = 8,4 milhões de versões por documento).
const DOC_BITS: u32 = 40;

/// O maior ID de documento.
pub(super) const MAX_DOC: DocId = (1 << DOC_BITS) - 1;

/// A maior versão da faixa alta.
const MAX_VERSION: u64 = (1 << (63 - DOC_BITS)) - 1;

/// O ID interno da primeira versão da faixa alta de `doc`.
pub(super) fn first_high(doc: DocId) -> u64 {
    debug_assert!(doc <= MAX_DOC);
    FIRST_VERSION + doc
}

/// O ID interno da versão seguinte a `id` (da faixa alta), se ainda houver.
pub(super) fn next_high(id: u64) -> Option<u64> {
    ((id - FIRST_VERSION) >> DOC_BITS < MAX_VERSION).then_some(id + (1 << DOC_BITS))
}

/// O ID do usuário de um ID interno.
#[inline]
pub(super) fn user_of(id: u64) -> DocId {
    if id < FIRST_VERSION { id } else { (id - FIRST_VERSION) & MAX_DOC }
}

/// IDs internos em ordem → IDs do usuário em ordem. Traduz a faixa alta no
/// lugar; ela vira um trecho em ordem por versão presente. Sem versões da
/// faixa alta (o caso comum), não mexe em nada.
pub(super) fn users_sorted(mut ids: Vec<u64>) -> Vec<DocId> {
    let low = ids.partition_point(|&id| id < FIRST_VERSION);
    if low == ids.len() {
        return ids;
    }
    for id in &mut ids[low..] {
        *id = user_of(*id);
    }
    let high = ids.len() - low;
    if high * 16 >= low {
        // A ordenação estável acha os trechos já ordenados e só os junta.
        ids.sort();
        return ids;
    }
    // Poucos traduzidos perto da lista: cada um acha o lugar por busca
    // binária, e os trechos entre eles são copiados de uma vez, de trás para
    // frente.
    let mut translated: Vec<DocId> = ids[low..].to_vec();
    translated.sort_unstable();
    let mut end = low;
    let mut write = ids.len();
    for &doc in translated.iter().rev() {
        let at = ids[..end].partition_point(|&d| d < doc);
        write -= end - at;
        ids.copy_within(at..end, write);
        write -= 1;
        ids[write] = doc;
        end = at;
    }
    ids
}

/// Os mortos que as buscas filtram. Cada morto guarda o commit em que morreu:
/// uma busca com snapshot anterior ainda vê aquela versão viva (e a nova
/// ainda não existe para ela).
#[derive(Default)]
pub(super) struct Deaths {
    /// Há mortos: sem eles, a busca não pega a trava nem filtra nada.
    any: AtomicBool,
    set: RwLock<Arc<DeadSet>>,
}

/// Um conjunto imutável de mortos, trocado inteiro a cada commit que mata.
#[derive(Default)]
pub(super) struct DeadSet {
    /// IDs internos, em ordem.
    pub(super) ids: Vec<u64>,
    /// Commit em que cada um morreu (0 = antes deste `open`).
    epochs: Vec<u64>,
    /// O maior de `epochs`.
    newest: u64,
    /// `ids` num mapa de bits, montado pela primeira busca que precisar.
    bits: OnceLock<DeadBits>,
}

impl DeadSet {
    /// Todos os mortos, para a busca pular ao decodificar.
    pub(super) fn bits(&self) -> &DeadBits {
        self.bits.get_or_init(|| DeadBits::new(&self.ids))
    }

    /// Os mortos que um snapshot do commit `epoch` vê.
    pub(super) fn visible(&self, epoch: u64) -> DeadBits {
        let ids: Vec<u64> =
            self.ids.iter().zip(&self.epochs).filter(|(_, e)| **e <= epoch).map(|(id, _)| *id).collect();
        DeadBits::new(&ids)
    }

    /// Todos morreram até o commit `epoch`.
    pub(super) fn all_by(&self, epoch: u64) -> bool {
        self.newest <= epoch
    }
}

impl Deaths {
    /// Os mortos de agora, se houver. A busca chama logo depois de criar o
    /// snapshot: um commit que mata publica os mortos antes de gravar.
    pub(super) fn current(&self) -> Option<Arc<DeadSet>> {
        if !self.any.load(AtomicOrdering::SeqCst) {
            return None;
        }
        let set = self.set.read().unwrap_or_else(|e| e.into_inner()).clone();
        (!set.ids.is_empty()).then_some(set)
    }

    /// Antes do commit `epoch` que grava as mortes `ids` (em ordem).
    pub(super) fn add(&self, ids: &[u64], epoch: u64) {
        if ids.is_empty() {
            return;
        }
        let mut set = self.set.write().unwrap_or_else(|e| e.into_inner());
        let (old, mut next) = (&**set, DeadSet::default());
        next.ids.reserve(old.ids.len() + ids.len());
        next.epochs.reserve(old.ids.len() + ids.len());
        let (mut i, mut j) = (0, 0);
        while i < old.ids.len() || j < ids.len() {
            if j == ids.len() || (i < old.ids.len() && old.ids[i] < ids[j]) {
                next.ids.push(old.ids[i]);
                next.epochs.push(old.epochs[i]);
                i += 1;
            } else {
                debug_assert!(i == old.ids.len() || old.ids[i] != ids[j], "um ID interno morre uma vez só");
                next.ids.push(ids[j]);
                next.epochs.push(epoch);
                j += 1;
            }
        }
        next.newest = old.newest.max(epoch);
        *set = Arc::new(next);
        self.any.store(true, AtomicOrdering::SeqCst);
    }

    /// O commit `epoch` não foi gravado: as mortes dele não valem.
    pub(super) fn rollback(&self, epoch: u64) {
        self.retain(|_, e| e != epoch);
    }

    /// Depois da compactação que tirou `purged` (em ordem) das listas.
    pub(super) fn remove(&self, purged: &[u64]) {
        self.retain(|id, _| purged.binary_search(&id).is_err());
    }

    fn retain(&self, keep: impl Fn(u64, u64) -> bool) {
        let mut set = self.set.write().unwrap_or_else(|e| e.into_inner());
        let mut next = DeadSet::default();
        for (&id, &epoch) in set.ids.iter().zip(&set.epochs) {
            if keep(id, epoch) {
                next.ids.push(id);
                next.epochs.push(epoch);
                next.newest = next.newest.max(epoch);
            }
        }
        self.any.store(!next.ids.is_empty(), AtomicOrdering::SeqCst);
        *set = Arc::new(next);
    }
}

/// O que o `open` lê das versões: o mapa do escritor e os mortos.
pub(super) struct Loaded {
    /// Documento → ID interno da última versão (só os que já mudaram).
    pub(super) latest: FxHashMap<DocId, u64>,
    /// Mortos ainda nas listas, em ordem.
    pub(super) dead: Vec<u64>,
}

/// Lê `ver:` e `meta:dead`.
pub(super) fn load(db: &DB, deaths: &Deaths) -> Result<Loaded> {
    let mut latest = FxHashMap::default();
    let mut opts = rocksdb::ReadOptions::default();
    opts.set_iterate_upper_bound(b"ver;".to_vec()); // ';' vem logo depois de ':'
    for item in db.iterator_opt(rocksdb::IteratorMode::From(VER, rocksdb::Direction::Forward), opts) {
        let (key, value) = item?;
        let doc: DocId = key
            .strip_prefix(VER)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| d.parse().ok())
            .ok_or_else(|| Error::Corrupt(format!("chave de versão inválida: {}", String::from_utf8_lossy(&key))))?;
        let id = codec::get_varint(&value)?;
        if user_of(id) != doc {
            return Err(Error::Corrupt(format!("ver:{doc} aponta para a versão {id}, de outro documento")));
        }
        latest.insert(doc, id);
    }
    let dead = match db.get_pinned(META_DEAD)? {
        Some(v) => codec::decode_node_set(&v)?,
        None => Vec::new(),
    };
    deaths.add(&dead, 0);
    Ok(Loaded { latest, dead })
}
