//! Listas de posições (`pos:<id>`) e o cursor que salta blocos.

use crate::{DocId, Error};
use crate::codec::{SKIP_BLOCK, put_varint};
use crate::codec::list::{Block, ListHeader, write_list};
use crate::codec::varint::{Reader, corrupt};

/// Grava em `buf` as posições de cada documento. `entries` traz
/// `(doc, tf, tf no título)` ordenado por doc (só o tf é usado); `positions`
/// traz as posições de todos, em sequência (`tf` posições crescentes por
/// documento).
pub fn encode_positions_into(buf: &mut Vec<u8>, entries: &[(DocId, u32, u32)], positions: &[u32]) {
    debug_assert_eq!(entries.iter().map(|e| e.1 as usize).sum::<usize>(), positions.len());
    let mut at = 0usize;
    encode_positions_with(buf, entries.len(), entries.iter().map(|&(doc, tf, _)| {
        let slice = &positions[at..at + tf as usize];
        at += tf as usize;
        (doc, slice)
    }));
}

pub(crate) fn encode_positions_with<'p>(buf: &mut Vec<u8>, n: usize, entries: impl Iterator<Item = (DocId, &'p [u32])>) {
    let write = |out: &mut Vec<u8>, blocks: &mut Vec<(DocId, usize)>| {
        let start = out.len();
        let mut prev = 0;
        for (i, (doc, positions)) in entries.enumerate() {
            debug_assert!(doc > prev || prev == 0);
            debug_assert!(!positions.is_empty() && positions.windows(2).all(|w| w[0] < w[1]));
            put_varint(out, doc - prev);
            put_varint(out, positions.len() as u64);
            let mut last = 0;
            for &p in positions {
                put_varint(out, u64::from(p - last));
                last = p;
            }
            prev = doc;
            if (i + 1) % SKIP_BLOCK == 0 || i + 1 == n {
                blocks.push((doc, out.len() - start));
            }
        }
    };
    write_list(buf, n, write);
}

/// Lista completa de posições (diagnóstico e testes; a busca usa o cursor).
pub fn decode_positions(bytes: &[u8]) -> Result<Vec<(DocId, Vec<u32>)>, Error> {
    let mut cursor = PositionsCursor::new(bytes)?;
    let mut out = Vec::new();
    while let Some(doc) = cursor.next_doc()? {
        let mut positions = Vec::new();
        cursor.read_positions(&mut positions)?;
        out.push((doc, positions));
    }
    cursor.r.finish()?;
    Ok(out)
}

/// Percorre uma lista `pos:` sem decodificá-la inteira: salta pela tabela de
/// saltos até o bloco do documento e, dentro do bloco, pula as posições dos
/// documentos que não interessam byte a byte.
pub struct PositionsCursor<'a> {
    pub(crate) r: Reader<'a>,
    pub(crate) body: usize,
    pub(crate) n: u64,
    pub(crate) blocks: Vec<Block>,
    /// Bloco da próxima entrada a ler (a de `head` já foi lida).
    pub(crate) block: usize,
    pub(crate) remaining: u64,
    pub(crate) prev: DocId,
    /// Cabeçalho (doc, k) já lido, com as posições ainda por ler.
    pub(crate) head: Option<(DocId, u64)>,
}

impl<'a> PositionsCursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let ListHeader { n, blocks, body, .. } = ListHeader::parse(bytes, true)?;
        Ok(Self { r: Reader { buf: bytes, pos: body }, body, n, blocks, block: 0, remaining: n, prev: 0, head: None })
    }

    /// Se o bloco da próxima entrada termina antes de `target`, vai direto
    /// ao primeiro bloco que pode conter `target`.
    pub(crate) fn jump(&mut self, target: DocId) {
        if self.blocks.is_empty() || self.remaining == 0 {
            return;
        }
        // Se `head` é o alvo ou além, ou o bloco da próxima entrada ainda pode
        // ter o alvo, fica. (O bloco é acompanhado a cada entrada lida: uma
        // busca binária aqui, por documento conferido, deixava frases de
        // termos frequentes 2× mais lentas no p99.)
        let block = self.block;
        if self.head.is_some_and(|(doc, _)| doc >= target) || self.blocks[block].last >= target {
            return;
        }
        let b = block + self.blocks[block..].partition_point(|blk| blk.last < target);
        self.head = None;
        if b == self.blocks.len() {
            self.remaining = 0;
            return;
        }
        self.r.pos = self.body + self.blocks[b].start;
        self.prev = if b > 0 { self.blocks[b - 1].last } else { 0 };
        self.remaining = self.n - self.blocks[b].first;
        self.block = b;
    }

    /// Próximo documento da lista (sem consumir as posições dele).
    pub(crate) fn next_doc(&mut self) -> Result<Option<DocId>, Error> {
        if let Some((_, k)) = self.head.take() {
            self.r.skip_varints(k)?;
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        if self.block + 1 < self.blocks.len() && self.n - self.remaining >= self.blocks[self.block + 1].first {
            self.block += 1;
        }
        let doc = self.prev.checked_add(self.r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let k = self.r.varint()?;
        if k == 0 {
            return Err(corrupt("documento sem posições"));
        }
        self.prev = doc;
        self.head = Some((doc, k));
        Ok(Some(doc))
    }

    pub(crate) fn read_positions(&mut self, out: &mut Vec<u32>) -> Result<(), Error> {
        let (_, k) = self.head.take().ok_or_else(|| corrupt("posições sem documento"))?;
        out.clear();
        let mut p: u64 = 0;
        for i in 0..k {
            let delta = self.r.varint()?;
            if i > 0 && delta == 0 {
                return Err(corrupt("posições repetidas"));
            }
            p += delta;
            out.push(u32::try_from(p).map_err(|_| corrupt("posição estourou u32"))?);
        }
        Ok(())
    }

    /// Avança até `target` (que só pode crescer entre chamadas). Se o
    /// documento estiver na lista, escreve as posições dele em `out`.
    pub fn seek(&mut self, target: DocId, out: &mut Vec<u32>) -> Result<bool, Error> {
        self.jump(target);
        loop {
            let doc = match self.head {
                Some((doc, _)) => doc,
                None => match self.next_doc()? {
                    Some(doc) => doc,
                    None => return Ok(false),
                },
            };
            if doc > target {
                return Ok(false);
            }
            if doc == target {
                self.read_positions(out)?;
                return Ok(true);
            }
            let (_, k) = self.head.take().unwrap_or_default();
            self.r.skip_varints(k)?;
        }
    }
}
