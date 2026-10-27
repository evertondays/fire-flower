//! Harness de performance do fire-flower (ver `docs/plano-performance.md`).
//!
//! Todos os subcomandos leem o corpus JSONL da Wikipédia
//! (`{"id", "title", "text"}` por linha) e gravam resultados em
//! `results/<data>-<commit>-<tipo>-<rótulo>.json` do repositório
//! fire-flower-bench, clonado ao lado deste (`FF_BENCH_DIR` para mudar).
//!
//! ```text
//! cargo run --release --example wiki -- load    --db D --corpus F [--batch N] [--skip K] [--take N] [--remap-ids]
//! cargo run --release --example wiki -- vocab   --corpus F [--out V]
//! cargo run --release --example wiki -- queries --vocab V --corpus F [--seed 42]
//! cargo run --release --example wiki -- oracle  --db D --corpus F [--take N] [--sample-terms N]
//! cargo run --release --example wiki -- bench   --db D --queries Q... [--threads T] [--cold]
//! cargo run --release --example wiki -- sizes   --db D
//! cargo run --release --example wiki -- digest  --db D
//! cargo run --release --example wiki -- concurrent --db D --corpus F --skip K --queries Q [--threads T]
//! ```

use crate::concurrent::concurrent;
use crate::digest::digest;
use crate::sizes::sizes;
use crate::bench::bench;
use crate::relevance::run_relevance;
use crate::env::project_dir;
use crate::oracle::OracleArgs;
use crate::oracle::oracle;
use crate::queries::queries;
use crate::vocab::vocab;
use crate::load::load;
use crate::corpus::parse_languages;

// FxHash: os mapas do vocabulário e do oráculo têm centenas de milhares de termos.
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use fire_flower::{Stemming, Stopwords};
use fire_flower::Config;

mod bench;
mod concurrent;
mod corpus;
mod digest;
mod env;
mod load;
mod oracle;
mod queries;
mod relevance;
mod rng;
mod sizes;
mod vocab;


pub(crate) type AnyResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "wiki", about = "Harness de performance do fire-flower")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexa o corpus e mede docs/s, MB/s, RSS, tamanho do banco e write amp
    Load {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value_t = 10000)]
        batch: usize,
        #[arg(long, default_value_t = 0)]
        skip: u64,
        #[arg(long)]
        take: Option<u64>,
        /// Usa o número da linha (1..N) como ID, em vez do page id
        #[arg(long)]
        remap_ids: bool,
        /// Block cache em MB
        #[arg(long, default_value_t = 256)]
        cache_mb: usize,
        /// Indexa um documento por commit (`index_document`), para medir o custo fixo do commit
        #[arg(long)]
        streaming: bool,
        /// Nós da Trie que o escritor guarda entre lotes (0 desliga o cache)
        #[arg(long = "cache-nos", value_name = "CACHE_NOS")]
        node_cache: Option<usize>,
        /// Idiomas das stopwords de um banco novo ("pt,en", "en" ou "nenhum").
        /// Sem a opção, usa o que está gravado no banco (ou pt,en)
        #[arg(long = "idiomas", value_name = "IDIOMAS")]
        languages: Option<String>,
        /// Cria o banco sem posições (sem busca por frase)
        #[arg(long = "sem-posicoes")]
        no_positions: bool,
        /// Stemming de um banco novo ("pt", "pt-plural", "en" ou "nenhum"). Sem a opção,
        /// usa o que está gravado no banco (ou nenhum)
        #[arg(long)]
        stemming: Option<String>,
        /// Flexões na consulta num banco novo: o stemmer do mapa `stem:` ("pt",
        /// "pt-plural" ou "nenhum"). Sem a opção, usa o que está gravado
        #[arg(long = "flexoes", value_name = "FLEXOES")]
        inflections: Option<String>,
        /// Filtros de Bloom inteiros (um bloco por SST), como antes: reproduz o
        /// colapso da carga quando o filtro passa de uma parte do block cache
        #[arg(long = "filtro-inteiro")]
        whole_filter: bool,
        #[arg(long)]
        label: Option<String>,
    },
    /// Conta df e comprimento de cada termo; confere IDs únicos
    Vocab {
        #[arg(long)]
        corpus: PathBuf,
        /// Idiomas das stopwords, como no banco que será medido
        #[arg(long = "idiomas", value_name = "IDIOMAS", default_value = "pt,en")]
        languages: String,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Gera os conjuntos de consulta da Fase 3 em bench/queries/
    Queries {
        #[arg(long)]
        vocab: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        /// Idiomas das stopwords, como no banco que será medido
        #[arg(long = "idiomas", value_name = "IDIOMAS", default_value = "pt,en")]
        languages: String,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 1000)]
        per_set: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compara o motor com um índice ingênuo em memória (Fase 1)
    Oracle {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        remap_ids: bool,
        /// 0 = compara todos os termos; N = amostra estratificada por df
        #[arg(long, default_value_t = 0)]
        sample_terms: usize,
        #[arg(long, default_value_t = 5000)]
        and_queries: usize,
        #[arg(long, default_value_t = 2000)]
        prefixes: usize,
        /// Títulos para a revocação e para a métrica título → artigo (top-1, top-10, MRR)
        #[arg(long, default_value_t = 1000)]
        titles: usize,
        /// Frases tiradas de trechos reais (e com a ordem trocada)
        #[arg(long, default_value_t = 2000)]
        phrases: usize,
        /// Consultas cujo top 10 é comparado com o BM25 calculado no oráculo
        #[arg(long, default_value_t = 1000)]
        rank_queries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[command(flatten)]
        inflections: InflectionArgs,
        #[arg(long)]
        label: Option<String>,
    },
    /// Avalia a ordenação com consultas de resposta conhecida (docs/plano-relevancia.md)
    #[command(name = "relevancia")]
    Relevance {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        /// Pasta dos conjuntos rel-*.tsv. Os que faltarem (todos, com --gerar)
        /// são gerados do corpus e gravados ali
        #[arg(long)]
        sets: PathBuf,
        /// Consultas escritas à mão (padrão: bench/relevancia/manual.tsv)
        #[arg(long)]
        manual: Option<PathBuf>,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 1000)]
        per_set: usize,
        /// Gera os conjuntos de novo mesmo que já existam
        #[arg(long = "gerar")]
        generate: bool,
        /// Peso do título no BM25F (padrão: o do Config)
        #[arg(long = "peso-titulo", value_name = "PESO_TITULO")]
        title_weight: Option<f64>,
        /// `b` do título no BM25F (padrão: o do Config)
        #[arg(long = "b-titulo", value_name = "B_TITULO")]
        title_b: Option<f64>,
        /// Não imprime as consultas à mão (para a grade)
        #[arg(long = "resumo")]
        brief: bool,
        #[command(flatten)]
        inflections: InflectionArgs,
        #[arg(long)]
        label: Option<String>,
    },
    /// Mede latência das consultas (p50/p95/p99/máx, qps, Gets e block reads por consulta)
    Bench {
        #[arg(long)]
        db: PathBuf,
        /// Arquivos de consulta ou pastas com *.txt; `s-*` são medidos com
        /// suggest e `rank-*` com search_ranked (10 melhores)
        #[arg(long, num_args = 1.., required = true)]
        queries: Vec<PathBuf>,
        #[arg(long, default_value_t = 1)]
        threads: usize,
        #[arg(long, default_value_t = 1)]
        passes: usize,
        /// Esvazia o cache do sistema antes de cada conjunto (precisa de `sudo -n`)
        #[arg(long)]
        cold: bool,
        /// Mede também tempos no PerfContext (mais preciso, mas pesa na latência)
        #[arg(long)]
        perf_time: bool,
        /// Profundidade do cache de nós das buscas ("nenhum" desliga)
        #[arg(long = "cache-leitura", value_name = "CACHE_LEITURA")]
        reader_cache: Option<String>,
        #[arg(long, default_value_t = 256)]
        cache_mb: usize,
        #[command(flatten)]
        inflections: InflectionArgs,
        #[arg(long)]
        label: Option<String>,
    },
    /// Quanto ocupa cada espaço de chave e quais são os maiores valores
    Sizes {
        #[arg(long)]
        db: PathBuf,
    },
    /// Resumo do conteúdo lógico (termos + listas), independente dos IDs de nó
    Digest {
        #[arg(long)]
        db: PathBuf,
    },
    /// Buscas com e sem indexação em paralelo (Fase 5)
    Concurrent {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        skip: u64,
        #[arg(long)]
        take: Option<u64>,
        #[arg(long)]
        queries: PathBuf,
        #[arg(long, default_value_t = 4)]
        threads: usize,
        #[arg(long, default_value_t = 1000)]
        batch: usize,
        /// Segundos de buscas sem escrita, para a linha de base
        #[arg(long, default_value_t = 15)]
        baseline_secs: u64,
        #[arg(long)]
        label: Option<String>,
    },
}

fn main() -> AnyResult {
    match Cli::parse().command {
        Cmd::Load { db, corpus, batch, skip, take, remap_ids, cache_mb, streaming, node_cache, languages, no_positions, stemming, inflections, whole_filter, label } => {
            let languages = languages.as_deref().map(parse_languages).transpose()?;
            let code = |s: String| Stemming::from_code(&s).ok_or_else(|| format!("stemming desconhecido: {s:?} (use pt, pt-plural, en ou nenhum)"));
            let stemming = stemming.map(code).transpose()?;
            let inflections = inflections.map(code).transpose()?;
            let config = Config {
                block_cache_bytes: cache_mb << 20,
                writer_cache_nodes: node_cache.unwrap_or(Config::default().writer_cache_nodes),
                languages,
                positions: no_positions.then_some(false),
                stemming,
                inflections,
                partition_filters: !whole_filter,
                statistics: true,
                ..Config::default()
            };
            load(&db, &corpus, batch, skip, take, remap_ids, streaming, config, label)
        }
        Cmd::Vocab { corpus, languages, take, out } => vocab(&corpus, &Stopwords::new(&parse_languages(&languages)?), take, out),
        Cmd::Queries { vocab, corpus, languages, seed, per_set, out } => {
            queries(&vocab, &corpus, &Stopwords::new(&parse_languages(&languages)?), seed, per_set, out)
        }
        Cmd::Oracle { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, phrases, rank_queries, seed, inflections, label } => {
            let config = inflections.apply(Config::default())?;
            oracle(OracleArgs { db, corpus, take, remap_ids, sample_terms, and_queries, prefixes, titles, phrases, rank_queries, seed, config, label })
        }
        Cmd::Relevance { db, corpus, sets, manual, take, seed, per_set, generate, title_weight, title_b, brief, inflections, label } => {
            let manual = manual.unwrap_or_else(|| project_dir().join("bench").join("relevancia").join("manual.tsv"));
            let defaults = Config::default();
            let config = inflections.apply(Config {
                title_weight: title_weight.unwrap_or(defaults.title_weight),
                title_b: title_b.unwrap_or(defaults.title_b),
                ..defaults
            })?;
            run_relevance(&db, &corpus, &sets, &manual, take, seed, per_set, generate, config, brief, label)
        }
        Cmd::Bench { db, queries, threads, passes, cold, perf_time, reader_cache, cache_mb, inflections, label } => {
            let mut config = inflections.apply(Config { block_cache_bytes: cache_mb << 20, ..Config::default() })?;
            if let Some(depth) = reader_cache {
                config.reader_cache_depth = if depth == "nenhum" { None } else { Some(depth.parse()?) };
            }
            bench(&db, &queries, threads.max(1), passes.max(1), cold, perf_time, config, label)
        }
        Cmd::Sizes { db } => sizes(&db),
        Cmd::Digest { db } => digest(&db),
        Cmd::Concurrent { db, corpus, skip, take, queries, threads, batch, baseline_secs, label } => {
            concurrent(&db, &corpus, skip, take, &queries, threads.max(1), batch, baseline_secs, label)
        }
    }
}

/// Peso e modo das flexões na consulta (valem por busca; o banco precisa ter
/// sido criado com `--flexoes`).
#[derive(clap::Args)]
struct InflectionArgs {
    /// α: quanto as flexões valem perto da palavra exata (0 a 1)
    #[arg(long = "peso-flexoes", value_name = "PESO_FLEXOES")]
    inflection_weight: Option<f64>,
    /// "sempre" ou N: só quando a busca exata acha menos de N documentos
    #[arg(long = "modo-flexoes", value_name = "MODO_FLEXOES")]
    inflection_mode: Option<String>,
}

impl InflectionArgs {
    fn apply(&self, mut config: Config) -> AnyResult<Config> {
        if let Some(w) = self.inflection_weight {
            config.inflection_weight = w;
        }
        if let Some(m) = &self.inflection_mode {
            config.inflection_mode = match m.as_str() {
                "sempre" => fire_flower::InflectionMode::Always,
                n => fire_flower::InflectionMode::IfFewer(n.parse().map_err(|_| format!("modo das flexões: {m:?} (use sempre ou um número)"))?),
            };
        }
        Ok(config)
    }
}
