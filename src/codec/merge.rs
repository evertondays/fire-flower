//! Merge operator das listas: une pedaços gravados em lotes diferentes.

use crate::{DocId, Error};
use crate::codec::{PositionsCursor, SKIP_BLOCK, decode_postings, encode_postings_into, put_varint};
use crate::codec::list::{ListHeader, SkipEntry, put_skips};
use crate::codec::positions::encode_positions_with;
use crate::codec::varint::{Reader, corrupt};

/// Uma lista codificada, pronta para o merge.
pub(crate) struct Scan {
    n: u64,
    first: DocId,
    last: DocId,
    /// Onde começa a primeira entrada (depois da tabela).
    body: usize,
    /// Onde termina o Δdoc da primeira entrada.
    after_first_delta: usize,
    /// Tabela de saltos (vazia numa lista pequena).
    skips: Vec<SkipEntry>,
}

/// Lista grande: o último documento vem da tabela e o primeiro da primeira
/// entrada, sem percorrer o resto (o merge na leitura de um termo frequente
/// com pedaços pendentes fazia isso a cada leitura). Lista pequena: percorre
/// e valida.
fn scan(bytes: &[u8], positions: bool) -> Result<Scan, Error> {
    let header = ListHeader::parse(bytes, true)?;
    let n = header.n;
    let mut r = Reader { buf: bytes, pos: header.body };
    let body = r.pos;
    if let Some(last) = header.entries.last() {
        let first = r.varint()?;
        return Ok(Scan { n, first, last: last.last, body, after_first_delta: r.pos, skips: header.entries });
    }
    let (mut first, mut doc, mut after_first_delta) = (0, 0u64, body);
    for i in 0..n {
        doc = doc.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        if i == 0 {
            first = doc;
            after_first_delta = r.pos;
        }
        let k = r.varint()?;
        if positions {
            r.skip_varints(k)?;
        } else if k & 1 == 1 {
            r.skip_varints(1)?; // tf do título
        }
    }
    r.finish()?;
    Ok(Scan { n, first, last: doc, body, after_first_delta, skips: Vec::new() })
}

/// Merge operator de `post:`: une as listas por documento; para o mesmo
/// documento, a entrada mais nova vence (reindexar troca o tf). `inputs` vai
/// do mais antigo (o valor gravado) para o mais novo. É associativo.
/// `None` = dado corrompido (a leitura falha em vez de mentir).
pub fn merge_postings<'a>(inputs: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    merge_lists(inputs, false)
}

/// Merge operator de `pos:`, com a mesma regra de [`merge_postings`].
pub fn merge_positions<'a>(inputs: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    merge_lists(inputs, true)
}

pub(super) fn merge_lists<'a>(inputs: impl Iterator<Item = &'a [u8]>, positions: bool) -> Option<Vec<u8>> {
    let mut lists: Vec<(&[u8], Scan)> = Vec::new();
    for bytes in inputs {
        let s = scan(bytes, positions).ok()?;
        if s.n > 0 {
            lists.push((bytes, s));
        }
    }
    // Caminho comum (carga com IDs crescentes): cada pedaço começa depois do
    // fim do anterior. Basta concatenar os bytes, refazendo um delta por pedaço.
    if lists.windows(2).all(|w| w[0].1.last < w[1].1.first) {
        let total: u64 = lists.iter().map(|(_, s)| s.n).sum();
        let mut body = Vec::with_capacity(lists.iter().map(|(b, _)| b.len()).sum::<usize>());
        // A tabela da lista unida é a concatenação das tabelas dos pedaços
        // (um pedaço pequeno vira um bloco). Só o primeiro bloco de cada
        // pedaço muda de tamanho: o delta da primeira entrada é refeito.
        let mut blocks: Vec<SkipEntry> = Vec::new();
        let mut prev_last = 0;
        for (i, (bytes, s)) in lists.iter().enumerate() {
            let before = body.len();
            if i == 0 {
                body.extend_from_slice(&bytes[s.body..]);
            } else {
                put_varint(&mut body, s.first - prev_last);
                body.extend_from_slice(&bytes[s.after_first_delta..]);
            }
            let grown = (body.len() - before) as isize - (bytes.len() - s.body) as isize;
            if s.skips.is_empty() {
                blocks.push(SkipEntry { last: s.last, bytes: bytes.len() - s.body, count: s.n });
            } else {
                blocks.extend_from_slice(&s.skips[..]);
            }
            let first_block = blocks.len() - s.skips.len().max(1);
            blocks[first_block].bytes = (blocks[first_block].bytes as isize + grown) as usize;
            prev_last = s.last;
        }
        if total as usize <= 2 * SKIP_BLOCK {
            blocks.clear();
        }
        let mut out = Vec::with_capacity(16 + blocks.len() * 5 + body.len());
        put_varint(&mut out, total);
        put_skips(&mut out, &blocks);
        out.extend_from_slice(&body);
        return Some(out);
    }
    if positions { merge_positions_slow(&lists) } else { merge_postings_slow(&lists) }.ok()
}

/// Ordenação estável por doc: entre entradas do mesmo doc, a ordem dos
/// pedaços se mantém e a última (a mais nova) fica.
fn merge_postings_slow(lists: &[(&[u8], Scan)]) -> Result<Vec<u8>, Error> {
    let mut all: Vec<(DocId, u32, u32)> = Vec::with_capacity(lists.iter().map(|(_, s)| s.n as usize).sum());
    for (bytes, _) in lists {
        all.extend(decode_postings(bytes)?.into_iter().map(|p| (p.doc, p.tf, p.title_tf)));
    }
    all.sort_by_key(|e| e.0);
    let mut kept: Vec<(DocId, u32, u32)> = Vec::with_capacity(all.len());
    for e in all {
        match kept.last_mut() {
            Some(last) if last.0 == e.0 => *last = e,
            _ => kept.push(e),
        }
    }
    let mut out = Vec::new();
    encode_postings_into(&mut out, &kept);
    Ok(out)
}

fn merge_positions_slow(lists: &[(&[u8], Scan)]) -> Result<Vec<u8>, Error> {
    let mut flat: Vec<u32> = Vec::new();
    // (doc, início em `flat`, quantas)
    let mut all: Vec<(DocId, usize, usize)> = Vec::new();
    let mut buf = Vec::new();
    for (bytes, _) in lists {
        let mut cursor = PositionsCursor::new(bytes)?;
        while let Some(doc) = cursor.next_doc()? {
            cursor.read_positions(&mut buf)?;
            all.push((doc, flat.len(), buf.len()));
            flat.extend_from_slice(&buf);
        }
    }
    all.sort_by_key(|e| e.0);
    let mut kept: Vec<(DocId, usize, usize)> = Vec::with_capacity(all.len());
    for e in all {
        match kept.last_mut() {
            Some(last) if last.0 == e.0 => *last = e,
            _ => kept.push(e),
        }
    }
    let mut out = Vec::new();
    encode_positions_with(&mut out, kept.len(), kept.iter().map(|&(doc, start, k)| (doc, &flat[start..start + k])));
    Ok(out)
}
