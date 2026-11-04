//! Desenho do painel: cabeçalho, processo, busca, indexação, edição e
//! remoção, RocksDB, memória, disco, alertas e rodapé.

use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Sparkline, Table};

use super::Panel;
use crate::style::{ACCENT, MUTED, format_count, panel, truncate_left};

impl Panel {
    pub(in crate::monitor) fn draw(&self, frame: &mut Frame) {
        let ops = self.reading.snapshot.as_ref().map_or(0, |s| s.queries.len()) as u16;
        let alerts = self.alerts.len().max(1) as u16;
        let [header, process, search, middle, bottom, alert, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(ops + 4),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(alerts + 2),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_header(frame, header);
        if self.help {
            self.draw_help(frame, process.union(bottom));
        } else {
            self.draw_process(frame, process);
            self.draw_search(frame, search);
            let [indexing, edits, rocksdb] =
                Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(middle);
            self.draw_indexing(frame, indexing);
            self.draw_edits(frame, edits);
            self.draw_rocksdb(frame, rocksdb);
            let [memory, disk] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(bottom);
            self.draw_memory(frame, memory);
            self.draw_disk(frame, disk);
        }
        self.draw_alerts(frame, alert);
        self.draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::styled(" ✿ fire-flower monitor ", Style::new().fg(Color::Black).bg(ACCENT).bold())];
        if let Some(s) = &self.reading.snapshot {
            let up = self.reading.read_unix - s.opened_unix;
            let age = self.reading.snapshot_age_secs.unwrap_or(0.0);
            let text = format!(
                "  {} · pid {} · aberto há {} · formato v{} · snapshot de {} s{}",
                truncate_left(&s.db, 40),
                s.pid,
                duration(up),
                s.format_version,
                dec(age, 1),
                if self.paused { " · PAUSADO" } else { "" }
            );
            spans.push(Span::styled(text, Style::new().fg(MUTED)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_process(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Processo ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [spark, text] = Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).areas(inner);
        sparkline(frame, spark, &self.cpu);
        let r = self.rates.as_ref();
        let line = match &self.reading.process {
            Some(p) => {
                let fds = match (p.open_fds, p.max_fds) {
                    (Some(open), Some(max)) => format!("{open}/{max}"),
                    (Some(open), None) => open.to_string(),
                    _ => "—".into(),
                };
                vec![
                    key("CPU "),
                    value(r.and_then(|r| r.cpu_percent).map_or("—".into(), |c| format!("{}%", dec(c, 0)))),
                    key("   RSS "),
                    value(bytes(p.resident_bytes)),
                    key("   threads "),
                    value(p.threads.to_string()),
                    key("   fds "),
                    value(fds),
                ]
            }
            None => vec![key("processo não visível (outro usuário ou PID namespace)")],
        };
        frame.render_widget(Paragraph::new(Line::from(line)), text);
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Busca ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [left, right] = Layout::horizontal([Constraint::Length(24), Constraint::Min(30)]).areas(inner);
        let [label, spark] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(left);
        let qps = self.rates.as_ref().map_or("—".into(), |r| dec(r.qps, 0));
        frame.render_widget(Paragraph::new(Line::from(vec![key("consultas/s "), value(qps)])), label);
        sparkline(frame, spark, &self.qps);
        let header = Row::new(["op", "qps", "p50*", "p99*", "Gets/consulta", "erros"]).style(Style::new().fg(MUTED));
        let rows: Vec<Row> = match &self.rates {
            Some(r) => r
                .ops
                .iter()
                .map(|o| {
                    Row::new([
                        o.op.clone(),
                        dec(o.qps, 0),
                        seconds(o.p50),
                        seconds(o.p99),
                        o.reads_per_query.map_or("—".into(), |g| dec(g, 1)),
                        o.errors.to_string(),
                    ])
                })
                .collect(),
            None => vec![Row::new(["aguardando a segunda leitura".to_string()])],
        };
        let widths = [14, 7, 9, 9, 14, 6].map(Constraint::Length);
        let [table, cache] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(right);
        frame.render_widget(Table::new(rows, widths).header(header), table);
        let hit = |v: Option<f64>| v.map_or("—".into(), |h| format!("{}%", dec(h * 100.0, 1)));
        let r = self.rates.as_ref();
        let line = Line::from(vec![
            key("cache de nós: "),
            value(hit(r.and_then(|r| r.node_hit))),
            key(" acerto   cache de norm: "),
            value(hit(r.and_then(|r| r.norm_hit))),
            key(" acerto"),
        ]);
        frame.render_widget(Paragraph::new(line), cache);
    }

    fn draw_indexing(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Indexação ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(s) = &self.reading.snapshot else { return };
        let r = self.rates.clone().unwrap_or_default();
        let w = &s.writer;
        let [spark, text] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        sparkline(frame, spark, &self.docs);
        let lines = vec![
            Line::from(vec![
                key("docs/s "),
                value(dec(r.docs_per_sec, 0)),
                key("   commits/s "),
                value(dec(r.commits_per_sec, 1)),
            ]),
            Line::from(vec![
                key("commit p50*/p99* "),
                value(format!("{} / {}", seconds(r.commit_p50), seconds(r.commit_p99))),
            ]),
            Line::from(vec![key("espera do lote "), value(format!("{} ms/s", dec(r.lock_wait_ms, 1)))]),
            Line::from(vec![key("documentos "), value(format_count(w.documents as usize))]),
            Line::from(vec![
                key("commits "),
                value(format_count(w.commits as usize)),
                key("  erros "),
                value(w.commit_errors.to_string()),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), text);
    }

    fn draw_edits(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Edição e remoção ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(s) = &self.reading.snapshot else { return };
        let r = self.rates.clone().unwrap_or_default();
        let w = &s.writer;
        let [spark, text] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        sparkline(frame, spark, &self.edits);
        let dead_style = if super::too_many_dead(s) { Style::new().fg(ACCENT).bold() } else { Style::new().bold() };
        let lines = vec![
            Line::from(vec![
                key("substituídos/s "),
                value(dec(r.replaced_per_sec, 0)),
                key("   removidos/s "),
                value(dec(r.deleted_per_sec, 0)),
            ]),
            Line::from(vec![
                key("versões mortas "),
                Span::styled(format_count(w.dead_versions as usize), dead_style),
                key("   edições pendentes "),
                value(format_count(w.pending_edits as usize)),
            ]),
            Line::from(vec![
                key("total: "),
                value(format_count(w.replaced_documents as usize)),
                key(" substituídos, "),
                value(format_count(w.deleted_documents as usize)),
                key(" removidos"),
            ]),
            Line::from(vec![
                key("compactações "),
                value(w.compactions.to_string()),
                key("  ("),
                value(format!("{} s", dec(w.compaction_nanos as f64 / 1e9, 1))),
                key(")"),
            ]),
            Line::from(vec![
                key("limpas: "),
                value(format_count(w.purged_versions as usize)),
                key(" versões, "),
                value(format_count(w.emptied_words as usize)),
                key(" palavras"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), text);
    }

    fn draw_rocksdb(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" RocksDB (propriedades) ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(p) = self.reading.snapshot.as_ref().and_then(|s| s.rocksdb.as_ref()) else { return };
        let levels: Vec<Span> = p
            .sst_files
            .iter()
            .enumerate()
            .flat_map(|(i, n)| [key(format!("L{i} ")), value(format!("{n}  "))])
            .collect();
        let yes_no = |v: u64| if v > 0 { "sim" } else { "não" };
        let oldest = if p.oldest_snapshot_unix == 0 {
            String::new()
        } else {
            format!(" ({} s)", dec((p.read_unix - p.oldest_snapshot_unix as f64).max(0.0), 0))
        };
        let lines = vec![
            Line::from(levels),
            Line::from(vec![
                key("compactação pendente "),
                value(bytes(p.pending_compaction_bytes)),
                key("   rodando "),
                value(p.running_compactions.to_string()),
            ]),
            Line::from(vec![
                key("flush pendente "),
                value(yes_no(p.memtable_flush_pending)),
                key("   escrita parada "),
                value(yes_no(p.write_stopped)),
            ]),
            Line::from(vec![
                key("erros de fundo "),
                value(p.background_errors.to_string()),
                key("   snapshots "),
                value(format!("{}{oldest}", p.snapshots)),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_memory(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Memória ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(s) = &self.reading.snapshot else { return };
        let mut lines = Vec::new();
        if let Some(p) = &s.rocksdb {
            lines.push(Line::from(vec![
                key("block cache "),
                value(format!("{} / {}", bytes(p.block_cache_bytes), bytes(p.block_cache_capacity_bytes))),
                key(" (fixo "),
                value(bytes(p.block_cache_pinned_bytes)),
                key(")"),
            ]));
            lines.push(Line::from(vec![
                key("memtables "),
                value(bytes(p.memtables_bytes)),
                key("   leitores de SST "),
                value(bytes(p.table_readers_bytes)),
            ]));
        }
        let cache_nodes = format_count(s.writer.cache_nodes as usize);
        lines.push(Line::from(vec![key("nós no cache do escritor "), value(cache_nodes)]));
        if let Some(cpu) = s.sampler_cpu_secs {
            let up = (s.written_unix - s.opened_unix).max(1.0);
            lines.push(Line::from(vec![
                key("amostradora "),
                value(format!("{}% de um núcleo", dec(cpu / up * 100.0, 3))),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_disk(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Disco ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = Vec::new();
        if let Some(d) = &self.reading.disk {
            lines.push(Line::from(vec![
                key("índice "),
                value(bytes(d.sst_bytes + d.wal_bytes + d.other_bytes)),
                key(" (SST "),
                value(bytes(d.sst_bytes)),
                key(", WAL "),
                value(bytes(d.wal_bytes)),
                key(")"),
            ]));
            if let (Some(free), Some(size)) = (d.free_bytes, d.size_bytes) {
                let free = format!("{} de {}", bytes(free), bytes(size));
                lines.push(Line::from(vec![key("livre no disco "), value(free)]));
            }
        }
        if let Some(r) = &self.rates {
            let rate = |v: Option<f64>| v.map_or("—".into(), |b| format!("{}/s", bytes(b as u64)));
            lines.push(Line::from(vec![
                key("I/O leitura "),
                value(rate(r.io_read_per_sec)),
                key("   escrita "),
                value(rate(r.io_write_per_sec)),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_alerts(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Alertas ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines: Vec<Line> = if self.alerts.is_empty() {
            vec![Line::from(key("nenhum"))]
        } else {
            let style = Style::new().fg(ACCENT).bold();
            self.alerts.iter().map(|a| Line::from(Span::styled(format!("▲ {a}"), style))).collect()
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Ajuda ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = [
            "O painel lê o arquivo de snapshot que o motor grava (Config::metrics), o /proc do",
            "processo dele e a pasta do banco. Nunca abre o banco nem fala com o motor.",
            "",
            "Taxas e quantis são do último intervalo. * p50 e p99 são estimados pelos baldes do",
            "histograma, como o histogram_quantile do Prometheus.",
            "",
            "Versões mortas: documentos substituídos ou removidos que ainda estão nas listas.",
            "A compactação (fire-flower compact) as tira; até lá ocupam espaço e contam no df.",
        ];
        frame.render_widget(Paragraph::new(lines.map(Line::from).to_vec()), inner);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let interval = format!("intervalo ({} s)", self.interval.as_secs());
        let keys = [
            ("q", "sair"),
            ("p", if self.paused { "continuar" } else { "pausar" }),
            ("1/5/i", interval.as_str()),
            ("?", "ajuda"),
        ];
        let mut spans = vec![Span::raw(" ")];
        for (k, action) in keys {
            spans.push(Span::styled(k, Style::new().fg(ACCENT).bold()));
            spans.push(Span::styled(format!(" {action}   "), Style::new().fg(MUTED)));
        }
        spans.push(Span::styled("* estimado pelos baldes do histograma", Style::new().fg(MUTED)));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn sparkline(frame: &mut Frame, area: Rect, data: &VecDeque<u64>) {
    // As amostras mais recentes que cabem, da esquerda para a direita.
    let skip = data.len().saturating_sub(area.width as usize);
    let points: Vec<u64> = data.iter().skip(skip).copied().collect();
    frame.render_widget(Sparkline::default().data(&points).style(Style::new().fg(ACCENT)), area);
}

fn key(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(MUTED))
}

fn value(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().bold())
}

/// Número com `places` casas e vírgula decimal.
pub(in crate::monitor) fn dec(v: f64, places: usize) -> String {
    format!("{v:.places$}").replace('.', ",")
}

/// Segundos em µs, ms ou s; `—` sem valor.
pub(in crate::monitor) fn seconds(v: Option<f64>) -> String {
    match v {
        None => "—".into(),
        Some(s) if s < 1e-3 => format!("{} µs", dec(s * 1e6, 0)),
        Some(s) if s < 1.0 => format!("{} ms", dec(s * 1e3, 1)),
        Some(s) => format!("{} s", dec(s, 1)),
    }
}

/// Bytes em KB, MB ou GB (base 1024).
pub(in crate::monitor) fn bytes(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < units.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{b} B") } else { format!("{} {}", dec(v, 1), units[unit]) }
}

/// "3h12", "4min", "12 s".
fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{}min", s / 60),
        _ => format!("{}h{:02}", s / 3600, s % 3600 / 60),
    }
}
