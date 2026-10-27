//! Busca ranqueada (BM25F) com top-k.

use crate::{DocId, Hit, InflectionMode, Posting, Result, TopHits, codec};
use crate::codec::PostingsCursor;
use crate::engine::conjunction::{AndList, Member, use_cursor};
use crate::engine::groups::{GroupParts, decode_group};
use crate::engine::phrase::PhraseMatcher;
use crate::engine::ranking::bm25::{Bm25, Bound};
use crate::engine::ranking::score::{Scorer, doc_bound, doc_score};
use crate::engine::ranking::topk::TopK;
use crate::engine::view::{NormCursor, View};
use crate::query::Clause;

pub(crate) mod bm25;
pub(crate) mod score;
pub(crate) mod topk;


impl View<'_> {
    pub(crate) fn search_ranked(&self, query: &str, k: usize) -> Result<TopHits> {
        let clauses = self.engine.parse_query(query);
        self.check_phrases(&clauses)?;
        if clauses.is_empty() {
            return Ok(TopHits::default());
        }
        match self.inflection_mode() {
            None => self.search_ranked_with(&clauses, k, false),
            Some(InflectionMode::Always) => self.search_ranked_with(&clauses, k, true),
            Some(InflectionMode::IfFewer(n)) => {
                let exact = self.search_ranked_with(&clauses, k, false)?;
                if exact.total < n { self.search_ranked_with(&clauses, k, true) } else { Ok(exact) }
            }
        }
    }

    /// `expand`: como em [`View::search_with`].
    pub(crate) fn search_ranked_with(&self, clauses: &[Clause], k: usize, expand: bool) -> Result<TopHits> {
        let stats = self.stats()?;
        let config = &self.engine.config;
        let bm25 = Bm25::new(config, stats);

        // 1. As listas. Todas entram no E; cada parte da consulta sabe quais
        //    são as suas. As exatas ficam como bytes (o df vem do cabeçalho)
        //    até se saber quem conduz.
        let mut bytes: Vec<rocksdb::DBPinnableSlice<'_>> = Vec::new();
        let mut lists: Vec<Member<Posting>> = Vec::new();
        let mut scorers: Vec<Scorer> = Vec::with_capacity(clauses.len());
        let mut phrases: Vec<Vec<(u32, u64)>> = Vec::new();
        let mut groups: Vec<GroupParts> = Vec::new();
        for clause in clauses {
            match clause {
                Clause::Term(t) if !t.prefix => {
                    let (exact, variants) =
                        if expand { self.inflections(&t.text)? } else { (self.term_node(&t.text)?, Vec::new()) };
                    if variants.is_empty() {
                        let df = self.add_exact(exact, &mut bytes, &mut lists)?;
                        if df == 0 {
                            return Ok(TopHits::default());
                        }
                        let weight = bm25.weight(bm25.idf(df));
                        scorers.push(Scorer::Term { weight, list: lists.len() - 1, bound: Bound::new(weight, &bm25) });
                        continue;
                    }
                    let list = lists.len();
                    let max_df = self.add_group(exact, variants, &mut bytes, &mut lists)?;
                    if max_df == 0 {
                        return Ok(TopHits::default());
                    }
                    let weight = bm25.weight(bm25.idf(max_df));
                    scorers.push(Scorer::Group {
                        weight,
                        alpha: self.engine.config.inflection_weight,
                        list,
                        group: groups.len(),
                        bound: Box::new(Bound::new(weight, &bm25)),
                    });
                    groups.push(GroupParts::new());
                }
                Clause::Term(t) => {
                    // Prefixo como um termo só: um documento por entrada, com o
                    // tf somado das completações que ele contém, e o idf do
                    // conjunto. Uma completação rara não vale mais que a
                    // palavra comum (R1 do plano de relevância).
                    let list = self.prefix_postings(&t.text, t.stem.as_deref())?;
                    if list.is_empty() {
                        return Ok(TopHits::default());
                    }
                    let weight = bm25.weight(bm25.idf(list.len()));
                    scorers.push(Scorer::Term { weight, list: lists.len(), bound: Bound::new(weight, &bm25) });
                    lists.push(Member::Decoded(list));
                }
                Clause::Phrase(parts) => {
                    let mut idf = 0.0;
                    let mut nodes = Vec::with_capacity(parts.len());
                    for part in parts {
                        let node = self.term_node(&part.text)?;
                        let df = self.add_exact(node, &mut bytes, &mut lists)?;
                        if df == 0 {
                            return Ok(TopHits::default());
                        }
                        idf += bm25.idf(df);
                        nodes.push((part.offset, node.unwrap_or_default()));
                    }
                    let weight = bm25.weight(idf);
                    scorers.push(Scorer::Phrase { weight, phrase: phrases.len(), bound: Bound::new(weight, &bm25) });
                    phrases.push(nodes);
                }
            }
        }
        let slices: Vec<Vec<rocksdb::DBPinnableSlice<'_>>> =
            phrases.iter().map(|p| self.load_positions(p)).collect::<Result<_>>()?;
        let mut matchers: Vec<PhraseMatcher<'_>> =
            phrases.iter().zip(&slices).map(|(p, s)| PhraseMatcher::new(p, s)).collect::<Result<_>>()?;
        // (ocorrências da frase, quantas no título), por frase.
        let mut counts = vec![(0u32, 0u32); matchers.len()];

        // O grupo de flexões de cada lista que é um.
        let mut group_of: Vec<Option<usize>> = vec![None; lists.len()];
        for scorer in &scorers {
            if let Scorer::Group { list, group, .. } = scorer {
                group_of[*list] = Some(*group);
            }
        }
        // Decodifica uma lista inteira (a de um grupo guarda as entradas em `groups`).
        let decode = |m: Member<Posting>, i: usize, groups: &mut Vec<GroupParts>| -> Result<Vec<Posting>> {
            match m {
                Member::Bytes { idx, .. } => codec::decode_postings(&bytes[idx]),
                Member::Decoded(list) => Ok(list),
                Member::Group { parts, .. } => {
                    let (union, entries) = decode_group(&bytes, &parts)?;
                    groups[group_of[i].expect("lista de grupo")] = entries;
                    Ok(union)
                }
            }
        };
        if let [Scorer::Term { weight, bound, .. }] = scorers.as_slice()
            && matchers.is_empty()
        {
            let list = decode(lists.pop().expect("um termo, uma lista"), 0, &mut groups)?;
            let hits = self.top_single_term(&list, *weight, bound, &bm25, k)?;
            return Ok(TopHits { hits, total: list.len() });
        }
        if let [Scorer::Group { weight, alpha, bound, .. }] = scorers.as_slice()
            && matchers.is_empty()
            && let Some(Member::Group { parts, .. }) = lists.pop()
        {
            let (_, entries) = decode_group(&bytes, &parts)?;
            let hits = self.top_single_group(&entries, *weight, *alpha, bound, &bm25, k)?;
            return Ok(TopHits { hits, total: entries.len() });
        }

        // 2. E pelas listas: a menor conduz (decodificada inteira); nas
        //    outras, cada documento dela é procurado por saltos, sem
        //    decodificar os blocos do meio.
        let driver = (0..lists.len()).min_by_key(|&i| lists[i].len()).unwrap_or(0);
        let driver_len = lists[driver].len();
        let mut sources: Vec<AndList<'_>> = Vec::with_capacity(lists.len());
        for (i, m) in lists.into_iter().enumerate() {
            sources.push(match m {
                Member::Bytes { idx, len } if i != driver && use_cursor(len, driver_len) => {
                    AndList::Cursor { cursor: PostingsCursor::new(&bytes[idx])?, current: Posting { doc: 0, tf: 1, title_tf: 0 } }
                }
                Member::Group { parts, len } if i != driver && use_cursor(len, driver_len) => {
                    let cursors = parts.iter().map(|&(e, idx)| Ok((e, PostingsCursor::new(&bytes[idx])?))).collect::<Result<_>>()?;
                    let none = Posting { doc: 0, tf: 0, title_tf: 0 };
                    AndList::Group { cursors, current: (none, none) }
                }
                m => AndList::Decoded { list: decode(m, i, &mut groups)?, at: 0 },
            });
        }
        let driver_docs: Vec<DocId> = match &sources[driver] {
            AndList::Decoded { list, .. } => list.iter().map(|p| p.doc).collect(),
            AndList::Cursor { .. } | AndList::Group { .. } => unreachable!("quem conduz é decodificada"),
        };
        let mut norms = NormCursor::new(self);
        let mut top = TopK::new(k);
        let mut total = 0usize;
        'docs: for (i, &doc) in driver_docs.iter().enumerate() {
            if let AndList::Decoded { at, .. } = &mut sources[driver] {
                *at = i;
            }
            for (j, source) in sources.iter_mut().enumerate() {
                if j != driver && !source.seek(doc)? {
                    continue 'docs;
                }
            }
            // Com frase, o tamanho vem antes: a extensão do título diz quais
            // ocorrências estão nele.
            let mut lens = None;
            if !matchers.is_empty() {
                let l = norms.lens(doc)?;
                for (m, matcher) in matchers.iter_mut().enumerate() {
                    counts[m] = matcher.count(doc, false, u32::from(l.title_span))?;
                    if counts[m].0 == 0 {
                        continue 'docs;
                    }
                }
                lens = Some(l);
            }
            total += 1;
            if k == 0 {
                continue;
            }

            // 3. Nota. Com o top-k cheio, um documento que nem com tamanho 0
            //    passaria do pior não precisa do tamanho (nem da nota exata).
            //    Empate também perde: os IDs chegam em ordem crescente.
            if let Some(worst) = top.threshold()
                && doc_bound(&scorers, &sources, &groups, &counts) <= worst
            {
                continue;
            }
            let lens = match lens {
                Some(l) => l,
                None => norms.lens(doc)?,
            };
            let score = doc_score(&bm25, &scorers, &sources, &groups, &counts, bm25.doc_norm(lens));
            top.push(Hit { doc, score });
        }
        Ok(TopHits { hits: top.into_sorted(), total })
    }
}
