//! Manutenção em segundo plano: depois de muitas edições, compacta o banco e
//! limpa as palavras sem documento ([`Maintenance`]).

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{Engine, Error, Result};

/// Quando a [`Maintenance`] compacta.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceOptions {
    /// Edições (documentos substituídos ou removidos) desde a última
    /// compactação que disparam a próxima. `None`: 1% dos documentos do
    /// banco, no mínimo 1.000.
    pub edits_before_compact: Option<u64>,
    /// De quanto em quanto tempo a thread confere a contagem.
    pub poll: Duration,
}

impl Default for MaintenanceOptions {
    fn default() -> Self {
        Self { edits_before_compact: None, poll: Duration::from_secs(1) }
    }
}

/// Thread que mantém as buscas rápidas depois de edições.
///
/// Editar e remover são baratos no commit, mas cada lista que perdeu ou trocou
/// documentos ganha um pedaço pendente, e até a compactação toda leitura une
/// esses pedaços de novo: na fatia de 100k da Wikipédia, 1.000 edições de um
/// documento deixam a busca de um termo frequente ~7× mais lenta, e a
/// compactação (4 s) a devolve ao tempo de antes. Esta thread compacta quando
/// as edições acumuladas passam do limite e depois limpa as palavras que
/// ficaram sem documento ([`Engine::collect_empty_words`]).
///
/// Para ao ser descartada (espera a compactação em andamento terminar).
pub struct Maintenance {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<Error>>>,
    thread: Option<JoinHandle<()>>,
}

impl Maintenance {
    pub fn spawn(engine: Arc<Engine>, options: MaintenanceOptions) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, error) = (Arc::clone(&stop), Arc::clone(&error));
            std::thread::Builder::new()
                .name("fire-flower-maintenance".into())
                .spawn(move || {
                    while !stop.load(AtomicOrdering::Acquire) {
                        std::thread::park_timeout(options.poll);
                        if stop.load(AtomicOrdering::Acquire) {
                            break;
                        }
                        if let Err(e) = run_once(&engine, &options) {
                            error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                        }
                    }
                })
                .expect("o sistema criou a thread de manutenção")
        };
        Self { stop, error, thread: Some(thread) }
    }

    /// Para a thread e devolve o primeiro erro que ela encontrou, se houve.
    pub fn stop(mut self) -> Result<()> {
        self.halt();
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take().map_or(Ok(()), Err)
    }

    fn halt(&mut self) {
        self.stop.store(true, AtomicOrdering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Uma rodada: compacta e limpa se as edições passaram do limite.
fn run_once(engine: &Engine, options: &MaintenanceOptions) -> Result<()> {
    let limit = match options.edits_before_compact {
        Some(n) => n,
        None => (engine.stats()?.doc_count / 100).max(1000),
    };
    if engine.pending_edits() < limit {
        return Ok(());
    }
    engine.compact();
    engine.collect_empty_words()?;
    Ok(())
}
