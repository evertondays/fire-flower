//! Conjuntos de nós (`stem:<radical>`), para as flexões na consulta.

use crate::codec::varint::corrupt;
use crate::codec::varint::Reader;
use crate::Error;
use crate::codec::put_varint;

/// Conjunto de nós de `stem:<radical>` (as palavras com esse radical): IDs
/// crescentes, o primeiro inteiro e os outros como diferença.
pub fn encode_node_set(nodes: &[u64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * 3);
    let mut prev = 0u64;
    for (i, &n) in nodes.iter().enumerate() {
        debug_assert!(i == 0 || n > prev, "nós em ordem crescente e sem repetição");
        put_varint(&mut buf, n - prev);
        prev = n;
    }
    buf
}

pub fn decode_node_set(bytes: &[u8]) -> Result<Vec<u64>, Error> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    let mut prev = 0u64;
    while r.pos < bytes.len() {
        let delta = r.varint()?;
        if !out.is_empty() && delta == 0 {
            return Err(corrupt("nó repetido no conjunto de stem:"));
        }
        prev = prev.checked_add(delta).ok_or_else(|| corrupt("nó estourou u64"))?;
        out.push(prev);
    }
    Ok(out)
}

/// Merge operator de `stem:`: a união dos conjuntos (associativa e comutativa).
pub fn merge_node_sets<'a>(inputs: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    let mut all: Vec<u64> = Vec::new();
    for bytes in inputs {
        all.extend(decode_node_set(bytes).ok()?);
    }
    all.sort_unstable();
    all.dedup();
    Some(encode_node_set(&all))
}
