//! Binding Node.js do fire-flower (napi-rs): uma chamada por busca ou por
//! lote, resultados em `Float64Array` sem cópia. A camada JS (`index.js`)
//! monta a API pública por cima; ver `docs/plano-bindings.md`.

// Compilado como teste (`cargo clippy --all-targets`), o napi-derive não gera o
// registro das funções no Node: tudo pareceria sem uso. Os testes são em JS.
#![cfg_attr(test, allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use fire_flower::{Config, Engine, Reader};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::convert::{Mode, OpenOptions};
use crate::handle::{Backend, NewHandle};
use crate::tasks::OpenTask;

mod convert;
mod engine;
mod handle;
mod tasks;

/// Abre o banco fora do event loop (dezenas de ms num banco grande).
#[napi(catch_unwind)]
pub fn open_engine(path: String, options: Option<OpenOptions>) -> Result<AsyncTask<OpenTask>> {
    let (config, mode) = convert::config(options.unwrap_or_default())?;
    Ok(AsyncTask::new(OpenTask { path: PathBuf::from(path), config, mode }))
}

/// Como [`open_engine`], na hora (scripts, testes).
#[napi(catch_unwind)]
pub fn open_engine_sync(path: String, options: Option<OpenOptions>) -> Result<NewHandle> {
    let (config, mode) = convert::config(options.unwrap_or_default())?;
    Ok(NewHandle(open_shared(Path::new(&path), config, mode).map_err(js_error)?))
}

/// Os bancos abertos neste processo, por (caminho, modo). Abrir de novo o
/// mesmo caminho devolve o mesmo `Engine` (ou `Reader`): as `worker_threads`
/// compartilham o índice e o block cache (o addon é carregado uma vez por
/// processo), e o segundo `open` para escrita não esbarra no `LOCK` do
/// primeiro. A config da primeira abertura vale para as seguintes.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

type Registry = Mutex<HashMap<(PathBuf, Mode), Shared>>;

enum Shared {
    Engine(Weak<Engine>),
    Reader(Weak<Reader>),
}

impl Shared {
    fn upgrade(&self) -> Option<Backend> {
        match self {
            Shared::Engine(e) => e.upgrade().map(Backend::Engine),
            Shared::Reader(r) => r.upgrade().map(Backend::Reader),
        }
    }
}

pub(crate) fn open_shared(path: &Path, config: Config, mode: Mode) -> fire_flower::Result<Backend> {
    let key = (std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()), mode);
    let mut open = REGISTRY.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(backend) = open.get(&key).and_then(Shared::upgrade) {
        return Ok(backend);
    }
    let (backend, shared) = match mode {
        Mode::Write => {
            let e = Arc::new(Engine::open(path, config)?);
            (Backend::Engine(Arc::clone(&e)), Shared::Engine(Arc::downgrade(&e)))
        }
        Mode::ReadOnly => {
            let e = Arc::new(Engine::open_read_only(path, config)?);
            (Backend::Engine(Arc::clone(&e)), Shared::Engine(Arc::downgrade(&e)))
        }
        Mode::Refreshing(ms) => {
            let r = Arc::new(Reader::open(path, config, Duration::from_millis(u64::from(ms)))?);
            (Backend::Reader(Arc::clone(&r)), Shared::Reader(Arc::downgrade(&r)))
        }
    };
    open.retain(|_, s| s.upgrade().is_some());
    open.insert(key, shared);
    Ok(backend)
}

/// Um erro do core para o JS: a mensagem começa pelo código estável
/// (`FF_BUSY: …`), que a camada JS põe em `FireFlowerError.code`. Assim o
/// código chega igual por chamadas síncronas e por `Promise`.
pub(crate) fn js_error(e: fire_flower::Error) -> Error {
    Error::new(Status::GenericFailure, format!("{}: {e}", e.code()))
}

/// Um erro do próprio binding (argumento inválido, pânico).
pub(crate) fn binding_error(code: &str, message: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("{code}: {message}"))
}

