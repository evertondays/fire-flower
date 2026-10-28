//! `wiki load`: carga com o monitor por janela e a divisão do tempo do commit.

use std::path::Path;
use std::time::Instant;

use fire_flower::{CommitStats, Config, Engine};
use rustc_hash::FxHashMap as HashMap;
use serde_json::{Value, json};

use crate::AnyResult;
use crate::corpus::{Corpus, Doc, stem};
use crate::edits::{EditPlanArgs, final_docs};
use crate::env::{config_json, dir_size, machine, mb, rss, save_result, swap_used, tickers, write_metrics};

#[allow(clippy::too_many_arguments)]
pub(crate) fn load(
    db: &Path,
    corpus_path: &Path,
    batch_size: usize,
    skip: u64,
    take: Option<u64>,
    remap_ids: bool,
    streaming: bool,
    config: Config,
    edits: Option<EditPlanArgs>,
    label: Option<String>,
) -> AnyResult {
    let engine = Engine::open(db, config.clone())?;
    // Com `edits`: o estado final da fatia depois do plano, para comparar
    // (pelo `digest`) com um banco que passou pelas edições.
    let corpus: Box<dyn Iterator<Item = AnyResult<Doc>>> = match &edits {
        Some(plan) => {
            let take = take.ok_or("--edits precisa de --take (o tamanho da fatia)")?;
            Box::new(final_docs(corpus_path, take, plan)?.into_iter().map(Ok))
        }
        None => Box::new(Corpus::open(corpus_path, skip, take, remap_ids)?),
    };
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
        "languages": engine.languages().iter().map(|l| l.code()).collect::<Vec<_>>(),
        "format": fire_flower::FORMAT_VERSION, "positions": engine.has_positions(), "edits": engine.has_edits(),
        "stemming": engine.stemmer().stemming().code(),
        "inflections": engine.inflector().stemming().code(),
        "docs": docs, "bytes_corpus": bytes, "seconds": secs,
        "docs_per_s": docs as f64 / secs, "mb_per_s": mb(bytes) / secs,
        "peak_rss": peak, "db_bytes": db_bytes,
        "db_per_text": db_bytes as f64 / bytes.max(1) as f64,
        "trie_nodes": engine.trie_node_count()?,
        "estimated_keys": engine.property("rocksdb.estimate-num-keys").and_then(|v| v.parse::<u64>().ok()),
        "l0_files": engine.property("rocksdb.num-files-at-level0").and_then(|v| v.parse::<u64>().ok()),
        "write": write_metrics(&engine),
        "swap_growth": swap_used().saturating_sub(swap_before),
        "windows": monitor.windows, "collapse": collapse,
        "commit_steps": monitor.all.json(secs, docs as f64),
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
pub(crate) const LOAD_WINDOW: u64 = 10_000;

/// Acompanha a carga janela a janela: velocidade e os sinais que explicam uma
/// queda (leituras do escritor, filtros relidos, travamento de escrita do
/// RocksDB, nível 0, memória). Foi assim que se achou o colapso da carga
/// inteira: filtros de Bloom maiores que uma parte do block cache.
pub(crate) struct LoadMonitor {
    pub(crate) at: Instant,
    pub(crate) docs: u64,
    pub(crate) bytes: u64,
    pub(crate) reads: u64,
    /// Soma dos commits desde o começo da janela.
    pub(crate) commits: CommitTotals,
    /// Soma desde o começo da carga.
    pub(crate) all: CommitTotals,
    pub(crate) tickers: HashMap<String, u64>,
    pub(crate) windows: Vec<Value>,
}

/// Soma de [`CommitStats`] de vários commits.
#[derive(Default, Clone, Copy)]
pub(crate) struct CommitTotals {
    pub(crate) commits: u64,
    pub(crate) terms: u64,
    pub(crate) node_reads: u64,
    pub(crate) new_nodes: u64,
    pub(crate) batch_bytes: u64,
    pub(crate) cache_nodes: u64,
    pub(crate) sort: f64,
    pub(crate) trie: f64,
    pub(crate) encode: f64,
    pub(crate) nodes: f64,
    pub(crate) write: f64,
    pub(crate) evict: f64,
    pub(crate) documents: f64,
    pub(crate) fwd_reads: u64,
    pub(crate) total: f64,
}

impl CommitTotals {
    pub(crate) fn add(&mut self, c: &CommitStats) {
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
        self.documents += c.documents_secs;
        self.fwd_reads += c.fwd_reads;
        self.total += c.total_secs;
    }

    /// Frações do tempo `secs` em cada etapa do commit, mais contagens por documento.
    pub(crate) fn json(&self, secs: f64, docs: f64) -> Value {
        let f = |v: f64| v / secs.max(1e-9);
        json!({
            "commit_fraction": f(self.total), "sort_fraction": f(self.sort), "trie_fraction": f(self.trie),
            "encode_fraction": f(self.encode), "nodes_fraction": f(self.nodes), "write_fraction": f(self.write),
            "evict_fraction": f(self.evict), "documents_fraction": f(self.documents), "commits": self.commits,
            "fwd_reads_per_doc": self.fwd_reads as f64 / docs.max(1.0),
            "terms_per_commit": self.terms as f64 / self.commits.max(1) as f64,
            "commit_reads_per_doc": self.node_reads as f64 / docs.max(1.0),
            "new_nodes_per_doc": self.new_nodes as f64 / docs.max(1.0),
            "batch_mb": mb(self.batch_bytes) / self.commits.max(1) as f64,
            "node_cache": self.cache_nodes,
        })
    }
}

impl LoadMonitor {
    pub(crate) fn new(engine: &Engine) -> Self {
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

    pub(crate) fn add_commit(&mut self, stats: CommitStats) {
        self.commits.add(&stats);
        self.all.add(&stats);
    }

    pub(crate) fn window(&mut self, engine: &Engine, db: &Path, docs: u64, bytes: u64, started: Instant) {
        let dt = self.at.elapsed().as_secs_f64().max(1e-9);
        let now = engine.rocksdb_stats().map(|s| tickers(&s)).unwrap_or_default();
        let delta = |k: &str| now.get(k).copied().unwrap_or(0).saturating_sub(self.tickers.get(k).copied().unwrap_or(0));
        let reads = fire_flower::internals::reads_on_this_thread();
        let prop = |name: &str| engine.property(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let n = (docs - self.docs).max(1) as f64;
        let (cur, _) = rss();
        let mut w = json!({
            "docs": docs, "seconds": started.elapsed().as_secs_f64(), "window_s": dt, "window_mb": mb(bytes - self.bytes),
            "docs_per_s": (docs - self.docs) as f64 / dt, "mb_per_s": mb(bytes - self.bytes) / dt,
            "gets_per_doc": (reads - self.reads) as f64 / n,
            "filter_mb": mb(delta("rocksdb.block.cache.filter.bytes.insert")),
            "filter_miss": delta("rocksdb.block.cache.filter.miss"),
            "data_miss": delta("rocksdb.block.cache.data.miss"),
            "stall_s": delta("rocksdb.stall.micros") as f64 / 1e6,
            "l0": prop("rocksdb.num-files-at-level0"),
            "pending_compaction_mb": mb(prop("rocksdb.estimate-pending-compaction-bytes")),
            "rss_mb": mb(cur), "ram_free_mb": mb(machine()["ram_free"].as_u64().unwrap_or(0)),
            "swap_mb": mb(swap_used()), "db_mb": mb(dir_size(db)),
        });
        if let (Value::Object(map), Value::Object(extra)) = (&mut w, self.commits.json(dt, n)) {
            map.extend(extra);
        }
        let f = |k: &str| w[k].as_f64().unwrap_or(0.0);
        eprintln!(
            "{docs:>9} docs {:>6.0} docs/s {:>5.1} MB/s | commit {:>3.0}% (trie {:>3.0}% codif {:>3.0}% grava {:>3.0}% nós {:>2.0}%) leituras/doc {:>5.1} cache {:>4.0}k | filtro {:>4.0} MB stall {:>3.1}s L0 {:>2} | RSS {:>5.0} MB ({:.0}s)",
            f("docs_per_s"), f("mb_per_s"), f("commit_fraction") * 100.0, f("trie_fraction") * 100.0,
            f("encode_fraction") * 100.0, f("write_fraction") * 100.0, f("nodes_fraction") * 100.0,
            f("commit_reads_per_doc"), f("node_cache") / 1000.0, f("filter_mb"), f("stall_s"), f("l0"),
            f("rss_mb"), f("seconds")
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
    pub(crate) fn collapse(&self, batch: usize) -> Value {
        let group = (batch as u64).div_ceil(LOAD_WINDOW).max(1) as usize;
        let num = |w: &Value, k: &str| w[k].as_f64().unwrap_or(0.0);
        let speeds: Vec<(f64, &Value)> = self
            .windows
            .chunks(group)
            .map(|c| (c.iter().map(|w| num(w, "window_mb")).sum::<f64>() / c.iter().map(|w| num(w, "window_s")).sum::<f64>().max(1e-9), c.last().unwrap()))
            .collect();
        for i in 3..speeds.len() {
            let mut before: Vec<f64> = speeds[i.saturating_sub(10)..i].iter().map(|s| s.0).collect();
            before.sort_by(f64::total_cmp);
            let median = before[before.len() / 2];
            let (speed, w) = speeds[i];
            if speed < 0.2 * median {
                eprintln!(
                    "\n⚠ colapso em {} docs: {:.1} → {:.1} MB/s; filtro relido {:.0} MB, Gets/doc {:.0}, stall {:.1}s, L0 {}, RAM livre {:.0} MB",
                    w["docs"], median, speed, num(w, "filter_mb"), num(w, "gets_per_doc"), num(w, "stall_s"), w["l0"],
                    num(w, "ram_free_mb")
                );
                return json!({ "docs": w["docs"], "mb_per_s_before": median, "window": w });
            }
        }
        Value::Null
    }
}
