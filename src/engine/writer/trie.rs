//! A Trie do escritor: inserção dos termos e o cache de nós entre lotes.

use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;

use crate::{IndexBatch, Result};
use crate::codec::TrieNode;
use crate::engine::keys::ROOT_ID;
use crate::engine::view::{load_node, missing_node};

/// Caminho na Trie do último termo inserido no commit: `ids[d]` é o nó na
/// profundidade `d` (`ids[0]` é a raiz) e `chars` são os caracteres até ele.
#[derive(Default)]
pub(crate) struct TriePath {
    chars: Vec<char>,
    ids: Vec<u64>,
}

pub(crate) struct CachedNode {
    pub(crate) node: TrieNode,
    pub(crate) dirty: bool,
    /// Lote que usou o nó por último.
    pub(crate) used: u64,
    /// Distância até a raiz. Os nós rasos são os mais compartilhados.
    pub(crate) depth: u8,
}

impl IndexBatch<'_> {
    /// Cria (se preciso) o caminho de `term` na Trie e devolve o nó terminal.
    ///
    /// Os termos do lote chegam em ordem alfabética, e vizinhos compartilham o
    /// começo ("casa", "casaco", "casamento"): `path` guarda o caminho do
    /// termo anterior, e a descida recomeça do fim do prefixo comum em vez da
    /// raiz. Os nós desse caminho já estão no mapa do lote (o descarte só
    /// acontece depois do commit).
    /// Devolve o nó do termo e se ele virou palavra agora (não estava no índice).
    pub(super) fn insert_term(&mut self, term: &str, path: &mut TriePath) -> Result<(u64, bool)> {
        if path.ids.is_empty() {
            path.ids.push(ROOT_ID);
        }
        let common = path.chars.iter().zip(term.chars()).take_while(|(a, b)| *a == b).count();
        path.chars.truncate(common);
        path.ids.truncate(common + 1);
        let mut id = path.ids[common];
        // Termos têm no máximo MAX_TOKEN_CHARS (64) caracteres.
        let mut depth = u8::try_from(common).unwrap_or(u8::MAX);
        for ch in term.chars().skip(common) {
            let next_free = self.next_node_id;
            let generation = self.generation;
            let parent = self.node_mut(id, depth)?;
            id = match parent.node.children.get(&ch) {
                Some(&child) => child,
                None => {
                    parent.node.children.insert(ch, next_free);
                    if !parent.dirty {
                        parent.dirty = true;
                        self.dirty.push(id);
                    }
                    self.next_node_id += 1;
                    self.dirty.push(next_free);
                    let node = CachedNode { node: TrieNode::default(), dirty: true, used: generation, depth: depth + 1 };
                    self.nodes.insert(next_free, node);
                    next_free
                }
            };
            depth = depth.saturating_add(1);
            path.chars.push(ch);
            path.ids.push(id);
        }
        let leaf = self.node_mut(id, depth)?;
        let new_word = !leaf.node.is_end_of_word;
        if new_word {
            leaf.node.is_end_of_word = true;
            if !leaf.dirty {
                leaf.dirty = true;
                self.dirty.push(id);
            }
        }
        Ok((id, new_word))
    }

    /// Nó do cache, lendo do banco na primeira vez.
    pub(super) fn node_mut(&mut self, id: u64, depth: u8) -> Result<&mut CachedNode> {
        let generation = self.generation;
        match self.nodes.entry(id) {
            Entry::Occupied(e) => {
                let cached = e.into_mut();
                cached.used = generation;
                // A limpeza de palavras põe no cache, como fundo, um nó que
                // ainda não estava nele: a profundidade de verdade vem aqui, ou
                // o commit deixaria de marcá-lo no cache das buscas.
                cached.depth = cached.depth.min(depth);
                Ok(cached)
            }
            Entry::Vacant(e) => {
                let (node, dirty) = match load_node(&*self.engine.db, id)? {
                    Some(node) => (node, false),
                    // Banco novo: a raiz nasce no primeiro commit.
                    None if id == ROOT_ID => (TrieNode::default(), true),
                    None => return Err(missing_node(id)),
                };
                if dirty {
                    self.dirty.push(id);
                }
                Ok(e.insert(CachedNode { node, dirty, used: generation, depth }))
            }
        }
    }
}

/// Mantém o cache de nós dentro de `limit`. Quando passa, descarta até 80%
/// do limite (para não descartar a cada lote): ficam os nós usados mais
/// recentemente e, no lote em que o corte cai, os mais rasos, que são os mais
/// compartilhados.
///
/// Antes ficavam só os nós usados pelo último lote. Na Wikipédia isso jogava
/// fora ~75% do cache a cada vez que ele enchia (1 milhão → 240 mil nós), e as
/// leituras do escritor subiam de ~7 para ~18 por artigo logo depois.
pub(crate) fn evict(nodes: &mut FxHashMap<u64, CachedNode>, generation: u64, limit: usize) {
    if nodes.len() <= limit {
        return;
    }
    let target = limit - limit / 5;
    // Nós por (idade em lotes, profundidade). Idades acima de MAX_AGE contam juntas.
    const MAX_AGE: usize = 255;
    let age = |c: &CachedNode| (generation.saturating_sub(c.used) as usize).min(MAX_AGE);
    let mut per_age = vec![[0usize; 256]; MAX_AGE + 1];
    for c in nodes.values() {
        per_age[age(c)][c.depth as usize] += 1;
    }
    // Maior idade que cabe inteira no alvo; da seguinte, os mais rasos que couberem.
    let mut kept = 0usize;
    let mut cut_age = None;
    for (a, depths) in per_age.iter().enumerate() {
        let n: usize = depths.iter().sum();
        if kept + n > target {
            cut_age = Some(a);
            break;
        }
        kept += n;
    }
    let Some(cut_age) = cut_age else { return };
    let mut max_depth = None;
    for (d, &n) in per_age[cut_age].iter().enumerate() {
        if kept + n > target {
            break;
        }
        kept += n;
        max_depth = Some(d);
    }
    nodes.retain(|_, c| {
        let a = age(c);
        a < cut_age || (a == cut_age && max_depth.is_some_and(|d| c.depth as usize <= d))
    });
}
