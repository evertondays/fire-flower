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

/// Uma linha de progresso. Com `--progresso-json`, vai para a saída padrão
/// como JSON (é o que o `scripts/carregar_wikipedia.py` lê).
#[derive(Serialize, Default)]
struct Progress {
    /// `indexando`, `gravando` (commit em andamento) ou `fim`.
    fase: &'static str,
    linhas: u64,
    documentos: u64,
    invalidas: u64,
    bytes: u64,
    bytes_total: u64,
    /// Linhas já gravadas de forma definitiva. `--pular` com este valor
    /// retoma uma carga interrompida sem perder nada.
    linhas_gravadas: u64,
    commits: u64,
    segundos: f64,
    ultimo_titulo: String,
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
    let mut text = String::new();

    let mut batch = engine.batch();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        progress.bytes += read as u64;
        progress.linhas += 1;
        if progress.linhas <= opts.skip {
            progress.linhas_gravadas = progress.linhas;
            continue;
        }

        match serde_json::from_slice::<Article>(&line) {
            Ok(article) => {
                // Título junto do texto: buscar o título traz o próprio artigo.
                text.clear();
                text.push_str(&article.title);
                text.push('\n');
                text.push_str(&article.text);
                batch.add_document(article.id, &text)?;
                batch.set_label(article.id, &article.title);
                progress.documentos += 1;
                progress.ultimo_titulo = article.title;
            }
            Err(_) if line.iter().all(u8::is_ascii_whitespace) => {}
            Err(e) => {
                progress.invalidas += 1;
                eprintln!("linha {} ignorada: {e}", progress.linhas);
            }
        }

        if batch.len() >= opts.batch_size {
            progress.fase = "gravando";
            report(&mut progress, started, opts.json);
            batch.commit()?;
            progress.commits += 1;
            progress.linhas_gravadas = progress.linhas;
            batch = engine.batch();
        }
        if last_report.elapsed() >= REPORT_EVERY {
            progress.fase = "indexando";
            report(&mut progress, started, opts.json);
            last_report = Instant::now();
        }
    }

    progress.fase = "gravando";
    report(&mut progress, started, opts.json);
    batch.commit()?;
    progress.commits += 1;
    progress.linhas_gravadas = progress.linhas;
    progress.fase = "fim";
    report(&mut progress, started, opts.json);
    if !opts.json {
        eprintln!();
    }
    Ok(())
}

fn report(progress: &mut Progress, started: Instant, json: bool) {
    progress.segundos = started.elapsed().as_secs_f64();
    if json {
        let mut out = std::io::stdout().lock();
        let _ = serde_json::to_writer(&mut out, progress);
        let _ = writeln!(out);
        let _ = out.flush();
    } else {
        let pct = progress.bytes as f64 * 100.0 / progress.bytes_total.max(1) as f64;
        let rate = progress.documentos as f64 / progress.segundos.max(0.001);
        eprint!(
            "\r\x1b[2K{pct:5.1}%  {} documentos  {rate:.0}/s  {}",
            progress.documentos, progress.fase
        );
    }
}
