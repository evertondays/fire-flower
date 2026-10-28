//! Contadores do merge operator, desligados fora da medição.
//!
//! Um load atômico por chamada quando estão desligados. A sonda
//! (`examples/edit_probe.rs`) liga em volta de cada consulta para separar o
//! tempo gasto dentro do merge do resto da leitura.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use rocksdb::MergeOperands;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FULL_CALLS: AtomicU64 = AtomicU64::new(0);
static PARTIAL_CALLS: AtomicU64 = AtomicU64::new(0);
static FULL_NS: AtomicU64 = AtomicU64::new(0);
static PARTIAL_NS: AtomicU64 = AtomicU64::new(0);
static FULL_WITH_BASE: AtomicU64 = AtomicU64::new(0);
static FULL_OPERANDS: AtomicU64 = AtomicU64::new(0);
static FULL_OPERAND_BYTES: AtomicU64 = AtomicU64::new(0);
static FULL_BASE_BYTES: AtomicU64 = AtomicU64::new(0);
static FULL_OUT_BYTES: AtomicU64 = AtomicU64::new(0);
static PARTIAL_OPERANDS: AtomicU64 = AtomicU64::new(0);
static MAX_OPERANDS: AtomicU64 = AtomicU64::new(0);

/// Totais desde o último [`take_merge_trace`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeTrace {
    /// Chamadas do merge completo (leitura e compactação no último nível).
    pub full_calls: u64,
    /// Chamadas do merge parcial (só operandos).
    pub partial_calls: u64,
    /// Tempo dentro do merge completo.
    pub full_ns: u64,
    /// Tempo dentro do merge parcial.
    pub partial_ns: u64,
    /// Merges completos que receberam valor base.
    pub full_with_base: u64,
    /// Operandos somados nas chamadas completas.
    pub full_operands: u64,
    /// Bytes dos operandos nas chamadas completas.
    pub full_operand_bytes: u64,
    /// Bytes do valor base nas chamadas completas.
    pub full_base_bytes: u64,
    /// Bytes devolvidos pelas chamadas completas.
    pub full_out_bytes: u64,
    /// Operandos somados nas chamadas parciais.
    pub partial_operands: u64,
    /// Maior número de operandos numa chamada (completa ou parcial).
    pub max_operands: u64,
}

/// Liga ou desliga a coleta. Desligado, o merge só faz um load atômico.
pub fn set_merge_trace(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Lê os contadores e zera.
pub fn take_merge_trace() -> MergeTrace {
    MergeTrace {
        full_calls: FULL_CALLS.swap(0, Ordering::Relaxed),
        partial_calls: PARTIAL_CALLS.swap(0, Ordering::Relaxed),
        full_ns: FULL_NS.swap(0, Ordering::Relaxed),
        partial_ns: PARTIAL_NS.swap(0, Ordering::Relaxed),
        full_with_base: FULL_WITH_BASE.swap(0, Ordering::Relaxed),
        full_operands: FULL_OPERANDS.swap(0, Ordering::Relaxed),
        full_operand_bytes: FULL_OPERAND_BYTES.swap(0, Ordering::Relaxed),
        full_base_bytes: FULL_BASE_BYTES.swap(0, Ordering::Relaxed),
        full_out_bytes: FULL_OUT_BYTES.swap(0, Ordering::Relaxed),
        partial_operands: PARTIAL_OPERANDS.swap(0, Ordering::Relaxed),
        max_operands: MAX_OPERANDS.swap(0, Ordering::Relaxed),
    }
}

pub(super) fn trace_start() -> Option<Instant> {
    ENABLED.load(Ordering::Relaxed).then(Instant::now)
}

pub(super) fn trace_record(full: bool, base: Option<&[u8]>, operands: &MergeOperands, out: Option<&[u8]>, started: Instant) {
    let ns = started.elapsed().as_nanos() as u64;
    let n = operands.len() as u64;
    MAX_OPERANDS.fetch_max(n, Ordering::Relaxed);
    if full {
        FULL_CALLS.fetch_add(1, Ordering::Relaxed);
        FULL_NS.fetch_add(ns, Ordering::Relaxed);
        FULL_OPERANDS.fetch_add(n, Ordering::Relaxed);
        FULL_OPERAND_BYTES.fetch_add(operands.iter().map(|op| op.len() as u64).sum(), Ordering::Relaxed);
        if let Some(base) = base {
            FULL_WITH_BASE.fetch_add(1, Ordering::Relaxed);
            FULL_BASE_BYTES.fetch_add(base.len() as u64, Ordering::Relaxed);
        }
        if let Some(out) = out {
            FULL_OUT_BYTES.fetch_add(out.len() as u64, Ordering::Relaxed);
        }
    } else {
        PARTIAL_CALLS.fetch_add(1, Ordering::Relaxed);
        PARTIAL_NS.fetch_add(ns, Ordering::Relaxed);
        PARTIAL_OPERANDS.fetch_add(n, Ordering::Relaxed);
    }
}
