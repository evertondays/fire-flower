//! As funções nativas sobre um handle (ver `handle`). Busca e autocompletar
//! são síncronos (latência nativa); o resto, `Promise`.

use fire_flower::Engine;
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::convert::{self, Doc, Doubles};
use crate::handle::{F64Slice, Handle};
use crate::js_error;
use crate::tasks::{self, CompactTask, IndexResult, Op, SearchTask, SearchTopTask, WriteTask};

/// Estatísticas do índice (as do BM25).
#[napi(object)]
pub struct IndexStats {
    pub doc_count: f64,
    pub total_len: f64,
    pub total_title_len: f64,
}

/// Não faz nada: mede o custo de uma chamada pela fronteira.
#[napi]
pub fn noop(_h: Handle) {}

/// Os `k` melhores (BM25) em `out`, `[total, ids…, notas…]`, e quantos são.
/// `out` é o buffer de trabalho do JS (cabe `1 + 2k`): nenhuma alocação na
/// fronteira.
#[napi(catch_unwind)]
pub fn search_top_into(h: Handle, query: String, k: u32, mut out: F64Slice) -> Result<u32> {
    let k = (k as usize).min(out.len().saturating_sub(1) / 2);
    h.with(|e| {
        let top = e.search_ranked(&query, k).map_err(js_error)?;
        let n = top.hits.len();
        out[0] = top.total as f64;
        for (i, hit) in top.hits.iter().enumerate() {
            out[1 + i] = hit.doc as f64;
            out[1 + n + i] = hit.score;
        }
        Ok(n as u32)
    })
}

#[napi(catch_unwind)]
pub fn search_top(h: Handle, query: String, k: u32) -> Result<AsyncTask<SearchTopTask>> {
    Ok(AsyncTask::new(SearchTopTask { engine: h.engine()?, query, k: k as usize }))
}

/// Todos os documentos que casam, em ordem de ID.
#[napi(catch_unwind)]
pub fn search_sync(h: Handle, query: String) -> Result<Doubles> {
    h.with(|e| Ok(convert::ids_buffer(e.search(&query).map_err(js_error)?)))
}

#[napi(catch_unwind)]
pub fn search(h: Handle, query: String) -> Result<AsyncTask<SearchTask>> {
    Ok(AsyncTask::new(SearchTask { engine: h.engine()?, query }))
}

#[napi(catch_unwind)]
pub fn suggest_sync(h: Handle, prefix: String, limit: u32) -> Result<Vec<String>> {
    h.with(|e| e.suggest(&prefix, limit as usize).map_err(js_error))
}

/// Os rótulos dos `n` primeiros IDs de `ids`, num `MultiGet` só.
///
/// Numa string só, separados por `\0`, com `\u{1}` no lugar de quem não tem
/// rótulo: criar uma string na fronteira em vez de dez economiza ~1,5 µs (a
/// camada JS faz o `split`). Um rótulo com um desses caracteres faz voltar a
/// lista de strings.
#[napi(catch_unwind)]
pub fn labels(h: Handle, ids: F64Slice, n: u32) -> Result<Either<String, Vec<Option<String>>>> {
    let ids = ids.iter().take(n as usize).map(|&v| convert::doc_id(v)).collect::<Result<Vec<_>>>()?;
    let labels = h.with(|e| e.labels(&ids).map_err(js_error))?;
    let plain = labels.iter().flatten().all(|l| !l.contains(['\0', '\u{1}']));
    if !plain {
        return Ok(Either::B(labels));
    }
    let mut joined = String::with_capacity(labels.iter().flatten().map(|l| l.len() + 1).sum::<usize>() + labels.len());
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            joined.push('\0');
        }
        joined.push_str(label.as_deref().unwrap_or("\u{1}"));
    }
    Ok(Either::A(joined))
}

/// Adiciona (ou substitui) documentos num lote só, fora do event loop.
#[napi(catch_unwind)]
pub fn index(h: Handle, docs: Vec<Doc>) -> Result<AsyncTask<WriteTask>> {
    let ops = docs.into_iter().map(Op::Add).collect();
    Ok(AsyncTask::new(WriteTask { engine: h.engine()?, ops }))
}

/// Como `index`, na hora: `FF_BUSY` se outro lote estiver aberto.
#[napi(catch_unwind)]
pub fn index_sync(h: Handle, docs: Vec<Doc>) -> Result<IndexResult> {
    let ops: Vec<Op> = docs.into_iter().map(Op::Add).collect();
    h.with(|e| tasks::apply(e, &ops, false))
}

#[napi(catch_unwind)]
pub fn delete_documents(h: Handle, ids: Vec<f64>) -> Result<AsyncTask<WriteTask>> {
    let ops = ids.into_iter().map(|v| convert::doc_id(v).map(Op::Delete)).collect::<Result<_>>()?;
    Ok(AsyncTask::new(WriteTask { engine: h.engine()?, ops }))
}

#[napi(catch_unwind)]
pub fn delete_documents_sync(h: Handle, ids: Vec<f64>) -> Result<IndexResult> {
    let ops: Vec<Op> = ids.into_iter().map(|v| convert::doc_id(v).map(Op::Delete)).collect::<Result<_>>()?;
    h.with(|e| tasks::apply(e, &ops, false))
}

#[napi(catch_unwind)]
pub fn compact(h: Handle) -> Result<AsyncTask<CompactTask>> {
    Ok(AsyncTask::new(CompactTask { engine: h.engine()? }))
}

#[napi(catch_unwind)]
pub fn contains(h: Handle, id: f64) -> Result<bool> {
    let id = convert::doc_id(id)?;
    h.with(|e| e.contains(id).map_err(js_error))
}

#[napi(catch_unwind)]
pub fn stats(h: Handle) -> Result<IndexStats> {
    h.with(|e| {
        let s = e.stats().map_err(js_error)?;
        Ok(IndexStats {
            doc_count: s.doc_count as f64,
            total_len: s.total_len as f64,
            total_title_len: s.total_title_len as f64,
        })
    })
}

/// O snapshot das métricas em JSON, se o banco abriu com `metrics: true`.
#[napi(catch_unwind)]
pub fn metrics_json(h: Handle) -> Result<Option<String>> {
    h.with(|e| {
        let Some(snapshot) = e.metrics_snapshot() else { return Ok(None) };
        serde_json::to_string(&snapshot).map(Some).map_err(|e| crate::binding_error("FF_PANIC", e))
    })
}

#[napi(catch_unwind)]
pub fn is_read_only(h: Handle) -> Result<bool> {
    h.with(|e| Ok(Engine::is_read_only(e)))
}

#[napi]
pub fn is_closed(h: Handle) -> bool {
    h.is_closed()
}

/// Solta o motor deste handle. Promessas em andamento terminam (têm o próprio
/// `Arc`); o banco fecha quando o último handle do processo soltar.
#[napi]
pub fn close(h: Handle) {
    h.close();
}

/// Diagnóstico do harness: os rótulos dos `n` primeiros IDs `iterations` vezes
/// dentro do addon, em nanossegundos por leitura.
#[napi(catch_unwind)]
pub fn native_labels_nanos(h: Handle, ids: F64Slice, n: u32, iterations: u32) -> Result<f64> {
    let ids = ids.iter().take(n as usize).map(|&v| convert::doc_id(v)).collect::<Result<Vec<_>>>()?;
    h.with(|e| {
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(e.labels(&ids).map_err(js_error)?);
        }
        Ok(started.elapsed().as_nanos() as f64 / f64::from(iterations.max(1)))
    })
}

/// Diagnóstico do harness: a mesma busca `iterations` vezes dentro do addon
/// (no mesmo processo, com os mesmos caches), em nanossegundos por busca.
/// A diferença para a chamada pelo JS é o custo da fronteira.
#[napi(catch_unwind)]
pub fn native_search_top_nanos(h: Handle, query: String, k: u32, iterations: u32) -> Result<f64> {
    h.with(|e| {
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(e.search_ranked(&query, k as usize).map_err(js_error)?);
        }
        Ok(started.elapsed().as_nanos() as f64 / f64::from(iterations.max(1)))
    })
}

/// Diagnóstico do harness: cada consulta de `queries` dentro do addon, como o
/// `wiki bench` (`mode`: `top10`, `search` ou `suggest`), em nanossegundos.
/// Separa o custo da fronteira (a mesma consulta pelo JS) do efeito de rodar
/// dentro do processo do Node (a mesma consulta no `wiki`).
#[napi(catch_unwind)]
pub fn native_query_nanos(h: Handle, queries: Vec<String>, mode: String) -> Result<Vec<f64>> {
    h.with(|e| {
        let mut out = Vec::with_capacity(queries.len());
        for q in &queries {
            let started = std::time::Instant::now();
            match mode.as_str() {
                "suggest" => drop(std::hint::black_box(e.suggest(q, 10).map_err(js_error)?)),
                "top10" => drop(std::hint::black_box(e.search_ranked(q, 10).map_err(js_error)?)),
                _ => drop(std::hint::black_box(e.search(q).map_err(js_error)?)),
            }
            out.push(started.elapsed().as_nanos() as f64);
        }
        Ok(out)
    })
}
