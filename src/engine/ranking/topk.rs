//! Top-k: os k melhores documentos sem ordenar todos.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::{DocId, Hit, Posting, Result};
use crate::engine::ranking::bm25::{Bm25, Bound, DocNorm};
use crate::engine::view::{NormCursor, View};

impl View<'_> {
    /// Um termo só: a nota depende só dos tf e dos tamanhos, e o limite só
    /// da chave (`Bound::key`: o tf, sem título).
    ///
    /// Os documentos são agrupados em níveis de chave (nível 0 = chave ≥
    /// `CAP`, nível `CAP − chave` para o resto), e o limite de um nível vale
    /// para todos os níveis abaixo dele. A lista não é reordenada: cada faixa de níveis é
    /// uma passada sequencial em ordem de ID, o que mantém a leitura dos
    /// tamanhos em ordem de bloco.
    ///
    /// 1. Uma passada conta os documentos de cada nível.
    /// 2. A primeira faixa vai do maior tf até juntar `k` documentos, que
    ///    enchem o top-k e dão um pior resultado de referência.
    /// 3. A faixa seguinte vai até o primeiro nível cujo limite não alcança
    ///    esse pior; ali a busca para, porque os níveis abaixo também não
    ///    alcançam. Dentro da faixa, quem nem pelo limite passa do pior atual
    ///    não lê o tamanho.
    ///
    /// Num termo frequente, a maior parte da lista (tf baixo) fica de fora sem
    /// leitura de tamanho nem nota exata.
    pub(crate) fn top_single_term(&self, list: &[Posting], weight: f64, bound: &Bound, bm25: &Bm25, k: usize) -> Result<Vec<Hit>> {
        self.top_by_levels(
            list,
            |p| p.doc,
            |p| bound.key(p.tf, p.title_tf),
            |p, norm| bm25.score(weight, Bm25::pseudo_tf(norm, p.tf, p.title_tf)),
            bound,
            bm25,
            k,
        )
    }

    /// Uma palavra com flexões sozinha: como [`View::top_single_term`], com a
    /// chave da exata somada à das flexões (com peso `alpha`) e arredondada
    /// para cima, que continua sendo um limite da nota.
    pub(crate) fn top_single_group(
        &self,
        entries: &[(Posting, Posting)],
        weight: f64,
        alpha: f64,
        bound: &Bound,
        bm25: &Bm25,
        k: usize,
    ) -> Result<Vec<Hit>> {
        // A chave tem conta de ponto flutuante: calculada uma vez por entrada.
        let keyed: Vec<(u32, Posting, Posting)> = entries
            .iter()
            .map(|&(e, v)| {
                let x = f64::from(bound.key(e.tf, e.title_tf)) + alpha * f64::from(bound.key(v.tf, v.title_tf));
                let key = if x.is_finite() && x < f64::from(u32::MAX - 1) { (x + 1e-9).ceil() as u32 } else { u32::MAX };
                (key, e, v)
            })
            .collect();
        self.top_by_levels(
            &keyed,
            |(_, e, _)| e.doc,
            |(key, _, _)| *key,
            |(_, e, v), norm| {
                bm25.score(weight, Bm25::pseudo_tf(norm, e.tf, e.title_tf) + alpha * Bm25::pseudo_tf(norm, v.tf, v.title_tf))
            },
            bound,
            bm25,
            k,
        )
    }

    /// O top-k de uma parte só, por níveis de chave: `key` é um limite da nota
    /// (pela tabela de `bound`) e `score`, a nota exata.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn top_by_levels<T>(
        &self,
        list: &[T],
        doc_of: impl Fn(&T) -> DocId,
        key_of: impl Fn(&T) -> u32,
        score_of: impl Fn(&T, DocNorm) -> f64,
        bound: &Bound,
        bm25: &Bm25,
        k: usize,
    ) -> Result<Vec<Hit>> {
        if k == 0 || list.is_empty() {
            return Ok(Vec::new());
        }
        const CAP: usize = 64;
        let level = |p: &T| CAP - (key_of(p) as usize).min(CAP);
        // Menor chave de um nível: o limite do nível inteiro.
        let level_key = |l: usize| (CAP - l) as u32;
        let mut count = [0usize; CAP + 1];
        for p in list {
            count[level(p)] += 1;
        }

        let mut top = TopK::new(k);
        let mut norms = NormCursor::new(self);
        // Fora de ordem de nota: no empate, o ID menor ainda entra.
        let loses = |top: &TopK, b: f64, doc: DocId| top.worst().is_some_and(|w| b < w.score || (b == w.score && doc > w.doc));
        let mut lo = 0;
        loop {
            // Níveis vazios no começo da faixa renderiam uma passada à toa.
            while lo <= CAP && count[lo] == 0 {
                lo += 1;
            }
            if lo > CAP {
                break;
            }
            // Fim da faixa: até juntar k documentos (top-k ainda não cheio) ou
            // até o primeiro nível que não alcança o pior guardado.
            let mut hi = lo;
            let mut gathered = 0;
            while hi <= CAP {
                if count[hi] > 0 {
                    if top.worst().is_some() {
                        if hi > 0 && loses(&top, bound.get_key(level_key(hi)), 0) {
                            break;
                        }
                    } else if gathered >= k {
                        break;
                    }
                    gathered += count[hi];
                }
                hi += 1;
            }
            if hi == lo {
                break; // o nível `lo` já não alcança o pior
            }
            for p in list {
                let l = level(p);
                let doc = doc_of(p);
                if l < lo || l >= hi || loses(&top, bound.get_key(key_of(p)), doc) {
                    continue;
                }
                let norm = bm25.doc_norm(norms.lens(doc)?);
                top.push(Hit { doc, score: score_of(p, norm) });
            }
            lo = hi;
        }
        Ok(top.into_sorted())
    }
}

/// Os `k` melhores resultados vistos até agora.
pub(crate) struct TopK {
    pub(crate) k: usize,
    /// O topo do heap é o pior dos guardados.
    pub(crate) heap: BinaryHeap<Worst>,
}

/// Ordem de "pior": nota menor; no empate, ID maior.
pub(crate) struct Worst(pub(crate) Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Worst {}

impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.score.total_cmp(&self.0.score).then(self.0.doc.cmp(&other.0.doc))
    }
}

impl TopK {
    pub(crate) fn new(k: usize) -> Self {
        Self { k, heap: BinaryHeap::with_capacity(k.min(1 << 16) + 1) }
    }

    /// Nota do pior guardado, quando já há `k`: só entra quem passar dela.
    #[inline]
    pub(crate) fn threshold(&self) -> Option<f64> {
        self.worst().map(|w| w.score)
    }

    /// O pior guardado, quando já há `k`.
    #[inline]
    pub(crate) fn worst(&self) -> Option<Hit> {
        if self.heap.len() < self.k { None } else { self.heap.peek().map(|w| w.0) }
    }

    #[inline]
    pub(crate) fn push(&mut self, hit: Hit) {
        if self.heap.len() < self.k {
            self.heap.push(Worst(hit));
        } else if let Some(mut worst) = self.heap.peek_mut()
            && Worst(hit) < *worst
        {
            *worst = Worst(hit);
        }
    }

    pub(crate) fn into_sorted(self) -> Vec<Hit> {
        // `into_sorted_vec` é crescente na ordem de "pior": o melhor primeiro.
        self.heap.into_sorted_vec().into_iter().map(|w| w.0).collect()
    }
}
