//! O que roda no pool de threads do libuv (`Promise`): abrir, buscas
//! pesadas, indexar, remover e compactar. Um pânico vira erro `FF_PANIC`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;

use fire_flower::{Config, DocId, Engine};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::convert::{self, Doc};
use crate::engine::NativeEngine;
use crate::{binding_error, js_error, open_shared};

/// Roda `f` pegando um pânico: sem isso, ele atravessaria a fronteira e
/// derrubaria o processo do Node.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "pânico sem mensagem".into());
        Err(binding_error("FF_PANIC", message))
    })
}

pub struct OpenTask {
    pub(crate) path: PathBuf,
    pub(crate) config: Config,
    pub(crate) read_only: bool,
}

impl Task for OpenTask {
    type Output = Arc<Engine>;
    type JsValue = NativeEngine;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| open_shared(&self.path, self.config.clone(), self.read_only).map_err(js_error))
    }

    fn resolve(&mut self, _: Env, engine: Self::Output) -> Result<Self::JsValue> {
        Ok(NativeEngine::new(engine))
    }
}

pub struct SearchTopTask {
    pub(crate) engine: Arc<Engine>,
    pub(crate) query: String,
    pub(crate) k: usize,
}

impl Task for SearchTopTask {
    type Output = Float64Array;
    type JsValue = Float64Array;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let top = self.engine.search_ranked(&self.query, self.k).map_err(js_error)?;
            Ok(convert::top_buffer(top.total, &top.hits))
        })
    }

    fn resolve(&mut self, _: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}

pub struct SearchTask {
    pub(crate) engine: Arc<Engine>,
    pub(crate) query: String,
}

impl Task for SearchTask {
    type Output = Float64Array;
    type JsValue = Float64Array;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| Ok(convert::ids_buffer(self.engine.search(&self.query).map_err(js_error)?)))
    }

    fn resolve(&mut self, _: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}

/// O que um lote gravou.
#[napi(object)]
pub struct IndexResult {
    /// Documentos adicionados ou substituídos.
    pub docs: u32,
    pub replaced: u32,
    pub deleted: u32,
    pub seconds: f64,
}

/// Uma operação de um lote: adicionar (ou substituir) e remover.
pub(crate) enum Op {
    Add(Doc),
    Delete(DocId),
}

/// Aplica `ops` num lote só e faz commit. `wait`: espera um lote aberto (no
/// pool de threads) ou dá `FF_BUSY` (no event loop).
pub(crate) fn apply(engine: &Arc<Engine>, ops: &[Op], wait: bool) -> Result<IndexResult> {
    let mut batch = if wait { engine.owned_batch() } else { engine.try_owned_batch().map_err(js_error)? };
    for op in ops {
        match op {
            Op::Add(doc) => {
                let id = convert::doc_id(doc.id)?;
                let text = convert::text(doc)?;
                batch.add_titled(id, doc.title.as_deref().unwrap_or(""), text).map_err(js_error)?;
                if let Some(label) = &doc.label {
                    batch.set_label(id, label);
                }
            }
            Op::Delete(id) => batch.delete_document(*id).map_err(js_error)?,
        }
    }
    let stats = batch.commit_with_stats().map_err(js_error)?;
    Ok(IndexResult {
        docs: stats.docs as u32,
        replaced: stats.replaced as u32,
        deleted: stats.deleted as u32,
        seconds: stats.total_secs,
    })
}

pub struct WriteTask {
    pub(crate) engine: Arc<Engine>,
    pub(crate) ops: Vec<Op>,
}

impl Task for WriteTask {
    type Output = IndexResult;
    type JsValue = IndexResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| apply(&self.engine, &self.ops, true))
    }

    fn resolve(&mut self, _: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}

/// O que uma compactação limpou.
#[napi(object)]
pub struct CompactResult {
    pub purged: u32,
    pub emptied_words: u32,
}

pub struct CompactTask {
    pub(crate) engine: Arc<Engine>,
}

impl Task for CompactTask {
    type Output = CompactResult;
    type JsValue = CompactResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let stats = self.engine.compact().map_err(js_error)?;
            Ok(CompactResult { purged: stats.purged as u32, emptied_words: stats.emptied_words as u32 })
        })
    }

    fn resolve(&mut self, _: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}
