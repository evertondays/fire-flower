//! A nota de um documento: soma das partes da consulta.

use crate::engine::conjunction::AndList;
use crate::engine::groups::GroupParts;
use crate::engine::ranking::bm25::{Bm25, Bound, DocNorm};

/// Uma parte da consulta já carregada, para o ranking. `weight` é
/// idf · (k1 + 1), a parte da nota que não depende do documento.
pub(super) enum Scorer {
    /// Termo exato, ou prefixo tratado como um termo só (R1 do plano de
    /// relevância): a lista `list` em `lists`.
    Term { weight: f64, list: usize, bound: Bound },
    /// Frase: entra na nota como um termo, com tf = ocorrências da frase (no
    /// texto e no título, pela posição) e idf = soma dos idfs dos termos
    /// (como o Lucene).
    Phrase { weight: f64, phrase: usize, bound: Bound },
    /// Palavra com flexões (`Config::inflections`): um termo só, como no
    /// BM25F, em que as flexões são um campo de peso α: `tf~ = tf~_exata + α ·
    /// tf~_flexões`, com uma saturação só. O idf é o do maior df entre as
    /// palavras (como o `SynonymQuery` do Lucene), que vem dos cabeçalhos das
    /// listas sem decodificá-las. Com α < 1, a palavra exata ganha os empates.
    /// `list` é a lista do grupo no E; `group`, onde ficam as entradas (exata,
    /// flexões) de cada documento quando ela é decodificada.
    Group { weight: f64, alpha: f64, list: usize, group: usize, bound: Box<Bound> },
}

/// Nota de um documento (já posicionado em todas as listas): soma das
/// partes, na ordem da consulta.
#[inline]
pub(super) fn doc_score(
    bm25: &Bm25,
    scorers: &[Scorer],
    sources: &[AndList<'_>],
    groups: &[GroupParts],
    counts: &[(u32, u32)],
    norm: DocNorm,
) -> f64 {
    let mut score = 0.0;
    for scorer in scorers {
        score += match scorer {
            Scorer::Term { weight, list, .. } => {
                let p = sources[*list].current();
                bm25.score(*weight, Bm25::pseudo_tf(norm, p.tf, p.title_tf))
            }
            Scorer::Phrase { weight, phrase, .. } => {
                let (tf, title_tf) = counts[*phrase];
                bm25.score(*weight, Bm25::pseudo_tf(norm, tf, title_tf))
            }
            Scorer::Group { weight, alpha, list, group, .. } => {
                let (e, v) = sources[*list].group_parts(groups, *group);
                let pseudo = Bm25::pseudo_tf(norm, e.tf, e.title_tf) + alpha * Bm25::pseudo_tf(norm, v.tf, v.title_tf);
                bm25.score(*weight, pseudo)
            }
        };
    }
    score
}

/// Limite superior de [`doc_score`]: a mesma soma, com o maior valor
/// possível de cada parte.
#[inline]
pub(super) fn doc_bound(scorers: &[Scorer], sources: &[AndList<'_>], groups: &[GroupParts], counts: &[(u32, u32)]) -> f64 {
    let mut bound = 0.0;
    for scorer in scorers {
        bound += match scorer {
            Scorer::Term { list, bound, .. } => {
                let p = sources[*list].current();
                bound.get(p.tf, p.title_tf)
            }
            Scorer::Phrase { phrase, bound, .. } => {
                let (tf, title_tf) = counts[*phrase];
                bound.get(tf, title_tf)
            }
            Scorer::Group { alpha, list, group, bound, .. } => {
                let (e, v) = sources[*list].group_parts(groups, *group);
                bound.get_mixed((e.tf, e.title_tf), (v.tf, v.title_tf), *alpha)
            }
        };
    }
    bound
}
