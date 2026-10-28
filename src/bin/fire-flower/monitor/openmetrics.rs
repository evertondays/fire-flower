//! Uma leitura no formato do `/metrics`: OpenMetrics 1.0 ou o texto do
//! Prometheus 0.0.4. Os nomes estão em `docs/plano-monitoramento.md` e não
//! mudam sem decisão explícita (dashboards e alertas dependem deles).

use std::fmt::Write;

use fire_flower::metrics::{COMMIT_BOUNDS, COMMIT_PHASES, QUERY_BOUNDS, Snapshot};

use super::Reading;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    OpenMetrics,
    /// Texto do Prometheus 0.0.4: sem `# UNIT`, sem `info` e sem `# EOF`.
    Prometheus,
}

impl Format {
    /// Pelo cabeçalho `Accept`: OpenMetrics só se for pedido (o Prometheus
    /// pede; navegador e `curl` recebem o texto simples).
    pub(super) fn from_accept(accept: &str) -> Self {
        if accept.contains("application/openmetrics-text") { Format::OpenMetrics } else { Format::Prometheus }
    }

    pub(super) fn content_type(self) -> &'static str {
        match self {
            Format::OpenMetrics => "application/openmetrics-text; version=1.0.0; charset=utf-8",
            Format::Prometheus => "text/plain; version=0.0.4; charset=utf-8",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
    Info,
}

struct Out {
    format: Format,
    text: String,
}

impl Out {
    /// Abre uma família. `name` sem `_total` nem `_info`; `unit` é o sufixo
    /// do nome (`seconds`, `bytes`), que o OpenMetrics declara em `# UNIT`.
    fn family(&mut self, name: &str, kind: Kind, unit: Option<&str>, help: &str) {
        let om = self.format == Format::OpenMetrics;
        let (type_name, kind_name) = match (kind, om) {
            (Kind::Counter, true) => (name.to_string(), "counter"),
            (Kind::Counter, false) => (format!("{name}_total"), "counter"),
            (Kind::Gauge, _) => (name.to_string(), "gauge"),
            (Kind::Histogram, _) => (name.to_string(), "histogram"),
            (Kind::Info, true) => (name.to_string(), "info"),
            (Kind::Info, false) => (format!("{name}_info"), "gauge"),
        };
        let _ = writeln!(self.text, "# TYPE {type_name} {kind_name}");
        if om && let Some(unit) = unit {
            let _ = writeln!(self.text, "# UNIT {name} {unit}");
        }
        let _ = writeln!(self.text, "# HELP {type_name} {help}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.text.push_str(name);
        if !labels.is_empty() {
            self.text.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.text.push(',');
                }
                let _ = write!(self.text, "{k}=\"{}\"", escape(v));
            }
            self.text.push('}');
        }
        let _ = writeln!(self.text, " {}", number(value));
    }

    fn counter(&mut self, name: &str, unit: Option<&str>, help: &str, value: f64) {
        self.family(name, Kind::Counter, unit, help);
        self.sample(&format!("{name}_total"), &[], value);
    }

    fn gauge(&mut self, name: &str, unit: Option<&str>, help: &str, value: f64) {
        self.family(name, Kind::Gauge, unit, help);
        self.sample(name, &[], value);
    }

    /// Um histograma por label: (valor do label, baldes não acumulados com o
    /// `+Inf` no fim, contagem, soma em segundos).
    fn histogram(&mut self, name: &str, help: &str, label: Option<&str>, bounds: &[f64], series: &[HistogramSeries]) {
        self.family(name, Kind::Histogram, Some("seconds"), help);
        for (value, buckets, sum) in series {
            let labels: Vec<(&str, &str)> = label.map(|l| (l, value.as_str())).into_iter().collect();
            let mut cumulative = 0;
            for (i, count) in buckets.iter().enumerate() {
                cumulative += count;
                let le = bounds.get(i).map_or_else(|| "+Inf".to_string(), |b| go_float(*b));
                let mut with_le = labels.clone();
                with_le.push(("le", &le));
                self.sample(&format!("{name}_bucket"), &with_le, cumulative as f64);
            }
            self.sample(&format!("{name}_count"), &labels, cumulative as f64);
            self.sample(&format!("{name}_sum"), &labels, *sum);
        }
    }
}

/// (valor do label, baldes, soma em segundos).
type HistogramSeries = (String, Vec<u64>, f64);

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Números como o Prometheus os escreve: inteiros sem ponto, o resto no
/// formato mais curto.
fn number(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { go_float(v) }
}

/// Como o `strconv.FormatFloat(v, 'g', -1, 64)` do Go, que o Prometheus usa
/// nos limites dos baldes: `1e-05`, `0.0001`, `2.5`, `100`.
pub(super) fn go_float(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        let text = if v.is_nan() {
            "NaN"
        } else if v > 0.0 {
            "+Inf"
        } else if v < 0.0 {
            "-Inf"
        } else {
            "0"
        };
        return text.into();
    }
    let exp = v.abs().log10().floor() as i32;
    if (-4..21).contains(&exp) {
        return format!("{v}");
    }
    let s = format!("{v:e}");
    let (mantissa, e) = s.split_once('e').expect("formato {:e} tem expoente");
    let e: i32 = e.parse().expect("expoente inteiro");
    format!("{mantissa}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

/// O texto do `/metrics` de uma leitura.
pub(super) fn render(reading: &Reading, format: Format) -> String {
    let mut out = Out { format, text: String::with_capacity(16 << 10) };
    if let Some(s) = &reading.snapshot {
        engine(&mut out, s);
        writer(&mut out, s);
        rocksdb(&mut out, s);
        out.family("fireflower_build", Kind::Info, None, "versão do fire-flower e do formato do banco");
        let format_version = s.format_version.to_string();
        out.sample("fireflower_build_info", &[("version", &s.version), ("format_version", &format_version)], 1.0);
    }
    if let Some(age) = reading.snapshot_age_secs {
        out.gauge(
            "fireflower_exporter_snapshot_age_seconds",
            Some("seconds"),
            "idade do snapshot lido (cresce com o motor parado)",
            age,
        );
    }
    process(&mut out, reading);
    disk(&mut out, reading);
    if format == Format::OpenMetrics {
        out.text.push_str("# EOF\n");
    }
    out.text
}

fn engine(out: &mut Out, s: &Snapshot) {
    let per_op = |out: &mut Out, name: &str, help: &str, value: &dyn Fn(&fire_flower::metrics::OpCounters) -> f64| {
        out.family(name, Kind::Counter, None, help);
        for q in &s.queries {
            out.sample(&format!("{name}_total"), &[("op", &q.op)], value(q));
        }
    };
    per_op(out, "fireflower_queries", "consultas por ponto de entrada da API", &|q| q.count as f64);
    per_op(out, "fireflower_query_errors", "consultas que deram erro", &|q| q.errors as f64);
    per_op(out, "fireflower_engine_reads", "Gets do motor nas consultas", &|q| q.reads as f64);
    let series: Vec<HistogramSeries> =
        s.queries.iter().map(|q| (q.op.clone(), q.buckets.clone(), q.sum_nanos as f64 / 1e9)).collect();
    out.histogram("fireflower_query_duration_seconds", "duração das consultas", Some("op"), &QUERY_BOUNDS, &series);
    out.family("fireflower_epoch_cache_lookups", Kind::Counter, None, "consultas aos caches de época das buscas");
    let c = &s.cache;
    for (cache, result, value) in [
        ("node", "hit", c.node_hits),
        ("node", "miss", c.node_misses),
        ("norm", "hit", c.norm_hits),
        ("norm", "miss", c.norm_misses),
    ] {
        out.sample("fireflower_epoch_cache_lookups_total", &[("cache", cache), ("result", result)], value as f64);
    }
}

fn writer(out: &mut Out, s: &Snapshot) {
    let w = &s.writer;
    let secs = |nanos: u64| nanos as f64 / 1e9;
    out.counter("fireflower_commits", None, "commits gravados", w.commits as f64);
    out.counter("fireflower_commit_errors", None, "commits que falharam", w.commit_errors as f64);
    let series = [(String::new(), w.commit_buckets.clone(), secs(w.commit_nanos))];
    out.histogram("fireflower_commit_duration_seconds", "duração dos commits", None, &COMMIT_BOUNDS, &series);
    out.family("fireflower_commit_phase_seconds", Kind::Counter, Some("seconds"), "tempo dos commits por fase");
    for (phase, nanos) in COMMIT_PHASES.iter().zip(&w.phase_nanos) {
        out.sample("fireflower_commit_phase_seconds_total", &[("phase", phase)], secs(*nanos));
    }
    let n = |v: u64| v as f64;
    let counters: [Metric; 10] = [
        ("fireflower_indexed_documents", None, "documentos adicionados ou substituídos", n(w.indexed_documents)),
        ("fireflower_replaced_documents", None, "documentos substituídos", n(w.replaced_documents)),
        ("fireflower_deleted_documents", None, "documentos removidos", n(w.deleted_documents)),
        ("fireflower_killed_versions", None, "versões que morreram (substituídas ou removidas)", n(w.killed_versions)),
        ("fireflower_commit_batch_bytes", Some("bytes"), "bytes dos lotes gravados", n(w.batch_bytes)),
        ("fireflower_writer_lock_wait_seconds", Some("seconds"), "espera pelo escritor", secs(w.lock_wait_nanos)),
        ("fireflower_compactions", None, "compactações", n(w.compactions)),
        ("fireflower_compaction_seconds", Some("seconds"), "tempo das compactações", secs(w.compaction_nanos)),
        ("fireflower_purged_versions", None, "versões mortas tiradas das listas", n(w.purged_versions)),
        ("fireflower_emptied_words", None, "palavras sem documento tiradas do índice", n(w.emptied_words)),
    ];
    for (name, unit, help, value) in counters {
        out.counter(name, unit, help, value);
    }
    let gauges: [Metric; 4] = [
        ("fireflower_writer_cache_nodes", None, "nós da Trie no cache do escritor", n(w.cache_nodes)),
        ("fireflower_documents", None, "documentos no banco", n(w.documents)),
        ("fireflower_dead_versions", None, "versões mortas ainda nas listas (saem na compactação)", n(w.dead_versions)),
        (
            "fireflower_pending_edits",
            None,
            "documentos substituídos ou removidos desde a última compactação",
            n(w.pending_edits),
        ),
    ];
    for (name, unit, help, value) in gauges {
        out.gauge(name, unit, help, value);
    }
}

/// (nome, unidade, ajuda, valor) de uma métrica sem labels.
type Metric = (&'static str, Option<&'static str>, &'static str, f64);

fn rocksdb(out: &mut Out, s: &Snapshot) {
    let Some(p) = &s.rocksdb else { return };
    out.family("fireflower_rocksdb_sst_files", Kind::Gauge, None, "arquivos SST por nível");
    for (level, files) in p.sst_files.iter().enumerate() {
        out.sample("fireflower_rocksdb_sst_files", &[("level", &level.to_string())], *files as f64);
    }
    out.family("fireflower_rocksdb_sst_size_bytes", Kind::Gauge, Some("bytes"), "tamanho dos SSTs");
    out.sample("fireflower_rocksdb_sst_size_bytes", &[("state", "live")], p.live_sst_bytes as f64);
    out.sample("fireflower_rocksdb_sst_size_bytes", &[("state", "total")], p.total_sst_bytes as f64);
    let n = |v: u64| v as f64;
    let gauges: [Metric; 8] = [
        (
            "fireflower_rocksdb_pending_compaction_bytes",
            Some("bytes"),
            "bytes que a compactação ainda precisa reescrever",
            n(p.pending_compaction_bytes),
        ),
        ("fireflower_rocksdb_running_compactions", None, "compactações rodando", n(p.running_compactions)),
        ("fireflower_rocksdb_running_flushes", None, "flushes rodando", n(p.running_flushes)),
        ("fireflower_rocksdb_memtable_flush_pending", None, "1 se há flush pendente", n(p.memtable_flush_pending)),
        ("fireflower_rocksdb_write_stopped", None, "1 se as escritas estão paradas", n(p.write_stopped)),
        (
            "fireflower_rocksdb_block_cache_capacity_bytes",
            Some("bytes"),
            "capacidade do block cache",
            n(p.block_cache_capacity_bytes),
        ),
        ("fireflower_rocksdb_snapshots", None, "snapshots do RocksDB abertos", n(p.snapshots)),
        (
            "fireflower_rocksdb_delayed_write_rate_bytes_per_second",
            None,
            "taxa das escritas atrasadas (0 = sem atraso)",
            n(p.delayed_write_rate),
        ),
    ];
    for (name, unit, help, value) in gauges {
        out.gauge(name, unit, help, value);
    }
    let errors = n(p.background_errors);
    out.counter("fireflower_rocksdb_background_errors", None, "erros em segundo plano do RocksDB", errors);
    out.family("fireflower_rocksdb_memory_bytes", Kind::Gauge, Some("bytes"), "memória do RocksDB por componente");
    for (component, value) in [
        ("memtables", p.memtables_bytes),
        ("table_readers", p.table_readers_bytes),
        ("block_cache", p.block_cache_bytes),
        ("block_cache_pinned", p.block_cache_pinned_bytes),
    ] {
        out.sample("fireflower_rocksdb_memory_bytes", &[("component", component)], value as f64);
    }
    let age = if p.oldest_snapshot_unix == 0 { 0.0 } else { (p.read_unix - p.oldest_snapshot_unix as f64).max(0.0) };
    out.gauge("fireflower_rocksdb_oldest_snapshot_age_seconds", Some("seconds"), "idade do snapshot mais antigo", age);
}

fn process(out: &mut Out, reading: &Reading) {
    let Some(p) = &reading.process else { return };
    out.counter("process_cpu_seconds", Some("seconds"), "CPU do processo do motor (usuário + sistema)", p.cpu_secs);
    out.gauge("process_resident_memory_bytes", Some("bytes"), "memória residente", p.resident_bytes as f64);
    out.gauge("process_virtual_memory_bytes", Some("bytes"), "memória virtual", p.virtual_bytes as f64);
    out.gauge("process_threads", None, "threads do processo", p.threads as f64);
    let optional: [(&str, Option<&str>, &str, Option<f64>); 3] = [
        ("process_open_fds", None, "descritores abertos", p.open_fds.map(|v| v as f64)),
        ("process_max_fds", None, "limite de descritores", p.max_fds.map(|v| v as f64)),
        ("process_start_time_seconds", Some("seconds"), "início do processo (Unix)", p.start_unix),
    ];
    for (name, unit, help, value) in optional {
        if let Some(v) = value {
            out.gauge(name, unit, help, v);
        }
    }
    if let (Some(read), Some(written)) = (p.io_read_bytes, p.io_write_bytes) {
        out.counter("process_io_read_bytes", Some("bytes"), "bytes lidos do disco", read as f64);
        out.counter("process_io_write_bytes", Some("bytes"), "bytes gravados no disco", written as f64);
    }
}

fn disk(out: &mut Out, reading: &Reading) {
    let Some(d) = &reading.disk else { return };
    let help = "tamanho da pasta do banco por tipo de arquivo";
    out.family("fireflower_disk_index_bytes", Kind::Gauge, Some("bytes"), help);
    for (kind, value) in [("sst", d.sst_bytes), ("wal", d.wal_bytes), ("other", d.other_bytes)] {
        out.sample("fireflower_disk_index_bytes", &[("kind", kind)], value as f64);
    }
    if let (Some(free), Some(size)) = (d.free_bytes, d.size_bytes) {
        out.gauge("fireflower_filesystem_free_bytes", Some("bytes"), "espaço livre no disco do banco", free as f64);
        out.gauge("fireflower_filesystem_size_bytes", Some("bytes"), "tamanho do disco do banco", size as f64);
    }
}
