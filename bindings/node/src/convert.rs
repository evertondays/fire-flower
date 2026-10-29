//! Conversões na fronteira: opções do JS para `Config`, documentos, IDs e o
//! buffer dos resultados.

use std::ptr;

use fire_flower::{Config, DocId, Hit, Language, Stemming};
use napi::bindgen_prelude::*;
use napi::{check_status, sys};
use napi_derive::napi;

use crate::binding_error;

/// Opções de `Engine.open` (ver `index.d.ts`).
#[napi(object)]
#[derive(Default)]
pub struct OpenOptions {
    pub languages: Option<Vec<String>>,
    pub positions: Option<bool>,
    pub stemming: Option<String>,
    pub inflections: Option<String>,
    pub block_cache_bytes: Option<f64>,
    pub read_only: Option<bool>,
    pub create_if_missing: Option<bool>,
    pub metrics: Option<bool>,
}

/// Um documento de `Engine.index`. O texto pode vir num `Buffer` (UTF-8):
/// é validado na thread do pool, sem cópia.
#[napi(object)]
pub struct Doc {
    pub id: f64,
    pub title: Option<String>,
    pub text: Either<String, Buffer>,
    pub label: Option<String>,
}

/// As opções viram a `Config` do core (e se o banco abre só para leitura).
pub(crate) fn config(o: OpenOptions) -> Result<(Config, bool)> {
    let read_only = o.read_only.unwrap_or(false);
    let mut config = if read_only { Config::reader() } else { Config::default() };
    if let Some(codes) = o.languages {
        let langs = codes
            .iter()
            .map(|c| Language::from_code(c).ok_or_else(|| binding_error("FF_ARGUMENT", format!("idioma {c:?}"))))
            .collect::<Result<Vec<_>>>()?;
        config.languages = Some(langs);
    }
    config.positions = o.positions;
    config.stemming = o.stemming.as_deref().map(stemming).transpose()?;
    config.inflections = o.inflections.as_deref().map(stemming).transpose()?;
    if let Some(bytes) = o.block_cache_bytes {
        config.block_cache_bytes = bytes.max(0.0) as usize;
    }
    if let Some(create) = o.create_if_missing {
        config.create_if_missing = create;
    }
    config.metrics = o.metrics.unwrap_or(false);
    Ok((config, read_only))
}

fn stemming(code: &str) -> Result<Stemming> {
    let code = if code == "off" { "none" } else { code };
    Stemming::from_code(code).ok_or_else(|| binding_error("FF_ARGUMENT", format!("stemming {code:?}")))
}

/// Um ID do JS: inteiro de 0 a 2^53 (o core recusa acima de 2^40 − 1).
pub(crate) fn doc_id(v: f64) -> Result<DocId> {
    if v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v <= 9_007_199_254_740_991.0 {
        Ok(v as DocId)
    } else {
        Err(binding_error("FF_ARGUMENT", format!("ID de documento inválido: {v}")))
    }
}

/// O texto de um documento, emprestado do `String` ou do `Buffer`.
pub(crate) fn text(doc: &Doc) -> Result<&str> {
    match &doc.text {
        Either::A(s) => Ok(s),
        Either::B(b) => std::str::from_utf8(b)
            .map_err(|_| binding_error("FF_ARGUMENT", format!("documento {}: o Buffer não é UTF-8", doc.id))),
    }
}

/// Um `Float64Array` com memória do próprio V8, copiada do Rust.
///
/// O `Float64Array` do napi-rs aponta para a memória do Rust (ArrayBuffer
/// externo, com finalizador): criar um custa microssegundos e enche o GC de
/// finalizadores, e um resultado grande fica preso até a coleta, sem a
/// memória voltar ao alocador. Copiar os 21 números de um top 10 custa
/// nanossegundos; uma lista grande custa um `memcpy`.
pub struct Doubles(pub(crate) Vec<f64>);

impl TypeName for Doubles {
    fn type_name() -> &'static str {
        "Float64Array"
    }

    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for Doubles {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let bytes = std::mem::size_of_val(val.0.as_slice());
        let (mut data, mut buffer, mut array) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        // SAFETY: `data` aponta para `bytes` bytes recém-alocados pelo V8, que
        // não se sobrepõem ao `Vec`; o array tipado cobre exatamente esse buffer.
        unsafe {
            check_status!(sys::napi_create_arraybuffer(env, bytes, &mut data, &mut buffer))?;
            if bytes > 0 {
                ptr::copy_nonoverlapping(val.0.as_ptr().cast::<u8>(), data.cast::<u8>(), bytes);
            }
            check_status!(sys::napi_create_typedarray(
                env,
                sys::TypedarrayType::float64_array,
                val.0.len(),
                buffer,
                0,
                &mut array
            ))?;
        }
        Ok(array)
    }
}

/// `[total, ids…, notas…]` num `Float64Array` só: um valor criado na
/// fronteira em vez de um objeto com três; a camada JS fatia com `subarray`
/// (sem cópia). IDs vão até 2^40 − 1: cabem num `f64` sem perda.
pub(crate) fn top_buffer(total: usize, hits: &[Hit]) -> Doubles {
    let mut buf = Vec::with_capacity(1 + 2 * hits.len());
    buf.push(total as f64);
    buf.extend(hits.iter().map(|h| h.doc as f64));
    buf.extend(hits.iter().map(|h| h.score));
    Doubles(buf)
}

pub(crate) fn ids_buffer(ids: Vec<DocId>) -> Doubles {
    // Mesmo tamanho de elemento: a conversão reaproveita a alocação.
    Doubles(ids.into_iter().map(|d| d as f64).collect())
}
