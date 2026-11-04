//! Remendo de edição (`pov:` / `psv:`): as entradas que uma edição muda, à
//! parte da lista base. A leitura aplica o remendo em cima da base sem
//! regravá-la; a compactação incorpora o remendo e apaga a chave.

use crate::{DocId, Error, Posting};
use crate::codec::list::ListHeader;
use crate::codec::merge::{ListKind, merge_lists};
use crate::codec::positions::PosEdit;
use crate::codec::postings::{decode_docs, decode_postings};
use crate::codec::varint::{Reader, corrupt};
use crate::codec::{decode_positions, put_varint};

/// Uma entrada do remendo de `post:`: substitui a da base, ou a apaga.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayPost {
    pub doc: DocId,
    pub tombstone: bool,
    pub tf: u32,
    pub title_tf: u32,
}

/// `zigzag(df_delta)` seguido da lista (com lápides, se houver). O delta é a
/// variação do df que este pedaço causa na lista base: +1 documento novo no
/// termo, −1 lápide, 0 troca de tf. Os pedaços somam os deltas.
pub fn wrap_delta(delta: i64, list: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(list.len() + 10);
    put_varint(&mut out, zigzag_encode(delta));
    out.extend_from_slice(list);
    out
}

/// O delta gravado na frente do valor, sem decodificar a lista.
pub fn peek_delta(bytes: &[u8]) -> Result<i64, Error> {
    Ok(split_delta(bytes)?.0)
}

/// Une pedaços de remendo (do mais antigo para o mais novo). Sempre guarda as
/// lápides: elas só saem quando a leitura ou a compactação aplicam o remendo
/// na lista base. Os deltas se somam.
pub fn merge_overlays<'a>(inputs: impl Iterator<Item = &'a [u8]>, kind: ListKind) -> Option<Vec<u8>> {
    let mut delta = 0i64;
    let mut lists = Vec::new();
    for bytes in inputs {
        let (d, rest) = split_delta(bytes).ok()?;
        delta = delta.checked_add(d)?;
        lists.push(rest);
    }
    let merged = merge_lists(lists.iter().copied(), kind, false)?;
    Some(wrap_delta(delta, &merged))
}

/// Entradas de um remendo de `post:` já unido.
pub fn decode_post_edits(bytes: &[u8]) -> Result<(i64, Vec<OverlayPost>), Error> {
    let (delta, rest) = split_delta(bytes)?;
    Ok((delta, read_post_entries(rest)?))
}

/// Entradas de um remendo de `pos:` já unido. O delta não é usado (é sempre 0).
pub fn decode_pos_edits(bytes: &[u8]) -> Result<Vec<PosEdit>, Error> {
    let (_, rest) = split_delta(bytes)?;
    read_pos_entries(rest)
}

/// df da lista com o remendo aplicado: o da base mais o delta. Não percorre
/// as entradas.
pub fn live_len(base: Option<&[u8]>, df_delta: i64) -> Result<u64, Error> {
    let n = match base {
        Some(bytes) => crate::codec::list_len(bytes)? as i64,
        None => 0,
    };
    let live = n.checked_add(df_delta).ok_or_else(|| corrupt("df do remendo estourou"))?;
    u64::try_from(live).map_err(|_| corrupt("df do remendo negativo"))
}

/// Documentos da base com o remendo aplicado, em ordem. Uma passada só: o
/// mesmo custo de decodificar a lista, mais um teste por documento.
pub fn apply_docs(base: Option<&[u8]>, edits: &[OverlayPost]) -> Result<Vec<DocId>, Error> {
    if edits.is_empty() {
        return match base {
            Some(bytes) => decode_docs(bytes),
            None => Ok(Vec::new()),
        };
    }
    let Some(bytes) = base else {
        return Ok(edits.iter().filter(|e| !e.tombstone).map(|e| e.doc).collect());
    };
    let header = ListHeader::parse_clean(bytes, false)?;
    let mut r = Reader { buf: bytes, pos: header.body };
    let mut prev = 0u64;
    let mut out = Vec::with_capacity(header.n as usize);
    let mut i = 0;
    for _ in 0..header.n {
        let doc = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        if r.varint()? & 1 == 1 {
            r.skip_varints(1)?;
        }
        prev = doc;
        while i < edits.len() && edits[i].doc < doc {
            if !edits[i].tombstone {
                out.push(edits[i].doc);
            }
            i += 1;
        }
        if i < edits.len() && edits[i].doc == doc {
            if !edits[i].tombstone {
                out.push(doc);
            }
            i += 1;
        } else {
            out.push(doc);
        }
    }
    r.finish()?;
    while i < edits.len() {
        if !edits[i].tombstone {
            out.push(edits[i].doc);
        }
        i += 1;
    }
    Ok(out)
}

/// Como [`apply_docs`], com tf.
pub fn apply_postings(base: Option<&[u8]>, edits: &[OverlayPost]) -> Result<Vec<Posting>, Error> {
    if edits.is_empty() {
        return match base {
            Some(bytes) => decode_postings(bytes),
            None => Ok(Vec::new()),
        };
    }
    let Some(bytes) = base else {
        return Ok(edits.iter().filter_map(live_post).collect());
    };
    let header = ListHeader::parse_clean(bytes, false)?;
    let mut r = Reader { buf: bytes, pos: header.body };
    let mut prev = 0u64;
    let mut out = Vec::with_capacity(header.n as usize);
    let mut i = 0;
    for _ in 0..header.n {
        let doc = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let head = r.varint()?;
        let tf = u32::try_from(head >> 1).map_err(|_| corrupt("tf estourou u32"))?;
        let title_tf = if head & 1 == 1 { r.u32()? } else { 0 };
        if tf == 0 || title_tf > tf {
            return Err(corrupt("tf inválido na lista"));
        }
        prev = doc;
        let posting = Posting { doc, tf, title_tf };
        while i < edits.len() && edits[i].doc < doc {
            if let Some(p) = live_post(&edits[i]) {
                out.push(p);
            }
            i += 1;
        }
        if i < edits.len() && edits[i].doc == doc {
            if let Some(p) = live_post(&edits[i]) {
                out.push(p);
            }
            i += 1;
        } else {
            out.push(posting);
        }
    }
    r.finish()?;
    while i < edits.len() {
        if let Some(p) = live_post(&edits[i]) {
            out.push(p);
        }
        i += 1;
    }
    Ok(out)
}

/// Posições da base com o remendo aplicado.
pub fn apply_positions(base: Option<&[u8]>, edits: &[PosEdit]) -> Result<Vec<(DocId, Vec<u32>)>, Error> {
    if edits.is_empty() {
        return match base {
            Some(bytes) => decode_positions(bytes),
            None => Ok(Vec::new()),
        };
    }
    let base = match base {
        Some(bytes) => decode_positions(bytes)?,
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(base.len());
    let mut i = 0;
    for (doc, positions) in base {
        while i < edits.len() && edits[i].doc < doc {
            if let Some(p) = &edits[i].positions {
                out.push((edits[i].doc, p.clone()));
            }
            i += 1;
        }
        if i < edits.len() && edits[i].doc == doc {
            if let Some(p) = &edits[i].positions {
                out.push((doc, p.clone()));
            }
            i += 1;
        } else {
            out.push((doc, positions));
        }
    }
    while i < edits.len() {
        if let Some(p) = &edits[i].positions {
            out.push((edits[i].doc, p.clone()));
        }
        i += 1;
    }
    Ok(out)
}

fn live_post(edit: &OverlayPost) -> Option<Posting> {
    if edit.tombstone {
        None
    } else {
        Some(Posting { doc: edit.doc, tf: edit.tf, title_tf: edit.title_tf })
    }
}

fn split_delta(bytes: &[u8]) -> Result<(i64, &[u8]), Error> {
    let mut r = Reader::new(bytes);
    let delta = zigzag_decode(r.varint()?);
    Ok((delta, &bytes[r.pos..]))
}

fn read_post_entries(list: &[u8]) -> Result<Vec<OverlayPost>, Error> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let header = ListHeader::parse(list, false)?;
    let mut r = Reader { buf: list, pos: header.body };
    let mut prev = 0u64;
    let mut out = Vec::with_capacity(header.n as usize);
    for _ in 0..header.n {
        let doc = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let head = r.varint()?;
        let tombstone = head == 0;
        let (tf, title_tf) = if tombstone {
            (0, 0)
        } else {
            let tf = u32::try_from(head >> 1).map_err(|_| corrupt("tf estourou u32"))?;
            let title_tf = if head & 1 == 1 { r.u32()? } else { 0 };
            if tf == 0 || title_tf > tf {
                return Err(corrupt("tf inválido no remendo"));
            }
            (tf, title_tf)
        };
        prev = doc;
        out.push(OverlayPost { doc, tombstone, tf, title_tf });
    }
    r.finish()?;
    Ok(out)
}

fn read_pos_entries(list: &[u8]) -> Result<Vec<PosEdit>, Error> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let header = ListHeader::parse(list, false)?;
    let mut r = Reader { buf: list, pos: header.body };
    let mut prev = 0u64;
    let mut out = Vec::with_capacity(header.n as usize);
    for _ in 0..header.n {
        let doc = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let k = r.varint()?;
        let positions = if k == 0 {
            None
        } else {
            let mut positions = Vec::with_capacity(k as usize);
            let mut p = 0u64;
            for i in 0..k {
                let delta = r.varint()?;
                if i > 0 && delta == 0 {
                    return Err(corrupt("posições repetidas no remendo"));
                }
                p += delta;
                positions.push(u32::try_from(p).map_err(|_| corrupt("posição estourou u32"))?);
            }
            Some(positions)
        };
        prev = doc;
        out.push(PosEdit { doc, positions });
    }
    r.finish()?;
    Ok(out)
}

fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn zigzag_decode(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}
