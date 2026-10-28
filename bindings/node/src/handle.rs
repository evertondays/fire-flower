//! O handle de um banco aberto e os argumentos lidos direto do Node-API.
//!
//! Um método de classe do napi-rs custa ~240 ns por chamada (desembrulhar o
//! objeto, conferir a classe num mapa); uma função solta, ~30 ns. Por isso o
//! addon expõe funções que recebem o handle como primeiro argumento: um
//! `External` lido com `napi_get_value_external` e conferido por um número
//! mágico. Só a camada JS (`index.js`) chama essas funções.

use std::ptr;
use std::sync::{Arc, RwLock};

use fire_flower::{Engine, Reader};
use napi::bindgen_prelude::*;
use napi::{check_status, sys};

use crate::{binding_error, js_error};

/// "fireflow": confere que o `External` recebido é um handle deste addon.
const MAGIC: u64 = 0x6669_7265_666c_6f77;

/// O que um handle abre: um `Engine`, ou um `Reader` que troca a instância só
/// leitura quando o banco muda.
#[derive(Clone)]
pub enum Backend {
    Engine(Arc<Engine>),
    Reader(Arc<Reader>),
}

/// Um banco aberto do lado do JS. `close()` solta o banco; o handle em si vai
/// embora com o objeto JS (finalizador do `External`).
pub struct EngineHandle {
    magic: u64,
    backend: RwLock<Option<Backend>>,
}

impl EngineHandle {
    /// Roda `f` com o motor, ou dá `FF_CLOSED`. A trava de leitura é do
    /// próprio handle: cada thread JS tem os seus, sem disputa entre elas.
    #[inline]
    pub(crate) fn with<R>(&self, f: impl FnOnce(&Arc<Engine>) -> Result<R>) -> Result<R> {
        let backend = self.backend.read().unwrap_or_else(|e| e.into_inner());
        match backend.as_ref() {
            Some(Backend::Engine(engine)) => f(engine),
            // A instância atual; uma reabertura no meio não a solta.
            Some(Backend::Reader(reader)) => f(&reader.engine()),
            None => Err(js_error(fire_flower::Error::Closed)),
        }
    }

    /// O `Reader`, se o banco abriu com `refreshMs`.
    pub(crate) fn reader(&self) -> Result<Option<Arc<Reader>>> {
        match self.backend.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(Backend::Reader(reader)) => Ok(Some(Arc::clone(reader))),
            Some(Backend::Engine(_)) => Ok(None),
            None => Err(js_error(fire_flower::Error::Closed)),
        }
    }

    /// O motor, para levar a uma tarefa no pool de threads.
    pub(crate) fn engine(&self) -> Result<Arc<Engine>> {
        self.with(|e| Ok(Arc::clone(e)))
    }

    pub(crate) fn close(&self) {
        *self.backend.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.backend.read().unwrap_or_else(|e| e.into_inner()).is_none()
    }
}

/// Um handle novo, devolvido ao JS como `External`.
pub struct NewHandle(pub(crate) Backend);

impl TypeName for NewHandle {
    fn type_name() -> &'static str {
        "ExternalObject<EngineHandle>"
    }

    fn value_type() -> ValueType {
        ValueType::External
    }
}

unsafe extern "C" fn finalize_handle(_env: sys::napi_env, data: *mut std::ffi::c_void, _hint: *mut std::ffi::c_void) {
    // SAFETY: `data` é o `Box<EngineHandle>` criado em `to_napi_value`, e o
    // Node chama o finalizador uma vez só.
    drop(unsafe { Box::from_raw(data.cast::<EngineHandle>()) });
}

impl ToNapiValue for NewHandle {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let handle = Box::into_raw(Box::new(EngineHandle { magic: MAGIC, backend: RwLock::new(Some(val.0)) }));
        let mut out = ptr::null_mut();
        // SAFETY: o Node guarda o ponteiro e chama `finalize_handle` quando o
        // objeto JS for coletado.
        unsafe {
            check_status!(sys::napi_create_external(
                env,
                handle.cast(),
                Some(finalize_handle),
                ptr::null_mut(),
                &mut out
            ))?;
        }
        Ok(out)
    }
}

/// O handle recebido numa chamada. Vale durante a chamada (o objeto JS é um
/// argumento dela); quem precisa do motor depois leva um `Arc`.
pub struct Handle(&'static EngineHandle);

impl std::ops::Deref for Handle {
    type Target = EngineHandle;

    #[inline]
    fn deref(&self) -> &EngineHandle {
        self.0
    }
}

impl TypeName for Handle {
    fn type_name() -> &'static str {
        "ExternalObject<EngineHandle>"
    }

    fn value_type() -> ValueType {
        ValueType::External
    }
}

impl FromNapiValue for Handle {
    unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
        let mut data = ptr::null_mut();
        // SAFETY: um `External` qualquer devolve o ponteiro que guardou; só um
        // criado por `NewHandle` tem o número mágico no começo.
        unsafe {
            check_status!(sys::napi_get_value_external(env, value, &mut data))?;
            let handle = data.cast::<EngineHandle>();
            if handle.is_null() || (*handle).magic != MAGIC {
                return Err(binding_error("FF_ARGUMENT", "handle inválido"));
            }
            Ok(Handle(&*handle))
        }
    }
}

/// Os números de um `Float64Array` recebido, sem copiar nem criar referência
/// (o `Float64Array` do napi-rs cria uma referência por argumento). O addon
/// também escreve nele: é o buffer de trabalho de `searchTopSync`, que evita
/// criar um array tipado por busca (~0,5 µs no Node 24).
pub struct F64Slice(&'static mut [f64]);

impl std::ops::Deref for F64Slice {
    type Target = [f64];

    fn deref(&self) -> &[f64] {
        self.0
    }
}

impl std::ops::DerefMut for F64Slice {
    fn deref_mut(&mut self) -> &mut [f64] {
        self.0
    }
}

impl TypeName for F64Slice {
    fn type_name() -> &'static str {
        "Float64Array"
    }

    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl FromNapiValue for F64Slice {
    unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
        let (mut kind, mut len, mut data, mut buffer, mut offset) = (0, 0, ptr::null_mut(), ptr::null_mut(), 0);
        // SAFETY: o array tipado vive durante a chamada, e o JS não roda
        // enquanto o addon o usa; o tipo é conferido antes de tocar em `len`
        // números de `data`.
        unsafe {
            check_status!(sys::napi_get_typedarray_info(
                env,
                value,
                &mut kind,
                &mut len,
                &mut data,
                &mut buffer,
                &mut offset
            ))?;
            if kind != sys::TypedarrayType::float64_array {
                return Err(binding_error("FF_ARGUMENT", "esperava um Float64Array"));
            }
            Ok(F64Slice(if len == 0 { &mut [] } else { std::slice::from_raw_parts_mut(data.cast::<f64>(), len) }))
        }
    }
}
