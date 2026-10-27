//! `wiki queries`: os conjuntos de consulta da Fase 3.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use fire_flower::Stopwords;
use fire_flower::analysis::{index_terms, tokenize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::AnyResult;
use crate::corpus::Corpus;
use crate::env::project_dir;
use crate::relevance::regular_inflections;
use crate::rng::Rng;
use crate::vocab::{VocabEntry, read_vocab};

/// Artigos cujos tokens o `queries` guarda para sortear frases.
pub(crate) const SNIPPET_DOCS: usize = 2000;

pub(crate) fn queries(vocab_path: &Path, corpus_path: &Path, stopwords: &Stopwords, seed: u64, per_set: usize, out: Option<PathBuf>) -> AnyResult {
    let vocab = read_vocab(vocab_path)?;
    let mut rng = Rng(seed);
    let known: HashSet<&str> = vocab.iter().map(|v| v.term.as_str()).collect();
    let band = |lo: u32, hi: u32| -> Vec<&str> {
        vocab.iter().filter(|v| v.df >= lo && v.df <= hi).map(|v| v.term.as_str()).collect()
    };
    let rare = band(1, 10);
    let medium = band(100, 10_000);
    let freq = band(100_000, u32::MAX);
    let freq = if freq.is_empty() {
        // Corpus pequeno: os 50 termos mais frequentes fazem o papel.
        let mut all: Vec<&VocabEntry> = vocab.iter().collect();
        all.sort_by_key(|v| std::cmp::Reverse(v.df));
        all.iter().take(50).map(|v| v.term.as_str()).collect()
    } else {
        freq
    };
    let by_len = |len: usize| -> Vec<&str> {
        let exact: Vec<&str> = vocab.iter().filter(|v| v.len == len && (100..=10_000).contains(&v.df)).map(|v| v.term.as_str()).collect();
        if exact.len() >= 50 { exact } else { vocab.iter().filter(|v| v.len == len && v.df >= 10).map(|v| v.term.as_str()).collect() }
    };

    let mut sets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let sample = |rng: &mut Rng, pool: &[&str]| -> Vec<String> {
        if pool.is_empty() { Vec::new() } else { (0..per_set).map(|_| rng.pick(pool).to_string()).collect() }
    };
    sets.insert("t-rare".into(), sample(&mut rng, &rare));
    sets.insert("t-mid".into(), sample(&mut rng, &medium));
    sets.insert("t-freq".into(), sample(&mut rng, &freq));
    for len in [3, 8, 15] {
        sets.insert(format!("t-len-{len}"), sample(&mut rng, &by_len(len)));
    }

    // Termos inexistentes: quase-acertos (divergem só no fim) e aleatórios.
    let mut miss = Vec::new();
    while miss.len() < per_set {
        let candidate = match miss.len() % 4 {
            0 | 1 => {
                let base = rng.pick(&medium);
                let extra = (b'a' + rng.below(26) as u8) as char;
                format!("{base}{extra}")
            }
            2 => (0..rng.range(3, 6)).map(|_| (b'a' + rng.below(26) as u8) as char).collect(),
            _ => (0..rng.range(12, 20)).map(|_| (b'a' + rng.below(26) as u8) as char).collect(),
        };
        if !known.contains(candidate.as_str()) {
            miss.push(candidate);
        }
    }
    sets.insert("t-miss".into(), miss);

    let and_of = |rng: &mut Rng, pools: &[&[&str]]| -> String {
        pools.iter().map(|p| *rng.pick(p)).collect::<Vec<_>>().join(" ")
    };
    sets.insert("and-rare-freq".into(), (0..per_set).map(|_| and_of(&mut rng, &[&rare, &freq])).collect());
    sets.insert(
        "and-freq".into(),
        (0..per_set).map(|i| if i % 2 == 0 { and_of(&mut rng, &[&freq, &freq]) } else { and_of(&mut rng, &[&freq, &freq, &freq]) }).collect(),
    );
    for n in [2usize, 3, 5] {
        let pools = vec![&medium[..]; n];
        sets.insert(format!("and-{n}"), (0..per_set).map(|_| and_of(&mut rng, &pools)).collect());
    }

    // Prefixos: cortes de termos médios; p-raro = prefixo com até 5 completamentos.
    let sorted: Vec<&str> = vocab.iter().map(|v| v.term.as_str()).collect(); // já em ordem
    let completions = |prefix: &str| {
        let start = sorted.partition_point(|t| *t < prefix);
        sorted[start..].iter().take_while(|t| t.starts_with(prefix)).count()
    };
    let cut = |term: &str, n: usize| term.chars().take(n).collect::<String>();
    let prefix_set = |rng: &mut Rng, lo: usize, hi: usize| -> Vec<String> {
        let mut out = Vec::new();
        while out.len() < per_set {
            let term = rng.pick(&medium);
            let n = rng.range(lo, hi);
            if term.chars().count() >= n {
                out.push(cut(term, n));
            }
        }
        out
    };
    let p1 = prefix_set(&mut rng, 1, 1);
    let p2 = prefix_set(&mut rng, 2, 3);
    let p4 = prefix_set(&mut rng, 4, 6);
    let mut p_rare = Vec::new();
    let mut attempts = 0;
    while p_rare.len() < per_set && attempts < per_set * 50 {
        attempts += 1;
        let term = rng.pick(&rare);
        let chars = term.chars().count();
        if let Some(n) = (3..=chars).find(|&n| completions(&cut(term, n)) <= 5) {
            p_rare.push(cut(term, n));
        }
    }
    for (name, list) in [("1", &p1), ("2", &p2), ("4", &p4), ("rare", &p_rare)] {
        sets.insert(format!("p-{name}"), list.iter().map(|p| format!("{p}*")).collect());
        sets.insert(format!("s-{name}"), list.clone());
    }

    // Títulos reais: amostragem de reservatório sobre o corpus inteiro.
    // Junto, os tokens de alguns artigos, de onde saem as frases. Gerador
    // próprio: os conjuntos que já existiam continuam iguais.
    let mut srng = Rng(seed ^ 0x00F1_A5E5);
    let mut snippets: Vec<Vec<String>> = Vec::with_capacity(SNIPPET_DOCS);
    let mut docs_seen = 0usize;
    let mut titles: Vec<String> = Vec::with_capacity(per_set);
    let mut seen = 0usize;
    for doc in Corpus::open(corpus_path, 0, None, false)? {
        let doc = doc?;
        docs_seen += 1;
        if snippets.len() < SNIPPET_DOCS {
            snippets.push(tokenize(&doc.body));
        } else {
            let j = srng.below(docs_seen);
            if j < SNIPPET_DOCS {
                snippets[j] = tokenize(&doc.body);
            }
        }
        // Título só de stopwords ("Ela") vira consulta vazia.
        if index_terms(&doc.title, stopwords).is_empty() {
            continue;
        }
        seen += 1;
        if titles.len() < per_set {
            titles.push(doc.title);
        } else {
            let j = rng.below(seen);
            if j < per_set {
                titles[j] = doc.title;
            }
        }
    }
    sets.insert("titles".into(), titles);

    // Misto: 70% títulos/termos médios, 20% AND, 10% prefixo.
    let mut mixed = Vec::new();
    for i in 0..per_set {
        let source = match i % 10 {
            0..=3 => "titles",
            4..=6 => "t-mid",
            7 => "and-2",
            8 => "and-rare-freq",
            _ => "p-4",
        };
        mixed.push(rng.pick(&sets[source]).clone());
    }
    sets.insert("mixed".into(), mixed);

    // Frases (Fase E do plano de frases e ranking), tiradas de trechos reais.
    let df: HashMap<&str, u32> = vocab.iter().map(|v| (v.term.as_str(), v.df)).collect();
    let mut by_df: Vec<u32> = vocab.iter().map(|v| v.df).collect();
    by_df.sort_unstable_by(|a, b| b.cmp(a));
    // "Frequente" para frases: os 500 termos de maior df.
    let freq_df = by_df.get(499).or(by_df.last()).copied().unwrap_or(1);
    let is_stop = |t: &str| stopwords.contains(t);
    let indexed = |t: &str| !is_stop(t) && df.contains_key(t);
    let phrases = |rng: &mut Rng, len: usize, want: &dyn Fn(&[String]) -> bool| -> Vec<Vec<String>> {
        let mut out = Vec::new();
        let mut attempts = 0;
        while out.len() < per_set && attempts < per_set * 500 && !snippets.is_empty() {
            attempts += 1;
            let doc = rng.pick(&snippets);
            if doc.len() < len {
                continue;
            }
            let start = rng.below(doc.len() - len + 1);
            let w = &doc[start..start + len];
            if want(w) {
                out.push(w.to_vec());
            }
        }
        out
    };
    let quote = |w: &[String]| format!("\"{}\"", w.join(" "));
    let f2 = phrases(&mut srng, 2, &|w| w.iter().all(|t| indexed(t)));
    let f3 = phrases(&mut srng, 3, &|w| w.iter().all(|t| indexed(t)));
    let fstop = phrases(&mut srng, 3, &|w| indexed(&w[0]) && is_stop(&w[1]) && indexed(&w[2]));
    let ffreq = phrases(&mut srng, 2, &|w| w.iter().all(|t| indexed(t) && df[t.as_str()] >= freq_df));
    // Palavras reais em ordem trocada: quase nunca existem.
    let fmiss: Vec<String> = f2
        .iter()
        .chain(&f3)
        .filter(|w| w.first() != w.last())
        .map(|w| w.iter().rev().cloned().collect::<Vec<_>>())
        .take(per_set)
        .map(|w| quote(&w))
        .collect();
    for (name, list) in [("phrase-2", &f2), ("phrase-3", &f3), ("phrase-stop", &fstop), ("phrase-freq", &ffreq)] {
        sets.insert(name.into(), list.iter().map(|w| quote(w)).collect());
    }
    sets.insert("phrase-miss".into(), fmiss);

    // Ranking (medidos com os 10 melhores): termo frequente, E de termos
    // que aparecem juntos e títulos.
    sets.insert("rank-freq".into(), sample(&mut srng, &freq));
    let mut rank_and = Vec::new();
    let mut attempts = 0;
    while rank_and.len() < per_set && attempts < per_set * 50 && !snippets.is_empty() {
        attempts += 1;
        let doc = srng.pick(&snippets);
        let pool: Vec<&String> = doc.iter().filter(|t| indexed(t) && df[t.as_str()] >= 100).collect();
        let k = srng.range(2, 3);
        if pool.len() < k {
            continue;
        }
        let mut terms: Vec<&str> = (0..k).map(|_| srng.pick(&pool).as_str()).collect();
        terms.dedup();
        if terms.len() >= 2 {
            rank_and.push(terms.join(" "));
        }
    }
    sets.insert("rank-and".into(), rank_and);
    sets.insert("rank-titles".into(), sets["titles"].clone());

    // Palavras de frequência média cuja forma regular de plural ou singular
    // também está no índice (para medir o custo de somar as flexões). Gerador
    // próprio: os conjuntos acima não mudam.
    let mut frng = Rng(seed ^ 0xf1e0_5eed);
    let with_other: Vec<&str> = medium
        .iter()
        .copied()
        .filter(|t| regular_inflections(t).iter().any(|v| df.get(v.as_str()).is_some_and(|&n| n >= 10)))
        .collect();
    sets.insert("t-inflection".into(), sample(&mut frng, &with_other));

    let out = out.unwrap_or_else(|| project_dir().join("bench/queries"));
    std::fs::create_dir_all(&out)?;
    for (name, list) in &sets {
        let mut w = BufWriter::new(File::create(out.join(format!("{name}.txt")))?);
        for q in list {
            writeln!(w, "{q}")?;
        }
        w.flush()?;
        println!("{name:<14} {:>5} consultas  ex.: {}", list.len(), list.first().map_or("", String::as_str));
    }
    println!("conjuntos em {}", out.display());
    Ok(())
}
