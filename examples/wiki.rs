//! Harness de performance do fire-flower (ver `docs/plano-performance.md`).
//!
//! Todos os subcomandos leem o corpus JSONL da Wikipédia
//! (`{"id", "title", "text"}` por linha) e gravam resultados em
//! `bench/results/<data>-<commit>-<tipo>-<rótulo>.json`.
//!
//! ```text
//! cargo run --release --example wiki -- load    --db D --corpus F [--batch N] [--skip K] [--take N] [--remap-ids]
//! cargo run --release --example wiki -- vocab   --corpus F [--out V]
//! cargo run --release --example wiki -- queries --vocab V --corpus F [--seed 42]
//! cargo run --release --example wiki -- oracle  --db D --corpus F [--take N] [--sample-terms N]
//! cargo run --release --example wiki -- bench   --db D --queries Q... [--threads T] [--cold]
//! cargo run --release --example wiki -- sizes   --db D
//! cargo run --release --example wiki -- digest  --db D
//! cargo run --release --example wiki -- concurrent --db D --corpus F --skip K --queries Q [--threads T]
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use fire_flower::text::tokenize;
use fire_flower::{Config, DocId, Engine};
use rocksdb::perf::{PerfContext, PerfStatsLevel, set_perf_stats};
use serde::Deserialize;
use serde_json::{Value, json};

type AnyResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "wiki", about = "Harness de performance do fire-flower")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexa o corpus e mede docs/s, MB/s, RSS, tamanho do banco e write amp
    Load {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value_t = 1000)]
        batch: usize,
        #[arg(long, default_value_t = 0)]
        skip: u64,
        #[arg(long)]
        take: Option<u64>,
        /// Usa o número da linha (1..N) como ID, em vez do page id
        #[arg(long)]
        remap_ids: bool,
        /// Block cache em MB
        #[arg(long, default_value_t = 256)]
        cache_mb: usize,
        /// Indexa um documento por commit (`index_document`), para medir o custo fixo do commit
        #[arg(long)]
        streaming: bool,
        #[arg(long)]
        label: Option<String>,
    },
    /// Conta df e comprimento de cada termo; confere IDs únicos
    Vocab {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Gera os conjuntos de consulta da Fase 3 em bench/queries/
    Queries {
        #[arg(long)]
        vocab: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 1000)]
        per_set: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compara o motor com um índice ingênuo em memória (Fase 1)
    Oracle {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        remap_ids: bool,
        /// 0 = compara todos os termos; N = amostra estratificada por df
        #[arg(long, default_value_t = 0)]
        sample_terms: usize,
        #[arg(long, default_value_t = 5000)]
        and_queries: usize,
        #[arg(long, default_value_t = 2000)]
        prefixes: usize,
        #[arg(long, default_value_t = 1000)]
        titles: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long)]
        label: Option<String>,
    },
    /// Mede latência das consultas (p50/p95/p99/máx, qps, Gets e block reads por consulta)
    Bench {
        #[arg(long)]
        db: PathBuf,
        /// Arquivos de consulta ou pastas com *.txt; `s-*` são medidos com suggest
        #[arg(long, num_args = 1.., required = true)]
        queries: Vec<PathBuf>,
        #[arg(long, default_value_t = 1)]
        threads: usize,
        #[arg(long, default_value_t = 1)]
        passes: usize,
        /// Esvazia o cache do sistema antes de cada conjunto (precisa de `sudo -n`)
        #[arg(long)]
        cold: bool,
        /// Mede também tempos no PerfContext (mais preciso, mas pesa na latência)
        #[arg(long)]
        perf_time: bool,
        #[arg(long, default_value_t = 256)]
        cache_mb: usize,
        #[arg(long)]
        label: Option<String>,
    },
    /// Quanto ocupa cada espaço de chave e quais são os maiores valores
    Sizes {
        #[arg(long)]
        db: PathBuf,
    },
    /// Resumo do conteúdo lógico (termos + listas), independente dos IDs de nó
    Digest {
        #[arg(long)]
        db: PathBuf,
    },
    /// Buscas com e sem indexação em paralelo (Fase 5)
    Concurrent {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        skip: u64,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        queries: PathBuf,
        #[arg(long, default_value_t = 4)]
        threads: usize,
        #[arg(long, default_value_t = 1000)]
        batch: usize,
        /// Segundos de buscas sem escrita, para a linha de base
        #[arg(long, default_value_t = 15)]
        baseline_secs: u64,
        #[arg(long)]
        label: Option<String>,
    },
}

fn main() -> AnyResult {
    match Cli::parse().command {
        Cmd::Load { db, corpus, batch, skip, take, remap_ids, cache_mb, streaming, label } => {
            load(&db, &corpus, batch, skip, take, remap_ids, cache_mb, streaming, label)
        }
        Cmd::Vocab { corpus, take, out } => vocab(&corpus, take, out),
        Cmd::Queries { vocab, corpus, seed, per_set, out } => queries(&vocab, &corpus, seed, per_set, out),
        Cmd::Oracle { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, seed, label } => {
            oracle(OracleArgs { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, seed, label })
        }
        Cmd::Bench { db, queries, threads, passes, cold, perf_time, cache_mb, label } => {
            bench(&db, &queries, threads.max(1), passes.max(1), cold, perf_time, cache_mb, label)
        }
        Cmd::Sizes { db } => sizes(&db),
        Cmd::Digest { db } => digest(&db),
        Cmd::Concurrent { db, corpus, skip, take, queries, threads, batch, baseline_secs, label } => {
            concurrent(&db, &corpus, skip, take, &queries, threads.max(1), batch, baseline_secs, label)
        }
    }
}

// ================================================================== corpus

#[derive(Deserialize)]
struct Article {
    id: u64,
    #[serde(default)]
    title: String,
    text: String,
}

struct Doc {
    /// Número da linha no corpus, começando em 1.
    line: u64,
    id: DocId,
    title: String,
    /// `title + "\n" + text`, o que o motor indexa.
    body: String,
    bytes: usize,
}

/// Lê o corpus linha a linha, aplicando `skip`/`take` e a troca de IDs.
struct Corpus {
    reader: BufReader<File>,
    line: u64,
    skip: u64,
    end: Option<u64>,
    remap_ids: bool,
    buf: Vec<u8>,
}

impl Corpus {
    fn open(path: &Path, skip: u64, take: Option<u64>, remap_ids: bool) -> AnyResult<Self> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            reader: BufReader::with_capacity(1 << 20, file),
            line: 0,
            skip,
            end: take.map(|t| skip + t),
            remap_ids,
            buf: Vec::new(),
        })
    }
}

impl Iterator for Corpus {
    type Item = AnyResult<Doc>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.end.is_some_and(|end| self.line >= end) {
                return None;
            }
            self.buf.clear();
            match self.reader.read_until(b'\n', &mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
            self.line += 1;
            if self.line <= self.skip || self.buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let article: Article = match serde_json::from_slice(&self.buf) {
                Ok(a) => a,
                Err(e) => return Some(Err(format!("linha {}: {e}", self.line).into())),
            };
            let mut body = String::with_capacity(article.title.len() + article.text.len() + 1);
            body.push_str(&article.title);
            body.push('\n');
            body.push_str(&article.text);
            let id = if self.remap_ids { self.line } else { article.id };
            return Some(Ok(Doc { line: self.line, id, title: article.title, body, bytes: self.buf.len() }));
        }
    }
}

/// Termos únicos de um documento, como o motor os indexa.
fn unique_terms(body: &str) -> Vec<String> {
    let mut terms = tokenize(body);
    terms.sort_unstable();
    terms.dedup();
    terms
}

fn stem(path: &Path) -> String {
    path.file_stem().map_or("x".into(), |s| s.to_string_lossy().into_owned())
}

// ================================================================== ambiente

fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn git_commit() -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(project_dir())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let hash = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "desconhecido".into());
    let dirty = run(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{hash}-modificado") } else { hash }
}

/// Data e hora UTC, sem dependências: "2026-10-25T18-04-11".
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Algoritmo de Howard Hinnant: dias desde 1970 -> data civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// (RSS atual, pico de RSS) em bytes.
fn rss() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
        })
        .unwrap_or(0)
}

fn machine() -> Value {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem = |name: &str| {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    json!({
        "nucleos": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "ram_total": mem("MemTotal:"),
        "ram_livre": mem("MemAvailable:"),
        "swap_usado": mem("SwapTotal:").saturating_sub(mem("SwapFree:")),
        "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
    })
}

/// Swap em uso no sistema inteiro, em bytes.
fn swap_used() -> u64 {
    machine()["swap_usado"].as_u64().unwrap_or(0)
}

fn config_json(config: &Config) -> Value {
    json!({
        "block_cache_bytes": config.block_cache_bytes,
        "bloom_bits_per_key": config.bloom_bits_per_key,
        "max_prefix_terms": config.max_prefix_terms,
        "statistics": config.statistics,
    })
}

/// Grava o resultado em bench/results/ e devolve o caminho.
fn save_result(kind: &str, label: &str, mut value: Value) -> AnyResult<PathBuf> {
    let dir = project_dir().join("bench").join("results");
    std::fs::create_dir_all(&dir)?;
    let commit = git_commit();
    let ts = timestamp();
    if let Value::Object(map) = &mut value {
        map.insert("tipo".into(), json!(kind));
        map.insert("rotulo".into(), json!(label));
        map.insert("commit".into(), json!(commit));
        map.insert("quando".into(), json!(ts));
        map.insert("maquina".into(), machine());
    }
    let path = dir.join(format!("{ts}-{commit}-{kind}-{label}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&value)? + "\n")?;
    eprintln!("resultado: {}", path.display());
    Ok(path)
}

/// Contadores do RocksDB lidos do texto de estatísticas ("nome COUNT : valor").
fn tickers(stats: &str) -> HashMap<String, u64> {
    stats
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(" COUNT : ")?;
            Some((name.trim().to_string(), rest.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

/// Write amplification e stalls a partir dos contadores.
fn write_metrics(engine: &Engine) -> Value {
    let Some(stats) = engine.rocksdb_stats() else { return Value::Null };
    let t = tickers(&stats);
    let get = |k: &str| t.get(k).copied().unwrap_or(0);
    let user = get("rocksdb.bytes.written");
    let flush = get("rocksdb.flush.write.bytes");
    let compact = get("rocksdb.compact.write.bytes");
    json!({
        "bytes_escritos_usuario": user,
        "bytes_flush": flush,
        "bytes_compactacao": compact,
        "write_amp": if user > 0 { (flush + compact) as f64 / user as f64 } else { 0.0 },
        "stall_micros": get("rocksdb.stall.micros"),
        "block_cache_hit": get("rocksdb.block.cache.hit"),
        "block_cache_miss": get("rocksdb.block.cache.miss"),
        "bloom_util": get("rocksdb.bloom.filter.useful"),
    })
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

// ================================================================== load

#[allow(clippy::too_many_arguments)]
fn load(
    db: &Path,
    corpus_path: &Path,
    batch_size: usize,
    skip: u64,
    take: Option<u64>,
    remap_ids: bool,
    cache_mb: usize,
    streaming: bool,
    label: Option<String>,
) -> AnyResult {
    let config = Config { block_cache_bytes: cache_mb << 20, statistics: true, ..Config::default() };
    let engine = Engine::open(db, config.clone())?;
    let corpus = Corpus::open(corpus_path, skip, take, remap_ids)?;
    let label = label.unwrap_or_else(|| format!("{}-b{batch_size}", stem(corpus_path)));
    // O sistema pode já ter swap em uso; o que conta é o aumento durante a carga.
    let swap_before = swap_used();
    let started = Instant::now();
    let (mut docs, mut bytes) = (0u64, 0u64);
    let mut window = (Instant::now(), 0u64, 0u64);
    let mut batch = (!streaming).then(|| engine.batch());

    for doc in corpus {
        let doc = doc?;
        match batch.as_mut() {
            Some(b) => {
                b.add_document(doc.id, &doc.body)?;
                b.set_label(doc.id, &doc.title);
                if b.len() >= batch_size {
                    batch.take().unwrap().commit()?;
                    batch = Some(engine.batch());
                }
            }
            None => engine.index_document(doc.id, &doc.body)?,
        }
        docs += 1;
        bytes += doc.bytes as u64;
        if docs.is_multiple_of(10_000) {
            let dt = window.0.elapsed().as_secs_f64();
            let (cur, _) = rss();
            eprintln!(
                "{docs:>9} docs  {:>7.0} docs/s  {:>6.1} MB/s  RSS {:>6.0} MB  banco {:>7.0} MB  ({:.0}s)",
                (docs - window.1) as f64 / dt,
                mb(bytes - window.2) / dt,
                mb(cur),
                mb(dir_size(db)),
                started.elapsed().as_secs_f64()
            );
            window = (Instant::now(), docs, bytes);
        }
    }
    if let Some(b) = batch {
        b.commit()?;
    }
    let secs = started.elapsed().as_secs_f64();
    let (_, peak) = rss();
    let db_bytes = dir_size(db);
    let result = json!({
        "db": db, "corpus": corpus_path, "batch": batch_size, "skip": skip, "take": take,
        "remap_ids": remap_ids, "streaming": streaming, "config": config_json(engine.config()),
        "docs": docs, "bytes_corpus": bytes, "segundos": secs,
        "docs_por_s": docs as f64 / secs, "mb_por_s": mb(bytes) / secs,
        "pico_rss": peak, "banco_bytes": db_bytes,
        "banco_por_texto": db_bytes as f64 / bytes.max(1) as f64,
        "nos_trie": engine.trie_node_count()?,
        "chaves_estimadas": engine.property("rocksdb.estimate-num-keys").and_then(|v| v.parse::<u64>().ok()),
        "arquivos_l0": engine.property("rocksdb.num-files-at-level0").and_then(|v| v.parse::<u64>().ok()),
        "escrita": write_metrics(&engine),
        "swap_aumento": swap_used().saturating_sub(swap_before),
    });
    println!(
        "{docs} docs em {secs:.1}s: {:.0} docs/s, {:.1} MB/s, pico RSS {:.0} MB, banco {:.0} MB ({:.2}× o texto)",
        docs as f64 / secs,
        mb(bytes) / secs,
        mb(peak),
        mb(db_bytes),
        db_bytes as f64 / bytes.max(1) as f64
    );
    save_result("load", &label, result)?;
    Ok(())
}

// ================================================================== vocab

fn vocab(corpus_path: &Path, take: Option<u64>, out: Option<PathBuf>) -> AnyResult {
    let started = Instant::now();
    let mut df: HashMap<String, u32> = HashMap::new();
    let mut ids = HashSet::new();
    let mut duplicates = Vec::new();
    let mut docs = 0u64;
    for doc in Corpus::open(corpus_path, 0, take, false)? {
        let doc = doc?;
        if !ids.insert(doc.id) {
            duplicates.push((doc.id, doc.line));
        }
        for term in unique_terms(&doc.body) {
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

struct VocabEntry {
    term: String,
    df: u32,
    len: usize,
}

fn read_vocab(path: &Path) -> AnyResult<Vec<VocabEntry>> {
    let mut out = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let mut parts = line.split('\t');
        let (Some(term), Some(df), Some(len)) = (parts.next(), parts.next(), parts.next()) else { continue };
        out.push(VocabEntry { term: term.to_string(), df: df.parse()?, len: len.parse()? });
    }
    Ok(out)
}

// ================================================================== aleatoriedade

/// SplitMix64: gerador pequeno e determinístico (mesma seed, mesmas consultas).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn range(&mut self, lo: usize, hi_inclusive: usize) -> usize {
        lo + self.below(hi_inclusive - lo + 1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ================================================================== queries

fn queries(vocab_path: &Path, corpus_path: &Path, seed: u64, per_set: usize, out: Option<PathBuf>) -> AnyResult {
    let vocab = read_vocab(vocab_path)?;
    let mut rng = Rng(seed);
    let known: HashSet<&str> = vocab.iter().map(|v| v.term.as_str()).collect();
    let band = |lo: u32, hi: u32| -> Vec<&str> {
        vocab.iter().filter(|v| v.df >= lo && v.df <= hi).map(|v| v.term.as_str()).collect()
    };
    let raro = band(1, 10);
    let medio = band(100, 10_000);
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
    sets.insert("t-raro".into(), sample(&mut rng, &raro));
    sets.insert("t-medio".into(), sample(&mut rng, &medio));
    sets.insert("t-freq".into(), sample(&mut rng, &freq));
    for len in [3, 8, 15] {
        sets.insert(format!("t-len-{len}"), sample(&mut rng, &by_len(len)));
    }

    // Termos inexistentes: quase-acertos (divergem só no fim) e aleatórios.
    let mut miss = Vec::new();
    while miss.len() < per_set {
        let candidate = match miss.len() % 4 {
            0 | 1 => {
                let base = rng.pick(&medio);
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
    sets.insert("and-raro-freq".into(), (0..per_set).map(|_| and_of(&mut rng, &[&raro, &freq])).collect());
    sets.insert(
        "and-freq".into(),
        (0..per_set).map(|i| if i % 2 == 0 { and_of(&mut rng, &[&freq, &freq]) } else { and_of(&mut rng, &[&freq, &freq, &freq]) }).collect(),
    );
    for n in [2usize, 3, 5] {
        let pools = vec![&medio[..]; n];
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
            let term = rng.pick(&medio);
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
    let mut praro = Vec::new();
    let mut attempts = 0;
    while praro.len() < per_set && attempts < per_set * 50 {
        attempts += 1;
        let term = rng.pick(&raro);
        let chars = term.chars().count();
        if let Some(n) = (3..=chars).find(|&n| completions(&cut(term, n)) <= 5) {
            praro.push(cut(term, n));
        }
    }
    for (name, list) in [("1", &p1), ("2", &p2), ("4", &p4), ("raro", &praro)] {
        sets.insert(format!("p-{name}"), list.iter().map(|p| format!("{p}*")).collect());
        sets.insert(format!("s-{name}"), list.clone());
    }

    // Títulos reais: amostragem de reservatório sobre o corpus inteiro.
    let mut titles: Vec<String> = Vec::with_capacity(per_set);
    let mut seen = 0usize;
    for doc in Corpus::open(corpus_path, 0, None, false)? {
        let doc = doc?;
        if tokenize(&doc.title).is_empty() {
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
    sets.insert("titulos".into(), titles);

    // Misto: 70% títulos/termos médios, 20% AND, 10% prefixo.
    let mut misto = Vec::new();
    for i in 0..per_set {
        let source = match i % 10 {
            0..=3 => "titulos",
            4..=6 => "t-medio",
            7 => "and-2",
            8 => "and-raro-freq",
            _ => "p-4",
        };
        misto.push(rng.pick(&sets[source]).clone());
    }
    sets.insert("misto".into(), misto);

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

// ================================================================== oracle

struct OracleArgs {
    db: PathBuf,
    corpus: PathBuf,
    take: Option<u64>,
    remap_ids: bool,
    sample_terms: usize,
    and_queries: usize,
    prefixes: usize,
    titles: usize,
    seed: u64,
    label: Option<String>,
}

#[derive(Default)]
struct Check {
    name: &'static str,
    checked: usize,
    divergences: usize,
    examples: Vec<String>,
    notes: Vec<String>,
}

impl Check {
    fn new(name: &'static str) -> Self {
        Self { name, ..Default::default() }
    }

    fn compare<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, got: &T, expected: &T) {
        self.checked += 1;
        if got != expected {
            self.divergences += 1;
            if self.examples.len() < 5 {
                let show = |v: &T| {
                    let s = format!("{v:?}");
                    if s.len() > 200 { format!("{}…", &s[..s.char_indices().nth(200).map_or(s.len(), |(i, _)| i)]) } else { s }
                };
                self.examples.push(format!("{what}: motor={} oráculo={}", show(got), show(expected)));
            }
        }
    }

    fn json(&self) -> Value {
        json!({ "verificacao": self.name, "casos": self.checked, "divergencias": self.divergences,
                "exemplos": self.examples, "notas": self.notes })
    }
}

/// Ordem do autocompletar e da expansão de prefixo: comprimento, depois alfabética.
fn trie_order(a: &&str, b: &&str) -> std::cmp::Ordering {
    a.chars().count().cmp(&b.chars().count()).then_with(|| a.cmp(b))
}

fn oracle(args: OracleArgs) -> AnyResult {
    let started = Instant::now();
    let mut rng = Rng(args.seed);

    // 1. Índice ingênuo em memória.
    let mut index: HashMap<String, Vec<DocId>> = HashMap::new();
    let mut sampled_docs: Vec<Vec<String>> = Vec::new();
    let mut titles: Vec<(DocId, String)> = Vec::new();
    let mut docs = 0usize;
    for doc in Corpus::open(&args.corpus, 0, args.take, args.remap_ids)? {
        let doc = doc?;
        docs += 1;
        let terms = unique_terms(&doc.body);
        // Reservatórios: documentos para consultas AND e títulos para revocação.
        if sampled_docs.len() < 2000 {
            sampled_docs.push(terms.clone());
        } else if rng.below(docs) < 2000 {
            let j = rng.below(2000);
            sampled_docs[j] = terms.clone();
        }
        if !tokenize(&doc.title).is_empty() {
            if titles.len() < args.titles {
                titles.push((doc.id, doc.title.clone()));
            } else if rng.below(docs) < args.titles {
                let j = rng.below(args.titles);
                titles[j] = (doc.id, doc.title.clone());
            }
        }
        for term in terms {
            index.entry(term).or_default().push(doc.id);
        }
        if docs.is_multiple_of(20_000) {
            eprintln!("oráculo: {docs} docs, {} termos ({:.0}s)", index.len(), started.elapsed().as_secs_f64());
        }
    }
    for list in index.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    let mut vocab: Vec<&str> = index.keys().map(String::as_str).collect();
    vocab.sort_unstable();
    eprintln!("oráculo pronto: {docs} docs, {} termos ({:.0}s)", vocab.len(), started.elapsed().as_secs_f64());

    let engine = Engine::open(&args.db, Config::default())?;
    let max_prefix = engine.config().max_prefix_terms;
    let mut checks = Vec::new();

    // 2. Listas de postagem.
    let mut postings = Check::new("listas de postagem");
    if args.sample_terms == 0 {
        // Todos os termos, nos dois sentidos: o motor não tem termo a mais nem a menos.
        let mut engine_terms = 0usize;
        let mut nodes = Vec::new();
        engine.for_each_term(|term, node| nodes.push((term.to_string(), node)))?;
        for (term, node) in &nodes {
            engine_terms += 1;
            let got = engine.postings_of_node(*node)?;
            let expected = index.get(term).cloned().unwrap_or_default();
            postings.compare(term, &got, &expected);
        }
        let engine_set: HashSet<&str> = nodes.iter().map(|(t, _)| t.as_str()).collect();
        for term in &vocab {
            if !engine_set.contains(term) {
                postings.compare(term, &Vec::<DocId>::new(), &index[*term]);
            }
        }
        postings.notes.push(format!("{engine_terms} termos no motor, {} no oráculo", vocab.len()));
    } else {
        // Amostra estratificada por faixas de df (potências de 2).
        let mut buckets: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        for term in &vocab {
            buckets.entry(index[*term].len().ilog2()).or_default().push(term);
        }
        let per_bucket = args.sample_terms.div_ceil(buckets.len());
        for terms in buckets.values() {
            for _ in 0..per_bucket.min(terms.len()) {
                let term = rng.pick(terms);
                postings.compare(term, &engine.postings(term)?, &index[*term]);
            }
        }
        postings.notes.push(format!("amostra de {} termos em {} faixas de df", postings.checked, buckets.len()));
    }
    checks.push(postings);

    // 3. AND: metade com termos do mesmo documento (resultado não vazio).
    let mut and = Check::new("AND");
    let frequent: Vec<&str> = vocab.iter().copied().filter(|t| index[*t].len() >= 50).collect();
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
        let mut expected = index[&terms[0]].clone();
        for t in &terms[1..] {
            expected = fire_flower::internals::intersect(&expected, &index[t]);
        }
        let query = terms.join(" ");
        and.compare(&query, &engine.search(&query)?, &expected);
    }
    checks.push(and);

    // 4. Prefixo e 5. autocompletar.
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
        let mut expected: Vec<DocId> = matches.iter().take(max_prefix).flat_map(|t| index[*t].iter().copied()).collect();
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

    // 6. Título → artigo.
    let mut recall = Check::new("título → artigo");
    for (id, title) in &titles {
        let found = engine.search(title)?.binary_search(id).is_ok();
        recall.compare(title, &found, &true);
    }
    checks.push(recall);

    // 7. Normalização.
    let mut norm = Check::new("normalização");
    for group in [["São Paulo", "sao paulo", "SÃO PAULO"], ["Ação", "acao", "AÇÃO"], ["Coração", "coracao", "CORAÇÃO"], ["Península Ibérica", "peninsula iberica", "PENÍNSULA IBÉRICA"]] {
        let base = engine.search(group[0])?;
        for variant in &group[1..] {
            norm.compare(&format!("{} vs {variant}", group[0]), &engine.search(variant)?, &base);
        }
    }
    checks.push(norm);

    // Relatório.
    let total: usize = checks.iter().map(|c| c.divergences).sum();
    println!("\n{:<22} {:>8} {:>12}", "verificação", "casos", "divergências");
    for c in &checks {
        println!("{:<22} {:>8} {:>12}", c.name, c.checked, c.divergences);
        for ex in &c.examples {
            println!("    ✗ {ex}");
        }
        for note in &c.notes {
            println!("    · {note}");
        }
    }
    println!("\n{} em {:.1}s", if total == 0 { "✔ zero divergências".to_string() } else { format!("✗ {total} divergências") }, started.elapsed().as_secs_f64());

    let label = args.label.unwrap_or_else(|| stem(&args.corpus));
    save_result(
        "oracle",
        &label,
        json!({ "db": args.db, "corpus": args.corpus, "take": args.take, "remap_ids": args.remap_ids,
                "docs": docs, "termos_oraculo": vocab.len(), "divergencias": total,
                "verificacoes": checks.iter().map(Check::json).collect::<Vec<_>>() }),
    )?;
    if total > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ================================================================== bench

struct QuerySet {
    name: String,
    suggest: bool,
    queries: Vec<String>,
}

fn read_query_sets(paths: &[PathBuf]) -> AnyResult<Vec<QuerySet>> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(p)?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "txt")).collect();
            entries.sort();
            files.extend(entries);
        } else {
            files.push(p.clone());
        }
    }
    files
        .into_iter()
        .map(|f| {
            let name = stem(&f);
            let queries = std::fs::read_to_string(&f)?.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
            Ok(QuerySet { suggest: name.starts_with("s-"), name, queries })
        })
        .collect()
}

#[derive(Default, Clone)]
struct Sample {
    nanos: u64,
    results: usize,
    gets: u64,
    cache_hits: u64,
    block_reads: u64,
    bloom_useful: u64,
    merges: u64,
    merge_nanos: u64,
}

/// Contadores do PerfContext lidos pelo nome. O enum `PerfMetric` do crate
/// rocksdb 0.23 está desalinhado com o RocksDB 10.4 embutido (lê o contador
/// errado), então usamos o relatório em texto ("nome = valor, ...").
fn perf_counters(ctx: &PerfContext) -> HashMap<String, u64> {
    ctx.report(true)
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

fn run_query(engine: &Engine, set: &QuerySet, q: &str, ctx: &mut PerfContext) -> AnyResult<Sample> {
    ctx.reset();
    let reads_before = fire_flower::internals::reads_on_this_thread();
    let t = Instant::now();
    let results = if set.suggest { engine.suggest(q, 10)?.len() } else { engine.search(q)?.len() };
    let nanos = t.elapsed().as_nanos() as u64;
    let gets = fire_flower::internals::reads_on_this_thread() - reads_before;
    let c = perf_counters(ctx);
    let get = |k: &str| c.get(k).copied().unwrap_or(0);
    Ok(Sample {
        nanos,
        results,
        gets,
        cache_hits: get("block_cache_hit_count"),
        block_reads: get("block_read_count"),
        bloom_useful: get("bloom_sst_miss_count"),
        merges: get("internal_merge_point_lookup_count"),
        merge_nanos: get("merge_operator_time_nanos"),
    })
}

/// Roda o conjunto em `threads` threads (consultas divididas entre elas).
fn run_set(engine: &Engine, set: &QuerySet, threads: usize, perf_level: PerfStatsLevel) -> AnyResult<(Vec<Sample>, f64)> {
    let started = Instant::now();
    let samples = std::thread::scope(|scope| -> AnyResult<Vec<Sample>> {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || -> Result<Vec<Sample>, String> {
                    set_perf_stats(perf_level); // o PerfContext é por thread
                    let mut ctx = PerfContext::default();
                    set.queries
                        .iter()
                        .skip(t)
                        .step_by(threads)
                        .map(|q| run_query(engine, set, q, &mut ctx).map_err(|e| e.to_string()))
                        .collect()
                })
            })
            .collect();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().map_err(|_| "thread de busca entrou em pânico")??);
        }
        Ok(all)
    })?;
    Ok((samples, started.elapsed().as_secs_f64()))
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx] as f64 / 1000.0
}

fn summarize(name: &str, mode: &str, samples: &[Sample], wall: f64, threads: usize) -> Value {
    let mut lat: Vec<u64> = samples.iter().map(|s| s.nanos).collect();
    lat.sort_unstable();
    let n = samples.len().max(1) as f64;
    let avg = |f: fn(&Sample) -> u64| samples.iter().map(f).sum::<u64>() as f64 / n;
    json!({
        "conjunto": name, "modo": mode, "consultas": samples.len(), "threads": threads,
        "p50_us": percentile(&lat, 0.50), "p95_us": percentile(&lat, 0.95),
        "p99_us": percentile(&lat, 0.99), "max_us": percentile(&lat, 1.0),
        "media_us": avg(|s| s.nanos) / 1000.0, "qps": samples.len() as f64 / wall,
        "resultados_media": samples.iter().map(|s| s.results).sum::<usize>() as f64 / n,
        "gets_por_consulta": avg(|s| s.gets), "cache_hits_por_consulta": avg(|s| s.cache_hits),
        "block_reads_por_consulta": avg(|s| s.block_reads), "bloom_util_por_consulta": avg(|s| s.bloom_useful),
        "merges_por_consulta": avg(|s| s.merges), "merge_us_por_consulta": avg(|s| s.merge_nanos) / 1000.0,
    })
}

fn drop_os_caches() -> AnyResult {
    let ok = std::process::Command::new("sudo")
        .args(["-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err("--cold precisa esvaziar o cache do sistema com `sudo -n`. Rode `sudo -v` antes, ou meça sem --cold.".into())
    }
}

#[allow(clippy::too_many_arguments)]
fn bench(db: &Path, paths: &[PathBuf], threads: usize, passes: usize, cold: bool, perf_time: bool, cache_mb: usize, label: Option<String>) -> AnyResult {
    let sets = read_query_sets(paths)?;
    let config = Config { block_cache_bytes: cache_mb << 20, ..Config::default() };
    // O enum do crate também está deslocado: `EnableTime` corresponde a medir tempos no RocksDB 10.
    let level = if perf_time { PerfStatsLevel::EnableTime } else { PerfStatsLevel::EnableCount };
    let mut engine = if cold { None } else { Some(Engine::open(db, config.clone())?) };
    let mut results = Vec::new();

    println!("| conjunto | p50 µs | p95 µs | p99 µs | máx µs | qps | resultados | Gets | block reads |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for set in &sets {
        if cold {
            drop(engine.take()); // fecha antes de esvaziar o cache
            drop_os_caches()?;
            engine = Some(Engine::open(db, config.clone())?);
        } else {
            run_set(engine.as_ref().unwrap(), set, threads, level)?; // aquecimento, descartado
        }
        let mut samples = Vec::new();
        let mut wall = 0.0;
        for _ in 0..if cold { 1 } else { passes } {
            let (s, w) = run_set(engine.as_ref().unwrap(), set, threads, level)?;
            samples.extend(s);
            wall += w;
        }
        let summary = summarize(&set.name, if set.suggest { "suggest" } else { "search" }, &samples, wall, threads);
        let f = |k: &str| summary[k].as_f64().unwrap_or(0.0);
        println!(
            "| {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.2} |",
            set.name, f("p50_us"), f("p95_us"), f("p99_us"), f("max_us"), f("qps"), f("resultados_media"), f("gets_por_consulta"), f("block_reads_por_consulta")
        );
        results.push(summary);
    }
    let label = label.unwrap_or_else(|| {
        let db_name = stem(db);
        format!("{db_name}-t{threads}{}", if cold { "-frio" } else { "" })
    });
    save_result(
        "bench",
        &label,
        json!({ "db": db, "threads": threads, "passes": passes, "frio": cold, "perf_time": perf_time,
                "config": config_json(&config), "conjuntos": results }),
    )?;
    Ok(())
}

// ================================================================== sizes

fn sizes(db: &Path) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let watched: Vec<(&str, Option<u64>)> = ["de", "a", "o", "e", "em"].iter().map(|t| Ok((*t, engine.term_node(t)?))).collect::<AnyResult<_>>()?;

    #[derive(Default)]
    struct Space {
        keys: u64,
        key_bytes: u64,
        value_bytes: u64,
    }
    let mut spaces: BTreeMap<String, Space> = BTreeMap::new();
    let mut largest: Vec<(usize, String)> = Vec::new();
    let mut watched_sizes: HashMap<String, usize> = HashMap::new();
    let mut root_size = 0;
    engine.for_each_raw(|k, v| {
        let key = String::from_utf8_lossy(k);
        let space = key.split(':').next().unwrap_or("").to_string();
        let s = spaces.entry(space).or_default();
        s.keys += 1;
        s.key_bytes += k.len() as u64;
        s.value_bytes += v.len() as u64;
        if key == "node:0" {
            root_size = v.len();
        }
        if largest.len() < 20 || v.len() > largest.last().unwrap().0 {
            largest.push((v.len(), key.to_string()));
            largest.sort_by(|a, b| b.0.cmp(&a.0));
            largest.truncate(20);
        }
        if let Some(id) = key.strip_prefix("post:").and_then(|i| i.parse::<u64>().ok()) {
            for (term, node) in &watched {
                if *node == Some(id) {
                    watched_sizes.insert(term.to_string(), v.len());
                }
            }
        }
    })?;

    // Termos dos maiores valores (só nós terminais têm termo).
    let wanted: HashSet<u64> = largest.iter().filter_map(|(_, k)| k.split_once(':').and_then(|(_, id)| id.parse().ok())).collect();
    let mut names: HashMap<u64, String> = HashMap::new();
    engine.for_each_term(|term, node| {
        if wanted.contains(&node) {
            names.insert(node, term.to_string());
        }
    })?;

    println!("| espaço | chaves | chaves (MB) | valores (MB) |");
    println!("|---|---|---|---|");
    for (name, s) in &spaces {
        println!("| `{name}:` | {} | {:.1} | {:.1} |", s.keys, mb(s.key_bytes), mb(s.value_bytes));
    }
    println!("\nraiz (`node:0`): {root_size} bytes; nós da Trie: {}; termos: {}", engine.trie_node_count()?, spaces.get("post").map_or(0, |s| s.keys));
    for (term, _) in &watched {
        if let Some(size) = watched_sizes.get(*term) {
            println!("lista de \"{term}\": {:.1} KB", *size as f64 / 1024.0);
        }
    }
    println!("\n20 maiores valores:");
    let mut top = Vec::new();
    for (size, key) in &largest {
        let id = key.split_once(':').and_then(|(_, id)| id.parse::<u64>().ok());
        let term = id.and_then(|i| names.get(&i)).map_or(String::new(), |t| format!(" ({t})"));
        println!("  {size:>10} B  {key}{term}");
        top.push(json!({ "chave": key, "bytes": size, "termo": term.trim().trim_matches(['(', ')']) }));
    }
    save_result(
        "sizes",
        &stem(db),
        json!({
            "db": db,
            "espacos": spaces.iter().map(|(n, s)| json!({"espaco": n, "chaves": s.keys, "bytes_chaves": s.key_bytes, "bytes_valores": s.value_bytes})).collect::<Vec<_>>(),
            "raiz_bytes": root_size, "nos_trie": engine.trie_node_count()?,
            "listas": watched_sizes, "maiores": top,
        }),
    )?;
    Ok(())
}

// ================================================================== digest

/// FNV-1a de 64 bits: estável entre versões do Rust (o SipHash da std não garante isso).
struct Fnv(u64);

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn digest(db: &Path) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let mut nodes = Vec::new();
    engine.for_each_term(|term, node| nodes.push((term.to_string(), node)))?;
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    let mut postings = 0u64;
    for (term, node) in &nodes {
        h.write(term.as_bytes());
        h.write(&[0]);
        for id in engine.postings_of_node(*node)? {
            h.write(&id.to_le_bytes());
            postings += 1;
        }
        h.write(&[0xff]);
    }
    println!("{}", json!({ "db": db, "termos": nodes.len(), "postagens": postings, "digest": format!("{:016x}", h.0) }));
    Ok(())
}

// ================================================================== concurrent

#[allow(clippy::too_many_arguments)]
fn concurrent(db: &Path, corpus_path: &Path, skip: u64, take: Option<u64>, queries_path: &Path, threads: usize, batch_size: usize, baseline_secs: u64, label: Option<String>) -> AnyResult {
    let set = read_query_sets(&[queries_path.to_path_buf()])?.pop().ok_or("conjunto de consultas vazio")?;
    let engine = Arc::new(Engine::open(db, Config { statistics: true, ..Config::default() })?);

    // Leitores rodam até `stop`; cada um devolve suas amostras.
    let readers = |stop: Arc<AtomicBool>| -> Vec<std::thread::JoinHandle<Vec<Sample>>> {
        (0..threads)
            .map(|t| {
                let engine = Arc::clone(&engine);
                let stop = Arc::clone(&stop);
                let set = QuerySet { name: set.name.clone(), suggest: set.suggest, queries: set.queries.clone() };
                std::thread::spawn(move || {
                    set_perf_stats(PerfStatsLevel::EnableCount);
                    let mut ctx = PerfContext::default();
                    let mut out = Vec::new();
                    let mut i = t;
                    while !stop.load(Ordering::Relaxed) {
                        let q = &set.queries[i % set.queries.len()];
                        if let Ok(s) = run_query(&engine, &set, q, &mut ctx) {
                            out.push(s);
                        }
                        i += threads;
                    }
                    out
                })
            })
            .collect()
    };
    let collect = |handles: Vec<std::thread::JoinHandle<Vec<Sample>>>| -> Vec<Sample> {
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    };

    // A. Só leitura, para a linha de base.
    eprintln!("fase A: {threads} leitores sem escrita por {baseline_secs}s");
    let stop = Arc::new(AtomicBool::new(false));
    let handles = readers(Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(baseline_secs));
    stop.store(true, Ordering::Relaxed);
    let alone = collect(handles);
    let before = write_metrics(&engine);

    // B. Leitores + indexação.
    eprintln!("fase B: {threads} leitores + indexação a partir da linha {}", skip + 1);
    let stop = Arc::new(AtomicBool::new(false));
    let handles = readers(Arc::clone(&stop));
    let started = Instant::now();
    let (mut docs, mut bytes) = (0u64, 0u64);
    let mut batch = engine.batch();
    for doc in Corpus::open(corpus_path, skip, take, false)? {
        let doc = doc?;
        batch.add_document(doc.id, &doc.body)?;
        batch.set_label(doc.id, &doc.title);
        docs += 1;
        bytes += doc.bytes as u64;
        if batch.len() >= batch_size {
            batch.commit()?;
            batch = engine.batch();
        }
        if docs.is_multiple_of(10_000) {
            eprintln!("{docs:>8} docs  {:.0} docs/s", docs as f64 / started.elapsed().as_secs_f64());
        }
    }
    batch.commit()?;
    let write_secs = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let during = collect(handles);
    let after = write_metrics(&engine);

    let a = summarize(&set.name, "sem escrita", &alone, baseline_secs as f64, threads);
    let b = summarize(&set.name, "com escrita", &during, write_secs, threads);
    for (name, s) in [("sem escrita", &a), ("com escrita", &b)] {
        println!(
            "{name:<12} p50 {:>7.0} µs  p99 {:>8.0} µs  máx {:>9.0} µs  {:>8.0} qps",
            s["p50_us"].as_f64().unwrap_or(0.0), s["p99_us"].as_f64().unwrap_or(0.0), s["max_us"].as_f64().unwrap_or(0.0), s["qps"].as_f64().unwrap_or(0.0)
        );
    }
    let stall = |v: &Value| v["stall_micros"].as_u64().unwrap_or(0);
    println!("indexação: {docs} docs em {write_secs:.1}s ({:.0} docs/s) com {threads} leitores; stalls: {} ms", docs as f64 / write_secs, stall(&after).saturating_sub(stall(&before)) / 1000);
    save_result(
        "concurrent",
        &label.unwrap_or_else(|| format!("{}-t{threads}", stem(db))),
        json!({ "db": db, "corpus": corpus_path, "skip": skip, "take": take, "threads": threads, "batch": batch_size,
                "leitura_sem_escrita": a, "leitura_com_escrita": b,
                "indexacao": { "docs": docs, "bytes": bytes, "segundos": write_secs, "docs_por_s": docs as f64 / write_secs },
                "escrita_antes": before, "escrita_depois": after }),
    )?;
    Ok(())
}
