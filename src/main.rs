use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use fire_flower::{Config, Engine, text};

const USAGE: &str = "\
uso:
  fire-flower index   <banco> <arquivo|pasta>...   indexa arquivos de texto/HTML (pastas recursivamente)
  fire-flower search  <banco> <consulta>           busca (termos com E; termo* = prefixo)
  fire-flower suggest <banco> <prefixo> [limite]   autocompletar";

/// Documentos por commit. Limita a memória usada pelo lote.
const DOCS_PER_COMMIT: usize = 1000;
const MAX_PRINTED_RESULTS: usize = 50;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["index", db, paths @ ..] if !paths.is_empty() => index(db, paths),
        ["search", db, query @ ..] if !query.is_empty() => search(db, &query.join(" ")),
        ["suggest", db, prefix] => suggest(db, prefix, "10"),
        ["suggest", db, prefix, limit] => suggest(db, prefix, limit),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::FAILURE
        }
    }
}

type AnyResult = Result<(), Box<dyn std::error::Error>>;

fn index(db: &str, paths: &[&str]) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let started = Instant::now();
    let mut files = Vec::new();
    for p in paths {
        collect_files(Path::new(p), &mut files)?;
    }

    let (mut indexed, mut skipped) = (0usize, 0usize);
    let mut batch = engine.batch();
    for path in &files {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("ignorado {}: {e}", path.display());
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

    println!(
        "{indexed} documento(s) indexado(s), {skipped} ignorado(s), em {:.2?}",
        started.elapsed()
    );
    Ok(())
}

fn search(db: &str, query: &str) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let started = Instant::now();
    let docs = engine.search(query)?;
    let elapsed = started.elapsed();

    for &doc in docs.iter().take(MAX_PRINTED_RESULTS) {
        match engine.label(doc)? {
            Some(label) => println!("{doc}\t{label}"),
            None => println!("{doc}"),
        }
    }
    if docs.len() > MAX_PRINTED_RESULTS {
        println!("... e mais {}", docs.len() - MAX_PRINTED_RESULTS);
    }
    println!("{} resultado(s) em {elapsed:.2?}", docs.len());
    Ok(())
}

fn suggest(db: &str, prefix: &str, limit: &str) -> AnyResult {
    let limit: usize = limit.parse().map_err(|_| format!("limite inválido: {limit}"))?;
    let engine = Engine::open(db, Config::default())?;
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
