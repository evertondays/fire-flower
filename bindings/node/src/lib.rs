//! Binding Node.js do fire-flower (napi-rs): uma chamada por busca ou por
//! lote, resultados em `Float64Array` sem cópia. A camada JS (`index.js`)
//! monta a API pública por cima; ver `docs/plano-bindings.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use fire_flower::{Config, Engine};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::convert::OpenOptions;
use crate::engine::NativeEngine;
use crate::tasks::OpenTask;

mod convert;
mod engine;
mod tasks;

/// Abre o banco fora do event loop (dezenas de ms num banco grande).
#[napi(catch_unwind)]
pub fn open_engine(path: String, options: Option<OpenOptions>) -> Result<AsyncTask<OpenTask>> {
    let (config, read_only) = convert::config(options.unwrap_or_default())?;
    Ok(AsyncTask::new(OpenTask { path: PathBuf::from(path), config, read_only }))
}

/// Como [`open_engine`], na hora (scripts, testes).
#[napi(catch_unwind)]
pub fn open_engine_sync(path: String, options: Option<OpenOptions>) -> Result<NativeEngine> {
    let (config, read_only) = convert::config(options.unwrap_or_default())?;
    Ok(NativeEngine::new(open_shared(Path::new(&path), config, read_only).map_err(js_error)?))
}

/// Os bancos abertos neste processo, por (caminho, só leitura). Abrir de novo
/// o mesmo caminho devolve o mesmo `Engine`: as `worker_threads` compartilham
/// o índice e o block cache (o addon é carregado uma vez por processo), e o
/// segundo `open` para escrita não esbarra no `LOCK` do primeiro. A config da
/// primeira abertura vale para as seguintes.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

type Registry = Mutex<HashMap<(PathBuf, bool), Weak<Engine>>>;

pub(crate) fn open_shared(path: &Path, config: Config, read_only: bool) -> fire_flower::Result<Arc<Engine>> {
    let key = (std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()), read_only);
    let mut open = REGISTRY.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(engine) = open.get(&key).and_then(Weak::upgrade) {
        return Ok(engine);
    }
    let engine = Arc::new(if read_only { Engine::open_read_only(path, config)? } else { Engine::open(path, config)? });
    open.retain(|_, e| e.strong_count() > 0);
    open.insert(key, Arc::downgrade(&engine));
    Ok(engine)
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
