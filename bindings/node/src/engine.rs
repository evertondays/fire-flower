//! A classe nativa (`NativeEngine`): um handle para um `Engine` compartilhado.
//! Busca e autocompletar são síncronos (latência nativa); o resto, `Promise`.

use std::sync::Arc;

use fire_flower::Engine;
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::convert::{self, Doc};
use crate::js_error;
use crate::tasks::{self, CompactTask, IndexResult, Op, SearchTask, SearchTopTask, WriteTask};

/// Um banco aberto. `close()` solta o handle; o banco fecha quando o último
/// handle do processo for solto (ver `REGISTRY`).
#[napi]
pub struct NativeEngine {
    engine: Option<Arc<Engine>>,
}

/// Estatísticas do índice (as do BM25).
#[napi(object)]
pub struct IndexStats {
    pub doc_count: f64,
    pub total_len: f64,
    pub total_title_len: f64,
}

impl NativeEngine {
    pub(crate) fn new(engine: Arc<Engine>) -> Self {
        Self { engine: Some(engine) }
    }

    fn engine(&self) -> Result<&Arc<Engine>> {
        self.engine.as_ref().ok_or_else(|| js_error(fire_flower::Error::Closed))
    }
}

#[napi]
impl NativeEngine {
    /// Não faz nada: mede o custo de uma chamada pela fronteira.
    #[napi]
    pub fn noop(&self) {}

    /// `[total, ids…, notas…]` dos `k` melhores (BM25).
    #[napi(catch_unwind)]
    pub fn search_top_sync(&self, query: String, k: u32) -> Result<Float64Array> {
        let top = self.engine()?.search_ranked(&query, k as usize).map_err(js_error)?;
        Ok(convert::top_buffer(top.total, &top.hits))
    }

    #[napi(catch_unwind)]
    pub fn search_top(&self, query: String, k: u32) -> Result<AsyncTask<SearchTopTask>> {
        Ok(AsyncTask::new(SearchTopTask { engine: Arc::clone(self.engine()?), query, k: k as usize }))
    }

    /// Todos os documentos que casam, em ordem de ID.
    #[napi(catch_unwind)]
    pub fn search_sync(&self, query: String) -> Result<Float64Array> {
        Ok(convert::ids_buffer(self.engine()?.search(&query).map_err(js_error)?))
    }

    #[napi(catch_unwind)]
    pub fn search(&self, query: String) -> Result<AsyncTask<SearchTask>> {
        Ok(AsyncTask::new(SearchTask { engine: Arc::clone(self.engine()?), query }))
    }

    #[napi(catch_unwind)]
    pub fn suggest_sync(&self, prefix: String, limit: u32) -> Result<Vec<String>> {
        self.engine()?.suggest(&prefix, limit as usize).map_err(js_error)
    }

    /// Os rótulos de `ids`, num `MultiGet` só.
    #[napi(catch_unwind)]
    pub fn labels(&self, ids: Float64Array) -> Result<Vec<Option<String>>> {
        let ids = ids.iter().map(|&v| convert::doc_id(v)).collect::<Result<Vec<_>>>()?;
        self.engine()?.labels(&ids).map_err(js_error)
    }

    /// Adiciona (ou substitui) documentos num lote só, fora do event loop.
    #[napi(catch_unwind)]
    pub fn index(&self, docs: Vec<Doc>) -> Result<AsyncTask<WriteTask>> {
        let ops = docs.into_iter().map(Op::Add).collect();
        Ok(AsyncTask::new(WriteTask { engine: Arc::clone(self.engine()?), ops }))
    }

    /// Como `index`, na hora: `FF_BUSY` se outro lote estiver aberto.
    #[napi(catch_unwind)]
    pub fn index_sync(&self, docs: Vec<Doc>) -> Result<IndexResult> {
        let ops: Vec<Op> = docs.into_iter().map(Op::Add).collect();
        tasks::apply(self.engine()?, &ops, false)
    }

    #[napi(catch_unwind)]
    pub fn delete_documents(&self, ids: Vec<f64>) -> Result<AsyncTask<WriteTask>> {
        let ops = ids.into_iter().map(|v| convert::doc_id(v).map(Op::Delete)).collect::<Result<_>>()?;
        Ok(AsyncTask::new(WriteTask { engine: Arc::clone(self.engine()?), ops }))
    }

    #[napi(catch_unwind)]
    pub fn delete_documents_sync(&self, ids: Vec<f64>) -> Result<IndexResult> {
        let ops: Vec<Op> = ids.into_iter().map(|v| convert::doc_id(v).map(Op::Delete)).collect::<Result<_>>()?;
        tasks::apply(self.engine()?, &ops, false)
    }

    #[napi(catch_unwind)]
    pub fn compact(&self) -> Result<AsyncTask<CompactTask>> {
        Ok(AsyncTask::new(CompactTask { engine: Arc::clone(self.engine()?) }))
    }

    #[napi(catch_unwind)]
    pub fn contains(&self, id: f64) -> Result<bool> {
        self.engine()?.contains(convert::doc_id(id)?).map_err(js_error)
    }

    #[napi(catch_unwind)]
    pub fn stats(&self) -> Result<IndexStats> {
        let s = self.engine()?.stats().map_err(js_error)?;
        Ok(IndexStats {
            doc_count: s.doc_count as f64,
            total_len: s.total_len as f64,
            total_title_len: s.total_title_len as f64,
        })
    }

    /// O snapshot das métricas em JSON, se o banco abriu com `metrics: true`.
    #[napi(catch_unwind)]
    pub fn metrics_json(&self) -> Result<Option<String>> {
        let Some(snapshot) = self.engine()?.metrics_snapshot() else { return Ok(None) };
        serde_json::to_string(&snapshot).map(Some).map_err(|e| crate::binding_error("FF_PANIC", e))
    }

    #[napi(getter)]
    pub fn read_only(&self) -> Result<bool> {
        Ok(self.engine()?.is_read_only())
    }

    #[napi(getter)]
    pub fn closed(&self) -> bool {
        self.engine.is_none()
    }

    /// Solta este handle. Promessas em andamento terminam (têm o próprio `Arc`).
    #[napi]
    pub fn close(&mut self) {
        self.engine = None;
    }
}
