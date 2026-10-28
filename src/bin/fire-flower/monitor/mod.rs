//! `fire-flower monitor`: lê o snapshot que o motor grava, o `/proc` do
//! processo dele e a pasta do banco, e mostra num painel, em JSON ou no
//! `/metrics` (OpenMetrics). Nunca abre o RocksDB nem fala com o motor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fire_flower::metrics::Snapshot;
use serde::Serialize;

use self::disk::Disk;
use self::process::Process;

mod disk;
mod openmetrics;
mod panel;
mod process;
mod serve;
#[cfg(test)]
mod tests;

/// O tamanho da pasta e o espaço livre mudam devagar e custam uma listagem e
/// um `df`: relidos no máximo a cada 30 s.
const DISK_EVERY: Duration = Duration::from_secs(30);

/// Uma leitura de tudo o que o monitor mostra.
#[derive(Debug, Clone, Default, Serialize)]
struct Reading {
    /// Quando foi lida (Unix, s).
    read_unix: f64,
    /// `None` se o arquivo não existe (motor nunca aberto com métricas).
    snapshot: Option<Snapshot>,
    snapshot_age_secs: Option<f64>,
    /// O processo que gravou o snapshot, se ainda existe e é visível.
    process: Option<Process>,
    disk: Option<Disk>,
}

/// De onde o monitor lê.
struct Source {
    file: PathBuf,
    db: PathBuf,
    proc: PathBuf,
    disk: Option<(Instant, Option<Disk>)>,
}

impl Source {
    fn new(db: &Path, file: Option<PathBuf>) -> Self {
        let file = file.unwrap_or_else(|| fire_flower::metrics::default_path(db));
        Self { file, db: db.to_path_buf(), proc: PathBuf::from("/proc"), disk: None }
    }

    fn file(&self) -> &Path {
        &self.file
    }

    fn read(&mut self) -> Reading {
        let read_unix = unix_now();
        let snapshot = fire_flower::metrics::read(&self.file).ok();
        let snapshot_age_secs = snapshot.as_ref().map(|s| (read_unix - s.written_unix).max(0.0));
        let process = snapshot.as_ref().and_then(|s| process::read(&self.proc, s.pid));
        if self.disk.as_ref().is_none_or(|(at, _)| at.elapsed() >= DISK_EVERY) {
            self.disk = Some((Instant::now(), disk::read(&self.db)));
        }
        let disk = self.disk.as_ref().and_then(|(_, d)| d.clone());
        Reading { read_unix, snapshot, snapshot_age_secs, process, disk }
    }
}

/// Opções do `fire-flower monitor`.
pub(super) struct Options {
    pub(super) db: PathBuf,
    pub(super) metrics_file: Option<PathBuf>,
    pub(super) serve: Option<String>,
    pub(super) json: bool,
}

pub(super) fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut source = Source::new(&opts.db, opts.metrics_file);
    if opts.json {
        let reading = source.read();
        if reading.snapshot.is_none() {
            return Err(no_snapshot(source.file()).into());
        }
        println!("{}", serde_json::to_string_pretty(&reading)?);
        return Ok(());
    }
    if let Some(addr) = opts.serve {
        return serve::serve(&addr, source);
    }
    Ok(panel::run(source)?)
}

fn no_snapshot(file: &Path) -> String {
    format!(
        "{} não existe: o processo que grava o banco precisa abri-lo com Config::metrics (ou \
         `fire-flower index-jsonl --metrics`)",
        file.display()
    )
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}
