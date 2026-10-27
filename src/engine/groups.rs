//! Grupos de flexões ([`Config::inflections`](crate::Config::inflections)):
//! a palavra exata e as do mesmo radical como um termo só.

use crate::{DocId, InflectionMode, Posting, Result, codec};
use crate::engine::conjunction::Member;
use crate::engine::keys::{POST, stem_key};
use crate::engine::view::{Source, View};

/// (exata, flexões) de cada documento da união de um [`Scorer::Group`].
pub(super) type GroupParts = Vec<(Posting, Posting)>;

/// Une a lista da palavra exata e a das flexões (as duas em ordem de
/// documento): a união para o E e as duas entradas de cada documento.
fn align_group(exact: &[Posting], variants: &[Posting]) -> (Vec<Posting>, GroupParts) {
    let none = |doc| Posting { doc, tf: 0, title_tf: 0 };
    let (mut union, mut parts) = (Vec::with_capacity(exact.len() + variants.len()), GroupParts::new());
    let (mut i, mut j) = (0, 0);
    while i < exact.len() || j < variants.len() {
        let (e, v) = match (exact.get(i), variants.get(j)) {
            (Some(e), Some(v)) if e.doc == v.doc => {
                i += 1;
                j += 1;
                (*e, *v)
            }
            (Some(e), Some(v)) if e.doc < v.doc => {
                i += 1;
                (*e, none(e.doc))
            }
            (Some(e), None) => {
                i += 1;
                (*e, none(e.doc))
            }
            (_, Some(v)) => {
                j += 1;
                (none(v.doc), *v)
            }
            (None, None) => unreachable!(),
        };
        union.push(Posting { doc: e.doc, tf: e.tf.saturating_add(v.tf), title_tf: e.title_tf.saturating_add(v.title_tf) });
        parts.push((e, v));
    }
    (union, parts)
}

/// Documentos de um grupo de flexões: a união das listas.
pub(super) fn decode_group_docs(bytes: &[rocksdb::DBPinnableSlice<'_>], parts: &[(bool, usize)]) -> Result<Vec<DocId>> {
    let mut all = Vec::new();
    for &(_, idx) in parts {
        all.extend(codec::decode_docs(&bytes[idx])?);
    }
    all.sort(); // trechos já ordenados: ver `View::union_postings`
    all.dedup();
    Ok(all)
}

/// Um grupo de flexões decodificado: a união e as entradas (exata, flexões).
pub(super) fn decode_group(bytes: &[rocksdb::DBPinnableSlice<'_>], parts: &[(bool, usize)]) -> Result<(Vec<Posting>, GroupParts)> {
    let (mut exact, mut variants) = (Vec::new(), Vec::new());
    for &(is_exact, idx) in parts {
        let list = codec::decode_postings(&bytes[idx])?;
        if is_exact { exact = list } else { variants.extend(list) }
    }
    variants.sort_by_key(|p| p.doc);
    let mut merged: Vec<Posting> = Vec::with_capacity(variants.len());
    for p in variants {
        match merged.last_mut() {
            Some(last) if last.doc == p.doc => {
                last.tf = last.tf.saturating_add(p.tf);
                last.title_tf = last.title_tf.saturating_add(p.title_tf);
            }
            _ => merged.push(p),
        }
    }
    Ok(align_group(&exact, &merged))
}

impl View<'_> {
    /// Se e quando a busca soma as flexões: `None` sem flexões no banco.
    pub(super) fn inflection_mode(&self) -> Option<InflectionMode> {
        self.engine.inflector.is_on().then_some(self.engine.config.inflection_mode)
    }

    /// A palavra exata e as outras do mesmo radical (`stem:`), sem a exata.
    pub(super) fn inflections(&self, term: &str) -> Result<(Option<u64>, Vec<u64>)> {
        let exact = self.term_node(term)?;
        let stem = self.engine.inflector.stem(term);
        let mut variants = match self.snap.get_bytes(&stem_key(&stem))? {
            Some(v) => codec::decode_node_set(&v)?,
            None => Vec::new(),
        };
        variants.retain(|&n| Some(n) != exact);
        Ok((exact, variants))
    }

    /// Guarda os bytes das listas de uma palavra e das suas flexões como um
    /// grupo e devolve o maior df entre elas (0 se nenhuma existe).
    pub(super) fn add_group<'s, T>(
        &'s self,
        exact: Option<u64>,
        variants: Vec<u64>,
        bytes: &mut Vec<rocksdb::DBPinnableSlice<'s>>,
        members: &mut Vec<Member<T>>,
    ) -> Result<usize> {
        let mut parts = Vec::with_capacity(variants.len() + 1);
        let (mut max_df, mut len) = (0usize, 0usize);
        let mut push = |is_exact: bool, value: rocksdb::DBPinnableSlice<'s>| -> Result<()> {
            let n = codec::list_len(&value)? as usize;
            max_df = max_df.max(n);
            len += n;
            parts.push((is_exact, bytes.len()));
            bytes.push(value);
            Ok(())
        };
        if let Some(value) = exact.map(|n| self.post_bytes(n)).transpose()?.flatten() {
            push(true, value)?;
        }
        for value in self.get_many(POST, variants.into_iter())?.into_iter().flatten() {
            push(false, value)?;
        }
        if len > 0 {
            members.push(Member::Group { parts, len });
        }
        Ok(max_df)
    }
}
