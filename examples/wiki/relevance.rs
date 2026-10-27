//! `wiki relevancia`: consultas com resposta conhecida (top-1, top-3, top-10, MRR).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use fire_flower::{Config, DocId, Engine, Language, Stemming, Stopwords};
use fire_flower::analysis::{index_terms, tokenize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde_json::{Value, json};

use crate::AnyResult;
use crate::corpus::{Corpus, stem};
use crate::env::{config_json, save_result};
use crate::rng::Rng;

/// Uma consulta com resposta conhecida: algum dos `expected` tem que vir no topo.
pub(crate) struct RelCase {
    pub(crate) query: String,
    pub(crate) expected: Vec<DocId>,
    /// O título esperado (só para mostrar).
    pub(crate) title: String,
}

/// Conjuntos gerados do corpus e a posição que conta como acerto em cada um
/// (a métrica principal; top-1, top-3, top-10 e MRR saem para todos).
pub(crate) const REL_SETS: [(&str, usize); 6] = [
    ("rel-titulo", 1),
    ("rel-digitando", 1),
    ("rel-hub", 1),
    ("rel-corpo", 10),
    ("rel-flexao", 10),
    ("rel-plural", 10),
];

/// Plural e singular regulares de uma palavra normalizada (sem acentos),
/// pelas regras do português, sem stemmer: "eleicoes" → "eleicao",
/// "papel" → "papeis", "casa" → "casas". Os candidatos podem não existir; quem
/// chama confere no vocabulário.
pub(crate) fn regular_inflections(w: &str) -> Vec<String> {
    let n = w.chars().count();
    if n < 4 || !w.chars().all(|c| c.is_ascii_lowercase()) {
        return Vec::new();
    }
    let swap = |from: &str, to: &str| w.strip_suffix(from).map(|stem| format!("{stem}{to}"));
    let mut out: Vec<String> = Vec::new();
    if w.ends_with('s') {
        let rules: &[(&str, &str)] = &[
            ("coes", "cao"), ("oes", "ao"), ("aes", "ao"), ("ais", "al"), ("eis", "el"), ("ois", "ol"),
            ("uis", "ul"), ("ns", "m"), ("res", "r"), ("zes", "z"), ("s", ""),
        ];
        out.extend(rules.iter().filter_map(|(f, t)| swap(f, t)));
    } else {
        let rules: &[(&str, &str)] = &[
            ("cao", "coes"), ("ao", "oes"), ("ao", "aes"), ("ao", "aos"), ("al", "ais"), ("el", "eis"),
            ("ol", "ois"), ("ul", "uis"), ("m", "ns"),
        ];
        out.extend(rules.iter().filter_map(|(f, t)| swap(f, t)));
        match w.chars().last() {
            Some('r' | 'z') => out.push(format!("{w}es")),
            Some('a' | 'e' | 'i' | 'o' | 'u') => out.push(format!("{w}s")),
            _ => {}
        }
    }
    out.retain(|v| v != w && v.chars().count() >= 3);
    out.dedup();
    out
}

pub(crate) fn write_rel_set(path: &Path, cases: &[RelCase]) -> AnyResult {
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(w, "# consulta\tIDs esperados\ttítulo (gerado por `wiki relevancia`)")?;
    for c in cases {
        let ids: Vec<String> = c.expected.iter().map(u64::to_string).collect();
        writeln!(w, "{}\t{}\t{}", c.query, ids.join(","), c.title)?;
    }
    Ok(())
}

pub(crate) fn read_rel_set(path: &Path) -> AnyResult<Vec<RelCase>> {
    let mut out = Vec::new();
    for (i, line) in std::fs::read_to_string(path)?.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let (Some(query), Some(ids)) = (cols.next(), cols.next()) else {
            return Err(format!("{}:{}: linha sem IDs", path.display(), i + 1).into());
        };
        let expected = ids
            .split(',')
            .map(|id| id.trim().parse::<DocId>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?;
        out.push(RelCase { query: query.to_string(), expected, title: cols.next().unwrap_or("").to_string() });
    }
    Ok(out)
}

/// Reservatório: guarda `n` itens sorteados de um fluxo de tamanho desconhecido.
pub(crate) fn reservoir<T>(rng: &mut Rng, kept: &mut Vec<T>, seen: usize, n: usize, item: impl FnOnce() -> T) {
    if kept.len() < n {
        kept.push(item());
    } else if rng.below(seen) < n {
        let j = rng.below(n);
        kept[j] = item();
    }
}

/// Gera os conjuntos rel-* a partir do corpus (seed fixa: mesmas consultas).
///
/// - `rel-titulo`: o título inteiro → o artigo.
/// - `rel-digitando`: o título com a última palavra cortada em ~60% e `*`,
///   como o TUI busca enquanto se digita (`coreia do nor*`).
/// - `rel-hub`: títulos cujos termos aparecem em ≥ 3 outros títulos
///   ("Coreia do Norte" ⊂ "Bandeira da Coreia do Norte"): o artigo principal
///   tem que vir em 1º, acima dos derivados.
/// - `rel-corpo`: 2 termos de frequência média do corpo, fora do título: o
///   artigo no top-10. Segura o exagero do peso do título.
/// - `rel-plural`: como o `rel-flexao`, mas com o plural ou o singular
///   regular (`regular_inflections`), sem depender do Snowball: o artigo no
///   top-10.
/// - `rel-flexao`: as consultas do `rel-corpo` com cada palavra trocada pela
///   flexão mais comum do mesmo radical (Snowball do português) que não está
///   no artigo ("eleitos" → "eleito"): o artigo no top-10. Sem stemming não
///   tem como achar; mede o que o stemming recupera.
pub(crate) fn generate_rel_sets(corpus: &Path, take: Option<u64>, stopwords: &Stopwords, seed: u64, per_set: usize) -> AnyResult<Vec<(&'static str, Vec<RelCase>)>> {
    let mut rng = Rng(seed);
    let mut titles: Vec<(DocId, String, Vec<String>)> = Vec::new();
    let mut df: HashMap<String, u32> = HashMap::default();
    let mut by_title: Vec<RelCase> = Vec::new();
    let mut typing: Vec<RelCase> = Vec::new();
    let mut body_docs: Vec<(DocId, String, Vec<String>)> = Vec::new();
    let (mut seen, mut seen_typing) = (0usize, 0usize);
    for doc in Corpus::open(corpus, 0, take, false)? {
        let doc = doc?;
        let title_terms = index_terms(&doc.title, stopwords);
        let body_terms = index_terms(&doc.body, stopwords);
        for t in &body_terms {
            *df.entry(t.clone()).or_insert(0) += 1;
        }
        if title_terms.is_empty() {
            continue;
        }
        seen += 1;
        let case = || RelCase { query: doc.title.clone(), expected: vec![doc.id], title: doc.title.clone() };
        reservoir(&mut rng, &mut by_title, seen, per_set, case);

        // Digitando: a última palavra do título, se for indexável e tiver ≥ 5 letras.
        let tokens = tokenize(&doc.title);
        if let Some(last) = tokens.last()
            && !stopwords.contains(last)
            && last.chars().count() >= 5
        {
            seen_typing += 1;
            let n = last.chars().count();
            let cut: String = last.chars().take((n * 6).div_ceil(10).max(3).min(n - 1)).collect();
            let mut words = tokens[..tokens.len() - 1].to_vec();
            words.push(format!("{cut}*"));
            let query = words.join(" ");
            reservoir(&mut rng, &mut typing, seen_typing, per_set, || RelCase { query, expected: vec![doc.id], title: doc.title.clone() });
        }

        let outside: Vec<String> = body_terms.into_iter().filter(|t| title_terms.binary_search(t).is_err()).collect();
        reservoir(&mut rng, &mut body_docs, seen, per_set * 3, || (doc.id, doc.title.clone(), outside));
        titles.push((doc.id, doc.title, title_terms));
    }

    // Hubs: índice invertido só dos títulos.
    let mut title_index: HashMap<&str, Vec<u32>> = HashMap::default();
    for (i, (_, _, terms)) in titles.iter().enumerate() {
        for t in terms {
            title_index.entry(t.as_str()).or_default().push(i as u32);
        }
    }
    let mut same_terms: HashMap<&[String], u32> = HashMap::default();
    for (_, _, terms) in &titles {
        *same_terms.entry(terms.as_slice()).or_insert(0) += 1;
    }
    let mut hubs: Vec<RelCase> = Vec::new();
    let mut seen_hubs = 0usize;
    for (id, title, terms) in &titles {
        // Outro título com exatamente os mesmos termos deixaria a resposta ambígua.
        if same_terms[terms.as_slice()] > 1 {
            continue;
        }
        let mut lists: Vec<&Vec<u32>> = terms.iter().map(|t| &title_index[t.as_str()]).collect();
        lists.sort_by_key(|l| l.len());
        if lists[0].len() < 4 {
            continue;
        }
        let mut containing: Vec<u32> = lists[0].clone();
        for l in &lists[1..] {
            containing.retain(|i| l.binary_search(i).is_ok());
            if containing.len() < 4 {
                break;
            }
        }
        if containing.len() >= 4 {
            seen_hubs += 1;
            reservoir(&mut rng, &mut hubs, seen_hubs, per_set, || RelCase { query: title.clone(), expected: vec![*id], title: title.clone() });
        }
    }

    // Corpo: 2 termos com df entre 20 e 2000 que não estão no título.
    let mut body_cases: Vec<RelCase> = Vec::new();
    for (id, title, terms) in &body_docs {
        if body_cases.len() >= per_set {
            break;
        }
        let mid: Vec<&String> = terms.iter().filter(|t| (20..=2000).contains(&df[*t])).collect();
        if mid.len() < 2 {
            continue;
        }
        let a = rng.below(mid.len());
        let mut b = rng.below(mid.len() - 1);
        if b >= a {
            b += 1;
        }
        body_cases.push(RelCase { query: format!("{} {}", mid[a], mid[b]), expected: vec![*id], title: title.clone() });
    }
    // Flexões: radical -> palavras do corpo com ele, da mais comum para a menos.
    let stemmer = fire_flower::Stemmer::new(Stemming::Snowball(Language::Portuguese));
    let mut by_stem: HashMap<String, Vec<(&str, u32)>> = HashMap::default();
    for (word, &n) in &df {
        if n >= 3 {
            by_stem.entry(stemmer.stem(word)).or_default().push((word.as_str(), n));
        }
    }
    for words in by_stem.values_mut() {
        words.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    }
    let mut inflected: Vec<RelCase> = Vec::new();
    for (id, title, terms) in &body_docs {
        if inflected.len() >= per_set {
            break;
        }
        let title_terms = index_terms(title, stopwords);
        let absent = |w: &str| terms.binary_search_by(|t| t.as_str().cmp(w)).is_err() && !title_terms.iter().any(|t| t == w);
        // Termos de frequência média com uma flexão que o artigo não tem.
        let pairs: Vec<&str> = terms
            .iter()
            .filter(|t| (20..=2000).contains(&df[*t]))
            .filter_map(|t| by_stem.get(&stemmer.stem(t))?.iter().map(|(w, _)| *w).find(|w| absent(w)))
            .collect();
        if pairs.len() < 2 {
            continue;
        }
        let a = rng.below(pairs.len());
        let mut b = rng.below(pairs.len() - 1);
        if b >= a {
            b += 1;
        }
        if pairs[a] == pairs[b] {
            continue;
        }
        inflected.push(RelCase { query: format!("{} {}", pairs[a], pairs[b]), expected: vec![*id], title: title.clone() });
    }
    // Plural/singular regular: a outra forma mais comum que o artigo não tem.
    let mut plural: Vec<RelCase> = Vec::new();
    for (id, title, terms) in &body_docs {
        if plural.len() >= per_set {
            break;
        }
        let title_terms = index_terms(title, stopwords);
        let absent = |w: &str| terms.binary_search_by(|t| t.as_str().cmp(w)).is_err() && !title_terms.iter().any(|t| t == w);
        let other = |t: &str| -> Option<String> {
            regular_inflections(t)
                .into_iter()
                .filter(|v| df.get(v).is_some_and(|&n| n >= 3) && !stopwords.contains(v) && absent(v))
                .max_by(|a, b| df[a].cmp(&df[b]).then(b.cmp(a)))
        };
        let pairs: Vec<String> = terms.iter().filter(|t| (20..=2000).contains(&df[*t])).filter_map(|t| other(t)).collect();
        if pairs.len() < 2 {
            continue;
        }
        let a = rng.below(pairs.len());
        let mut b = rng.below(pairs.len() - 1);
        if b >= a {
            b += 1;
        }
        if pairs[a] == pairs[b] {
            continue;
        }
        plural.push(RelCase { query: format!("{} {}", pairs[a], pairs[b]), expected: vec![*id], title: title.clone() });
    }
    Ok(vec![
        ("rel-titulo", by_title),
        ("rel-digitando", typing),
        ("rel-hub", hubs),
        ("rel-corpo", body_cases),
        ("rel-flexao", inflected),
        ("rel-plural", plural),
    ])
}

/// Consultas à mão: `consulta \t títulos esperados (|) \t nota`. Os títulos
/// viram IDs pelo corpus; consultas sem nenhum artigo no corpus são puladas.
pub(crate) fn read_manual(path: &Path, corpus: &Path, take: Option<u64>) -> AnyResult<Vec<RelCase>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut wanted: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let query = cols.next().unwrap_or("").to_string();
        let titles = cols.next().unwrap_or("").split('|').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        wanted.push((query, titles));
    }
    let all: HashSet<&str> = wanted.iter().flat_map(|(_, ts)| ts.iter().map(String::as_str)).collect();
    let mut ids: HashMap<String, DocId> = HashMap::default();
    for doc in Corpus::open(corpus, 0, take, false)? {
        let doc = doc?;
        if all.contains(doc.title.as_str()) {
            ids.insert(doc.title, doc.id);
        }
    }
    let mut out = Vec::new();
    for (query, titles) in wanted {
        let expected: Vec<DocId> = titles.iter().filter_map(|t| ids.get(t).copied()).collect();
        if expected.is_empty() {
            // Numa fatia pequena o artigo pode não existir: a consulta não conta.
            eprintln!("consulta à mão ignorada (artigo fora do corpus): {query} → {}", titles.join(" | "));
            continue;
        }
        out.push(RelCase { query, expected, title: titles.join(" | ") });
    }
    Ok(out)
}

/// Posição (0 = primeiro) do primeiro esperado nos 1000 melhores, e o top 3.
pub(crate) fn rank_of(engine: &Engine, case: &RelCase) -> AnyResult<(Option<usize>, Vec<DocId>)> {
    let hits = engine.search_top(&case.query, 1000)?;
    let pos = hits.iter().position(|h| case.expected.contains(&h.doc));
    Ok((pos, hits.iter().take(3).map(|h| h.doc).collect()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_relevance(
    db: &Path,
    corpus: &Path,
    sets_dir: &Path,
    manual: &Path,
    take: Option<u64>,
    seed: u64,
    per_set: usize,
    generate: bool,
    config: Config,
    brief: bool,
    label: Option<String>,
) -> AnyResult {
    let started = Instant::now();
    let engine = Engine::open(db, config)?;
    let stopwords = engine.stopwords().clone();
    std::fs::create_dir_all(sets_dir)?;

    let missing = REL_SETS.iter().any(|(name, _)| !sets_dir.join(format!("{name}.tsv")).exists());
    if generate || missing {
        eprintln!("gerando conjuntos em {}", sets_dir.display());
        for (name, cases) in generate_rel_sets(corpus, take, &stopwords, seed, per_set)? {
            let path = sets_dir.join(format!("{name}.tsv"));
            if generate || !path.exists() {
                write_rel_set(&path, &cases)?;
            }
        }
    }
    let mut sets: Vec<(String, usize, Vec<RelCase>)> = Vec::new();
    for (name, hit_at) in REL_SETS {
        sets.push((name.to_string(), hit_at, read_rel_set(&sets_dir.join(format!("{name}.tsv")))?));
    }
    sets.push(("rel-manual".into(), 3, read_manual(manual, corpus, take)?));
    // Flexões escritas à mão, ao lado do `manual.tsv`.
    let inflection_manual = manual.with_file_name("flexao-manual.tsv");
    if inflection_manual.exists() {
        sets.push(("rel-flexao-manual".into(), 3, read_manual(&inflection_manual, corpus, take)?));
    }

    let title_of = |doc: DocId| engine.label(doc).ok().flatten().unwrap_or_else(|| doc.to_string());
    let mut results = Vec::new();
    println!("{:<14} {:>8} {:>7} {:>7} {:>7} {:>7} {:>9}", "conjunto", "consultas", "top-1", "top-3", "top-10", "MRR", "fora 1000");
    let mut manual_lines = Vec::new();
    for (name, hit_at, cases) in &sets {
        let (mut t1, mut t3, mut t10, mut rr, mut out) = (0usize, 0usize, 0usize, 0.0f64, 0usize);
        let mut misses: Vec<Value> = Vec::new();
        for case in cases {
            let (pos, top3) = rank_of(&engine, case)?;
            match pos {
                Some(i) => {
                    t1 += usize::from(i < 1);
                    t3 += usize::from(i < 3);
                    t10 += usize::from(i < 10);
                    rr += 1.0 / (i + 1) as f64;
                }
                None => out += 1,
            }
            let hit = pos.is_some_and(|i| i < *hit_at);
            if name == "rel-manual" {
                let mark = if hit { "✔" } else { "✗" };
                let shown: Vec<String> = top3.iter().map(|&d| title_of(d)).collect();
                manual_lines.push(format!(
                    "  {mark} {:<26} {:>6}  → {}",
                    case.query,
                    pos.map_or("—".into(), |i| format!("{}º", i + 1)),
                    shown.join(" · ")
                ));
            }
            if !hit && misses.len() < 8 {
                misses.push(json!({ "consulta": case.query, "esperado": case.title, "posicao": pos.map(|i| i + 1),
                                    "top3": top3.iter().map(|&d| title_of(d)).collect::<Vec<_>>() }));
            }
        }
        let n = cases.len().max(1) as f64;
        let (top1, top3, top10, mrr) = (t1 as f64 / n, t3 as f64 / n, t10 as f64 / n, rr / n);
        println!("{name:<14} {:>8} {:>6.1}% {:>6.1}% {:>6.1}% {mrr:>7.3} {out:>9}", cases.len(), top1 * 100.0, top3 * 100.0, top10 * 100.0);
        results.push(json!({ "conjunto": name, "consultas": cases.len(), "acerto_ate": hit_at, "top1": top1, "top3": top3,
                             "top10": top10, "mrr": mrr, "fora_do_top1000": out, "erros": misses }));
    }
    if !brief {
        println!("\nconsultas à mão (posição do esperado → os 3 primeiros):");
        for line in &manual_lines {
            println!("{line}");
        }
    }
    println!("\n{:.1}s", started.elapsed().as_secs_f64());

    let label = label.unwrap_or_else(|| stem(sets_dir));
    save_result(
        "relevancia",
        &label,
        json!({ "db": db, "corpus": corpus, "take": take, "seed": seed, "config": config_json(engine.config()),
                "conjuntos": results }),
    )?;
    Ok(())
}
