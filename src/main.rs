mod jsonl;
mod tui;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use fire_flower::{Config, Engine, Language, Stemming, stopwords, text};

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
        #[command(flatten)]
        idiomas: Idiomas,
        #[command(flatten)]
        posicoes: Posicoes,
        #[command(flatten)]
        radicais: Radicais,
    },
    /// Indexa um JSONL com {"id", "title", "text"} por linha (ex.: Wikipédia)
    IndexJsonl {
        /// Pasta do banco (criada se não existir)
        banco: PathBuf,
        /// Arquivo .jsonl
        arquivo: PathBuf,
        /// Documentos por commit
        #[arg(long, default_value_t = 10000)]
        lote: usize,
        /// Pula as N primeiras linhas (para retomar uma carga interrompida)
        #[arg(long, default_value_t = 0)]
        pular: u64,
        /// Emite o progresso como JSON na saída padrão
        #[arg(long)]
        progresso_json: bool,
        #[command(flatten)]
        idiomas: Idiomas,
        #[command(flatten)]
        posicoes: Posicoes,
        #[command(flatten)]
        radicais: Radicais,
    },
    /// Compacta o banco: une os pedaços pendentes das listas de documentos
    Compact {
        banco: PathBuf,
    },
    /// Busca: termos combinados com E; termo* casa por prefixo; "frase" exata.
    /// Os resultados saem por relevância (BM25), com a nota.
    Search {
        banco: PathBuf,
        #[arg(required = true)]
        consulta: Vec<String>,
        /// Quantos resultados imprimir
        #[arg(short = 'n', long, default_value_t = 50)]
        limite: usize,
        /// Ordena por ID do documento, sem ranking
        #[arg(long)]
        por_id: bool,
    },
    /// Autocompletar: palavras indexadas que começam com o prefixo
    Suggest {
        banco: PathBuf,
        prefixo: String,
        #[arg(default_value_t = 10)]
        limite: usize,
    },
}

#[derive(clap::Args)]
struct Idiomas {
    /// Idiomas das stopwords de um banco novo: códigos separados por vírgula
    /// (pt, en) ou "nenhum". Padrão: pt,en. Num banco existente, precisa
    /// combinar com o que foi gravado na criação.
    #[arg(long, value_parser = parse_idiomas)]
    idiomas: Option<ListaIdiomas>,
}

#[derive(clap::Args)]
struct Posicoes {
    /// Cria o banco sem posições: menor e mais rápido de carregar, mas sem
    /// busca por frase. Num banco existente, precisa combinar com a criação.
    #[arg(long)]
    sem_posicoes: bool,
}

#[derive(clap::Args)]
struct Radicais {
    /// Stemming de um banco novo: "pt" (Snowball do português, "correndo"
    /// acha "corrida"), "en" ou "nenhum" (o padrão). Num banco existente,
    /// precisa combinar com o que foi gravado na criação.
    #[arg(long, value_parser = parse_stemming)]
    stemming: Option<Stemming>,
}

fn parse_stemming(s: &str) -> Result<Stemming, String> {
    Stemming::from_code(s).ok_or_else(|| format!("stemming desconhecido {s:?}; use pt, en ou \"nenhum\""))
}

impl Posicoes {
    /// `None` (usa o que o banco tiver) quando a opção não foi dada.
    fn config(&self) -> Option<bool> {
        self.sem_posicoes.then_some(false)
    }
}

/// Lista de idiomas já validada (o clap precisa de um tipo só para o valor).
#[derive(Clone)]
struct ListaIdiomas(Vec<Language>);

fn parse_idiomas(s: &str) -> Result<ListaIdiomas, String> {
    if s.trim().eq_ignore_ascii_case("nenhum") {
        return Ok(ListaIdiomas(Vec::new()));
    }
    let valid = || Language::ALL.iter().map(|l| l.code()).collect::<Vec<_>>().join(", ");
    s.split(',')
        .map(|c| Language::from_code(c).ok_or_else(|| format!("idioma desconhecido {c:?}; use {} ou \"nenhum\"", valid())))
        .collect::<Result<_, _>>()
        .map(ListaIdiomas)
}

/// Documentos por commit. Limita a memória usada pelo lote.
const DOCS_PER_COMMIT: usize = 1000;

type AnyResult = Result<(), Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Tui { banco } => open(&banco, None, None, None).and_then(|engine| {
            tui::run(&engine, &banco.display().to_string()).map_err(Into::into)
        }),
        Command::Index { banco, caminhos, idiomas, posicoes, radicais } => {
            index(&banco, &caminhos, idiomas.idiomas, posicoes.config(), radicais.stemming)
        }
        Command::IndexJsonl { banco, arquivo, lote, pular, progresso_json, idiomas, posicoes, radicais } => {
            let opts = jsonl::Options { batch_size: lote.max(1), skip: pular, json: progresso_json };
            open(&banco, idiomas.idiomas, posicoes.config(), radicais.stemming).and_then(|engine| {
                if !progresso_json {
                    eprintln!("stopwords: {}", stopwords::describe(engine.languages()));
                    eprintln!("posições: {}", if engine.has_positions() { "sim" } else { "não" });
                    eprintln!("stemming: {}", engine.stemmer().stemming());
                }
                jsonl::index(&engine, &arquivo, &opts)
            })
        }
        Command::Compact { banco } => open(&banco, None, None, None).map(|engine| {
            let started = Instant::now();
            engine.compact();
            println!("compactado em {:.2?}", started.elapsed());
        }),
        Command::Search { banco, consulta, limite, por_id } => search(&banco, &consulta.join(" "), limite, por_id),
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

/// Abre o banco. `idiomas`, `posicoes` e `stemming` só importam na criação;
/// `None` usa o que está gravado.
fn open(
    db: &Path,
    idiomas: Option<ListaIdiomas>,
    posicoes: Option<bool>,
    stemming: Option<Stemming>,
) -> Result<Engine, Box<dyn std::error::Error>> {
    let config = Config { languages: idiomas.map(|i| i.0), positions: posicoes, stemming, ..Config::default() };
    Ok(Engine::open(db, config)?)
}

fn index(
    db: &Path,
    paths: &[PathBuf],
    idiomas: Option<ListaIdiomas>,
    posicoes: Option<bool>,
    stemming: Option<Stemming>,
) -> AnyResult {
    let engine = open(db, idiomas, posicoes, stemming)?;
    eprintln!("stopwords: {}", stopwords::describe(engine.languages()));
    eprintln!("stemming: {}", engine.stemmer().stemming());
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
        // O nome do arquivo (sem extensão) é o título: pesa mais no ranking.
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        batch.add_labeled_titled(&path.display().to_string(), &title, &content)?;
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

fn search(db: &Path, query: &str, limit: usize, by_id: bool) -> AnyResult {
    let engine = open(db, None, None, None)?;
    let started = Instant::now();
    // Por ID: todos os documentos em ordem. Ranqueado: só os `limit` melhores.
    let (hits, total) = if by_id {
        let docs = engine.search(query)?;
        let total = docs.len();
        (docs.into_iter().take(limit).map(|doc| (doc, None)).collect::<Vec<_>>(), total)
    } else {
        let top = engine.search_ranked(query, limit)?;
        (top.hits.into_iter().map(|h| (h.doc, Some(h.score))).collect(), top.total)
    };
    let elapsed = started.elapsed();

    for (doc, score) in &hits {
        let label = engine.label(*doc)?;
        let score = score.map(|s| format!("{s:.3}\t")).unwrap_or_default();
        match label {
            Some(label) => println!("{score}{doc}\t{label}"),
            None => println!("{score}{doc}"),
        }
    }
    if total > hits.len() {
        println!("... e mais {}", total - hits.len());
    }
    println!("{total} resultado(s) em {elapsed:.2?}");
    Ok(())
}

fn suggest(db: &Path, prefix: &str, limit: usize) -> AnyResult {
    let engine = open(db, None, None, None)?;
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
