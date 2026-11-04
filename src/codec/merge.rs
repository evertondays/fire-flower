//! Merge operator das listas: une pedaços gravados em lotes diferentes, com
//! lápides para os pares (termo, documento) que saíram do índice.
//!
//! Regra: por documento, a entrada mais nova vence; uma lápide é uma entrada
//! que diz "este documento não está mais aqui". O merge **parcial** (só
//! operandos; pode haver valor mais antigo em outro nível do RocksDB) guarda
//! as lápides. O merge **completo** (com o valor base, ou com a certeza de
//! que não há base: leituras e compactação no último nível) as descarta. Toda
//! leitura passa pelo merge completo, então nenhuma busca vê uma lápide.

use crate::{DocId, Error};
use crate::codec::{SKIP_BLOCK, put_varint};
use crate::codec::list::{ListHeader, SkipEntry, put_skips, write_list};
use crate::codec::varint::{Reader, corrupt};

/// O que vem depois do Δdoc em cada entrada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// `post:`: `tf << 1 | tem título` e, se tiver, o tf do título. Lápide: `0`.
    Postings,
    /// `pos:`: `k` e `k` posições. Lápide: `k = 0`.
    Positions,
}

/// Grava uma lista só de lápides (os documentos que saíram de um termo),
/// pronta para ser um operando de merge em `post:` ou `pos:` (nos dois
/// formatos a lápide é o mesmo byte `0` depois do Δdoc). `docs` em ordem
/// crescente e sem repetição.
pub fn encode_tombstones_into(buf: &mut Vec<u8>, docs: &[DocId]) {
    debug_assert!(docs.windows(2).all(|w| w[0] < w[1]));
    let write = |out: &mut Vec<u8>, blocks: &mut Vec<(DocId, usize)>| {
        let start = out.len();
        let mut prev = 0;
        for (i, &doc) in docs.iter().enumerate() {
            put_varint(out, doc - prev);
            out.push(0);
            prev = doc;
            if (i + 1) % SKIP_BLOCK == 0 || i + 1 == docs.len() {
                blocks.push((doc, out.len() - start));
            }
        }
    };
    write_list(buf, docs.len(), !docs.is_empty(), write);
}

/// Merge completo de `post:` (`inputs` do mais antigo para o mais novo).
/// `None` = dado corrompido (a leitura falha em vez de mentir).
pub fn merge_postings<'a>(inputs: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    merge_lists(inputs, ListKind::Postings, true)
}

/// Merge completo de `pos:`, com a mesma regra de [`merge_postings`].
pub fn merge_positions<'a>(inputs: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    merge_lists(inputs, ListKind::Positions, true)
}

/// Une `inputs` (do mais antigo para o mais novo). `full` descarta as
/// lápides (merge completo); senão elas ficam (merge parcial).
///
/// A **base** é o maior trecho inicial de pedaços em que cada um começa
/// depois do fim do anterior (a carga, com IDs crescentes, grava assim); o
/// **remendo** é o resto (edições, reindexação), unido de uma vez só por
/// documento. Os blocos da base que o remendo não toca são copiados com um
/// `memcpy` (só o delta da primeira entrada de cada trecho é refeito), e só
/// os tocados são relidos e regravados. Sem remendo, isso é a concatenação dos
/// pedaços; sem base (o primeiro pedaço tem lápides que o merge completo
/// precisa tirar), tudo é relido e regravado.
///
/// Nenhum caminho decodifica o conteúdo das entradas (tf, posições): os bytes
/// de cada entrada são copiados como estão.
pub fn merge_lists<'a>(inputs: impl Iterator<Item = &'a [u8]>, kind: ListKind, full: bool) -> Option<Vec<u8>> {
    let mut lists: Vec<Scan<'a>> = Vec::new();
    for bytes in inputs {
        let s = Scan::new(bytes, kind).ok()?;
        if s.header.n > 0 {
            lists.push(s);
        }
    }
    let (bytes, blocks) = (lists.iter().map(|s| s.bytes.len()).sum(), lists.iter().map(|s| s.table().len()).sum());
    let mut out = Out::new(full, bytes, blocks);
    // Uma lista com lápides só entra na base num merge parcial: blocos
    // copiados levam as lápides junto.
    let usable = |s: &Scan<'_>| !(full && s.header.tombstones);
    let mut base = lists.iter().take_while(|s| usable(s)).count().min(1);
    while base > 0 && base < lists.len() && lists[base - 1].last < lists[base].first && usable(&lists[base]) {
        base += 1;
    }
    let overlay = newest_entries(&lists[base..], kind).ok()?;
    if base == 0 {
        for e in &overlay {
            out.push(e);
        }
    } else {
        apply_patch(&lists[..base], &overlay, &mut out).ok()?;
    }
    Some(out.finish())
}

/// Uma lista codificada, pronta para o merge.
struct Scan<'a> {
    bytes: &'a [u8],
    kind: ListKind,
    header: ListHeader,
    first: DocId,
    last: DocId,
    /// Lista pequena (sem tabela de saltos): ela inteira, como um bloco só.
    small: Option<SkipEntry>,
}

impl<'a> Scan<'a> {
    /// Lista grande: o último documento vem da tabela e o primeiro da
    /// primeira entrada, sem percorrer o resto (o merge na leitura de um
    /// termo frequente com pedaços pendentes fazia isso a cada leitura).
    /// Lista pequena: percorre e valida.
    fn new(bytes: &'a [u8], kind: ListKind) -> Result<Self, Error> {
        let header = ListHeader::parse(bytes, true)?;
        let body = header.body;
        let mut s = Self { bytes, kind, header, first: 0, last: 0, small: None };
        if s.header.n == 0 {
            Reader { buf: bytes, pos: body }.finish()?;
        } else if let Some(last) = s.header.entries.last() {
            s.first = Reader { buf: bytes, pos: body }.varint()?;
            s.last = last.last;
        } else {
            let mut entries = s.entries(body, 0, s.header.n);
            let first = entries.next_entry()?.expect("a lista tem entradas").doc;
            let mut last = first;
            while let Some(e) = entries.next_entry()? {
                last = e.doc;
            }
            entries.r.finish()?;
            (s.first, s.last) = (first, last);
            s.small = Some(SkipEntry { last, bytes: bytes.len() - body, count: s.header.n });
        }
        Ok(s)
    }

    /// Os blocos da lista (uma lista pequena é um bloco só).
    fn table(&self) -> &[SkipEntry] {
        match &self.small {
            Some(whole) => std::slice::from_ref(whole),
            None => &self.header.entries,
        }
    }

    /// Onde o bloco `i` começa em `bytes`.
    fn start(&self, i: usize) -> usize {
        self.header.body + self.header.blocks.get(i).map_or(0, |b| b.start)
    }

    /// `count` entradas a partir de `pos`, com `prev` como documento anterior.
    fn entries(&self, pos: usize, prev: DocId, count: u64) -> Entries<'a> {
        let r = Reader { buf: self.bytes, pos };
        Entries { r, prev, left: count, kind: self.kind, tombstones: self.header.tombstones }
    }

    /// Documento anterior ao bloco `i` (o último do bloco de antes).
    fn prev_of(&self, i: usize) -> DocId {
        if i > 0 { self.table()[i - 1].last } else { 0 }
    }

    /// Copia os blocos `range` para `out` de uma vez (os bytes são contíguos).
    fn copy_blocks(&self, range: std::ops::Range<usize>, out: &mut Out) -> Result<(), Error> {
        if range.is_empty() {
            return Ok(());
        }
        let table = self.table();
        let start = self.start(range.start);
        let end = self.start(range.end - 1) + table[range.end - 1].bytes;
        out.copy_blocks(&self.bytes[start..end], self.prev_of(range.start), &table[range], self.header.tombstones)
    }
}

/// Uma entrada sem decodificar o conteúdo: o documento e os bytes depois do Δdoc.
struct Entry<'a> {
    doc: DocId,
    payload: &'a [u8],
    tombstone: bool,
}

/// Percorre as entradas de uma lista sem decodificar tf nem posições.
struct Entries<'a> {
    r: Reader<'a>,
    prev: DocId,
    left: u64,
    kind: ListKind,
    /// A lista diz que pode ter lápides.
    tombstones: bool,
}

impl<'a> Entries<'a> {
    fn next_entry(&mut self) -> Result<Option<Entry<'a>>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        let doc = self.prev.checked_add(self.r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let start = self.r.pos;
        let head = self.r.varint()?;
        let tombstone = head == 0;
        match self.kind {
            ListKind::Postings if head == 1 => return Err(corrupt("tf inválido na lista")),
            ListKind::Postings if head & 1 == 1 => self.r.skip_varints(1)?,
            ListKind::Positions => self.r.skip_varints(head)?,
            ListKind::Postings => {}
        }
        if tombstone && !self.tombstones {
            return Err(corrupt("lápide numa lista que não diz ter lápides"));
        }
        self.prev = doc;
        Ok(Some(Entry { doc, payload: &self.r.buf[start..self.r.pos], tombstone }))
    }
}

/// As entradas de `lists` unidas por documento, com a mais nova vencendo
/// (lápides inclusive), em ordem de documento.
fn newest_entries<'a>(lists: &[Scan<'a>], kind: ListKind) -> Result<Vec<Entry<'a>>, Error> {
    let mut all: Vec<Entry<'a>> = Vec::with_capacity(lists.iter().map(|s| s.header.n as usize).sum());
    for s in lists {
        debug_assert_eq!(s.kind, kind);
        let mut entries = s.entries(s.header.body, 0, s.header.n);
        while let Some(e) = entries.next_entry()? {
            all.push(e);
        }
        entries.r.finish()?;
    }
    if lists.len() > 1 {
        // Estável: trechos já ordenados são só intercalados, e entre
        // entradas do mesmo documento fica a ordem dos pedaços.
        all.sort_by_key(|e| e.doc);
        let mut kept: Vec<Entry<'a>> = Vec::with_capacity(all.len());
        for e in all {
            match kept.last_mut() {
                Some(last) if last.doc == e.doc => *last = e,
                _ => kept.push(e),
            }
        }
        all = kept;
    }
    Ok(all)
}

/// Aplica `overlay` (mais novo, em ordem de documento) sobre `base` (listas
/// em sequência, cada uma depois do fim da anterior): os blocos sem documento
/// do remendo são copiados; os outros, relidos e regravados com as entradas
/// do remendo no lugar.
fn apply_patch(base: &[Scan<'_>], overlay: &[Entry<'_>], out: &mut Out) -> Result<(), Error> {
    let mut at = 0;
    for list in base {
        // Blocos sem documento do remendo desde `untouched`: copiados juntos.
        let mut untouched = 0;
        for (i, blk) in list.table().iter().enumerate() {
            let end = at + overlay[at..].partition_point(|e| e.doc <= blk.last);
            if end == at {
                continue;
            }
            list.copy_blocks(untouched..i, out)?;
            untouched = i + 1;
            let start = list.start(i);
            let mut entries = list.entries(start, list.prev_of(i), blk.count);
            let mut current = entries.next_entry()?;
            while current.is_some() || at < end {
                let take_overlay = match (&current, overlay.get(at).filter(|_| at < end)) {
                    (Some(c), Some(o)) => o.doc <= c.doc,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if take_overlay {
                    let o = &overlay[at];
                    if current.as_ref().is_some_and(|c| c.doc == o.doc) {
                        current = entries.next_entry()?;
                    }
                    out.push(o);
                    at += 1;
                } else {
                    let c = current.take().expect("há uma entrada da base");
                    out.push(&c);
                    current = entries.next_entry()?;
                }
            }
            if entries.r.pos != start + blk.bytes {
                return Err(corrupt("bloco da tabela de saltos não bate com as entradas"));
            }
            out.close_block();
        }
        list.copy_blocks(untouched..list.table().len(), out)?;
    }
    for e in &overlay[at..] {
        out.push(e);
    }
    Ok(())
}

/// A lista sendo montada pelo merge: as entradas, os blocos da tabela de
/// saltos e o bloco ainda aberto (entradas regravadas).
struct Out {
    /// Merge completo: lápides são descartadas.
    full: bool,
    body: Vec<u8>,
    blocks: Vec<SkipEntry>,
    prev: DocId,
    n: u64,
    tombstones: bool,
    open_start: usize,
    open_count: u64,
}

impl Out {
    /// `bytes` e `blocks`: o que os pedaços somam, quase sempre o tamanho
    /// final (sem isso, copiar uma lista de 145 mil entradas realocava várias
    /// vezes e o caminho rápido ficava 30% mais lento que no v5).
    fn new(full: bool, bytes: usize, blocks: usize) -> Self {
        Self {
            full,
            body: Vec::with_capacity(bytes),
            blocks: Vec::with_capacity(blocks),
            prev: 0,
            n: 0,
            tombstones: false,
            open_start: 0,
            open_count: 0,
        }
    }

    /// Regrava uma entrada (no merge completo, uma lápide some).
    fn push(&mut self, e: &Entry<'_>) {
        if e.tombstone && self.full {
            return;
        }
        debug_assert!(self.n == 0 || e.doc > self.prev);
        put_varint(&mut self.body, e.doc - self.prev);
        self.body.extend_from_slice(e.payload);
        self.prev = e.doc;
        self.n += 1;
        self.tombstones |= e.tombstone;
        self.open_count += 1;
        if self.open_count == SKIP_BLOCK as u64 {
            self.close_block();
        }
    }

    fn close_block(&mut self) {
        if self.open_count > 0 {
            let bytes = self.body.len() - self.open_start;
            self.blocks.push(SkipEntry { last: self.prev, bytes, count: self.open_count });
            self.open_start = self.body.len();
            self.open_count = 0;
        }
    }

    /// Copia blocos consecutivos de outra lista com um `memcpy` só. `bytes`
    /// começa com o Δdoc da primeira entrada em relação a `prev` (o documento
    /// anterior a esses blocos lá); só esse delta é refeito, e o primeiro bloco
    /// muda de tamanho. `tombstones` é a marca da lista de origem: as lápides
    /// vêm junto, então só se copiam blocos com lápides num merge parcial (ver
    /// `merge_lists`).
    fn copy_blocks(&mut self, bytes: &[u8], prev: DocId, blocks: &[SkipEntry], tombstones: bool) -> Result<(), Error> {
        debug_assert!(!(self.full && tombstones));
        self.close_block();
        let start = self.body.len();
        let mut r = Reader::new(bytes);
        let first = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        if self.n > 0 && first <= self.prev {
            return Err(corrupt("blocos fora de ordem no merge"));
        }
        put_varint(&mut self.body, first - self.prev);
        self.body.extend_from_slice(&bytes[r.pos..]);
        let grown = (self.body.len() - start) as isize - bytes.len() as isize;
        let first_block = self.blocks.len();
        self.blocks.extend_from_slice(blocks);
        self.blocks[first_block].bytes = (blocks[0].bytes as isize + grown) as usize;
        self.n += blocks.iter().map(|b| b.count).sum::<u64>();
        self.open_start = self.body.len();
        self.prev = blocks.last().expect("ao menos um bloco").last;
        self.tombstones |= tombstones;
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        self.close_block();
        let mut out = Vec::with_capacity(16 + self.blocks.len() * 5 + self.body.len());
        put_varint(&mut out, self.n);
        let blocks: &[SkipEntry] = if self.n as usize > 2 * SKIP_BLOCK { &self.blocks } else { &[] };
        put_skips(&mut out, blocks, self.tombstones);
        out.extend_from_slice(&self.body);
        out
    }
}
