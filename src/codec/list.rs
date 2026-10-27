//! O formato comum das listas `post:` e `pos:`: o cabeçalho e a tabela de
//! saltos (blocos para pular sem decodificar).

use crate::{DocId, Error};
use crate::codec::put_varint;
use crate::codec::varint::{Reader, corrupt};

/// Entradas por bloco da tabela de saltos.
pub const SKIP_BLOCK: usize = 128;

/// Grava `n`, a tabela de saltos (se a lista for grande) e as entradas.
/// `write` grava as entradas num buffer e anota, para cada bloco de
/// `SKIP_BLOCK` entradas (o último pode ser menor), o último documento e onde
/// o bloco termina (bytes desde o começo das entradas). `tombstones` diz se
/// alguma entrada é uma lápide (só operandos de merge têm).
pub(super) fn write_list(
    buf: &mut Vec<u8>,
    n: usize,
    tombstones: bool,
    write: impl FnOnce(&mut Vec<u8>, &mut Vec<(DocId, usize)>),
) {
    put_varint(buf, n as u64);
    if n <= 2 * SKIP_BLOCK {
        // Lista pequena: sem tabela, e as entradas vão direto para `buf`.
        put_skips(buf, &[], tombstones);
        write(buf, &mut Vec::new());
        return;
    }
    let mut body = Vec::with_capacity(n * 3);
    let mut ends = Vec::with_capacity(n.div_ceil(SKIP_BLOCK));
    write(&mut body, &mut ends);
    let mut blocks = Vec::with_capacity(ends.len());
    let mut start = 0;
    for (i, &(last, end)) in ends.iter().enumerate() {
        let count = if i + 1 == ends.len() { n - i * SKIP_BLOCK } else { SKIP_BLOCK };
        blocks.push(SkipEntry { last, bytes: end - start, count: count as u64 });
        start = end;
    }
    put_skips(buf, &blocks, tombstones);
    buf.extend_from_slice(&body);
}

/// Um bloco da tabela de saltos, como é gravado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkipEntry {
    /// Último documento do bloco (absoluto).
    pub(super) last: DocId,
    /// Tamanho do bloco em bytes.
    pub(super) bytes: usize,
    /// Entradas no bloco.
    pub(super) count: u64,
}

/// `s << 1 | tem lápide`, depois `(Δúltimo doc, bytes, entradas − 1)` de
/// cada bloco.
pub(super) fn put_skips(buf: &mut Vec<u8>, blocks: &[SkipEntry], tombstones: bool) {
    put_varint(buf, ((blocks.len() as u64) << 1) | u64::from(tombstones));
    let mut last = 0;
    for b in blocks {
        put_varint(buf, b.last - last);
        put_varint(buf, b.bytes as u64);
        put_varint(buf, b.count - 1); // 128 entradas cabem em 1 byte
        last = b.last;
    }
}

/// Um bloco já localizado: último documento, onde começa (relativo às
/// entradas) e o índice da sua primeira entrada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Block {
    pub(super) last: DocId,
    pub(super) start: usize,
    pub(super) first: u64,
}

/// Cabeçalho de uma lista: quantas entradas, a tabela de saltos e onde as
/// entradas começam.
pub(super) struct ListHeader {
    /// Entradas, lápides inclusive.
    pub(super) n: u64,
    /// Alguma entrada é uma lápide. Só operandos de merge têm: o merge
    /// completo as descarta, então uma busca nunca recebe uma.
    pub(super) tombstones: bool,
    pub(super) blocks: Vec<Block>,
    pub(super) entries: Vec<SkipEntry>,
    pub(super) body: usize,
}

impl ListHeader {
    /// `parse_skips = false` só pula a tabela (decodificação completa).
    pub(super) fn parse(bytes: &[u8], parse_skips: bool) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let n = r.varint()?;
        let packed = r.varint()?;
        let (s, tombstones) = (packed >> 1, packed & 1 == 1);
        // Tabela se e só se a lista for grande; cada bloco tem ao menos 3 bytes.
        if (s > 0) != (n as usize > 2 * SKIP_BLOCK) || s > n || s as usize > bytes.len() / 3 {
            return Err(corrupt("tabela de saltos inválida"));
        }
        let mut blocks = Vec::new();
        let mut entries = Vec::new();
        if parse_skips {
            blocks.reserve(s as usize);
            entries.reserve(s as usize);
            let (mut last, mut start, mut first) = (0u64, 0usize, 0u64);
            for _ in 0..s {
                last = last.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
                let len = usize::try_from(r.varint()?).map_err(|_| corrupt("bloco grande demais"))?;
                let count = r.varint()?.checked_add(1).ok_or_else(|| corrupt("bloco grande demais"))?;
                if len == 0 {
                    return Err(corrupt("bloco vazio na tabela de saltos"));
                }
                blocks.push(Block { last, start, first });
                entries.push(SkipEntry { last, bytes: len, count });
                start = start.checked_add(len).ok_or_else(|| corrupt("bloco grande demais"))?;
                first = first.checked_add(count).ok_or_else(|| corrupt("bloco grande demais"))?;
            }
            if s > 0 && (first != n || start != bytes.len() - r.pos) {
                return Err(corrupt("tabela de saltos não bate com as entradas"));
            }
        } else {
            r.skip_varints(3 * s)?;
        }
        Ok(Self { n, tombstones, blocks, entries, body: r.pos })
    }

    /// Como [`ListHeader::parse`], para quem lê a lista numa busca: uma
    /// lápide ali é dado corrompido (o merge completo já as tirou).
    pub(super) fn parse_clean(bytes: &[u8], parse_skips: bool) -> Result<Self, Error> {
        let header = Self::parse(bytes, parse_skips)?;
        if header.tombstones {
            return Err(corrupt("lápide numa lista já unida"));
        }
        Ok(header)
    }
}

/// Quantas entradas a lista tem (o df do termo), sem decodificá-la.
pub fn list_len(bytes: &[u8]) -> Result<u64, Error> {
    Reader::new(bytes).varint()
}
