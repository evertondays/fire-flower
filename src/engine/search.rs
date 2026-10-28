//! Busca sem ranking: os documentos que casam com todas as partes, em ordem de ID.

use crate::{DocId, InflectionMode, Result, codec};
use crate::codec::PostingsCursor;
use crate::engine::conjunction::{Member, seek_docs, use_cursor};
use crate::engine::groups::decode_group_docs;
use crate::engine::phrase::PhraseMatcher;
use crate::engine::view::View;
use crate::query::Clause;

impl View<'_> {
    pub(super) fn search(&self, query: &str) -> Result<Vec<DocId>> {
        let clauses = self.engine.parse_query(query);
        self.check_phrases(&clauses)?;
        match self.inflection_mode() {
            None => self.search_with(&clauses, false),
            Some(InflectionMode::Always) => self.search_with(&clauses, true),
            Some(InflectionMode::IfFewer(n)) => {
                let exact = self.search_with(&clauses, false)?;
                if exact.len() < n { self.search_with(&clauses, true) } else { Ok(exact) }
            }
        }
    }

    /// `expand`: palavras exatas casam também as flexões (frases e prefixos não).
    fn search_with(&self, clauses: &[Clause], expand: bool) -> Result<Vec<DocId>> {
        // Listas exatas ficam como bytes até se saber quem conduz o E.
        let mut bytes: Vec<rocksdb::DBPinnableSlice<'_>> = Vec::new();
        let mut members: Vec<Member<DocId>> = Vec::new();
        let mut phrases: Vec<Vec<(u32, u64)>> = Vec::new();
        for clause in clauses {
            match clause {
                Clause::Term(t) if t.prefix => {
                    let list = self.prefix_docs(&t.text, t.stem.as_deref())?;
                    if list.is_empty() {
                        return Ok(Vec::new());
                    }
                    members.push(Member::Decoded(list));
                }
                Clause::Term(t) => {
                    let (exact, variants) =
                        if expand { self.inflections(&t.text)? } else { (self.term_node(&t.text)?, Vec::new()) };
                    let df = if variants.is_empty() {
                        self.add_exact(exact, &mut bytes, &mut members)?
                    } else {
                        self.add_group(exact, variants, &mut bytes, &mut members)?
                    };
                    if df == 0 {
                        return Ok(Vec::new());
                    }
                }
                Clause::Phrase(parts) => {
                    // Os termos da frase entram no E como termos comuns; as
                    // posições só são conferidas nos documentos que sobrarem.
                    let mut nodes = Vec::with_capacity(parts.len());
                    for part in parts {
                        let node = self.term_node(&part.text)?;
                        if self.add_exact(node, &mut bytes, &mut members)? == 0 {
                            return Ok(Vec::new());
                        }
                        nodes.push((part.offset, node.unwrap_or_default()));
                    }
                    phrases.push(nodes);
                }
            }
        }
        if members.is_empty() {
            return Ok(Vec::new());
        }

        // A lista menor conduz e é a única decodificada inteira (sem as
        // versões mortas: as outras listas só são consultadas nos documentos
        // dela); nas outras, cada documento dela é procurado por saltos (das
        // menores para as maiores, que descartam mais cedo).
        members.sort_by_key(Member::len);
        let mut members = members.into_iter();
        let mut result = match members.next().expect("há pelo menos uma lista") {
            Member::Bytes { idx, .. } => self.docs_of(&bytes[idx])?,
            Member::Decoded(list) => list,
            Member::Group { parts, .. } => decode_group_docs(&bytes, &parts, self.dead()?)?,
        };
        let mut cursors: Vec<PostingsCursor<'_>> = Vec::new();
        // Grupos de flexões: o documento precisa estar em alguma das listas.
        let mut group_cursors: Vec<Vec<PostingsCursor<'_>>> = Vec::new();
        let mut decoded: Vec<(Vec<DocId>, usize)> = Vec::new();
        for m in members {
            match m {
                Member::Bytes { idx, len } if use_cursor(len, result.len()) => {
                    cursors.push(PostingsCursor::new(&bytes[idx])?)
                }
                Member::Bytes { idx, .. } => decoded.push((self.docs_of(&bytes[idx])?, 0)),
                Member::Decoded(list) => decoded.push((list, 0)),
                Member::Group { parts, len } if use_cursor(len, result.len()) => group_cursors
                    .push(parts.iter().map(|&(_, idx)| PostingsCursor::new(&bytes[idx])).collect::<Result<_>>()?),
                Member::Group { parts, .. } => decoded.push((decode_group_docs(&bytes, &parts, self.dead()?)?, 0)),
            }
        }
        if !cursors.is_empty() || !decoded.is_empty() || !group_cursors.is_empty() {
            let mut kept = Vec::with_capacity(result.len());
            'docs: for &doc in &result {
                for c in cursors.iter_mut() {
                    if c.seek(doc)?.is_none_or(|p| p.doc != doc) {
                        continue 'docs;
                    }
                }
                for group in group_cursors.iter_mut() {
                    let mut found = false;
                    for c in group.iter_mut() {
                        if c.seek(doc)?.is_some_and(|p| p.doc == doc) {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        continue 'docs;
                    }
                }
                for (list, at) in decoded.iter_mut() {
                    if !seek_docs(list, at, doc) {
                        continue 'docs;
                    }
                }
                kept.push(doc);
            }
            result = kept;
        }
        for phrase in &phrases {
            if result.is_empty() {
                break;
            }
            let slices = self.load_positions(phrase)?;
            let mut matcher = PhraseMatcher::new(phrase, &slices)?;
            let mut kept = Vec::with_capacity(result.len());
            for &doc in &result {
                if matcher.count(doc, true, 0)?.0 > 0 {
                    kept.push(doc);
                }
            }
            result = kept;
        }
        self.user_ids(result)
    }

    /// Guarda os bytes da lista `post:` do termo e devolve o df (0 se o termo
    /// não existe).
    pub(super) fn add_exact<'s, T>(
        &'s self,
        node: Option<u64>,
        bytes: &mut Vec<rocksdb::DBPinnableSlice<'s>>,
        members: &mut Vec<Member<T>>,
    ) -> Result<usize> {
        let Some(node) = node else { return Ok(0) };
        let Some(value) = self.post_bytes(node)? else { return Ok(0) };
        let len = codec::list_len(&value)? as usize;
        members.push(Member::Bytes { idx: bytes.len(), len });
        bytes.push(value);
        Ok(len)
    }
}
