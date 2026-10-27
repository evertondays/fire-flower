//! `wiki oracle`: índice ingênuo em memória comparado com o motor.

use crate::AnyResult;
use crate::env::save_result;
use crate::corpus::stem;
use rustc_hash::FxHashSet as HashSet;
use std::collections::BTreeMap;
use fire_flower::query::parse_query_stemmed;
use fire_flower::analysis::index_terms;
use crate::corpus::Corpus;
use fire_flower::Engine;
use crate::rng::Rng;
use std::time::Instant;
use fire_flower::query::Clause;
use rustc_hash::FxHashMap as HashMap;
use fire_flower::analysis::TITLE_GAP;
use fire_flower::analysis::tokenize;
use fire_flower::DocId;
use serde_json::Value;
use fire_flower::Config;
use std::path::PathBuf;
use serde_json::json;

pub(crate) struct OracleArgs {
    pub(crate) db: PathBuf,
    pub(crate) config: Config,
    pub(crate) corpus: PathBuf,
    pub(crate) take: Option<u64>,
    pub(crate) remap_ids: bool,
    pub(crate) sample_terms: usize,
    pub(crate) and_queries: usize,
    pub(crate) prefixes: usize,
    pub(crate) titles: usize,
    pub(crate) phrases: usize,
    pub(crate) rank_queries: usize,
    pub(crate) seed: u64,
    pub(crate) label: Option<String>,
}

#[derive(Default)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) checked: usize,
    pub(crate) divergences: usize,
    pub(crate) examples: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl Check {
    pub(crate) fn new(name: &'static str) -> Self {
        Self { name, ..Default::default() }
    }

    pub(crate) fn compare<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, got: &T, expected: &T) {
        self.verdict(what, got == expected, || (format!("{got:?}"), format!("{expected:?}")));
    }

    /// Registra um caso; `show` só roda se divergiu (e só para os 5 primeiros).
    pub(crate) fn verdict(&mut self, what: &str, ok: bool, show: impl FnOnce() -> (String, String)) {
        self.checked += 1;
        if !ok {
            self.divergences += 1;
            if self.examples.len() < 5 {
                let cut = |s: String| {
                    if s.len() > 200 { format!("{}…", &s[..s.char_indices().nth(200).map_or(s.len(), |(i, _)| i)]) } else { s }
                };
                let (got, expected) = show();
                self.examples.push(format!("{what}: motor={} oráculo={}", cut(got), cut(expected)));
            }
        }
    }

    pub(crate) fn json(&self) -> Value {
        json!({ "verificacao": self.name, "casos": self.checked, "divergencias": self.divergences,
                "exemplos": self.examples, "notas": self.notes })
    }
}

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

/// Mesmos documentos na mesma ordem, notas com diferença relativa < 1e-4
/// (o plano). Documentos trocados só valem num empate de verdade.
pub(crate) fn same_ranking(got: &[fire_flower::Hit], want: &[(DocId, f64)]) -> bool {
    let rel = |a: f64, b: f64| (a - b).abs() / b.abs().max(1e-12);
    got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| rel(g.score, w.1) < 1e-4 && (g.doc == w.0 || rel(g.score, w.1) < 1e-9))
}

/// Frase como o usuário digita, a partir de tokens já normalizados.
pub(crate) fn quote(tokens: &[String]) -> String {
    format!("\"{}\"", tokens.join(" "))
}

pub(crate) fn oracle(args: OracleArgs) -> AnyResult {
    let started = Instant::now();
    let mut rng = Rng(args.seed);
    // Aberto antes: o oráculo descarta as mesmas stopwords que estão gravadas no banco.
    let engine = Engine::open(&args.db, args.config.clone())?;
    let stopwords = engine.stopwords().clone();
    // E o mesmo stemming: o termo de um token é o radical dele.
    let stemmer = engine.stemmer().clone();
    let term_of = |t: &str| if stemmer.is_on() { stemmer.stem(t) } else { t.to_string() };
    // Radical -> uma palavra que o gera. As consultas são escritas com
    // palavras: o radical de um radical pode ser outro (`brasileir` → `brasil`).
    let mut word_of: HashMap<String, String> = HashMap::default();
    let has_positions = engine.has_positions();
    const AND_DOCS: usize = 2000;
    const PHRASE_DOCS: usize = 500;

    // 1. Índice ingênuo em memória: documentos, tf e tamanhos.
    let mut o = Oracle {
        terms: HashMap::default(),
        positions: HashMap::default(),
        lens: HashMap::default(),
        spans: HashMap::default(),
        total_len: 0,
        total_title_len: 0,
        k1: engine.config().bm25_k1,
        b: engine.config().bm25_b,
        title_weight: engine.config().title_weight,
        title_b: engine.config().title_b,
        max_prefix: engine.config().max_prefix_terms,
        by_root: HashMap::default(),
        inflector: engine.inflector().is_on().then(|| engine.inflector().clone()),
        alpha: engine.config().inflection_weight,
        mode: engine.config().inflection_mode,
    };
    let mut sampled_docs: Vec<Vec<String>> = Vec::new();
    let mut phrase_docs: Vec<Vec<String>> = Vec::new();
    let mut titles: Vec<(DocId, String)> = Vec::new();
    let (mut docs, mut repeated) = (0usize, 0usize);
    for doc in Corpus::open(&args.corpus, 0, args.take, args.remap_ids)? {
        let doc = doc?;
        docs += 1;
        let (positioned, n_title) = doc_tokens(&doc.title, &doc.text);
        let tokens: Vec<String> = positioned.iter().map(|(t, _)| t.clone()).collect();
        // termo -> (tf, tf no título)
        let mut counts: HashMap<String, (u32, u32)> = HashMap::default();
        let (mut len, mut title_len) = (0u32, 0u32);
        for (i, t) in tokens.iter().enumerate().filter(|(_, t)| !stopwords.contains(t)) {
            let in_title = i < n_title;
            let term = term_of(t);
            if stemmer.is_on() && !word_of.contains_key(&term) {
                word_of.insert(term.clone(), t.clone());
            }
            let c = counts.entry(term).or_insert((0, 0));
            c.0 += 1;
            c.1 += u32::from(in_title);
            len += 1;
            title_len += u32::from(in_title);
        }
        // Reservatórios: documentos para consultas AND e frases, títulos para revocação.
        let mut unique: Vec<String> = counts.keys().cloned().collect();
        unique.sort_unstable();
        if sampled_docs.len() < AND_DOCS {
            sampled_docs.push(unique);
        } else if rng.below(docs) < AND_DOCS {
            let j = rng.below(AND_DOCS);
            sampled_docs[j] = unique;
        }
        if phrase_docs.len() < PHRASE_DOCS {
            phrase_docs.push(tokens.clone());
        } else if rng.below(docs) < PHRASE_DOCS {
            let j = rng.below(PHRASE_DOCS);
            phrase_docs[j] = tokens.clone();
        }
        if !index_terms(&doc.title, &stopwords).is_empty() {
            if titles.len() < args.titles {
                titles.push((doc.id, doc.title.clone()));
            } else if rng.below(docs) < args.titles {
                let j = rng.below(args.titles);
                titles[j] = (doc.id, doc.title.clone());
            }
        }
        let add = |e: &mut OTerm, (tf, title_tf): (u32, u32)| {
            e.docs.push(doc.id);
            e.tfs.push(tf);
            e.title_tfs.push(title_tf);
        };
        for (t, c) in counts {
            match o.terms.get_mut(&t) {
                Some(e) => add(e, c),
                None => add(o.terms.entry(t).or_default(), c),
            }
        }
        o.spans.insert(doc.id, n_title as u32);
        match o.lens.insert(doc.id, (len, title_len)) {
            Some((old, old_title)) => {
                repeated += 1;
                o.total_len = o.total_len + u64::from(len) - u64::from(old);
                o.total_title_len = o.total_title_len + u64::from(title_len) - u64::from(old_title);
            }
            None => {
                o.total_len += u64::from(len);
                o.total_title_len += u64::from(title_len);
            }
        }
        if docs.is_multiple_of(20_000) {
            eprintln!("oráculo: {docs} docs, {} termos ({:.0}s)", o.terms.len(), started.elapsed().as_secs_f64());
        }
    }
    for t in o.terms.values_mut() {
        t.finish();
    }
    if let Some(inflector) = &o.inflector {
        let mut by_root: HashMap<String, Vec<String>> = HashMap::default();
        for word in o.terms.keys() {
            by_root.entry(inflector.stem(word)).or_default().push(word.clone());
        }
        o.by_root = by_root;
    }
    let mut vocab: Vec<&str> = o.terms.keys().map(String::as_str).collect();
    vocab.sort_unstable();
    eprintln!("oráculo pronto: {docs} docs, {} termos ({:.0}s)", vocab.len(), started.elapsed().as_secs_f64());
    // Uma palavra para usar numa consulta no lugar do termo.
    let word = |t: &str| -> String { word_of.get(t).cloned().unwrap_or_else(|| t.to_string()) };
    let parse = |q: &str| parse_query_stemmed(q, &stopwords, &stemmer);

    // 2. Termos a conferir: todos, ou uma amostra estratificada por df (potências de 2).
    let full = args.sample_terms == 0;
    let check_terms: Vec<&str> = if full {
        vocab.clone()
    } else {
        let mut buckets: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        for term in &vocab {
            buckets.entry(o.terms[*term].docs.len().ilog2()).or_default().push(term);
        }
        let per_bucket = args.sample_terms.div_ceil(buckets.len().max(1));
        let mut picked: Vec<&str> =
            buckets.values().flat_map(|terms| (0..per_bucket.min(terms.len())).map(|_| *rng.pick(terms)).collect::<Vec<_>>()).collect();
        picked.sort_unstable();
        picked.dedup();
        picked
    };

    // 3. Frases para conferir: trechos reais, com e sem stopword no meio, e
    //    trechos com a ordem trocada (que quase nunca existem).
    let is_stop = |t: &String| stopwords.contains(t);
    let mut phrase_cases: Vec<Vec<String>> = Vec::new();
    let usable: Vec<&Vec<String>> = phrase_docs.iter().filter(|d| d.len() >= 3).collect();
    for i in 0..if usable.is_empty() { 0 } else { args.phrases } {
        let kind = i % 4;
        let mut window = Vec::new();
        for _ in 0..20 {
            let doc = rng.pick(&usable);
            let len = if kind == 1 { 3 } else { rng.range(2, 4).min(doc.len()) };
            let start = rng.below(doc.len() - len + 1);
            window = doc[start..start + len].to_vec();
            let good = match kind {
                0 | 3 => window.iter().all(|t| !is_stop(t)),
                1 => !is_stop(&window[0]) && is_stop(&window[1]) && !is_stop(&window[2]),
                _ => true,
            };
            if good {
                break;
            }
        }
        if kind == 3 {
            window.reverse();
        }
        phrase_cases.push(window);
    }

    // 4. Consultas de ranking: termo frequente, termo qualquer, E, prefixo,
    //    frase e título (onde o peso do título mais pesa).
    let frequent: Vec<&str> = vocab.iter().copied().filter(|t| o.terms[*t].docs.len() >= 50).collect();
    let mut rank_cases: Vec<String> = Vec::new();
    for i in 0..args.rank_queries {
        let doc = rng.pick(&sampled_docs);
        let query = match i % 6 {
            5 if !titles.is_empty() => rng.pick(&titles).1.clone(),
            0 if !frequent.is_empty() => word(rng.pick::<&str>(&frequent)),
            0 | 1 => word(rng.pick::<&str>(&vocab)),
            2 if doc.len() >= 2 => (0..rng.range(2, 3)).map(|_| word(rng.pick(doc).as_str())).collect::<Vec<_>>().join(" "),
            3 if !doc.is_empty() => {
                let other = word(rng.pick(doc).as_str());
                let cut: String = other.chars().take(rng.range(2, 4)).collect();
                format!("{} {cut}*", word(rng.pick(doc).as_str()))
            }
            4 if has_positions && !usable.is_empty() => {
                let tokens = rng.pick(&usable);
                let len = rng.range(2, 3);
                let start = rng.below(tokens.len() - len + 1);
                let phrase = quote(&tokens[start..start + len]);
                // Metade com um termo do mesmo artigo junto.
                match tokens.iter().filter(|t| !is_stop(t)).nth(rng.below(8)) {
                    Some(extra) if rng.below(2) == 0 => format!("{phrase} {extra}"),
                    _ => phrase,
                }
            }
            _ => continue,
        };
        rank_cases.push(query);
    }

    // 5. Posições: uma segunda passada, só para os termos que serão usados
    //    (todos, se a conferência for completa).
    if has_positions {
        let mut needed: HashSet<String> = HashSet::default();
        if !full {
            needed.extend(check_terms.iter().map(|t| t.to_string()));
            needed.extend(phrase_cases.iter().flatten().map(|t| term_of(t)));
            for q in &rank_cases {
                for c in parse(q) {
                    if let Clause::Phrase(parts) = c {
                        needed.extend(parts.into_iter().map(|p| p.text));
                    }
                }
            }
        }
        for doc in Corpus::open(&args.corpus, 0, args.take, args.remap_ids)? {
            let doc = doc?;
            let mut found: HashMap<String, Vec<u32>> = HashMap::default();
            let (tokens, _) = doc_tokens(&doc.title, &doc.text);
            for (t, p) in &tokens {
                if stopwords.contains(t) {
                    continue;
                }
                let term = term_of(t);
                if full || needed.contains(&term) {
                    found.entry(term).or_default().push(*p);
                }
            }
            for (t, ps) in found {
                match o.positions.get_mut(&t) {
                    Some(e) => e.push(doc.id, &ps),
                    None => o.positions.entry(t).or_default().push(doc.id, &ps),
                }
            }
        }
        for p in o.positions.values_mut() {
            p.finish();
        }
        eprintln!("posições do oráculo: {} termos ({:.0}s)", o.positions.len(), started.elapsed().as_secs_f64());
    }

    let max_prefix = o.max_prefix;
    let mut checks = Vec::new();

    // 6. Listas de postagem (doc, tf) e posições.
    let mut postings = Check::new("listas (doc, tf)");
    let mut positions = Check::new("posições");
    let node_of: HashMap<String, u64> = {
        let mut nodes = HashMap::default();
        engine.for_each_term(|term, node| {
            nodes.insert(term.to_string(), node);
        })?;
        nodes
    };
    let posting_list = |t: &OTerm| -> Vec<fire_flower::Posting> {
        (0..t.docs.len()).map(|i| fire_flower::Posting { doc: t.docs[i], tf: t.tfs[i], title_tf: t.title_tfs[i] }).collect()
    };
    for term in &check_terms {
        let node = node_of.get(*term).copied();
        let got = match node {
            Some(n) => engine.term_postings_of_node(n)?,
            None => Vec::new(),
        };
        postings.compare(term, &got, &posting_list(&o.terms[*term]));
        if has_positions {
            let got = match node {
                Some(n) => engine.positions_of_node(n)?,
                None => Vec::new(),
            };
            let expected = o.positions.get(*term).map(OPos::entries).unwrap_or_default();
            positions.compare(term, &got, &expected);
        }
    }
    if full {
        // O motor não pode ter termo a mais.
        for (term, node) in &node_of {
            if !o.terms.contains_key(term) {
                postings.compare(term, &engine.term_postings_of_node(*node)?, &Vec::new());
            }
        }
        postings.notes.push(format!("{} termos no motor, {} no oráculo", node_of.len(), vocab.len()));
    } else {
        postings.notes.push(format!("amostra de {} termos estratificada por df", check_terms.len()));
    }
    if !has_positions {
        positions.notes.push("banco sem posições: nada a conferir".into());
    }
    checks.push(postings);
    checks.push(positions);

    // 7. Tamanhos dos documentos e estatísticas.
    let mut lengths = Check::new("tamanhos e estatísticas");
    let mut engine_lens: Vec<(DocId, (u32, u32))> = Vec::new();
    engine.for_each_doc_len(|d, l, t| engine_lens.push((d, (l, t))))?;
    engine_lens.sort_unstable();
    let mut oracle_lens: Vec<(DocId, (u32, u32))> = o.lens.iter().map(|(&d, &l)| (d, l)).collect();
    oracle_lens.sort_unstable();
    lengths.compare("documentos com tamanho", &engine_lens.len(), &oracle_lens.len());
    let engine_map: HashMap<DocId, (u32, u32)> = engine_lens.into_iter().collect();
    for (doc, lens) in &oracle_lens {
        lengths.compare(&format!("tamanhos (inteiro, título) do {doc}"), &engine_map.get(doc).copied(), &Some(*lens));
    }
    let stats = engine.stats()?;
    lengths.compare("doc_count", &stats.doc_count, &(o.lens.len() as u64));
    lengths.compare("total_len", &stats.total_len, &o.total_len);
    lengths.compare("total_title_len", &stats.total_title_len, &o.total_title_len);
    if repeated > 0 {
        lengths.notes.push(format!("{repeated} IDs repetidos no corpus (a última versão vence)"));
    }
    checks.push(lengths);

    // 8. AND: metade com termos do mesmo documento (resultado não vazio).
    let mut and = Check::new("AND");
    for i in 0..args.and_queries {
        let k = rng.range(2, 5);
        let terms: Vec<String> = if i % 2 == 0 {
            let doc = rng.pick(&sampled_docs);
            if doc.len() < k {
                continue;
            }
            (0..k).map(|_| rng.pick(doc).clone()).collect()
        } else {
            if frequent.is_empty() {
                continue;
            }
            (0..k).map(|_| rng.pick(&frequent).to_string()).collect()
        };
        let query = terms.iter().map(|t| word(t)).collect::<Vec<_>>().join(" ");
        let expected = if o.inflector.is_some() {
            o.resolve(&vocab, &parse(&query)).map(|(_, docs)| docs).unwrap_or_default()
        } else {
            let mut expected = o.terms[&terms[0]].docs.clone();
            for t in &terms[1..] {
                expected = fire_flower::internals::intersect(&expected, &o.terms[t].docs);
            }
            expected
        };
        and.compare(&query, &engine.search(&query)?, &expected);
    }
    checks.push(and);

    // 9. Prefixo e autocompletar.
    let mut prefix = Check::new("prefixo");
    let mut suggest = Check::new("autocompletar");
    let (mut truncated, mut ignored_terms) = (0usize, 0usize);
    for _ in 0..args.prefixes {
        let term = rng.pick(&vocab);
        let n = *rng.pick(&[1usize, 2, 2, 3, 3, 4, 6]);
        let p: String = term.chars().take(n).collect();
        let start = vocab.partition_point(|t| *t < p.as_str());
        let mut matches: Vec<&str> = vocab[start..].iter().copied().take_while(|t| t.starts_with(p.as_str())).collect();
        matches.sort_by(trie_order);
        if matches.len() > max_prefix {
            truncated += 1;
            ignored_terms += matches.len() - max_prefix;
        }
        let mut expanded: Vec<&str> = matches.iter().copied().take(max_prefix).collect();
        // Com stemming, o radical do prefixo também vale (ver `QueryTerm::stem`).
        let stem = term_of(&p);
        if stemmer.is_on() && o.terms.contains_key(&stem) && !expanded.contains(&stem.as_str()) {
            expanded.push(&stem);
        }
        let mut expected: Vec<DocId> = expanded.iter().flat_map(|t| o.terms[*t].docs.iter().copied()).collect();
        expected.sort_unstable();
        expected.dedup();
        prefix.compare(&format!("{p}*"), &engine.search(&format!("{p}*"))?, &expected);

        let expected_words: Vec<String> = matches.iter().take(10).map(|t| t.to_string()).collect();
        suggest.compare(&p, &engine.suggest(&p, 10)?, &expected_words);
    }
    prefix.notes.push(format!(
        "{truncated} de {} prefixos passaram de {max_prefix} termos; {ignored_terms} termos ignorados no total (resultado incompleto por projeto)",
        prefix.checked
    ));
    checks.push(prefix);
    checks.push(suggest);

    // 10. Título → artigo (revocação) e, com ranking, em que posição ele vem.
    let mut recall = Check::new("título → artigo");
    let (mut top1, mut top10, mut rr, mut missing) = (0usize, 0usize, 0.0f64, 0usize);
    for (id, title) in &titles {
        let found = engine.search(title)?.binary_search(id).is_ok();
        recall.compare(title, &found, &true);
        let hits = engine.search_top(title, 1000)?;
        match hits.iter().position(|h| h.doc == *id) {
            Some(i) => {
                top1 += usize::from(i == 0);
                top10 += usize::from(i < 10);
                rr += 1.0 / (i + 1) as f64;
            }
            None => missing += 1,
        }
    }
    checks.push(recall);
    let n_titles = titles.len().max(1) as f64;
    let relevance = json!({
        "consultas": titles.len(), "top1": top1 as f64 / n_titles, "top10": top10 as f64 / n_titles,
        "mrr": rr / n_titles, "fora_do_top1000": missing,
    });

    // 11. Normalização.
    let mut norm = Check::new("normalização");
    for group in [["São Paulo", "sao paulo", "SÃO PAULO"], ["Ação", "acao", "AÇÃO"], ["Coração", "coracao", "CORAÇÃO"], ["Península Ibérica", "peninsula iberica", "PENÍNSULA IBÉRICA"]] {
        let base = engine.search(group[0])?;
        for variant in &group[1..] {
            norm.compare(&format!("{} vs {variant}", group[0]), &engine.search(variant)?, &base);
        }
    }
    checks.push(norm);

    // 12. Frases: o esperado sai dos tokens do trecho, sem o `parse_query`.
    let mut phrase = Check::new("frases");
    let (mut phrase_hits, mut phrase_empty) = (0usize, 0usize);
    for window in &phrase_cases {
        let query = quote(window);
        let mut kept: Vec<(u32, String)> = Vec::new();
        let first = window.iter().position(|t| !is_stop(t));
        for (i, t) in window.iter().enumerate() {
            if let Some(f) = first
                && !is_stop(t)
            {
                kept.push(((i - f) as u32, term_of(t)));
            }
        }
        if !has_positions {
            // Frase de verdade num banco sem posições tem que dar erro, não resultado errado.
            if kept.len() >= 2 {
                phrase.verdict(&query, engine.search(&query).is_err(), || ("resultado".into(), "erro de config".into()));
            }
            continue;
        }
        let expected: Vec<DocId> = match kept.len() {
            0 => Vec::new(),
            1 if o.inflector.is_some() => o.resolve(&vocab, &parse(&query)).map(|(_, docs)| docs).unwrap_or_default(),
            1 => o.terms.get(&kept[0].1).map(|t| t.docs.clone()).unwrap_or_default(),
            _ => {
                let parts: Option<Vec<(u32, &OTerm, &OPos)>> =
                    kept.iter().map(|(off, t)| Some((*off, o.terms.get(t)?, o.positions.get(t)?))).collect();
                match parts {
                    Some(parts) => {
                        let mut docs = parts[0].1.docs.clone();
                        for (_, t, _) in &parts[1..] {
                            docs = fire_flower::internals::intersect(&docs, &t.docs);
                        }
                        docs.retain(|&d| Oracle::phrase_count(&parts, d, 0).0 > 0);
                        docs
                    }
                    None => Vec::new(),
                }
            }
        };
        if expected.is_empty() { phrase_empty += 1 } else { phrase_hits += 1 }
        phrase.compare(&query, &engine.search(&query)?, &expected);
    }
    phrase.notes.push(format!("{phrase_hits} frases com resultado, {phrase_empty} sem (ordem trocada, em geral)"));
    checks.push(phrase);

    // 13. Ranking: os 10 primeiros e o total, contra o BM25 calculado aqui.
    let mut ranking = Check::new("ranking BM25 (top 10)");
    for q in &rank_cases {
        let clauses = parse(q);
        let Some((want, total)) = o.rank(&vocab, &clauses, 10) else { continue };
        let got = engine.search_ranked(q, 10)?;
        ranking.verdict(q, got.total == total && same_ranking(&got.hits, &want), || {
            let g: Vec<(DocId, f64)> = got.hits.iter().map(|h| (h.doc, h.score)).collect();
            (format!("total {} {g:?}", got.total), format!("total {total} {want:?}"))
        });
    }
    ranking.notes.push("termo frequente, termo qualquer, E, termo + prefixo, frase e título; BM25F; notas com diferença relativa < 1e-4".into());
    checks.push(ranking);

    // Relatório.
    let total: usize = checks.iter().map(|c| c.divergences).sum();
    println!("\n{:<24} {:>8} {:>12}", "verificação", "casos", "divergências");
    for c in &checks {
        println!("{:<24} {:>8} {:>12}", c.name, c.checked, c.divergences);
        for ex in &c.examples {
            println!("    ✗ {ex}");
        }
        for note in &c.notes {
            println!("    · {note}");
        }
    }
    println!(
        "\nrelevância (título → artigo, {} consultas): top-1 {:.1}%, top-10 {:.1}%, MRR {:.3}, fora do top-1000 {missing}",
        titles.len(),
        relevance["top1"].as_f64().unwrap_or(0.0) * 100.0,
        relevance["top10"].as_f64().unwrap_or(0.0) * 100.0,
        relevance["mrr"].as_f64().unwrap_or(0.0)
    );
    println!("\n{} em {:.1}s", if total == 0 { "✔ zero divergências".to_string() } else { format!("✗ {total} divergências") }, started.elapsed().as_secs_f64());

    let label = args.label.unwrap_or_else(|| stem(&args.corpus));
    save_result(
        "oracle",
        &label,
        json!({ "db": args.db, "corpus": args.corpus, "take": args.take, "remap_ids": args.remap_ids,
                "docs": docs, "termos_oraculo": vocab.len(), "divergencias": total, "posicoes": has_positions,
                "stemming": stemmer.stemming().code(),
                "flexoes": engine.inflector().stemming().code(),
                "verificacoes": checks.iter().map(Check::json).collect::<Vec<_>>(), "relevancia": relevance }),
    )?;
    if total > 0 {
        std::process::exit(1);
    }
    Ok(())
}
