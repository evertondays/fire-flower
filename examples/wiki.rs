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

use std::collections::BTreeMap;

// FxHash: os mapas do vocabulário e do oráculo têm centenas de milhares de termos.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use fire_flower::text::{Clause, TITLE_GAP, index_terms, parse_query, tokenize};
use fire_flower::{Language, Stopwords};
use fire_flower::{CommitStats, Config, DocId, Engine};
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
        #[arg(long, default_value_t = 10000)]
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
        /// Nós da Trie que o escritor guarda entre lotes (0 desliga o cache)
        #[arg(long)]
        cache_nos: Option<usize>,
        /// Idiomas das stopwords de um banco novo ("pt,en", "en" ou "nenhum").
        /// Sem a opção, usa o que está gravado no banco (ou pt,en)
        #[arg(long)]
        idiomas: Option<String>,
        /// Cria o banco sem posições (sem busca por frase)
        #[arg(long)]
        sem_posicoes: bool,
        /// Filtros de Bloom inteiros (um bloco por SST), como antes: reproduz o
        /// colapso da carga quando o filtro passa de uma parte do block cache
        #[arg(long)]
        filtro_inteiro: bool,
        #[arg(long)]
        label: Option<String>,
    },
    /// Conta df e comprimento de cada termo; confere IDs únicos
    Vocab {
        #[arg(long)]
        corpus: PathBuf,
        /// Idiomas das stopwords, como no banco que será medido
        #[arg(long, default_value = "pt,en")]
        idiomas: String,
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
        /// Idiomas das stopwords, como no banco que será medido
        #[arg(long, default_value = "pt,en")]
        idiomas: String,
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
        /// Títulos para a revocação e para a métrica título → artigo (top-1, top-10, MRR)
        #[arg(long, default_value_t = 1000)]
        titles: usize,
        /// Frases tiradas de trechos reais (e com a ordem trocada)
        #[arg(long, default_value_t = 2000)]
        phrases: usize,
        /// Consultas cujo top 10 é comparado com o BM25 calculado no oráculo
        #[arg(long, default_value_t = 1000)]
        rank_queries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long)]
        label: Option<String>,
    },
    /// Avalia a ordenação com consultas de resposta conhecida (docs/plano-relevancia.md)
    Relevancia {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        /// Pasta dos conjuntos rel-*.tsv. Os que faltarem (todos, com --gerar)
        /// são gerados do corpus e gravados ali
        #[arg(long)]
        sets: PathBuf,
        /// Consultas escritas à mão (padrão: bench/relevancia/manual.tsv)
        #[arg(long)]
        manual: Option<PathBuf>,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 1000)]
        per_set: usize,
        /// Gera os conjuntos de novo mesmo que já existam
        #[arg(long)]
        gerar: bool,
        /// Peso do título no BM25F (padrão: o do Config)
        #[arg(long)]
        peso_titulo: Option<f64>,
        /// `b` do título no BM25F (padrão: o do Config)
        #[arg(long)]
        b_titulo: Option<f64>,
        /// Não imprime as consultas à mão (para a grade)
        #[arg(long)]
        resumo: bool,
        #[arg(long)]
        label: Option<String>,
    },
    /// Mede latência das consultas (p50/p95/p99/máx, qps, Gets e block reads por consulta)
    Bench {
        #[arg(long)]
        db: PathBuf,
        /// Arquivos de consulta ou pastas com *.txt; `s-*` são medidos com
        /// suggest e `rank-*` com search_ranked (10 melhores)
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
        /// Profundidade do cache de nós das buscas ("nenhum" desliga)
        #[arg(long)]
        cache_leitura: Option<String>,
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
        Cmd::Load { db, corpus, batch, skip, take, remap_ids, cache_mb, streaming, cache_nos, idiomas, sem_posicoes, filtro_inteiro, label } => {
            let languages = idiomas.as_deref().map(parse_languages).transpose()?;
            let config = Config {
                block_cache_bytes: cache_mb << 20,
                writer_cache_nodes: cache_nos.unwrap_or(Config::default().writer_cache_nodes),
                languages,
                positions: sem_posicoes.then_some(false),
                partition_filters: !filtro_inteiro,
                statistics: true,
                ..Config::default()
            };
            load(&db, &corpus, batch, skip, take, remap_ids, streaming, config, label)
        }
        Cmd::Vocab { corpus, idiomas, take, out } => vocab(&corpus, &Stopwords::new(&parse_languages(&idiomas)?), take, out),
        Cmd::Queries { vocab, corpus, idiomas, seed, per_set, out } => {
            queries(&vocab, &corpus, &Stopwords::new(&parse_languages(&idiomas)?), seed, per_set, out)
        }
        Cmd::Oracle { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, phrases, rank_queries, seed, label } => {
            oracle(OracleArgs { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, phrases, rank_queries, seed, label })
        }
        Cmd::Relevancia { db, corpus, sets, manual, take, seed, per_set, gerar, peso_titulo, b_titulo, resumo, label } => {
            let manual = manual.unwrap_or_else(|| project_dir().join("bench").join("relevancia").join("manual.tsv"));
            let defaults = Config::default();
            let config = Config {
                title_weight: peso_titulo.unwrap_or(defaults.title_weight),
                title_b: b_titulo.unwrap_or(defaults.title_b),
                ..defaults
            };
            relevancia(&db, &corpus, &sets, &manual, take, seed, per_set, gerar, config, resumo, label)
        }
        Cmd::Bench { db, queries, threads, passes, cold, perf_time, cache_leitura, cache_mb, label } => {
            let mut config = Config { block_cache_bytes: cache_mb << 20, ..Config::default() };
            if let Some(depth) = cache_leitura {
                config.reader_cache_depth = if depth == "nenhum" { None } else { Some(depth.parse()?) };
            }
            bench(&db, &queries, threads.max(1), passes.max(1), cold, perf_time, config, label)
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
    /// O texto sem o título (o motor indexa os dois, como campos).
    text: String,
    /// `title + "\n" + text`: para quem só quer o conjunto de termos.
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
            return Some(Ok(Doc { line: self.line, id, title: article.title, text: article.text, body, bytes: self.buf.len() }));
        }
    }
}

/// Termos únicos de um documento, como o motor os indexa (sem stopwords).
fn unique_terms(body: &str, stopwords: &Stopwords) -> Vec<String> {
    index_terms(body, stopwords)
}

/// "pt,en", "en" ou "nenhum".
fn parse_languages(codes: &str) -> AnyResult<Vec<Language>> {
    if codes.trim().eq_ignore_ascii_case("nenhum") {
        return Ok(Vec::new());
    }
    codes
        .split(',')
        .map(|c| Language::from_code(c).ok_or_else(|| format!("idioma desconhecido: {c:?}").into()))
        .collect()
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
    // bench/ fica de fora: o perf.py regrava consultas, baseline e relatório no meio da rodada.
    let dirty = run(&["status", "--porcelain", "--untracked-files=no", "--", ".", ":!bench"]).is_some_and(|s| !s.is_empty());
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
        "writer_cache_nodes": config.writer_cache_nodes,
        "reader_cache_depth": config.reader_cache_depth,
        "languages": config.languages.as_ref().map(|l| l.iter().map(|l| l.code()).collect::<Vec<_>>()),
        "positions": config.positions,
        "partition_filters": config.partition_filters,
        "bm25_k1": config.bm25_k1,
        "bm25_b": config.bm25_b,
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
    streaming: bool,
    config: Config,
    label: Option<String>,
) -> AnyResult {
    let engine = Engine::open(db, config.clone())?;
    let corpus = Corpus::open(corpus_path, skip, take, remap_ids)?;
    let label = label.unwrap_or_else(|| format!("{}-b{batch_size}", stem(corpus_path)));
    // O sistema pode já ter swap em uso; o que conta é o aumento durante a carga.
    let swap_before = swap_used();
    let started = Instant::now();
    let (mut docs, mut bytes) = (0u64, 0u64);
    let mut monitor = LoadMonitor::new(&engine);
    let mut batch = (!streaming).then(|| engine.batch());

    for doc in corpus {
        let doc = doc?;
        match batch.as_mut() {
            Some(b) => {
                b.add_titled(doc.id, &doc.title, &doc.text)?;
                b.set_label(doc.id, &doc.title);
                if b.len() >= batch_size {
                    monitor.add_commit(batch.take().unwrap().commit_with_stats()?);
                    batch = Some(engine.batch());
                }
            }
            None => {
                let mut b = engine.batch();
                b.add_titled(doc.id, &doc.title, &doc.text)?;
                b.set_label(doc.id, &doc.title);
                b.commit()?;
            }
        }
        docs += 1;
        bytes += doc.bytes as u64;
        if docs.is_multiple_of(LOAD_WINDOW) {
            monitor.window(&engine, db, docs, bytes, started);
        }
    }
    if let Some(b) = batch {
        monitor.add_commit(b.commit_with_stats()?);
    }
    let collapse = monitor.collapse(batch_size);
    let secs = started.elapsed().as_secs_f64();
    let (_, peak) = rss();
    let db_bytes = dir_size(db);
    let result = json!({
        "db": db, "corpus": corpus_path, "batch": batch_size, "skip": skip, "take": take,
        "remap_ids": remap_ids, "streaming": streaming, "config": config_json(engine.config()),
        "idiomas": engine.languages().iter().map(|l| l.code()).collect::<Vec<_>>(),
        "formato": fire_flower::FORMAT_VERSION, "posicoes": engine.has_positions(),
        "docs": docs, "bytes_corpus": bytes, "segundos": secs,
        "docs_por_s": docs as f64 / secs, "mb_por_s": mb(bytes) / secs,
        "pico_rss": peak, "banco_bytes": db_bytes,
        "banco_por_texto": db_bytes as f64 / bytes.max(1) as f64,
        "nos_trie": engine.trie_node_count()?,
        "chaves_estimadas": engine.property("rocksdb.estimate-num-keys").and_then(|v| v.parse::<u64>().ok()),
        "arquivos_l0": engine.property("rocksdb.num-files-at-level0").and_then(|v| v.parse::<u64>().ok()),
        "escrita": write_metrics(&engine),
        "swap_aumento": swap_used().saturating_sub(swap_before),
        "janelas": monitor.windows, "colapso": collapse,
        "commit_etapas": monitor.all.json(secs, docs as f64),
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

/// Documentos por janela do monitor da carga.
const LOAD_WINDOW: u64 = 10_000;

/// Acompanha a carga janela a janela: velocidade e os sinais que explicam uma
/// queda (leituras do escritor, filtros relidos, travamento de escrita do
/// RocksDB, nível 0, memória). Foi assim que se achou o colapso da carga
/// inteira: filtros de Bloom maiores que uma parte do block cache.
struct LoadMonitor {
    at: Instant,
    docs: u64,
    bytes: u64,
    reads: u64,
    /// Soma dos commits desde o começo da janela.
    commits: CommitTotals,
    /// Soma desde o começo da carga.
    all: CommitTotals,
    tickers: HashMap<String, u64>,
    windows: Vec<Value>,
}

/// Soma de [`CommitStats`] de vários commits.
#[derive(Default, Clone, Copy)]
struct CommitTotals {
    commits: u64,
    terms: u64,
    node_reads: u64,
    new_nodes: u64,
    batch_bytes: u64,
    cache_nodes: u64,
    sort: f64,
    trie: f64,
    encode: f64,
    nodes: f64,
    write: f64,
    evict: f64,
    total: f64,
}

impl CommitTotals {
    fn add(&mut self, c: &CommitStats) {
        self.commits += 1;
        self.terms += c.terms as u64;
        self.node_reads += c.node_reads;
        self.new_nodes += c.new_nodes as u64;
        self.batch_bytes += c.batch_bytes as u64;
        self.cache_nodes = c.cache_nodes as u64;
        self.sort += c.sort_secs;
        self.trie += c.trie_secs;
        self.encode += c.encode_secs;
        self.nodes += c.nodes_secs;
        self.write += c.write_secs;
        self.evict += c.evict_secs;
        self.total += c.total_secs;
    }

    /// Frações do tempo `secs` em cada etapa do commit, mais contagens por documento.
    fn json(&self, secs: f64, docs: f64) -> Value {
        let f = |v: f64| v / secs.max(1e-9);
        json!({
            "commit_fracao": f(self.total), "ordenar_fracao": f(self.sort), "trie_fracao": f(self.trie),
            "codificar_fracao": f(self.encode), "nos_fracao": f(self.nodes), "gravar_fracao": f(self.write),
            "descarte_fracao": f(self.evict), "commits": self.commits,
            "termos_por_commit": self.terms as f64 / self.commits.max(1) as f64,
            "leituras_commit_por_doc": self.node_reads as f64 / docs.max(1.0),
            "nos_novos_por_doc": self.new_nodes as f64 / docs.max(1.0),
            "lote_mb": mb(self.batch_bytes) / self.commits.max(1) as f64,
            "cache_nos": self.cache_nodes,
        })
    }
}

impl LoadMonitor {
    fn new(engine: &Engine) -> Self {
        Self {
            at: Instant::now(),
            docs: 0,
            bytes: 0,
            reads: fire_flower::internals::reads_on_this_thread(),
            commits: CommitTotals::default(),
            all: CommitTotals::default(),
            tickers: engine.rocksdb_stats().map(|s| tickers(&s)).unwrap_or_default(),
            windows: Vec::new(),
        }
    }

    fn add_commit(&mut self, stats: CommitStats) {
        self.commits.add(&stats);
        self.all.add(&stats);
    }

    fn window(&mut self, engine: &Engine, db: &Path, docs: u64, bytes: u64, started: Instant) {
        let dt = self.at.elapsed().as_secs_f64().max(1e-9);
        let now = engine.rocksdb_stats().map(|s| tickers(&s)).unwrap_or_default();
        let delta = |k: &str| now.get(k).copied().unwrap_or(0).saturating_sub(self.tickers.get(k).copied().unwrap_or(0));
        let reads = fire_flower::internals::reads_on_this_thread();
        let prop = |name: &str| engine.property(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let n = (docs - self.docs).max(1) as f64;
        let (cur, _) = rss();
        let mut w = json!({
            "docs": docs, "segundos": started.elapsed().as_secs_f64(), "janela_s": dt, "janela_mb": mb(bytes - self.bytes),
            "docs_por_s": (docs - self.docs) as f64 / dt, "mb_por_s": mb(bytes - self.bytes) / dt,
            "gets_por_doc": (reads - self.reads) as f64 / n,
            "filtro_mb": mb(delta("rocksdb.block.cache.filter.bytes.insert")),
            "filtro_miss": delta("rocksdb.block.cache.filter.miss"),
            "dados_miss": delta("rocksdb.block.cache.data.miss"),
            "stall_s": delta("rocksdb.stall.micros") as f64 / 1e6,
            "l0": prop("rocksdb.num-files-at-level0"),
            "compactacao_pendente_mb": mb(prop("rocksdb.estimate-pending-compaction-bytes")),
            "rss_mb": mb(cur), "ram_livre_mb": mb(machine()["ram_livre"].as_u64().unwrap_or(0)),
            "swap_mb": mb(swap_used()), "banco_mb": mb(dir_size(db)),
        });
        if let (Value::Object(map), Value::Object(extra)) = (&mut w, self.commits.json(dt, n)) {
            map.extend(extra);
        }
        let f = |k: &str| w[k].as_f64().unwrap_or(0.0);
        eprintln!(
            "{docs:>9} docs {:>6.0} docs/s {:>5.1} MB/s | commit {:>3.0}% (trie {:>3.0}% codif {:>3.0}% grava {:>3.0}% nós {:>2.0}%) leituras/doc {:>5.1} cache {:>4.0}k | filtro {:>4.0} MB stall {:>3.1}s L0 {:>2} | RSS {:>5.0} MB ({:.0}s)",
            f("docs_por_s"), f("mb_por_s"), f("commit_fracao") * 100.0, f("trie_fracao") * 100.0,
            f("codificar_fracao") * 100.0, f("gravar_fracao") * 100.0, f("nos_fracao") * 100.0,
            f("leituras_commit_por_doc"), f("cache_nos") / 1000.0, f("filtro_mb"), f("stall_s"), f("l0"),
            f("rss_mb"), f("segundos")
        );
        self.windows.push(w);
        *self = Self {
            at: Instant::now(),
            docs,
            bytes,
            reads,
            commits: CommitTotals::default(),
            all: self.all,
            tickers: now,
            windows: std::mem::take(&mut self.windows),
        };
    }

    /// O primeiro trecho com MB/s abaixo de 20% da mediana dos 10 anteriores
    /// (MB/s e não docs/s: o tamanho dos artigos varia muito ao longo do
    /// corpus). Com lotes maiores que a janela, as janelas são agrupadas até
    /// cobrir um lote: senão a janela que contém o commit inteiro parece um
    /// colapso, e as outras, rápidas demais.
    fn collapse(&self, batch: usize) -> Value {
        let group = (batch as u64).div_ceil(LOAD_WINDOW).max(1) as usize;
        let num = |w: &Value, k: &str| w[k].as_f64().unwrap_or(0.0);
        let speeds: Vec<(f64, &Value)> = self
            .windows
            .chunks(group)
            .map(|c| (c.iter().map(|w| num(w, "janela_mb")).sum::<f64>() / c.iter().map(|w| num(w, "janela_s")).sum::<f64>().max(1e-9), c.last().unwrap()))
            .collect();
        for i in 3..speeds.len() {
            let mut before: Vec<f64> = speeds[i.saturating_sub(10)..i].iter().map(|s| s.0).collect();
            before.sort_by(f64::total_cmp);
            let median = before[before.len() / 2];
            let (speed, w) = speeds[i];
            if speed < 0.2 * median {
                eprintln!(
                    "\n⚠ colapso em {} docs: {:.1} → {:.1} MB/s; filtro relido {:.0} MB, Gets/doc {:.0}, stall {:.1}s, L0 {}, RAM livre {:.0} MB",
                    w["docs"], median, speed, num(w, "filtro_mb"), num(w, "gets_por_doc"), num(w, "stall_s"), w["l0"],
                    num(w, "ram_livre_mb")
                );
                return json!({ "docs": w["docs"], "mb_por_s_antes": median, "janela": w });
            }
        }
        Value::Null
    }
}

// ================================================================== vocab

fn vocab(corpus_path: &Path, stopwords: &Stopwords, take: Option<u64>, out: Option<PathBuf>) -> AnyResult {
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

/// Artigos cujos tokens o `queries` guarda para sortear frases.
const SNIPPET_DOCS: usize = 2000;

fn queries(vocab_path: &Path, corpus_path: &Path, stopwords: &Stopwords, seed: u64, per_set: usize, out: Option<PathBuf>) -> AnyResult {
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
    for (name, list) in [("frase-2", &f2), ("frase-3", &f3), ("frase-stop", &fstop), ("frase-freq", &ffreq)] {
        sets.insert(name.into(), list.iter().map(|w| quote(w)).collect());
    }
    sets.insert("frase-miss".into(), fmiss);

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
    sets.insert("rank-titulos".into(), sets["titulos"].clone());

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
    phrases: usize,
    rank_queries: usize,
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
        self.verdict(what, got == expected, || (format!("{got:?}"), format!("{expected:?}")));
    }

    /// Registra um caso; `show` só roda se divergiu (e só para os 5 primeiros).
    fn verdict(&mut self, what: &str, ok: bool, show: impl FnOnce() -> (String, String)) {
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

    fn json(&self) -> Value {
        json!({ "verificacao": self.name, "casos": self.checked, "divergencias": self.divergences,
                "exemplos": self.examples, "notas": self.notes })
    }
}

/// Ordem do autocompletar e da expansão de prefixo: comprimento, depois alfabética.
fn trie_order(a: &&str, b: &&str) -> std::cmp::Ordering {
    a.chars().count().cmp(&b.chars().count()).then_with(|| a.cmp(b))
}

/// Lista de um termo no oráculo: documentos em ordem, o tf de cada um e
/// quantas ocorrências estão no título.
#[derive(Default)]
struct OTerm {
    docs: Vec<DocId>,
    tfs: Vec<u32>,
    title_tfs: Vec<u32>,
}

impl OTerm {
    /// (tf no documento inteiro, tf no título).
    fn tfs_of(&self, doc: DocId) -> Option<(u32, u32)> {
        self.docs.binary_search(&doc).ok().map(|i| (self.tfs[i], self.title_tfs[i]))
    }

    /// Ordena por documento; o mesmo documento duas vezes: a última vence.
    fn finish(&mut self) {
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
fn doc_tokens(title: &str, text: &str) -> (Vec<(String, u32)>, usize) {
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
struct OPos {
    docs: Vec<DocId>,
    /// Fim das posições de cada documento em `positions`.
    ends: Vec<usize>,
    positions: Vec<u32>,
}

impl OPos {
    fn push(&mut self, doc: DocId, positions: &[u32]) {
        self.docs.push(doc);
        self.positions.extend_from_slice(positions);
        self.ends.push(self.positions.len());
    }

    fn slice(&self, i: usize) -> &[u32] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.positions[start..self.ends[i]]
    }

    fn get(&self, doc: DocId) -> Option<&[u32]> {
        self.docs.binary_search(&doc).ok().map(|i| self.slice(i))
    }

    fn entries(&self) -> Vec<(DocId, Vec<u32>)> {
        (0..self.docs.len()).map(|i| (self.docs[i], self.slice(i).to_vec())).collect()
    }

    /// Mesma regra do `OTerm::finish`.
    fn finish(&mut self) {
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

/// Índice ingênuo em memória, construído só com `tokenize` e as stopwords:
/// nada da indexação do motor (lote, merge, Trie) é reaproveitado.
struct Oracle {
    terms: HashMap<String, OTerm>,
    positions: HashMap<String, OPos>,
    /// (tamanho inteiro, tamanho do título), sem stopwords.
    lens: HashMap<DocId, (u32, u32)>,
    /// Posições ocupadas pelo título (stopwords inclusive): uma frase que
    /// começa antes disso está no título.
    spans: HashMap<DocId, u32>,
    total_len: u64,
    total_title_len: u64,
    k1: f64,
    b: f64,
    title_weight: f64,
    title_b: f64,
    max_prefix: usize,
}

/// Uma parte da consulta resolvida no oráculo.
enum OClause<'a> {
    Term(&'a OTerm),
    /// As completações e o df do conjunto (documentos com alguma delas).
    Prefix(Vec<&'a OTerm>, usize),
    Phrase(Vec<(u32, &'a OTerm, &'a OPos)>),
}

impl Oracle {
    fn idf(&self, df: usize) -> f64 {
        let n = self.lens.len() as f64;
        let df = df as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// BM25F do plano de relevância (seção 1), escrito de novo aqui (não usa
    /// a conta do motor): `tf~ = tf_texto / B_texto + w · tf_título / B_título`.
    fn bm25(&self, idf: f64, tf: u32, title_tf: u32, doc: DocId) -> f64 {
        let n = self.lens.len().max(1) as f64;
        let (len, title_len) = self.lens[&doc];
        let body_len = f64::from(len - title_len);
        let avg_body = (self.total_len - self.total_title_len) as f64 / n;
        let avg_title = self.total_title_len as f64 / n;
        let norm = |b: f64, l: f64, avg: f64| if avg > 0.0 { 1.0 - b + b * l / avg } else { 1.0 - b };
        let (b_body, b_title) = (norm(self.b, body_len, avg_body), norm(self.title_b, f64::from(title_len), avg_title));
        let part = |tf: u32, weight: f64, b: f64| if tf == 0 || b <= 0.0 { 0.0 } else { weight * f64::from(tf) / b };
        let ptf = part(tf - title_tf, 1.0, b_body) + part(title_tf, self.title_weight, b_title);
        idf * ptf * (self.k1 + 1.0) / (ptf + self.k1)
    }

    /// Termos com o prefixo, na ordem da Trie, até o limite do motor.
    fn expand<'v>(&self, vocab: &[&'v str], prefix: &str) -> Vec<&'v str> {
        let start = vocab.partition_point(|t| *t < prefix);
        let mut matches: Vec<&str> = vocab[start..].iter().copied().take_while(|t| t.starts_with(prefix)).collect();
        matches.sort_by(trie_order);
        matches.truncate(self.max_prefix);
        matches
    }

    /// Ocorrências da frase no documento, por força bruta, e quantas
    /// começam antes de `span` (no título).
    fn phrase_count(parts: &[(u32, &OTerm, &OPos)], doc: DocId, span: u32) -> (u32, u32) {
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

    /// Documentos que casam com a consulta, em ordem. `None` se alguma frase
    /// precisaria de posições que o oráculo não coletou.
    fn resolve<'s>(&'s self, vocab: &[&str], clauses: &[Clause]) -> Option<(Vec<OClause<'s>>, Vec<DocId>)> {
        let empty = || Some((Vec::new(), Vec::new()));
        let mut resolved = Vec::new();
        for clause in clauses {
            match clause {
                Clause::Term(t) if !t.prefix => match self.terms.get(&t.text) {
                    Some(term) => resolved.push(OClause::Term(term)),
                    None => return empty(),
                },
                Clause::Term(t) => {
                    let terms: Vec<&OTerm> = self.expand(vocab, &t.text).iter().map(|w| &self.terms[*w]).collect();
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
    fn rank(&self, vocab: &[&str], clauses: &[Clause], k: usize) -> Option<(Vec<(DocId, f64)>, usize)> {
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
fn same_ranking(got: &[fire_flower::Hit], want: &[(DocId, f64)]) -> bool {
    let rel = |a: f64, b: f64| (a - b).abs() / b.abs().max(1e-12);
    got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| rel(g.score, w.1) < 1e-4 && (g.doc == w.0 || rel(g.score, w.1) < 1e-9))
}

/// Frase como o usuário digita, a partir de tokens já normalizados.
fn quote(tokens: &[String]) -> String {
    format!("\"{}\"", tokens.join(" "))
}

fn oracle(args: OracleArgs) -> AnyResult {
    let started = Instant::now();
    let mut rng = Rng(args.seed);
    // Aberto antes: o oráculo descarta as mesmas stopwords que estão gravadas no banco.
    let engine = Engine::open(&args.db, Config::default())?;
    let stopwords = engine.stopwords().clone();
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
        let mut counts: HashMap<&str, (u32, u32)> = HashMap::default();
        let (mut len, mut title_len) = (0u32, 0u32);
        for (i, t) in tokens.iter().enumerate().filter(|(_, t)| !stopwords.contains(t)) {
            let in_title = i < n_title;
            let c = counts.entry(t.as_str()).or_insert((0, 0));
            c.0 += 1;
            c.1 += u32::from(in_title);
            len += 1;
            title_len += u32::from(in_title);
        }
        // Reservatórios: documentos para consultas AND e frases, títulos para revocação.
        let mut unique: Vec<String> = counts.keys().map(|t| t.to_string()).collect();
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
            match o.terms.get_mut(t) {
                Some(e) => add(e, c),
                None => add(o.terms.entry(t.to_string()).or_default(), c),
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
    let mut vocab: Vec<&str> = o.terms.keys().map(String::as_str).collect();
    vocab.sort_unstable();
    eprintln!("oráculo pronto: {docs} docs, {} termos ({:.0}s)", vocab.len(), started.elapsed().as_secs_f64());

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
            0 if !frequent.is_empty() => rng.pick(&frequent).to_string(),
            0 | 1 => rng.pick(&vocab).to_string(),
            2 if doc.len() >= 2 => (0..rng.range(2, 3)).map(|_| rng.pick(doc).clone()).collect::<Vec<_>>().join(" "),
            3 if !doc.is_empty() => {
                let other = rng.pick(doc);
                let cut: String = other.chars().take(rng.range(2, 4)).collect();
                format!("{} {cut}*", rng.pick(doc))
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
            needed.extend(phrase_cases.iter().flatten().cloned());
            for q in &rank_cases {
                for c in parse_query(q, &stopwords) {
                    if let Clause::Phrase(parts) = c {
                        needed.extend(parts.into_iter().map(|p| p.text));
                    }
                }
            }
        }
        for doc in Corpus::open(&args.corpus, 0, args.take, args.remap_ids)? {
            let doc = doc?;
            let mut found: HashMap<&str, Vec<u32>> = HashMap::default();
            let (tokens, _) = doc_tokens(&doc.title, &doc.text);
            for (t, p) in &tokens {
                if !stopwords.contains(t) && (full || needed.contains(t)) {
                    found.entry(t.as_str()).or_default().push(*p);
                }
            }
            for (t, ps) in found {
                match o.positions.get_mut(t) {
                    Some(e) => e.push(doc.id, &ps),
                    None => o.positions.entry(t.to_string()).or_default().push(doc.id, &ps),
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
        let mut expected = o.terms[&terms[0]].docs.clone();
        for t in &terms[1..] {
            expected = fire_flower::internals::intersect(&expected, &o.terms[t].docs);
        }
        let query = terms.join(" ");
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
        let mut expected: Vec<DocId> = matches.iter().take(max_prefix).flat_map(|t| o.terms[*t].docs.iter().copied()).collect();
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
        let mut kept: Vec<(u32, &str)> = Vec::new();
        let first = window.iter().position(|t| !is_stop(t));
        for (i, t) in window.iter().enumerate() {
            if let Some(f) = first
                && !is_stop(t)
            {
                kept.push(((i - f) as u32, t.as_str()));
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
            1 => o.terms.get(kept[0].1).map(|t| t.docs.clone()).unwrap_or_default(),
            _ => {
                let parts: Option<Vec<(u32, &OTerm, &OPos)>> =
                    kept.iter().map(|&(off, t)| Some((off, o.terms.get(t)?, o.positions.get(t)?))).collect();
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
        let clauses = parse_query(q, &stopwords);
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
                "verificacoes": checks.iter().map(Check::json).collect::<Vec<_>>(), "relevancia": relevance }),
    )?;
    if total > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ================================================================== relevância

/// Uma consulta com resposta conhecida: algum dos `expected` tem que vir no topo.
struct RelCase {
    query: String,
    expected: Vec<DocId>,
    /// O título esperado (só para mostrar).
    title: String,
}

/// Conjuntos gerados do corpus e a posição que conta como acerto em cada um
/// (a métrica principal; top-1, top-3, top-10 e MRR saem para todos).
const REL_SETS: [(&str, usize); 4] = [("rel-titulo", 1), ("rel-digitando", 1), ("rel-hub", 1), ("rel-corpo", 10)];

fn write_rel_set(path: &Path, cases: &[RelCase]) -> AnyResult {
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(w, "# consulta\tIDs esperados\ttítulo (gerado por `wiki relevancia`)")?;
    for c in cases {
        let ids: Vec<String> = c.expected.iter().map(u64::to_string).collect();
        writeln!(w, "{}\t{}\t{}", c.query, ids.join(","), c.title)?;
    }
    Ok(())
}

fn read_rel_set(path: &Path) -> AnyResult<Vec<RelCase>> {
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
fn reservoir<T>(rng: &mut Rng, kept: &mut Vec<T>, seen: usize, n: usize, item: impl FnOnce() -> T) {
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
fn generate_rel_sets(corpus: &Path, take: Option<u64>, stopwords: &Stopwords, seed: u64, per_set: usize) -> AnyResult<Vec<(&'static str, Vec<RelCase>)>> {
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
    Ok(vec![("rel-titulo", by_title), ("rel-digitando", typing), ("rel-hub", hubs), ("rel-corpo", body_cases)])
}

/// Consultas à mão: `consulta \t títulos esperados (|) \t nota`. Os títulos
/// viram IDs pelo corpus; consultas sem nenhum artigo no corpus são puladas.
fn read_manual(path: &Path, corpus: &Path, take: Option<u64>) -> AnyResult<Vec<RelCase>> {
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
fn rank_of(engine: &Engine, case: &RelCase) -> AnyResult<(Option<usize>, Vec<DocId>)> {
    let hits = engine.search_top(&case.query, 1000)?;
    let pos = hits.iter().position(|h| case.expected.contains(&h.doc));
    Ok((pos, hits.iter().take(3).map(|h| h.doc).collect()))
}

#[allow(clippy::too_many_arguments)]
fn relevancia(
    db: &Path,
    corpus: &Path,
    sets_dir: &Path,
    manual: &Path,
    take: Option<u64>,
    seed: u64,
    per_set: usize,
    gerar: bool,
    config: Config,
    resumo: bool,
    label: Option<String>,
) -> AnyResult {
    let started = Instant::now();
    let engine = Engine::open(db, config)?;
    let stopwords = engine.stopwords().clone();
    std::fs::create_dir_all(sets_dir)?;

    let missing = REL_SETS.iter().any(|(name, _)| !sets_dir.join(format!("{name}.tsv")).exists());
    if gerar || missing {
        eprintln!("gerando conjuntos em {}", sets_dir.display());
        for (name, cases) in generate_rel_sets(corpus, take, &stopwords, seed, per_set)? {
            write_rel_set(&sets_dir.join(format!("{name}.tsv")), &cases)?;
        }
    }
    let mut sets: Vec<(String, usize, Vec<RelCase>)> = Vec::new();
    for (name, hit_at) in REL_SETS {
        sets.push((name.to_string(), hit_at, read_rel_set(&sets_dir.join(format!("{name}.tsv")))?));
    }
    sets.push(("rel-manual".into(), 3, read_manual(manual, corpus, take)?));

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
    if !resumo {
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

// ================================================================== bench

/// Como um conjunto é medido, pelo prefixo do nome do arquivo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `search`: todos os IDs em ordem.
    Search,
    /// `s-*`: `suggest` com limite 10.
    Suggest,
    /// `rank-*`: `search_ranked` com os 10 melhores (BM25).
    Top(usize),
}

impl Mode {
    fn of(set_name: &str) -> Self {
        if set_name.starts_with("s-") {
            Mode::Suggest
        } else if set_name.starts_with("rank-") {
            Mode::Top(10)
        } else {
            Mode::Search
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Search => "search",
            Mode::Suggest => "suggest",
            Mode::Top(_) => "top10",
        }
    }
}

#[derive(Clone)]
struct QuerySet {
    name: String,
    mode: Mode,
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
            Ok(QuerySet { mode: Mode::of(&name), name, queries })
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
    // Ranqueado: "resultados" é o total que casou, comparável com o `search`.
    let results = match set.mode {
        Mode::Search => engine.search(q)?.len(),
        Mode::Suggest => engine.suggest(q, 10)?.len(),
        Mode::Top(k) => engine.search_ranked(q, k)?.total,
    };
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
fn bench(db: &Path, paths: &[PathBuf], threads: usize, passes: usize, cold: bool, perf_time: bool, config: Config, label: Option<String>) -> AnyResult {
    let sets = read_query_sets(paths)?;
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
        let summary = summarize(&set.name, set.mode.name(), &samples, wall, threads);
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
    let watched: Vec<(&str, Option<u64>)> = ["area", "populacao", "historia", "anos", "parte"].iter().map(|t| Ok((*t, engine.term_node(t)?))).collect::<AnyResult<_>>()?;

    #[derive(Default)]
    struct Space {
        keys: u64,
        key_bytes: u64,
        value_bytes: u64,
    }
    let mut spaces: BTreeMap<String, Space> = BTreeMap::new();
    let mut largest: Vec<(usize, String)> = Vec::new();
    let mut watched_sizes: HashMap<String, usize> = HashMap::default();
    let mut watched_pos: HashMap<String, usize> = HashMap::default();
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
        for (prefix, map) in [("post:", &mut watched_sizes), ("pos:", &mut watched_pos)] {
            if let Some(id) = key.strip_prefix(prefix).and_then(|i| i.parse::<u64>().ok()) {
                for (term, node) in &watched {
                    if *node == Some(id) {
                        map.insert(term.to_string(), v.len());
                    }
                }
            }
        }
    })?;

    // Termos dos maiores valores (só nós terminais têm termo).
    let wanted: HashSet<u64> = largest.iter().filter_map(|(_, k)| k.split_once(':').and_then(|(_, id)| id.parse().ok())).collect();
    let mut names: HashMap<u64, String> = HashMap::default();
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
            let pos = watched_pos.get(*term).map_or(String::new(), |p| format!(", posições {:.1} KB", *p as f64 / 1024.0));
            println!("lista de \"{term}\": {:.1} KB{pos}", *size as f64 / 1024.0);
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
            "listas": watched_sizes, "listas_pos": watched_pos, "maiores": top,
            "posicoes": engine.has_positions(),
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

/// Resumo do conteúdo lógico: termos com (doc, tf) e posições, tamanhos dos
/// documentos e estatísticas. Não depende dos IDs de nó, então bancos
/// carregados com lotes diferentes têm que dar o mesmo resumo.
fn digest(db: &Path) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let mut nodes = Vec::new();
    engine.for_each_term(|term, node| nodes.push((term.to_string(), node)))?;
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    let (mut postings, mut positions) = (0u64, 0u64);
    for (term, node) in &nodes {
        h.write(term.as_bytes());
        h.write(&[0]);
        for p in engine.term_postings_of_node(*node)? {
            h.write(&p.doc.to_le_bytes());
            h.write(&p.tf.to_le_bytes());
            h.write(&p.title_tf.to_le_bytes());
            postings += 1;
        }
        h.write(&[0xfe]);
        for (doc, list) in engine.positions_of_node(*node)? {
            h.write(&doc.to_le_bytes());
            h.write(&(list.len() as u32).to_le_bytes());
            for p in list {
                h.write(&p.to_le_bytes());
                positions += 1;
            }
        }
        h.write(&[0xff]);
    }
    let mut lens = Vec::new();
    engine.for_each_doc_len(|doc, len, title_len| lens.push((doc, len, title_len)))?;
    lens.sort_unstable();
    for (doc, len, title_len) in &lens {
        h.write(&doc.to_le_bytes());
        h.write(&len.to_le_bytes());
        h.write(&title_len.to_le_bytes());
    }
    let stats = engine.stats()?;
    h.write(&stats.doc_count.to_le_bytes());
    h.write(&stats.total_len.to_le_bytes());
    h.write(&stats.total_title_len.to_le_bytes());
    println!(
        "{}",
        json!({ "db": db, "termos": nodes.len(), "postagens": postings, "posicoes": positions,
                "documentos": stats.doc_count, "tamanho_total": stats.total_len, "digest": format!("{:016x}", h.0) })
    );
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
                let set = set.clone();
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
        batch.add_titled(doc.id, &doc.title, &doc.text)?;
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
