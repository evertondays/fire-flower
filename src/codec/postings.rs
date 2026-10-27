//! Listas de documentos (`post:<id>`): (documento, tf, tf no título), e o
//! cursor que salta blocos.

use crate::codec::list::Block;
use crate::codec::list::ListHeader;
use crate::codec::varint::corrupt;
use crate::Error;
use crate::codec::varint::Reader;
use crate::Posting;
use crate::codec::list::write_list;
use crate::codec::SKIP_BLOCK;
use crate::codec::put_varint;
use crate::DocId;

/// Grava em `buf` a lista `(doc, tf, tf no título)`, **ordenada por doc e
/// sem repetições**.
///
/// Cada entrada: `Δdoc`, `tf << 1 | tem título` e, só se tiver, o tf do
/// título. A maioria das entradas não tem o termo no título e não paga nada
/// a mais que no formato v2.
pub fn encode_postings_into(buf: &mut Vec<u8>, entries: &[(DocId, u32, u32)]) {
    debug_assert!(entries.windows(2).all(|w| w[0].0 < w[1].0));
    debug_assert!(entries.iter().all(|e| e.1 > 0 && e.2 <= e.1));
    let write = |out: &mut Vec<u8>, blocks: &mut Vec<(DocId, usize)>| {
        let start = out.len();
        let mut prev = 0;
        for (i, &(doc, tf, title_tf)) in entries.iter().enumerate() {
            put_varint(out, doc - prev);
            put_varint(out, (u64::from(tf) << 1) | u64::from(title_tf > 0));
            if title_tf > 0 {
                put_varint(out, u64::from(title_tf));
            }
            prev = doc;
            if (i + 1) % SKIP_BLOCK == 0 || i + 1 == entries.len() {
                blocks.push((doc, out.len() - start));
            }
        }
    };
    write_list(buf, entries.len(), write);
}

pub fn encode_postings(postings: &[Posting]) -> Vec<u8> {
    let entries: Vec<(DocId, u32, u32)> = postings.iter().map(|p| (p.doc, p.tf, p.title_tf)).collect();
    let mut buf = Vec::new();
    encode_postings_into(&mut buf, &entries);
    buf
}

/// Lê o tf de uma entrada e, se houver, o do título: `(tf, tf no título)`.
/// Sempre embutida: está no laço mais quente da decodificação de listas.
#[inline(always)]
pub(crate) fn read_tfs(r: &mut Reader<'_>) -> Result<(u32, u32), Error> {
    let packed = r.varint()?;
    let tf = u32::try_from(packed >> 1).map_err(|_| corrupt("tf estourou u32"))?;
    let title_tf = if packed & 1 == 1 { r.u32()? } else { 0 };
    if tf == 0 || title_tf > tf || (packed & 1 == 1 && title_tf == 0) {
        return Err(corrupt("tf inválido na lista"));
    }
    Ok((tf, title_tf))
}

/// Lista completa: documento e frequência do termo (total e no título).
pub fn decode_postings(bytes: &[u8]) -> Result<Vec<Posting>, Error> {
    let header = ListHeader::parse(bytes, false)?;
    let n = header.n;
    let mut r = Reader { buf: bytes, pos: header.body };
    // Cada entrada ocupa pelo menos 2 bytes: limita a pré-alocação a dados reais.
    let mut out = Vec::with_capacity((n as usize).min(bytes.len() / 2));
    let mut doc: DocId = 0;
    for _ in 0..n {
        doc = doc.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let (tf, title_tf) = read_tfs(&mut r)?;
        out.push(Posting { doc, tf, title_tf });
    }
    r.finish()?;
    Ok(out)
}

/// Só os IDs, pulando as frequências: é o que a busca sem ranking precisa.
pub fn decode_docs(bytes: &[u8]) -> Result<Vec<DocId>, Error> {
    let header = ListHeader::parse(bytes, false)?;
    let n = header.n;
    let mut r = Reader { buf: bytes, pos: header.body };
    let mut out = Vec::with_capacity((n as usize).min(bytes.len() / 2));
    let mut doc: DocId = 0;
    for _ in 0..n {
        doc = doc.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        if r.varint()? & 1 == 1 {
            r.skip_varints(1)?;
        }
        out.push(doc);
    }
    r.finish()?;
    Ok(out)
}

/// Percorre uma lista `post:` em ordem crescente de documento, pulando
/// blocos inteiros pela tabela de saltos: achar um documento numa lista de
/// 100 mil entradas decodifica no máximo um bloco (`SKIP_BLOCK` entradas).
pub struct PostingsCursor<'a> {
    pub(crate) r: Reader<'a>,
    pub(crate) body: usize,
    pub(crate) n: u64,
    pub(crate) blocks: Vec<Block>,
    /// Bloco da próxima entrada a ler.
    pub(crate) block: usize,
    /// Entradas já lidas.
    pub(crate) read: u64,
    pub(crate) prev: DocId,
    /// A última entrada lida (a primeira com documento ≥ o último alvo).
    pub(crate) current: Option<Posting>,
}

impl<'a> PostingsCursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let ListHeader { n, blocks, body, .. } = ListHeader::parse(bytes, true)?;
        Ok(Self { r: Reader { buf: bytes, pos: body }, body, n, blocks, block: 0, read: 0, prev: 0, current: None })
    }

    /// Quantas entradas a lista tem.
    pub fn len(&self) -> u64 {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// A primeira entrada com documento ≥ `target` (o alvo só pode crescer
    /// entre chamadas), ou `None` se a lista acabou antes.
    pub fn seek(&mut self, target: DocId) -> Result<Option<Posting>, Error> {
        if let Some(c) = self.current
            && c.doc >= target
        {
            return Ok(Some(c));
        }
        // O bloco da próxima entrada termina antes do alvo: salta.
        if self.block < self.blocks.len() && self.blocks[self.block].last < target {
            let b = self.block + self.blocks[self.block..].partition_point(|blk| blk.last < target);
            if b == self.blocks.len() {
                self.read = self.n;
                self.current = None;
                return Ok(None);
            }
            self.r.pos = self.body + self.blocks[b].start;
            self.prev = if b > 0 { self.blocks[b - 1].last } else { 0 };
            self.read = self.blocks[b].first;
            self.block = b;
        }
        while self.read < self.n {
            let doc = self.prev.checked_add(self.r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
            let (tf, title_tf) = read_tfs(&mut self.r)?;
            self.read += 1;
            self.prev = doc;
            if self.block + 1 < self.blocks.len() && self.read >= self.blocks[self.block + 1].first {
                self.block += 1;
            }
            if doc >= target {
                let p = Posting { doc, tf, title_tf };
                self.current = Some(p);
                return Ok(Some(p));
            }
        }
        self.current = None;
        Ok(None)
    }
}
