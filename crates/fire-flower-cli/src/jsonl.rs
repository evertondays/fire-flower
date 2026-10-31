//! `fire-flower index-jsonl`: indexa um JSONL com um documento por linha,
//! no formato do corpus da Wikipédia: `{"id": u64, "title": str, "text": str}`.
//! Um ID que já existe é substituído; `{"op": "delete", "id": u64}` remove.
//!
//! O campo `external_id` (texto ou número; ou outro campo, com
//! `--external-id-field`) é o ID do usuário, devolvido nas buscas. Sem `id`,
//! o documento é achado (ou criado) pelo ID externo.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use fire_flower::{DocId, Engine, IndexBatch};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const REPORT_EVERY: Duration = Duration::from_millis(500);

#[derive(Deserialize)]
struct Article {
    /// O `DocId`. Sem ele, vale o `external_id`.
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    external_id: Option<Value>,
    /// `"delete"` remove o documento; sem `op`, adiciona ou substitui.
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    title: String,
    /// Obrigatório para adicionar; uma remoção não precisa.
    text: Option<String>,
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
    /// Linhas `{"op": "delete"}`.
    deleted: u64,
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
    /// O campo da linha com o ID externo (padrão: `external_id`).
    pub external_id_field: Option<String>,
}

/// O ID externo da linha: texto ou número, no campo pedido.
fn external_id(article: &Article, line: &[u8], field: Option<&str>) -> Option<String> {
    let value = match field {
        None | Some("external_id") => article.external_id.clone()?,
        // Outro campo: uma segunda leitura da linha, só com a opção.
        Some(name) => serde_json::from_slice::<serde_json::Map<String, Value>>(line).ok()?.remove(name)?,
    };
    match value {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Adiciona (ou substitui) o documento da linha: pelo `id`, com o ID externo
/// se houver, ou só pelo ID externo. Devolve o `DocId`.
fn add(batch: &mut IndexBatch<'_>, id: Option<u64>, external: Option<&str>, title: &str, text: &str) -> fire_flower::Result<DocId> {
    match (id, external) {
        (Some(id), external) => {
            batch.add_titled(id, title, text)?;
            if let Some(external) = external {
                batch.set_external_id(id, external)?;
            }
            Ok(id)
        }
        (None, Some(external)) => batch.add_with_external_id(external, title, text),
        (None, None) => unreachable!("conferido antes"),
    }
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
                let external = external_id(&article, &line, opts.external_id_field.as_deref());
                match (article.op.as_deref(), &article.text) {
                    _ if article.id.is_none() && external.is_none() => {
                        progress.invalid += 1;
                        eprintln!("linha {} ignorada: falta o campo id (ou o ID externo)", progress.lines);
                    }
                    (None, None) => {
                        progress.invalid += 1;
                        eprintln!("linha {} ignorada: falta o campo text", progress.lines);
                    }
                    (None, Some(text)) => {
                        // O título é um campo à parte: buscável e com peso próprio no ranking.
                        match add(&mut batch, article.id, external.as_deref(), &article.title, text) {
                            Ok(doc) => {
                                batch.set_label(doc, &article.title);
                                progress.documents += 1;
                                progress.last_title = article.title;
                            }
                            // ID externo repetido ou inválido. Com `id`, o texto já entrou: só o
                            // ID externo fica de fora. Sem `id`, nada entrou: a linha é inválida.
                            Err(e @ (fire_flower::Error::Duplicate(_) | fire_flower::Error::InvalidId(_))) => {
                                match article.id {
                                    Some(id) => {
                                        batch.set_label(id, &article.title);
                                        progress.documents += 1;
                                        progress.last_title = article.title;
                                        eprintln!("linha {}: indexada sem o ID externo ({e})", progress.lines);
                                    }
                                    None => {
                                        progress.invalid += 1;
                                        eprintln!("linha {} ignorada: {e}", progress.lines);
                                    }
                                }
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                    (Some("delete"), _) => {
                        let deleted = match (article.id, external.as_deref()) {
                            (Some(id), _) => batch.delete_document(id).map(|_| true),
                            (None, Some(external)) => batch.delete_by_external_id(external).map(|d| d.is_some()),
                            (None, None) => unreachable!("conferido antes"),
                        };
                        match deleted {
                            Ok(_) => progress.deleted += 1,
                            Err(e @ fire_flower::Error::InvalidId(_)) => {
                                progress.invalid += 1;
                                eprintln!("linha {}: {e}", progress.lines);
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                    (Some(op), _) => {
                        progress.invalid += 1;
                        eprintln!("linha {} ignorada: op {op:?} desconhecida (use \"delete\")", progress.lines);
                    }
                }
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
