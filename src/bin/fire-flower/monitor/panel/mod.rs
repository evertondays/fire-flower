//! O painel do `fire-flower monitor`: as mesmas leituras do `/metrics`, com
//! taxas, p50/p99 estimados pelos baldes, sparklines e alertas. O histórico
//! fica aqui; o motor não guarda nada.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use fire_flower::metrics::{COMMIT_BOUNDS, QUERY_BOUNDS, Snapshot};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use super::{Reading, Source};

mod draw;

/// Amostras guardadas para as sparklines.
const HISTORY: usize = 120;
/// L0 com tantos arquivos já atrasa as escritas (`level0_slowdown_writes_trigger`).
const L0_ALERT: u64 = 20;
/// Espaço livre abaixo disto vira alerta.
const DISK_FREE_ALERT: f64 = 0.10;
/// Versões mortas acima desta fração dos documentos: hora de compactar.
const DEAD_ALERT: f64 = 0.10;

pub(super) fn run(source: Source) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, Panel::new(source));
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, mut panel: Panel) -> io::Result<()> {
    panel.tick();
    let mut next = Instant::now() + panel.interval;
    let mut dirty = true;
    while !panel.quit {
        if dirty {
            terminal.draw(|frame| panel.draw(frame))?;
            dirty = false;
        }
        let wait = next.saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                panel.on_key(key.code, key.modifiers);
                dirty = true;
            }
            continue;
        }
        next = Instant::now() + panel.interval;
        if !panel.paused {
            // Só redesenha quando há snapshot novo ou os alertas mudaram.
            dirty |= panel.tick();
        }
    }
    Ok(())
}

/// Uma medida por segundo, derivada de duas leituras seguidas.
#[derive(Debug, Clone, Default)]
pub(super) struct OpView {
    pub(super) op: String,
    pub(super) qps: f64,
    pub(super) p50: Option<f64>,
    pub(super) p99: Option<f64>,
    pub(super) reads_per_query: Option<f64>,
    pub(super) errors: u64,
}

/// O que o painel mostra além da leitura crua: taxas e quantis do último
/// intervalo.
#[derive(Debug, Clone, Default)]
pub(super) struct Rates {
    pub(super) ops: Vec<OpView>,
    pub(super) qps: f64,
    pub(super) node_hit: Option<f64>,
    pub(super) norm_hit: Option<f64>,
    pub(super) docs_per_sec: f64,
    pub(super) commits_per_sec: f64,
    pub(super) commit_p50: Option<f64>,
    pub(super) commit_p99: Option<f64>,
    /// Espera pela trava do escritor, em ms por segundo.
    pub(super) lock_wait_ms: f64,
    pub(super) replaced_per_sec: f64,
    pub(super) deleted_per_sec: f64,
    pub(super) cpu_percent: Option<f64>,
    pub(super) io_read_per_sec: Option<f64>,
    pub(super) io_write_per_sec: Option<f64>,
}

pub(super) struct Panel {
    source: Source,
    pub(super) interval: Duration,
    pub(super) paused: bool,
    pub(super) help: bool,
    quit: bool,
    pub(super) reading: Reading,
    pub(super) rates: Option<Rates>,
    pub(super) alerts: Vec<String>,
    /// Sparklines: consultas/s, documentos/s, edições/s, CPU %.
    pub(super) qps: VecDeque<u64>,
    pub(super) docs: VecDeque<u64>,
    pub(super) edits: VecDeque<u64>,
    pub(super) cpu: VecDeque<u64>,
    /// Compactação pendente nas últimas leituras (alerta se só cresce).
    pending: VecDeque<u64>,
}

impl Panel {
    pub(super) fn new(source: Source) -> Self {
        Self {
            source,
            interval: Duration::from_secs(1),
            paused: false,
            help: false,
            quit: false,
            reading: Reading::default(),
            rates: None,
            alerts: Vec::new(),
            qps: VecDeque::new(),
            docs: VecDeque::new(),
            edits: VecDeque::new(),
            cpu: VecDeque::new(),
            pending: VecDeque::new(),
        }
    }

    /// Lê de novo; `true` se há algo novo para desenhar.
    fn tick(&mut self) -> bool {
        let reading = self.source.read();
        self.push(reading)
    }

    /// Acrescenta uma leitura. `true` se o snapshot mudou ou os alertas mudaram.
    pub(super) fn push(&mut self, reading: Reading) -> bool {
        let seq = |r: &Reading| r.snapshot.as_ref().map(|s| s.sequence);
        let changed = seq(&reading) != seq(&self.reading);
        if changed && let Some(rates) = rates(&self.reading, &reading) {
            let add = |series: &mut VecDeque<u64>, v: f64| {
                series.push_back(v.round().max(0.0) as u64);
                if series.len() > HISTORY {
                    series.pop_front();
                }
            };
            add(&mut self.qps, rates.qps);
            add(&mut self.docs, rates.docs_per_sec);
            add(&mut self.edits, rates.replaced_per_sec + rates.deleted_per_sec);
            add(&mut self.cpu, rates.cpu_percent.unwrap_or(0.0));
            self.rates = Some(rates);
        }
        if changed && let Some(p) = reading.snapshot.as_ref().and_then(|s| s.rocksdb.as_ref()) {
            self.pending.push_back(p.pending_compaction_bytes);
            if self.pending.len() > 4 {
                self.pending.pop_front();
            }
        }
        self.reading = reading;
        let alerts = alerts(&self.reading, &self.pending, self.interval);
        let alerts_changed = alerts != self.alerts;
        self.alerts = alerts;
        changed || alerts_changed
    }

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Char('1') => self.interval = Duration::from_secs(1),
            KeyCode::Char('5') => self.interval = Duration::from_secs(5),
            KeyCode::Char('i') => {
                let next = match self.interval.as_secs() {
                    1 => 5,
                    5 => 15,
                    _ => 1,
                };
                self.interval = Duration::from_secs(next);
            }
            _ => {}
        }
    }
}

/// As taxas entre duas leituras do mesmo processo; `None` sem as duas.
pub(super) fn rates(prev: &Reading, cur: &Reading) -> Option<Rates> {
    let (a, b) = (prev.snapshot.as_ref()?, cur.snapshot.as_ref()?);
    let dt = b.written_unix - a.written_unix;
    if a.pid != b.pid || a.opened_unix != b.opened_unix || dt <= 0.0 {
        return None; // outro processo, ou o motor reabriu: os contadores recomeçaram
    }
    let per_sec = |x: u64, y: u64| y.saturating_sub(x) as f64 / dt;
    let ops = b
        .queries
        .iter()
        .map(|q| {
            let before = a.queries.iter().find(|p| p.op == q.op).cloned().unwrap_or_default();
            let deltas = delta(&before.buckets, &q.buckets);
            let count = q.count.saturating_sub(before.count);
            OpView {
                op: q.op.clone(),
                qps: count as f64 / dt,
                p50: quantile(&QUERY_BOUNDS, &deltas, 0.5),
                p99: quantile(&QUERY_BOUNDS, &deltas, 0.99),
                reads_per_query: (count > 0).then(|| q.reads.saturating_sub(before.reads) as f64 / count as f64),
                errors: q.errors.saturating_sub(before.errors),
            }
        })
        .collect::<Vec<_>>();
    let hit = |hits: u64, misses: u64, h0: u64, m0: u64| {
        let (h, m) = (hits.saturating_sub(h0), misses.saturating_sub(m0));
        (h + m > 0).then(|| h as f64 / (h + m) as f64)
    };
    let (c0, c1) = (&a.cache, &b.cache);
    let (w0, w1) = (&a.writer, &b.writer);
    let commits = delta(&w0.commit_buckets, &w1.commit_buckets);
    let proc_dt = cur.read_unix - prev.read_unix;
    let (p0, p1) = (prev.process.as_ref(), cur.process.as_ref());
    let io = |f: fn(&super::process::Process) -> Option<u64>| {
        let (x, y) = (f(p0?)?, f(p1?)?);
        (proc_dt > 0.0).then(|| y.saturating_sub(x) as f64 / proc_dt)
    };
    Some(Rates {
        qps: ops.iter().map(|o| o.qps).sum(),
        ops,
        node_hit: hit(c1.node_hits, c1.node_misses, c0.node_hits, c0.node_misses),
        norm_hit: hit(c1.norm_hits, c1.norm_misses, c0.norm_hits, c0.norm_misses),
        docs_per_sec: per_sec(w0.indexed_documents, w1.indexed_documents),
        commits_per_sec: per_sec(w0.commits, w1.commits),
        commit_p50: quantile(&COMMIT_BOUNDS, &commits, 0.5),
        commit_p99: quantile(&COMMIT_BOUNDS, &commits, 0.99),
        lock_wait_ms: per_sec(w0.lock_wait_nanos, w1.lock_wait_nanos) / 1e6,
        replaced_per_sec: per_sec(w0.replaced_documents, w1.replaced_documents),
        deleted_per_sec: per_sec(w0.deleted_documents, w1.deleted_documents),
        cpu_percent: match (p0, p1) {
            (Some(x), Some(y)) if proc_dt > 0.0 => Some((y.cpu_secs - x.cpu_secs).max(0.0) / proc_dt * 100.0),
            _ => None,
        },
        io_read_per_sec: io(|p| p.io_read_bytes),
        io_write_per_sec: io(|p| p.io_write_bytes),
    })
}

fn delta(before: &[u64], after: &[u64]) -> Vec<u64> {
    after.iter().enumerate().map(|(i, &v)| v.saturating_sub(before.get(i).copied().unwrap_or(0))).collect()
}

/// O quantil `q` de um histograma (baldes não acumulados, o último é o
/// `+Inf`), com interpolação linear dentro do balde, como o
/// `histogram_quantile` do Prometheus. No `+Inf`, o maior limite.
pub(super) fn quantile(bounds: &[f64], buckets: &[u64], q: f64) -> Option<f64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = q * total as f64;
    let mut seen = 0u64;
    for (i, &count) in buckets.iter().enumerate() {
        if count > 0 && (seen + count) as f64 >= rank {
            let Some(&upper) = bounds.get(i) else { return bounds.last().copied() };
            let lower = if i == 0 { 0.0 } else { bounds[i - 1] };
            return Some(lower + (upper - lower) * ((rank - seen as f64) / count as f64).clamp(0.0, 1.0));
        }
        seen += count;
    }
    bounds.last().copied()
}

/// Os alertas de uma leitura, do mais grave para o menos.
pub(super) fn alerts(reading: &Reading, pending: &VecDeque<u64>, interval: Duration) -> Vec<String> {
    let mut out = Vec::new();
    let Some(s) = &reading.snapshot else {
        out.push("sem snapshot: o motor precisa abrir o banco com Config::metrics".into());
        return out;
    };
    let stale = 3.0 * s.interval_secs.max(interval.as_secs_f64()) + 2.0;
    if reading.snapshot_age_secs.is_some_and(|age| age > stale) {
        let age = reading.snapshot_age_secs.unwrap_or(0.0);
        out.push(format!("snapshot velho ({age:.0} s): o motor parou ou travou"));
    } else if reading.process.is_none() && cfg!(target_os = "linux") {
        out.push(format!("processo {} não visível no /proc", s.pid));
    }
    if let Some(p) = &s.rocksdb {
        if p.write_stopped > 0 {
            out.push("escrita parada pelo RocksDB".into());
        }
        if p.background_errors > 0 {
            out.push(format!("{} erro(s) em segundo plano do RocksDB", p.background_errors));
        }
        if p.sst_files.first().is_some_and(|&l0| l0 >= L0_ALERT) {
            out.push(format!("L0 com {} arquivos: escritas atrasando", p.sst_files[0]));
        }
    }
    if pending.len() >= 4 && pending.iter().zip(pending.iter().skip(1)).all(|(a, b)| b > a) {
        out.push("compactação pendente só cresce".into());
    }
    if let Some(d) = &reading.disk
        && let (Some(free), Some(size)) = (d.free_bytes, d.size_bytes)
        && size > 0
        && (free as f64) < DISK_FREE_ALERT * size as f64
    {
        out.push(format!("disco com {:.0}% livre", free as f64 * 100.0 / size as f64));
    }
    if too_many_dead(s) {
        out.push(format!("{} versões mortas: compacte (fire-flower compact)", s.writer.dead_versions));
    }
    out
}

fn too_many_dead(s: &Snapshot) -> bool {
    let w = &s.writer;
    w.dead_versions > 1000 && w.dead_versions as f64 > DEAD_ALERT * w.documents.max(1) as f64
}
