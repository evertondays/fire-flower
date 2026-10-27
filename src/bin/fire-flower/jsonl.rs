//! `fire-flower index-jsonl`: indexa um JSONL com um documento por linha,
//! no formato do corpus da Wikipédia: `{"id": u64, "title": str, "text": str}`.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use fire_flower::Engine;
use serde::{Deserialize, Serialize};

const REPORT_EVERY: Duration = Duration::from_millis(500);

#[derive(Deserialize)]
struct Article {
    id: u64,
    #[serde(default)]
    title: String,
    text: String,
}

/// Uma linha de progresso. Com `--progress-json`, vai para a saída padrão
/// como JSON (é o que o `scripts/carregar_wikipedia.py` lê).
/// As chaves do JSON ficam em português: é o formato que o script lê.
#[derive(Serialize, Default)]
struct Progress {
    /// `indexing`, `writing` (commit em andamento) ou `done`.
    phase: &'static str,
    lines: u64,
    documents: u64,
    invalid: u64,
    bytes: u64,
    bytes_total: u64,
    /// Linhas já gravadas de forma definitiva. `--skip` com este valor
    /// retoma uma carga interrompida sem perder nada.
    lines_committed: u64,
    commits: u64,
    seconds: f64,
    last_title: String,
}

pub struct Options {
    pub batch_size: usize,
    pub skip: u64,
    pub json: bool,
}

pub fn index(engine: &Engine, path: &Path, opts: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mut progress = Progress { bytes_total: file.metadata()?.len(), ..Default::default() };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut line = Vec::new();

    let mut batch = engine.batch();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        progress.bytes += read as u64;
        progress.lines += 1;
        if progress.lines <= opts.skip {
            progress.lines_committed = progress.lines;
            continue;
        }

        match serde_json::from_slice::<Article>(&line) {
            Ok(article) => {
                // O título é um campo à parte: buscável e com peso próprio no ranking.
                batch.add_titled(article.id, &article.title, &article.text)?;
                batch.set_label(article.id, &article.title);
                progress.documents += 1;
                progress.last_title = article.title;
            }
            Err(_) if line.iter().all(u8::is_ascii_whitespace) => {}
            Err(e) => {
                progress.invalid += 1;
                eprintln!("linha {} ignorada: {e}", progress.lines);
            }
        }

        if batch.len() >= opts.batch_size {
            progress.phase = "writing";
            report(&mut progress, started, opts.json);
            batch.commit()?;
            progress.commits += 1;
            progress.lines_committed = progress.lines;
            batch = engine.batch();
        }
        if last_report.elapsed() >= REPORT_EVERY {
            progress.phase = "indexing";
            report(&mut progress, started, opts.json);
            last_report = Instant::now();
        }
    }

    progress.phase = "writing";
    report(&mut progress, started, opts.json);
    batch.commit()?;
    progress.commits += 1;
    progress.lines_committed = progress.lines;
    progress.phase = "done";
    report(&mut progress, started, opts.json);
    if !opts.json {
        eprintln!();
    }
    Ok(())
}

fn report(progress: &mut Progress, started: Instant, json: bool) {
    progress.seconds = started.elapsed().as_secs_f64();
    if json {
        let mut out = std::io::stdout().lock();
        let _ = serde_json::to_writer(&mut out, progress);
        let _ = writeln!(out);
        let _ = out.flush();
    } else {
        let pct = progress.bytes as f64 * 100.0 / progress.bytes_total.max(1) as f64;
        let rate = progress.documents as f64 / progress.seconds.max(0.001);
        let phase = match progress.phase {
            "writing" => "gravando",
            "done" => "fim",
            _ => "indexando",
        };
        eprint!("\r\x1b[2K{pct:5.1}%  {} documentos  {rate:.0}/s  {phase}", progress.documents);
    }
}
