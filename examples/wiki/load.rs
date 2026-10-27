//! `wiki load`: carga com o monitor por janela e a divisão do tempo do commit.

use crate::AnyResult;
use crate::env::machine;
use crate::env::write_metrics;
use crate::env::config_json;
use crate::env::tickers;
use fire_flower::CommitStats;
use serde_json::Value;
use rustc_hash::FxHashMap as HashMap;
use crate::env::save_result;
use crate::env::mb;
use crate::env::dir_size;
use crate::env::rss;
use std::time::Instant;
use crate::env::swap_used;
use crate::corpus::stem;
use crate::corpus::Corpus;
use fire_flower::Engine;
use fire_flower::Config;
use std::path::Path;
use serde_json::json;

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
        "stemming": engine.stemmer().stemming().code(),
        "flexoes": engine.inflector().stemming().code(),
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
        self.total += c.total_secs;
    }

    /// Frações do tempo `secs` em cada etapa do commit, mais contagens por documento.
    pub(crate) fn json(&self, secs: f64, docs: f64) -> Value {
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
    pub(crate) fn collapse(&self, batch: usize) -> Value {
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
