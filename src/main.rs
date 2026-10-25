mod jsonl;
mod tui;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use fire_flower::{Config, Engine, text};

/// Motor de busca full-text embutido: Trie persistida no RocksDB.
#[derive(Parser)]
#[command(name = "fire-flower", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Busca interativa com autocompletar
    #[command(visible_alias = "ui")]
    Tui {
        /// Pasta do banco
        banco: PathBuf,
    },
    /// Indexa arquivos de texto/HTML (pastas recursivamente)
    Index {
        /// Pasta do banco (criada se não existir)
        banco: PathBuf,
        /// Arquivos ou pastas
        #[arg(required = true)]
        caminhos: Vec<PathBuf>,
    },
    /// Indexa um JSONL com {"id", "title", "text"} por linha (ex.: Wikipédia)
    IndexJsonl {
        /// Pasta do banco (criada se não existir)
        banco: PathBuf,
        /// Arquivo .jsonl
        arquivo: PathBuf,
        /// Documentos por commit
        #[arg(long, default_value_t = 2000)]
        lote: usize,
        /// Pula as N primeiras linhas (para retomar uma carga interrompida)
        #[arg(long, default_value_t = 0)]
        pular: u64,
        /// Emite o progresso como JSON na saída padrão
        #[arg(long)]
        progresso_json: bool,
    },
    /// Compacta o banco: une os pedaços pendentes das listas de documentos
    Compact {
        banco: PathBuf,
    },
    /// Busca: termos combinados com E; termo* casa por prefixo
    Search {
        banco: PathBuf,
        #[arg(required = true)]
        consulta: Vec<String>,
        /// Quantos resultados imprimir
        #[arg(short = 'n', long, default_value_t = 50)]
        limite: usize,
    },
    /// Autocompletar: palavras indexadas que começam com o prefixo
    Suggest {
        banco: PathBuf,
        prefixo: String,
        #[arg(default_value_t = 10)]
        limite: usize,
    },
}

/// Documentos por commit. Limita a memória usada pelo lote.
const DOCS_PER_COMMIT: usize = 1000;

type AnyResult = Result<(), Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Tui { banco } => open(&banco).and_then(|engine| {
            tui::run(&engine, &banco.display().to_string()).map_err(Into::into)
        }),
        Command::Index { banco, caminhos } => index(&banco, &caminhos),
        Command::IndexJsonl { banco, arquivo, lote, pular, progresso_json } => {
            let opts = jsonl::Options { batch_size: lote.max(1), skip: pular, json: progresso_json };
            open(&banco).and_then(|engine| jsonl::index(&engine, &arquivo, &opts))
        }
        Command::Compact { banco } => open(&banco).map(|engine| {
            let started = Instant::now();
            engine.compact();
            println!("compactado em {:.2?}", started.elapsed());
        }),
        Command::Search { banco, consulta, limite } => search(&banco, &consulta.join(" "), limite),
        Command::Suggest { banco, prefixo, limite } => suggest(&banco, &prefixo, limite),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open(db: &Path) -> Result<Engine, Box<dyn std::error::Error>> {
    Ok(Engine::open(db, Config::default())?)
}

fn index(db: &Path, paths: &[PathBuf]) -> AnyResult {
    let engine = open(db)?;
    let started = Instant::now();
    let mut files = Vec::new();
    for p in paths {
        collect_files(p, &mut files)?;
    }

    let progress = std::io::stderr().is_terminal();
    let (mut indexed, mut skipped) = (0usize, 0usize);
    let mut batch = engine.batch();
    for (i, path) in files.iter().enumerate() {
        if progress && (i % 64 == 0 || i + 1 == files.len()) {
            eprint!("\r\x1b[2Kindexando {}/{} ({}%)", i + 1, files.len(), (i + 1) * 100 / files.len());
            let _ = std::io::stderr().flush();
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("\r\x1b[2Kignorado {}: {e}", path.display());
                skipped += 1;
                continue;
            }
        };
        // Heurística simples: NUL nos primeiros 8 KiB = arquivo binário.
        if bytes.iter().take(8192).any(|&b| b == 0) {
            skipped += 1;
            continue;
        }
        let mut content = String::from_utf8_lossy(&bytes).into_owned();
        if is_html(path) {
            content = text::strip_html(&content);
        }
        batch.add_labeled(&path.display().to_string(), &content)?;
        indexed += 1;
        if batch.len() >= DOCS_PER_COMMIT {
            batch.commit()?;
            batch = engine.batch();
        }
    }
    batch.commit()?;
    if progress {
        eprint!("\r\x1b[2K");
    }
    println!("{indexed} documento(s) indexado(s), {skipped} ignorado(s), em {:.2?}", started.elapsed());
    Ok(())
}

fn search(db: &Path, query: &str, limit: usize) -> AnyResult {
    let engine = open(db)?;
    let started = Instant::now();
    let docs = engine.search(query)?;
    let elapsed = started.elapsed();

    for &doc in docs.iter().take(limit) {
        match engine.label(doc)? {
            Some(label) => println!("{doc}\t{label}"),
            None => println!("{doc}"),
        }
    }
    if docs.len() > limit {
        println!("... e mais {}", docs.len() - limit);
    }
    println!("{} resultado(s) em {elapsed:.2?}", docs.len());
    Ok(())
}

fn suggest(db: &Path, prefix: &str, limit: usize) -> AnyResult {
    let engine = open(db)?;
    for word in engine.suggest(prefix, limit)? {
        println!("{word}");
    }
    Ok(())
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue; // .git, .cache etc.
            }
            collect_files(&entry.path(), out)?;
        }
    } else if path.is_file() {
        out.push(path.to_path_buf());
    }
    Ok(())
}

fn is_html(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
}
