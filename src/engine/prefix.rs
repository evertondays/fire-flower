//! Prefixos e autocompletar: as completações de um prefixo na Trie e a
//! união das listas delas.

use std::sync::Arc;

use crate::{DocId, Posting, Result};
use crate::codec::TrieNode;
use crate::engine::keys::{NODE, POST};
use crate::engine::view::{View, missing_node};

impl View<'_> {
    /// Nós das completações de `prefix` e, com stemming, o do radical do
    /// prefixo (ver [`crate::query::QueryTerm::stem`]), sem repetir.
    fn prefix_nodes(&self, prefix: &str, stem: Option<&str>) -> Result<Vec<u64>> {
        let mut nodes: Vec<u64> =
            self.words_with_prefix(prefix, self.engine.config.max_prefix_terms)?.into_iter().map(|(_, n)| n).collect();
        if let Some(node) = stem.map(|s| self.term_node(s)).transpose()?.flatten()
            && !nodes.contains(&node)
        {
            nodes.push(node);
        }
        Ok(nodes)
    }

    /// Listas das completações de `prefix` unidas por documento, com o tf
    /// somado: o prefixo como se fosse um termo só.
    pub(super) fn prefix_postings(&self, prefix: &str, stem: Option<&str>) -> Result<Vec<Posting>> {
        self.union_postings(self.prefix_nodes(prefix, stem)?.into_iter())
    }

    /// Listas de vários nós unidas por documento, com o tf somado.
    fn union_postings(&self, nodes: impl Iterator<Item = u64>) -> Result<Vec<Posting>> {
        let mut all: Vec<Posting> = Vec::new();
        for value in self.get_many(POST, nodes)?.into_iter().flatten() {
            all.extend(self.postings_of(&value)?);
        }
        // `all` é a concatenação de listas já ordenadas: a ordenação estável
        // detecta os trechos ordenados e só os intercala (o `sort_unstable`
        // ordenava tudo de novo).
        all.sort_by_key(|p| p.doc);
        let mut merged: Vec<Posting> = Vec::with_capacity(all.len());
        for p in all {
            match merged.last_mut() {
                Some(last) if last.doc == p.doc => {
                    last.tf = last.tf.saturating_add(p.tf);
                    last.title_tf = last.title_tf.saturating_add(p.title_tf);
                }
                _ => merged.push(p),
            }
        }
        Ok(merged)
    }

    pub(super) fn prefix_docs(&self, prefix: &str, stem: Option<&str>) -> Result<Vec<DocId>> {
        self.union_docs(self.prefix_nodes(prefix, stem)?.into_iter())
    }

    /// Documentos de vários nós, unidos.
    fn union_docs(&self, nodes: impl Iterator<Item = u64>) -> Result<Vec<DocId>> {
        let mut all = Vec::new();
        for value in self.get_many(POST, nodes)?.into_iter().flatten() {
            all.extend(self.docs_of(&value)?);
        }
        all.sort(); // trechos já ordenados: ver `prefix_postings`
        all.dedup();
        Ok(all)
    }

    /// Busca em largura a partir do nó do prefixo, até `limit` palavras, das
    /// mais curtas para as mais longas e, no mesmo comprimento, em ordem
    /// alfabética. Retorna (palavra, id do nó).
    ///
    /// Uma palavra cujos documentos foram todos substituídos ou removidos
    /// continua aqui até a compactação (que a tira da Trie): conferir as
    /// listas de cada palavra custaria uma leitura por palavra.
    ///
    /// Percorre nível a nível (a mesma ordem de uma fila) e lê os nós que não
    /// estão no cache em lotes de até `WORDS_CHUNK` com um `MultiGet`: um
    /// prefixo curto visita centenas de nós, e um `Get` por nó custava metade
    /// do tempo de `p-2*` na Wikipédia inteira.
    pub(super) fn words_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<(String, u64)>> {
        const WORDS_CHUNK: usize = 64;
        let mut out = Vec::new();
        let Some((start, start_node)) = self.find_node(prefix)? else { return Ok(out) };
        let mut depth = prefix.chars().count();
        let mut level: Vec<(String, u64)> = vec![(prefix.to_string(), start)];
        let mut first = Some(start_node);
        while !level.is_empty() && out.len() < limit {
            let mut next = Vec::new();
            let mut at = 0;
            'level: while at < level.len() {
                // Lote do tamanho do que ainda falta (o autocompletar pede 10
                // palavras: ler 64 nós de uma vez desperdiçaria leituras).
                let size = (2 * (limit - out.len())).clamp(4, WORDS_CHUNK);
                let chunk = &level[at..(at + size).min(level.len())];
                at += chunk.len();
                let nodes = match first.take() {
                    Some(node) => vec![node],
                    None => self.nodes_many(chunk.iter().map(|(_, id)| *id), depth)?,
                };
                for ((word, id), node) in chunk.iter().zip(nodes) {
                    if out.len() >= limit {
                        break 'level;
                    }
                    for (&ch, &child) in &node.children {
                        let mut w = String::with_capacity(word.len() + ch.len_utf8());
                        w.push_str(word);
                        w.push(ch);
                        next.push((w, child));
                    }
                    if node.is_end_of_word {
                        out.push((word.clone(), *id));
                    }
                }
            }
            level = next;
            depth += 1;
        }
        Ok(out)
    }

    /// Vários nós à mesma `depth`, na ordem de `ids`: os rasos do cache, o
    /// resto num `MultiGet` só.
    fn nodes_many(&self, ids: impl Iterator<Item = u64>, depth: usize) -> Result<Vec<Arc<TrieNode>>> {
        let cached = self.engine.config.reader_cache_depth.is_some_and(|max| depth <= max);
        let mut out: Vec<Option<Arc<TrieNode>>> = Vec::new();
        let mut missing: Vec<(usize, u64)> = Vec::new();
        for id in ids {
            match cached.then(|| self.engine.nodes.get(id, self.epoch)).flatten() {
                Some(node) => out.push(Some(node)),
                None => {
                    missing.push((out.len(), id));
                    out.push(None);
                }
            }
        }
        let values = self.get_many(NODE, missing.iter().map(|&(_, id)| id))?;
        for ((slot, id), value) in missing.into_iter().zip(values) {
            let node = Arc::new(TrieNode::decode(&value.ok_or_else(|| missing_node(id))?)?);
            if cached {
                self.engine.nodes.offer(id, self.epoch, &node);
            }
            out[slot] = Some(node);
        }
        Ok(out.into_iter().map(|n| n.expect("todo nó foi preenchido")).collect())
    }
}
