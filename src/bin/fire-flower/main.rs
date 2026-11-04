mod jsonl;
mod tui;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use fire_flower::analysis::{self, stopwords};
use fire_flower::{Config, Engine, Language, Stemming};

/// Motor de busca full-text embutido: Trie persistida no RocksDB.
#[derive(Parser)]
#[command(name = "fire-flower", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Os nomes das opções e dos valores ficam em português (são a interface da
// CLI); os campos, em inglês.
#[derive(Subcommand)]
enum Command {
    /// Busca interativa com autocompletar
    #[command(visible_alias = "ui")]
    Tui {
        /// Pasta do banco
        db: PathBuf,
    },
    /// Indexa arquivos de texto/HTML (pastas recursivamente)
    Index {
        /// Pasta do banco (criada se não existir)
        db: PathBuf,
        /// Arquivos ou pastas
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Sincroniza com o que já está no banco: um arquivo já indexado é
        /// substituído (mesmo ID) e um que sumiu das pastas é removido. Sem a
        /// opção, cada arquivo vira um documento novo
        #[arg(long)]
        sync: bool,
        #[command(flatten)]
        languages: LanguageArgs,
        #[command(flatten)]
        positions: PositionArgs,
        #[command(flatten)]
        stemming: StemmingArgs,
    },
    /// Indexa um JSONL com {"id", "title", "text"} por linha (ex.: Wikipédia).
    /// Um ID que já existe é substituído; {"op": "delete", "id"} remove
    IndexJsonl {
        /// Pasta do banco (criada se não existir)
        db: PathBuf,
        /// Arquivo .jsonl
        file: PathBuf,
        /// Documentos por commit
        #[arg(long, default_value_t = 10000)]
        batch: usize,
        /// Pula as N primeiras linhas (para retomar uma carga interrompida)
        #[arg(long, default_value_t = 0)]
        skip: u64,
        /// Emite o progresso como JSON na saída padrão
        #[arg(long)]
        progress_json: bool,
        #[command(flatten)]
        languages: LanguageArgs,
        #[command(flatten)]
        positions: PositionArgs,
        #[command(flatten)]
        stemming: StemmingArgs,
    },
    /// Remove documentos pelo ID
    Delete {
        db: PathBuf,
        #[arg(required = true)]
        ids: Vec<u64>,
    },
    /// Compacta o banco: une os pedaços pendentes das listas de documentos e
    /// tira da Trie as palavras que ficaram sem documento
    Compact {
        db: PathBuf,
    },
    /// Busca: termos combinados com E; termo* casa por prefixo; "frase" exata.
    /// Os resultados saem por relevância (BM25), com a nota.
    Search {
        db: PathBuf,
        #[arg(required = true)]
        query: Vec<String>,
        /// Quantos resultados imprimir
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Ordena por ID do documento, sem ranking
        #[arg(long)]
        by_id: bool,
    },
    /// Autocompletar: palavras indexadas que começam com o prefixo
    Suggest {
        db: PathBuf,
        prefix: String,
        #[arg(default_value_t = 10)]
        limit: usize,
    },
}

#[derive(clap::Args)]
struct LanguageArgs {
    /// Idiomas das stopwords de um banco novo: códigos separados por vírgula
    /// (pt, en) ou "none". Padrão: pt,en. Num banco existente, precisa
    /// combinar com o que foi gravado na criação.
    #[arg(long, value_parser = parse_languages)]
    languages: Option<LanguageList>,
}

#[derive(clap::Args)]
struct PositionArgs {
    /// Cria o banco sem posições: menor e mais rápido de carregar, mas sem
    /// busca por frase. Num banco existente, precisa combinar com a criação.
    #[arg(long)]
    no_positions: bool,
    /// Cria o banco sem os termos de cada documento: menor e mais rápido de
    /// carregar, mas sem remover nem substituir documentos (reindexar um ID
    /// deixa os termos que saíram). Num banco existente, precisa combinar.
    #[arg(long)]
    no_edits: bool,
}

#[derive(clap::Args)]
struct StemmingArgs {
    /// Flexões na consulta num banco novo (o recomendado para achar plurais):
    /// "pt-plural" ("planetas" acha "Planeta", com a palavra exata valendo
    /// mais), "pt" (Snowball) ou "none" (o padrão). Não combina com
    /// --stemming. Num banco existente, precisa combinar com a criação.
    #[arg(long, value_parser = parse_stemming)]
    inflections: Option<Stemming>,
    /// Stemming de um banco novo: "pt" (Snowball do português, "correndo"
    /// acha "corrida"), "pt-plural" (só o plural: "casas" acha "casa"), "en",
    /// "en-plural" ou "none" (o padrão). Num banco existente, precisa
    /// combinar com o que foi gravado na criação.
    #[arg(long, value_parser = parse_stemming)]
    stemming: Option<Stemming>,
}

fn parse_stemming(s: &str) -> Result<Stemming, String> {
    Stemming::from_code(s).ok_or_else(|| format!("stemming desconhecido {s:?}; use pt, pt-plural, en, en-plural ou \"none\""))
}

impl PositionArgs {
    /// Posições e `fwd:` para a config: `None` (usa o que o banco tiver)
    /// quando a opção não foi dada.
    fn config(&self) -> Features {
        Features { positions: self.no_positions.then_some(false), edits: self.no_edits.then_some(false) }
    }
}

/// O que o banco grava, decidido na criação (`None`: o que estiver gravado).
#[derive(Default)]
struct Features {
    positions: Option<bool>,
    edits: Option<bool>,
}

/// Lista de idiomas já validada (o clap precisa de um tipo só para o valor).
#[derive(Clone)]
struct LanguageList(Vec<Language>);

fn parse_languages(s: &str) -> Result<LanguageList, String> {
    if s.trim().eq_ignore_ascii_case("none") {
        return Ok(LanguageList(Vec::new()));
    }
    let valid = || Language::ALL.iter().map(|l| l.code()).collect::<Vec<_>>().join(", ");
    s.split(',')
        .map(|c| Language::from_code(c).ok_or_else(|| format!("idioma desconhecido {c:?}; use {} ou \"none\"", valid())))
        .collect::<Result<_, _>>()
        .map(LanguageList)
}

/// Documentos por commit. Limita a memória usada pelo lote.
const DOCS_PER_COMMIT: usize = 1000;

type AnyResult = Result<(), Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Tui { db } => open(&db, None, Features::default(), None).and_then(|engine| {
            tui::run(&engine, &db.display().to_string()).map_err(Into::into)
        }),
        Command::Index { db, paths, sync, languages, positions, stemming } => {
            index(&db, &paths, sync, languages.languages, positions.config(), stemming)
        }
        Command::IndexJsonl { db, file, batch, skip, progress_json, languages, positions, stemming } => {
            let opts = jsonl::Options { batch_size: batch.max(1), skip, json: progress_json };
            open(&db, languages.languages, positions.config(), Some(stemming)).and_then(|engine| {
                if !progress_json {
                    eprintln!("stopwords: {}", stopwords::describe(engine.languages()));
                    eprintln!("posições: {}", if engine.has_positions() { "sim" } else { "não" });
                    eprintln!("edição de documentos: {}", if engine.has_edits() { "sim" } else { "não" });
                    eprintln!("stemming: {}, flexões: {}", engine.stemmer().stemming(), engine.inflector().stemming());
                }
                jsonl::index(&engine, &file, &opts)
            })
        }
        Command::Delete { db, ids } => delete(&db, &ids),
        Command::Compact { db } => open(&db, None, Features::default(), None).and_then(|engine| {
            let started = Instant::now();
            engine.compact();
            // Depois da compactação as listas não têm pedaços pendentes: conferir é barato.
            let emptied = engine.collect_empty_words()?;
            println!("compactado em {:.2?}", started.elapsed());
            if emptied > 0 {
                println!("{emptied} palavra(s) sem documento saíram do índice");
            }
            Ok(())
        }),
        Command::Search { db, query, limit, by_id } => search(&db, &query.join(" "), limit, by_id),
        Command::Suggest { db, prefix, limit } => suggest(&db, &prefix, limit),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Abre o banco. `languages`, `features` e `stemming` só importam na
/// criação; `None` usa o que está gravado.
fn open(
    db: &Path,
    languages: Option<LanguageList>,
    features: Features,
    stemming: Option<StemmingArgs>,
) -> Result<Engine, Box<dyn std::error::Error>> {
    let (stemming, inflections) = stemming.map_or((None, None), |s| (s.stemming, s.inflections));
    let config = Config {
        languages: languages.map(|l| l.0),
        positions: features.positions,
        edits: features.edits,
        stemming,
        inflections,
        ..Config::default()
    };
    Ok(Engine::open(db, config)?)
}

fn delete(db: &Path, ids: &[u64]) -> AnyResult {
    let engine = open(db, None, Features::default(), None)?;
    let mut batch = engine.batch();
    for &id in ids {
        batch.delete_document(id)?;
    }
    let stats = batch.commit_with_stats()?;
    println!("{} documento(s) removido(s), {} não existia(m)", stats.deleted, ids.len() - stats.deleted);
    Ok(())
}

/// Com `--sync`: os documentos já indexados cujo rótulo é um caminho dentro de
/// `paths` (o caminho como a CLI o grava), por rótulo.
fn indexed_files(engine: &Engine, paths: &[PathBuf]) -> Result<HashMap<String, u64>, Box<dyn std::error::Error>> {
    let roots: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let separator = std::path::MAIN_SEPARATOR;
    let under = |label: &str| {
        roots.iter().any(|r| {
            let inside = |rest: &str| rest.starts_with(separator) || r.ends_with(separator);
            label == r || label.strip_prefix(r.as_str()).is_some_and(inside)
        })
    };
    let mut found = HashMap::new();
    engine.for_each_label(|doc, label| {
        if under(label) {
            found.insert(label.to_string(), doc);
        }
    })?;
    Ok(found)
}

fn index(
    db: &Path,
    paths: &[PathBuf],
    sync: bool,
    languages: Option<LanguageList>,
    features: Features,
    stemming: StemmingArgs,
) -> AnyResult {
    let engine = open(db, languages, features, Some(stemming))?;
    if sync && !engine.has_edits() {
        return Err("--sync precisa de um banco com edição de documentos (criado sem --no-edits)".into());
    }
    let mut known = if sync { indexed_files(&engine, paths)? } else { HashMap::new() };
    eprintln!("stopwords: {}", stopwords::describe(engine.languages()));
    eprintln!("stemming: {}, flexões: {}", engine.stemmer().stemming(), engine.inflector().stemming());
    let started = Instant::now();
    let mut files = Vec::new();
    for p in paths {
        collect_files(p, &mut files)?;
    }

    let progress = std::io::stderr().is_terminal();
    let (mut indexed, mut skipped, mut replaced) = (0usize, 0usize, 0usize);
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
            content = analysis::strip_html(&content);
        }
        // O nome do arquivo (sem extensão) é o título: pesa mais no ranking.
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let label = path.display().to_string();
        match known.remove(&label) {
            Some(id) => {
                batch.add_titled(id, &title, &content)?;
                replaced += 1;
            }
            None => drop(batch.add_labeled_titled(&label, &title, &content)?),
        }
        indexed += 1;
        if batch.len() >= DOCS_PER_COMMIT {
            batch.commit()?;
            batch = engine.batch();
        }
    }
    // Com --sync, o que sobrou em `known` sumiu das pastas.
    let removed = known.len();
    for (_, id) in known {
        batch.delete_document(id)?;
    }
    batch.commit()?;
    if progress {
        eprint!("\r\x1b[2K");
    }
    println!("{indexed} documento(s) indexado(s), {skipped} ignorado(s), em {:.2?}", started.elapsed());
    if sync {
        println!("{replaced} substituído(s), {} novo(s), {removed} removido(s)", indexed - replaced);
    }
    Ok(())
}

fn search(db: &Path, query: &str, limit: usize, by_id: bool) -> AnyResult {
    let engine = open(db, None, Features::default(), None)?;
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
    let engine = open(db, None, Features::default(), None)?;
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
