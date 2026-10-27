//! `wiki vocab`: df e comprimento de cada termo do corpus.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use fire_flower::Stopwords;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::AnyResult;
use crate::corpus::{Corpus, stem, unique_terms};
use crate::env::project_dir;

pub(crate) fn vocab(corpus_path: &Path, stopwords: &Stopwords, take: Option<u64>, out: Option<PathBuf>) -> AnyResult {
    let started = Instant::now();
    let mut df: HashMap<String, u32> = HashMap::default();
    let mut ids = HashSet::default();
    let mut duplicates = Vec::new();
    let mut docs = 0u64;
    for doc in Corpus::open(corpus_path, 0, take, false)? {
        let doc = doc?;
        if !ids.insert(doc.id) {
            duplicates.push((doc.id, doc.line));
        }
        for term in unique_terms(&doc.body, stopwords) {
            *df.entry(term).or_insert(0) += 1;
        }
        docs += 1;
        if docs.is_multiple_of(100_000) {
            eprintln!("{docs:>9} docs  {:>9} termos  ({:.0}s)", df.len(), started.elapsed().as_secs_f64());
        }
    }
    let mut terms: Vec<(String, u32)> = df.into_iter().collect();
    terms.sort_unstable();

    let out = out.unwrap_or_else(|| project_dir().join("bench/data").join(format!("vocab-{}.tsv", stem(corpus_path))));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::new(File::create(&out)?);
    for (term, n) in &terms {
        writeln!(w, "{term}\t{n}\t{}", term.chars().count())?;
    }
    w.flush()?;

    let mut by_df: Vec<&(String, u32)> = terms.iter().collect();
    by_df.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("{docs} documentos, {} termos únicos, {} IDs repetidos, em {:.1}s", terms.len(), duplicates.len(), started.elapsed().as_secs_f64());
    for (id, line) in duplicates.iter().take(10) {
        println!("  ID repetido: {id} (linha {line})");
    }
    let top: Vec<String> = by_df.iter().take(15).map(|(t, n)| format!("{t} ({n})")).collect();
    println!("mais frequentes: {}", top.join(", "));
    println!("vocabulário: {}", out.display());
    Ok(())
}

pub(crate) struct VocabEntry {
    pub(crate) term: String,
    pub(crate) df: u32,
    pub(crate) len: usize,
}

pub(crate) fn read_vocab(path: &Path) -> AnyResult<Vec<VocabEntry>> {
    let mut out = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let mut parts = line.split('\t');
        let (Some(term), Some(df), Some(len)) = (parts.next(), parts.next(), parts.next()) else { continue };
        out.push(VocabEntry { term: term.to_string(), df: df.parse()?, len: len.parse()? });
    }
    Ok(out)
}
