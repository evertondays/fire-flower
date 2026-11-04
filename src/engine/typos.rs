//! Correção de erros de digitação ([`Config::typos`](crate::Config::typos)):
//! as palavras do índice a um erro de uma palavra da consulta.

use crate::{Correction, Result, TypoMode, codec};
use crate::engine::keys::{POST, ROOT_ID};
use crate::engine::view::View;

/// Erros aceitos por palavra. Com dois, o percurso na Trie da Wikipédia lê
/// ~2.300 nós por palavra (7,5 ms no p50) em vez de ~80 (0,5 ms): fase 0 do
/// `docs/plano-correcao-digitacao.md`.
const MAX_EDITS: u8 = 1;

/// Uma correção escolhida: a palavra e os bytes da lista dela.
pub(super) struct Fix<'s> {
    pub(super) word: String,
    pub(super) df: usize,
    pub(super) value: rocksdb::DBPinnableSlice<'s>,
}

/// O que um termo exato da consulta (sem flexões) vira.
pub(super) enum TermLists<'s> {
    /// A lista do termo, se ele existe (o caso comum).
    Exact(Option<rocksdb::DBPinnableSlice<'s>>),
    /// O termo é fraco (ausente, ou df < `typo_min_df`) e tem correções: a
    /// lista dele (se existe) e as das correções, como um grupo.
    Corrected { exact: Option<rocksdb::DBPinnableSlice<'s>>, fixes: Vec<Fix<'s>> },
}

impl TermLists<'_> {
    /// A troca feita, para o resultado.
    pub(super) fn correction(&self, from: &str) -> Option<Correction> {
        match self {
            TermLists::Exact(_) => None,
            TermLists::Corrected { fixes, .. } => {
                Some(Correction { from: from.to_string(), to: fixes.iter().map(|f| f.word.clone()).collect() })
            }
        }
    }
}

/// Um nó visitado pelo percurso: a linha dele na matriz de distâncias fica
/// em `Walk::rows`, na posição do estado.
struct State {
    node: u64,
    /// Estado do nó pai (`u32::MAX` na raiz).
    parent: u32,
    /// Caractere que leva do pai até aqui.
    ch: char,
}

impl View<'_> {
    /// A lista de um termo exato sem flexões ou, se [`TypoMode::Missing`]
    /// pede, o grupo com as correções dele. Sem correção ligada, é só a
    /// leitura da lista.
    pub(super) fn exact_or_corrected(&self, term: &str, node: Option<u64>) -> Result<TermLists<'_>> {
        let exact = node.map(|n| self.post_bytes(n)).transpose()?.flatten();
        let config = &self.engine.config;
        if config.typos == TypoMode::Off {
            return Ok(TermLists::Exact(exact));
        }
        let df = exact.as_ref().map(|v| codec::list_len(v)).transpose()?.unwrap_or(0) as usize;
        if df >= config.typo_min_df || term.chars().count() < config.typo_min_len {
            return Ok(TermLists::Exact(exact));
        }
        let fixes = self.fixes(term)?;
        Ok(if fixes.is_empty() { TermLists::Exact(exact) } else { TermLists::Corrected { exact, fixes } })
    }

    /// As correções de `term`: palavras a um erro, com a primeira letra
    /// igual e df ≥ `typo_min_df`, da de maior df para a de menor (no
    /// empate, em ordem alfabética), até `max_typo_terms`.
    fn fixes(&self, term: &str) -> Result<Vec<Fix<'_>>> {
        let config = &self.engine.config;
        let words = self.words_near(term)?;
        let values = self.get_many(POST, words.iter().map(|&(_, node)| node))?;
        let mut fixes = Vec::new();
        for ((word, _), value) in words.into_iter().zip(values) {
            let Some(value) = value else { continue };
            let df = codec::list_len(&value)? as usize;
            if df >= config.typo_min_df {
                fixes.push(Fix { word, df, value });
            }
        }
        fixes.sort_by(|a, b| b.df.cmp(&a.df).then_with(|| a.word.cmp(&b.word)));
        fixes.truncate(config.max_typo_terms);
        Ok(fixes)
    }

    /// As palavras da Trie a [`MAX_EDITS`] erros de `term` (sem ele mesmo),
    /// com a primeira letra igual. Retorna (palavra, nó).
    ///
    /// Percorre a Trie em largura levando, em cada nó, a linha da matriz de
    /// Damerau-Levenshtein (a distância "restrita", em que uma transposição
    /// vale 1) entre a palavra até o nó e cada prefixo de `term`. Um ramo
    /// para quando nenhuma posição da linha fica dentro do limite. Os nós de
    /// cada nível são lidos juntos (cache de nós e `MultiGet`), como no
    /// autocompletar.
    pub(super) fn words_near(&self, term: &str) -> Result<Vec<(String, u64)>> {
        let q: Vec<char> = term.chars().collect();
        let Some(&first) = q.first() else { return Ok(Vec::new()) };
        let Some(root) = self.nodes_many(std::iter::once(ROOT_ID), 0)?.pop() else { return Ok(Vec::new()) };
        let Some(&start) = root.children.get(&first) else { return Ok(Vec::new()) };
        let width = q.len() + 1;
        // Linha da raiz (a palavra vazia) e a do primeiro caractere, que já casa.
        let mut rows: Vec<u8> = (0..width as u8).collect();
        rows.extend((0..width).map(|j| if j == 0 { 1 } else { (j - 1) as u8 }));
        let mut states = vec![State { node: ROOT_ID, parent: u32::MAX, ch: '\0' }, State { node: start, parent: 0, ch: first }];
        let mut level: Vec<u32> = vec![1];
        let mut depth = 1;
        let mut found = Vec::new();
        let mut next_row = vec![0u8; width];
        while !level.is_empty() {
            let nodes = self.nodes_many(level.iter().map(|&s| states[s as usize].node), depth)?;
            let mut next = Vec::new();
            for (&s, node) in level.iter().zip(&nodes) {
                let s = s as usize;
                let row = s * width;
                if node.is_end_of_word && (1..=MAX_EDITS).contains(&rows[row + q.len()]) {
                    found.push((word_of(&states, s), states[s].node));
                }
                let (parent, ch) = (states[s].parent as usize, states[s].ch);
                for (&c, &child) in &node.children {
                    next_row[0] = rows[row] + 1;
                    let mut best = next_row[0];
                    for j in 1..width {
                        let swap = u8::from(q[j - 1] != c);
                        let mut v = (rows[row + j] + 1).min(next_row[j - 1] + 1).min(rows[row + j - 1] + swap);
                        // Transposição: ...ch c contra ...q[j-2] q[j-1] trocados.
                        if j > 1 && c == q[j - 2] && ch == q[j - 1] {
                            v = v.min(rows[parent * width + j - 2] + 1);
                        }
                        next_row[j] = v;
                        best = best.min(v);
                    }
                    if best > MAX_EDITS {
                        continue;
                    }
                    next.push(states.len() as u32);
                    states.push(State { node: child, parent: s as u32, ch: c });
                    rows.extend_from_slice(&next_row);
                }
            }
            level = next;
            depth += 1;
        }
        Ok(found)
    }
}

/// A palavra do estado `s`, subindo pelos pais.
fn word_of(states: &[State], mut s: usize) -> String {
    let mut chars = Vec::new();
    while states[s].parent != u32::MAX {
        chars.push(states[s].ch);
        s = states[s].parent as usize;
    }
    chars.iter().rev().collect()
}
