//! O E entre listas: quem conduz, cursores que saltam blocos e interseção.

use std::cmp::Ordering;

use crate::{DocId, Posting, Result};
use crate::codec::PostingsCursor;
use crate::engine::groups::GroupParts;

/// Uma lista do E, antes de escolher quem conduz: os bytes de `post:` (ainda
/// não decodificados; o tamanho vem do cabeçalho) ou uma lista já
/// decodificada (a união das completações de um prefixo).
pub(crate) enum Member<T> {
    Bytes { idx: usize, len: usize },
    Decoded(Vec<T>),
    /// Palavra com flexões: os bytes das listas (`true` = a palavra exata) e a
    /// soma dos tamanhos (um limite da união).
    Group { parts: Vec<(bool, usize)>, len: usize },
}

impl<T> Member<T> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Member::Bytes { len, .. } | Member::Group { len, .. } => *len,
            Member::Decoded(list) => list.len(),
        }
    }
}

/// Uma lista do E no ranking, já posicionada: decodificada (a que conduz e as
/// uniões de prefixo) ou um cursor que salta blocos pela tabela de saltos.
pub(crate) enum AndList<'s> {
    Decoded { list: Vec<Posting>, at: usize },
    Cursor { cursor: PostingsCursor<'s>, current: Posting },
    /// Grupo de flexões que não conduz: um cursor por lista, sem decodificar;
    /// `current` = (exata, flexões) do documento.
    Group { cursors: Vec<(bool, PostingsCursor<'s>)>, current: (Posting, Posting) },
}

impl AndList<'_> {
    /// A entrada do documento da última busca bem-sucedida.
    #[inline]
    pub(crate) fn current(&self) -> Posting {
        match self {
            AndList::Decoded { list, at } => list[*at],
            AndList::Cursor { current, .. } => *current,
            AndList::Group { current: (e, v), .. } => {
                Posting { doc: e.doc, tf: e.tf.saturating_add(v.tf), title_tf: e.title_tf.saturating_add(v.title_tf) }
            }
        }
    }

    /// (exata, flexões) do documento atual de um grupo de flexões: dos
    /// cursores, ou de `groups[group]` se a lista foi decodificada.
    #[inline]
    pub(crate) fn group_parts(&self, groups: &[GroupParts], group: usize) -> (Posting, Posting) {
        match self {
            AndList::Decoded { at, .. } => groups[group][*at],
            AndList::Group { current, .. } => *current,
            AndList::Cursor { .. } => unreachable!("grupo de flexões em cursor simples"),
        }
    }

    /// Avança até `doc` (os alvos só crescem) e diz se ele está na lista.
    #[inline]
    pub(crate) fn seek(&mut self, doc: DocId) -> Result<bool> {
        match self {
            AndList::Decoded { list, at } => Ok(seek(list, at, doc)),
            AndList::Cursor { cursor, current } => match cursor.seek(doc)? {
                Some(p) if p.doc == doc => {
                    *current = p;
                    Ok(true)
                }
                _ => Ok(false),
            },
            AndList::Group { cursors, current } => {
                let none = Posting { doc, tf: 0, title_tf: 0 };
                let (mut e, mut v) = (none, none);
                let mut found = false;
                for (is_exact, cursor) in cursors.iter_mut() {
                    if let Some(p) = cursor.seek(doc)?
                        && p.doc == doc
                    {
                        found = true;
                        let part = if *is_exact { &mut e } else { &mut v };
                        part.tf = part.tf.saturating_add(p.tf);
                        part.title_tf = part.title_tf.saturating_add(p.title_tf);
                    }
                }
                *current = (e, v);
                Ok(found)
            }
        }
    }
}

/// Procurar documento a documento com o cursor só compensa quando a lista é
/// bem maior que a que conduz: aí a maioria dos blocos de `SKIP_BLOCK`
/// entradas não tem nenhum candidato e é pulada sem decodificar. Com tamanhos
/// parecidos, decodificar a lista inteira e andar nela em sequência é mais
/// barato (E de dois termos frequentes: +16% com cursor).
pub(crate) fn use_cursor(len: usize, driver_len: usize) -> bool {
    len >= 32 * driver_len
}

/// Como [`seek`], numa lista só de IDs.
#[inline]
pub(crate) fn seek_docs(list: &[DocId], at: &mut usize, target: DocId) -> bool {
    let mut lo = *at;
    if lo >= list.len() {
        return false;
    }
    if list[lo] >= target {
        return list[lo] == target;
    }
    let mut step = 1;
    let mut hi = lo + 1;
    while hi < list.len() && list[hi] < target {
        lo = hi;
        step *= 2;
        hi = lo + step;
    }
    let hi = hi.min(list.len());
    let found = lo + 1 + list[lo + 1..hi].partition_point(|&d| d < target);
    *at = found;
    found < list.len() && list[found] == target
}

/// Avança `at` até a primeira entrada com documento ≥ `target` (busca
/// exponencial a partir da posição atual) e diz se ela é `target`.
#[inline]
pub(crate) fn seek(list: &[Posting], at: &mut usize, target: DocId) -> bool {
    let mut lo = *at;
    if lo >= list.len() {
        return false;
    }
    if list[lo].doc >= target {
        return list[lo].doc == target;
    }
    // list[lo] < target: dobra o passo até passar do alvo.
    let mut step = 1;
    let mut hi = lo + 1;
    while hi < list.len() && list[hi].doc < target {
        lo = hi;
        step *= 2;
        hi = lo + step;
    }
    let hi = hi.min(list.len());
    let found = lo + 1 + list[lo + 1..hi].partition_point(|p| p.doc < target);
    *at = found;
    found < list.len() && list[found].doc == target
}

/// Interseção de duas listas ordenadas.
pub fn intersect(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}
