//! O índice ingênuo do oráculo: listas por termo, posições e a busca.

use fire_flower::query::Clause;
use rustc_hash::FxHashMap as HashMap;
use fire_flower::analysis::TITLE_GAP;
use fire_flower::analysis::tokenize;
use fire_flower::DocId;

/// Ordem do autocompletar e da expansão de prefixo: comprimento, depois alfabética.
pub(crate) fn trie_order(a: &&str, b: &&str) -> std::cmp::Ordering {
    a.chars().count().cmp(&b.chars().count()).then_with(|| a.cmp(b))
}

/// Lista de um termo no oráculo: documentos em ordem, o tf de cada um e
/// quantas ocorrências estão no título.
#[derive(Default)]
pub(crate) struct OTerm {
    pub(crate) docs: Vec<DocId>,
    pub(crate) tfs: Vec<u32>,
    pub(crate) title_tfs: Vec<u32>,
}

impl OTerm {
    /// (tf no documento inteiro, tf no título).
    pub(crate) fn tfs_of(&self, doc: DocId) -> Option<(u32, u32)> {
        self.docs.binary_search(&doc).ok().map(|i| (self.tfs[i], self.title_tfs[i]))
    }

    /// Ordena por documento; o mesmo documento duas vezes: a última vence.
    pub(crate) fn finish(&mut self) {
        if self.docs.windows(2).all(|w| w[0] < w[1]) {
            return;
        }
        let mut entries: Vec<(DocId, u32, u32)> =
            (0..self.docs.len()).map(|i| (self.docs[i], self.tfs[i], self.title_tfs[i])).collect();
        entries.sort_by_key(|p| p.0);
        let mut kept: Vec<(DocId, u32, u32)> = Vec::with_capacity(entries.len());
        for p in entries {
            match kept.last_mut() {
                Some(last) if last.0 == p.0 => *last = p,
                _ => kept.push(p),
            }
        }
        self.docs = kept.iter().map(|p| p.0).collect();
        self.tfs = kept.iter().map(|p| p.1).collect();
        self.title_tfs = kept.iter().map(|p| p.2).collect();
    }
}

/// Tokens de um artigo com as posições do motor: o título em 0…T − 1 e o
/// texto a partir de T + `TITLE_GAP` (ou de 0, sem título). Também diz quantos
/// tokens são do título.
pub(crate) fn doc_tokens(title: &str, text: &str) -> (Vec<(String, u32)>, usize) {
    let title_tokens = tokenize(title);
    let span = title_tokens.len() as u32;
    let offset = if span > 0 { span + TITLE_GAP } else { 0 };
    let n_title = title_tokens.len();
    let mut out: Vec<(String, u32)> = title_tokens.into_iter().enumerate().map(|(i, t)| (t, i as u32)).collect();
    out.extend(tokenize(text).into_iter().enumerate().map(|(i, t)| (t, offset + i as u32)));
    (out, n_title)
}

/// Posições de um termo no oráculo, achatadas para caber na memória.
#[derive(Default)]
pub(crate) struct OPos {
    pub(crate) docs: Vec<DocId>,
    /// Fim das posições de cada documento em `positions`.
    pub(crate) ends: Vec<usize>,
    pub(crate) positions: Vec<u32>,
}

impl OPos {
    pub(crate) fn push(&mut self, doc: DocId, positions: &[u32]) {
        self.docs.push(doc);
        self.positions.extend_from_slice(positions);
        self.ends.push(self.positions.len());
    }

    pub(crate) fn slice(&self, i: usize) -> &[u32] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.positions[start..self.ends[i]]
    }

    pub(crate) fn get(&self, doc: DocId) -> Option<&[u32]> {
        self.docs.binary_search(&doc).ok().map(|i| self.slice(i))
    }

    pub(crate) fn entries(&self) -> Vec<(DocId, Vec<u32>)> {
        (0..self.docs.len()).map(|i| (self.docs[i], self.slice(i).to_vec())).collect()
    }

    /// Mesma regra do `OTerm::finish`.
    pub(crate) fn finish(&mut self) {
        if self.docs.windows(2).all(|w| w[0] < w[1]) {
            return;
        }
        let mut entries: Vec<(DocId, Vec<u32>)> = self.entries();
        entries.sort_by_key(|e| e.0);
        let mut kept: Vec<(DocId, Vec<u32>)> = Vec::with_capacity(entries.len());
        for e in entries {
            match kept.last_mut() {
                Some(last) if last.0 == e.0 => *last = e,
                _ => kept.push(e),
            }
        }
        *self = OPos::default();
        for (doc, ps) in kept {
            self.push(doc, &ps);
        }
    }
}

/// Índice ingênuo em memória, construído só com `tokenize`, as stopwords e o
/// stemmer do banco (o radical de cada token, sem o cache da indexação):
/// nada da indexação do motor (lote, merge, Trie) é reaproveitado.
pub(crate) struct Oracle {
    pub(crate) terms: HashMap<String, OTerm>,
    pub(crate) positions: HashMap<String, OPos>,
    /// (tamanho inteiro, tamanho do título), sem stopwords.
    pub(crate) lens: HashMap<DocId, (u32, u32)>,
    /// Posições ocupadas pelo título (stopwords inclusive): uma frase que
    /// começa antes disso está no título.
    pub(crate) spans: HashMap<DocId, u32>,
    pub(crate) total_len: u64,
    pub(crate) total_title_len: u64,
    pub(crate) k1: f64,
    pub(crate) b: f64,
    pub(crate) title_weight: f64,
    pub(crate) title_b: f64,
    pub(crate) max_prefix: usize,
    /// Flexões na consulta: radical -> palavras do vocabulário com ele, o
    /// peso α e o modo (vazio sem flexões).
    pub(crate) by_root: HashMap<String, Vec<String>>,
    pub(crate) inflector: Option<fire_flower::Stemmer>,
    pub(crate) alpha: f64,
    pub(crate) mode: fire_flower::InflectionMode,
}

/// Uma parte da consulta resolvida no oráculo.
pub(crate) enum OClause<'a> {
    Term(&'a OTerm),
    /// Palavra com flexões: a exata (se existe), as outras e o maior df entre elas.
    Group(Option<&'a OTerm>, Vec<&'a OTerm>, usize),
    /// As completações e o df do conjunto (documentos com alguma delas).
    Prefix(Vec<&'a OTerm>, usize),
    Phrase(Vec<(u32, &'a OTerm, &'a OPos)>),
}

impl Oracle {
    pub(crate) fn idf(&self, df: usize) -> f64 {
        let n = self.lens.len() as f64;
        let df = df as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// BM25F do plano de relevância (seção 1), escrito de novo aqui (não usa
    /// a conta do motor): `tf~ = tf_texto / B_texto + w · tf_título / B_título`.
    pub(crate) fn bm25(&self, idf: f64, tf: u32, title_tf: u32, doc: DocId) -> f64 {
        self.saturate(idf, self.pseudo_tf(tf, title_tf, doc))
    }

    /// idf · tf~ · (k1 + 1) / (tf~ + k1).
    pub(crate) fn saturate(&self, idf: f64, ptf: f64) -> f64 {
        idf * ptf * (self.k1 + 1.0) / (ptf + self.k1)
    }

    /// tf~ do BM25F (seção 1 do plano de relevância).
    pub(crate) fn pseudo_tf(&self, tf: u32, title_tf: u32, doc: DocId) -> f64 {
        let n = self.lens.len().max(1) as f64;
        let (len, title_len) = self.lens[&doc];
        let body_len = f64::from(len - title_len);
        let avg_body = (self.total_len - self.total_title_len) as f64 / n;
        let avg_title = self.total_title_len as f64 / n;
        let norm = |b: f64, l: f64, avg: f64| if avg > 0.0 { 1.0 - b + b * l / avg } else { 1.0 - b };
        let (b_body, b_title) = (norm(self.b, body_len, avg_body), norm(self.title_b, f64::from(title_len), avg_title));
        let part = |tf: u32, weight: f64, b: f64| if tf == 0 || b <= 0.0 { 0.0 } else { weight * f64::from(tf) / b };
        part(tf - title_tf, 1.0, b_body) + part(title_tf, self.title_weight, b_title)
    }

    /// Termos com o prefixo, na ordem da Trie, até o limite do motor.
    pub(crate) fn expand<'v>(&self, vocab: &[&'v str], prefix: &str) -> Vec<&'v str> {
        let start = vocab.partition_point(|t| *t < prefix);
        let mut matches: Vec<&str> = vocab[start..].iter().copied().take_while(|t| t.starts_with(prefix)).collect();
        matches.sort_by(trie_order);
        matches.truncate(self.max_prefix);
        matches
    }

    /// Ocorrências da frase no documento, por força bruta, e quantas
    /// começam antes de `span` (no título).
    pub(crate) fn phrase_count(parts: &[(u32, &OTerm, &OPos)], doc: DocId, span: u32) -> (u32, u32) {
        let lists: Vec<&[u32]> = match parts.iter().map(|(_, _, pos)| pos.get(doc)).collect::<Option<Vec<_>>>() {
            Some(lists) => lists,
            None => return (0, 0),
        };
        let (mut count, mut in_title) = (0, 0);
        for &p0 in lists[0] {
            let Some(start) = p0.checked_sub(parts[0].0) else { continue };
            let all = parts.iter().zip(&lists).skip(1).all(|((off, _, _), list)| {
                u32::try_from(u64::from(start) + u64::from(*off)).is_ok_and(|want| list.binary_search(&want).is_ok())
            });
            count += u32::from(all);
            in_title += u32::from(all && start < span);
        }
        (count, in_title)
    }

    /// As outras palavras do vocabulário com o radical de `word`.
    pub(crate) fn variants(&self, word: &str) -> Vec<&OTerm> {
        let Some(inflector) = &self.inflector else { return Vec::new() };
        self.by_root
            .get(&inflector.stem(word))
            .map(|ws| ws.iter().filter(|w| *w != word).map(|w| &self.terms[w]).collect())
            .unwrap_or_default()
    }

    /// Se a consulta soma as flexões, pelo modo: com `IfFewer(n)`, só quando
    /// a busca exata acha menos de `n` documentos.
    pub(crate) fn should_expand(&self, vocab: &[&str], clauses: &[Clause]) -> Option<bool> {
        Some(match (&self.inflector, self.mode) {
            (None, _) => false,
            (Some(_), fire_flower::InflectionMode::Always) => true,
            (Some(_), fire_flower::InflectionMode::IfFewer(n)) => self.resolve_with(vocab, clauses, false)?.1.len() < n,
        })
    }

    /// Documentos que casam com a consulta, em ordem (com as flexões, se o
    /// modo pedir). `None` se alguma frase precisaria de posições que o
    /// oráculo não coletou.
    pub(crate) fn resolve<'s>(&'s self, vocab: &[&str], clauses: &[Clause]) -> Option<(Vec<OClause<'s>>, Vec<DocId>)> {
        let expand = self.should_expand(vocab, clauses)?;
        self.resolve_with(vocab, clauses, expand)
    }

    pub(crate) fn resolve_with<'s>(&'s self, vocab: &[&str], clauses: &[Clause], expand: bool) -> Option<(Vec<OClause<'s>>, Vec<DocId>)> {
        let empty = || Some((Vec::new(), Vec::new()));
        let mut resolved = Vec::new();
        for clause in clauses {
            match clause {
                Clause::Term(t) if !t.prefix => {
                    let exact = self.terms.get(&t.text);
                    let others = if expand { self.variants(&t.text) } else { Vec::new() };
                    if others.is_empty() {
                        match exact {
                            Some(term) => resolved.push(OClause::Term(term)),
                            None => return empty(),
                        }
                    } else {
                        // idf do maior df entre as palavras (como o motor e o `SynonymQuery` do Lucene).
                        let df = exact.iter().chain(&others).map(|t| t.docs.len()).max().unwrap_or(0);
                        resolved.push(OClause::Group(exact, others, df));
                    }
                }
                Clause::Term(t) => {
                    let mut words = self.expand(vocab, &t.text);
                    // Com stemming, o radical do prefixo também vale (depois do limite).
                    if let Some(stem) = t.stem.as_deref().and_then(|s| vocab.iter().copied().find(|w| *w == s))
                        && !words.contains(&stem)
                    {
                        words.push(stem);
                    }
                    let terms: Vec<&OTerm> = words.iter().map(|w| &self.terms[*w]).collect();
                    if terms.is_empty() {
                        return empty();
                    }
                    let mut union: Vec<DocId> = terms.iter().flat_map(|t| t.docs.iter().copied()).collect();
                    union.sort_unstable();
                    union.dedup();
                    resolved.push(OClause::Prefix(terms, union.len()));
                }
                Clause::Phrase(parts) => {
                    let mut out = Vec::new();
                    for part in parts {
                        let Some(term) = self.terms.get(&part.text) else { return empty() };
                        out.push((part.offset, term, self.positions.get(&part.text)?));
                    }
                    resolved.push(OClause::Phrase(out));
                }
            }
        }
        if resolved.is_empty() {
            return empty();
        }
        let mut sets: Vec<Vec<DocId>> = Vec::new();
        for c in &resolved {
            match c {
                OClause::Term(t) => sets.push(t.docs.clone()),
                OClause::Group(exact, others, _) => {
                    let mut all: Vec<DocId> = exact.iter().chain(others).flat_map(|t| t.docs.iter().copied()).collect();
                    all.sort_unstable();
                    all.dedup();
                    sets.push(all);
                }
                OClause::Prefix(ts, _) => {
                    let mut all: Vec<DocId> = ts.iter().flat_map(|t| t.docs.iter().copied()).collect();
                    all.sort_unstable();
                    all.dedup();
                    sets.push(all);
                }
                OClause::Phrase(parts) => sets.extend(parts.iter().map(|(_, t, _)| t.docs.clone())),
            }
        }
        sets.sort_by_key(Vec::len);
        let mut docs = sets[0].clone();
        for s in &sets[1..] {
            docs = fire_flower::internals::intersect(&docs, s);
        }
        docs.retain(|&d| {
            resolved.iter().all(|c| match c {
                OClause::Phrase(parts) => Self::phrase_count(parts, d, 0).0 > 0,
                _ => true,
            })
        });
        Some((resolved, docs))
    }

    /// Os `k` melhores por BM25 (nota decrescente, ID crescente) e o total.
    pub(crate) fn rank(&self, vocab: &[&str], clauses: &[Clause], k: usize) -> Option<(Vec<(DocId, f64)>, usize)> {
        let (resolved, docs) = self.resolve(vocab, clauses)?;
        let mut scored: Vec<(DocId, f64)> = docs
            .iter()
            .map(|&d| {
                let mut score = 0.0;
                for c in &resolved {
                    score += match c {
                        OClause::Term(t) => {
                            let (tf, title_tf) = t.tfs_of(d).unwrap_or((0, 0));
                            self.bm25(self.idf(t.docs.len()), tf, title_tf, d)
                        }
                        // Um termo só: tf~ = tf~_exata + α · tf~_flexões, idf do maior df.
                        OClause::Group(exact, others, df) => {
                            let (tf, title_tf) = exact.and_then(|t| t.tfs_of(d)).unwrap_or((0, 0));
                            let (vtf, vtitle) = others.iter().filter_map(|t| t.tfs_of(d)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                            let ptf = self.pseudo_tf(tf, title_tf, d) + self.alpha * self.pseudo_tf(vtf, vtitle, d);
                            self.saturate(self.idf(*df), ptf)
                        }
                        // Um termo só: idf do conjunto, tf somado (R1 do plano de relevância).
                        OClause::Prefix(ts, df) => {
                            let (tf, title_tf) = ts.iter().filter_map(|t| t.tfs_of(d)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                            self.bm25(self.idf(*df), tf, title_tf, d)
                        }
                        OClause::Phrase(parts) => {
                            let idf: f64 = parts.iter().map(|(_, t, _)| self.idf(t.docs.len())).sum();
                            let (tf, title_tf) = Self::phrase_count(parts, d, self.spans[&d]);
                            self.bm25(idf, tf, title_tf, d)
                        }
                    };
                }
                (d, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let total = scored.len();
        scored.truncate(k);
        Some((scored, total))
    }
}
