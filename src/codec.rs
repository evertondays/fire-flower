//! Serialização compacta dos valores gravados no RocksDB (formato v2).
//!
//! Inteiros usam varint (LEB128): números pequenos ocupam 1–2 bytes em vez de
//! 8. Listas são ordenadas por documento e gravam a diferença (delta) entre IDs
//! consecutivos, então documentos próximos custam ~1 byte cada.
//!
//! | Chave | Valor |
//! |---|---|
//! | `node:<id>` | flags, `n`, `n × (caractere, filho)` |
//! | `post:<nó>` | `n`, `n × (Δdoc, tf)` |
//! | `pos:<nó>` | `n`, `n × (Δdoc, k, k × Δposição)` (a primeira posição é absoluta) |
//! | `norm:<bloco>` | `n`, `n × (Δdeslocamento, tamanho)` dos documentos do bloco |

use std::collections::BTreeMap;

use crate::{DocId, Error, Posting};

/// Nó da Trie, gravado em `node:<id>`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrieNode {
    /// Caractere -> ID do nó filho. `BTreeMap` mantém os filhos em ordem,
    /// o que deixa o autocompletar em ordem alfabética.
    pub children: BTreeMap<char, u64>,
    /// O caminho da raiz até aqui forma uma palavra indexada. Nesse caso a
    /// lista de postagem fica em `post:<id deste nó>`.
    pub is_end_of_word: bool,
}

const FLAG_END_OF_WORD: u8 = 1;

impl TrieNode {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.children.len() * 4);
        buf.push(if self.is_end_of_word { FLAG_END_OF_WORD } else { 0 });
        put_varint(&mut buf, self.children.len() as u64);
        for (&ch, &id) in &self.children {
            put_varint(&mut buf, ch as u64);
            put_varint(&mut buf, id);
        }
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let flags = r.byte()?;
        let n = r.varint()?;
        let mut children = BTreeMap::new();
        for _ in 0..n {
            let code = u32::try_from(r.varint()?).map_err(|_| corrupt("caractere inválido"))?;
            let ch = char::from_u32(code).ok_or_else(|| corrupt("caractere inválido"))?;
            children.insert(ch, r.varint()?);
        }
        r.finish()?;
        Ok(Self { children, is_end_of_word: flags & FLAG_END_OF_WORD != 0 })
    }
}

// ------------------------------------------------------------------ post:

/// Grava em `buf` a lista `(doc, tf, tf no título)`, **ordenada por doc e
/// sem repetições**.
///
/// Cada entrada: `Δdoc`, `tf << 1 | tem título` e, só se tiver, o tf do
/// título. A maioria das entradas não tem o termo no título e não paga nada
/// a mais que no formato v2.
pub fn encode_postings_into(buf: &mut Vec<u8>, entries: &[(DocId, u32, u32)]) {
    debug_assert!(entries.windows(2).all(|w| w[0].0 < w[1].0));
    debug_assert!(entries.iter().all(|e| e.1 > 0 && e.2 <= e.1));
    buf.reserve(1 + entries.len() * 3);
    put_varint(buf, entries.len() as u64);
    let mut prev = 0;
    for &(doc, tf, title_tf) in entries {
        put_varint(buf, doc - prev);
        put_varint(buf, (u64::from(tf) << 1) | u64::from(title_tf > 0));
        if title_tf > 0 {
            put_varint(buf, u64::from(title_tf));
        }
        prev = doc;
    }
}

pub fn encode_postings(postings: &[Posting]) -> Vec<u8> {
    let entries: Vec<(DocId, u32, u32)> = postings.iter().map(|p| (p.doc, p.tf, p.title_tf)).collect();
    let mut buf = Vec::new();
    encode_postings_into(&mut buf, &entries);
    buf
}

/// Lê o tf de uma entrada e, se houver, o do título: `(tf, tf no título)`.
#[inline]
fn read_tfs(r: &mut Reader<'_>) -> Result<(u32, u32), Error> {
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
    let mut r = Reader::new(bytes);
    let n = r.varint()?;
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
    let mut r = Reader::new(bytes);
    let n = r.varint()?;
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

// ------------------------------------------------------------------ pos:

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

fn encode_positions_with<'p>(buf: &mut Vec<u8>, n: usize, entries: impl Iterator<Item = (DocId, &'p [u32])>) {
    put_varint(buf, n as u64);
    let mut prev = 0;
    for (doc, positions) in entries {
        debug_assert!(doc > prev || prev == 0);
        debug_assert!(!positions.is_empty() && positions.windows(2).all(|w| w[0] < w[1]));
        put_varint(buf, doc - prev);
        put_varint(buf, positions.len() as u64);
        let mut last = 0;
        for &p in positions {
            put_varint(buf, u64::from(p - last));
            last = p;
        }
        prev = doc;
    }
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

/// Percorre uma lista `pos:` sem decodificá-la inteira: as posições dos
/// documentos que não interessam são puladas byte a byte.
pub struct PositionsCursor<'a> {
    r: Reader<'a>,
    remaining: u64,
    prev: DocId,
    /// Cabeçalho (doc, k) já lido, com as posições ainda por ler.
    head: Option<(DocId, u64)>,
}

impl<'a> PositionsCursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let remaining = r.varint()?;
        Ok(Self { r, remaining, prev: 0, head: None })
    }

    /// Próximo documento da lista (sem consumir as posições dele).
    fn next_doc(&mut self) -> Result<Option<DocId>, Error> {
        if let Some((_, k)) = self.head.take() {
            self.r.skip_varints(k)?;
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let doc = self.prev.checked_add(self.r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        let k = self.r.varint()?;
        if k == 0 {
            return Err(corrupt("documento sem posições"));
        }
        self.prev = doc;
        self.head = Some((doc, k));
        Ok(Some(doc))
    }

    fn read_positions(&mut self, out: &mut Vec<u32>) -> Result<(), Error> {
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

// ------------------------------------------------------------------ merge

/// Uma lista codificada, já percorrida e validada.
struct Scan {
    n: u64,
    first: DocId,
    last: DocId,
    /// Onde começa a primeira entrada (depois do `n`).
    body: usize,
    /// Onde termina o Δdoc da primeira entrada.
    after_first_delta: usize,
}

fn scan(bytes: &[u8], positions: bool) -> Result<Scan, Error> {
    let mut r = Reader::new(bytes);
    let n = r.varint()?;
    let body = r.pos;
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
    Ok(Scan { n, first, last: doc, body, after_first_delta })
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

fn merge_lists<'a>(inputs: impl Iterator<Item = &'a [u8]>, positions: bool) -> Option<Vec<u8>> {
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
        let mut out = Vec::with_capacity(10 + lists.iter().map(|(b, _)| b.len()).sum::<usize>());
        put_varint(&mut out, total);
        let mut prev_last = 0;
        for (i, (bytes, s)) in lists.iter().enumerate() {
            if i == 0 {
                out.extend_from_slice(&bytes[s.body..]);
            } else {
                put_varint(&mut out, s.first - prev_last);
                out.extend_from_slice(&bytes[s.after_first_delta..]);
            }
            prev_last = s.last;
        }
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

// ------------------------------------------------------------------ norm:

/// Tamanhos dos documentos de um bloco de IDs (`norm:<id ÷ NORM_BLOCK_DOCS>`):
/// o do documento inteiro (título e texto) e o do título.
///
/// No disco, só os documentos que existem: (Δdeslocamento, tamanho, tamanho
/// do título) em ordem. Em memória o bloco é denso (um par por deslocamento,
/// até o maior), para o ranking ler o tamanho de um documento com um acesso
/// direto: ele faz isso milhares de vezes por busca. Ocupa no máximo 6 KB.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NormBlock {
    /// Tamanho por deslocamento; [`ABSENT`] onde não há documento.
    lens: Vec<u32>,
    /// Tamanho do título por deslocamento (0 onde não há documento).
    title_lens: Vec<u16>,
    count: usize,
}

/// Documentos por bloco de `norm:`.
pub const NORM_BLOCK_DOCS: u64 = 1024;

/// Marca de "sem documento" no bloco denso. Nenhum documento tem 4 bilhões de tokens.
const ABSENT: u32 = u32::MAX;

impl NormBlock {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.count * 4);
        put_varint(&mut buf, self.count as u64);
        let mut prev = 0;
        for (off, len, title_len) in self.iter() {
            put_varint(&mut buf, u64::from(off - prev));
            put_varint(&mut buf, u64::from(len));
            put_varint(&mut buf, u64::from(title_len));
            prev = off;
        }
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let n = r.varint()?;
        if n > NORM_BLOCK_DOCS {
            return Err(corrupt("bloco de tamanhos grande demais"));
        }
        let mut block = Self::default();
        let mut off = 0u64;
        for i in 0..n {
            let delta = r.varint()?;
            if i > 0 && delta == 0 {
                return Err(corrupt("documento repetido no bloco de tamanhos"));
            }
            off += delta;
            if off >= NORM_BLOCK_DOCS {
                return Err(corrupt("documento fora do bloco de tamanhos"));
            }
            let len = r.u32()?;
            let title_len = u16::try_from(r.varint()?).map_err(|_| corrupt("tamanho de título inválido"))?;
            if len == ABSENT || u32::from(title_len) > len {
                return Err(corrupt("tamanho de documento inválido"));
            }
            block.set(off as u16, len, title_len);
        }
        r.finish()?;
        Ok(block)
    }

    /// Tamanho do documento inteiro (título e texto).
    #[inline]
    pub fn get(&self, offset: u16) -> Option<u32> {
        match self.lens.get(usize::from(offset)) {
            Some(&len) if len != ABSENT => Some(len),
            _ => None,
        }
    }

    /// (tamanho do documento inteiro, tamanho do título).
    #[inline]
    pub fn get_both(&self, offset: u16) -> Option<(u32, u16)> {
        let i = usize::from(offset);
        match self.lens.get(i) {
            Some(&len) if len != ABSENT => Some((len, self.title_lens[i])),
            _ => None,
        }
    }

    /// Grava os tamanhos e devolve os anteriores, se o documento já estava no bloco.
    pub fn set(&mut self, offset: u16, len: u32, title_len: u16) -> Option<(u32, u16)> {
        debug_assert!(u64::from(offset) < NORM_BLOCK_DOCS);
        debug_assert!(len != ABSENT && u32::from(title_len) <= len);
        let i = usize::from(offset);
        if i >= self.lens.len() {
            self.lens.resize(i + 1, ABSENT);
            self.title_lens.resize(i + 1, 0);
        }
        let old = std::mem::replace(&mut self.lens[i], len);
        let old_title = std::mem::replace(&mut self.title_lens[i], title_len);
        if old == ABSENT {
            self.count += 1;
            None
        } else {
            Some((old, old_title))
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// (deslocamento, tamanho, tamanho do título) em ordem crescente.
    pub fn iter(&self) -> impl Iterator<Item = (u16, u32, u16)> + '_ {
        self.lens
            .iter()
            .enumerate()
            .filter(|&(_, &len)| len != ABSENT)
            .map(|(off, &len)| (off as u16, len, self.title_lens[off]))
    }
}

// ------------------------------------------------------------------ varint

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn get_varint(bytes: &[u8]) -> Result<u64, Error> {
    let mut r = Reader::new(bytes);
    let v = r.varint()?;
    r.finish()?;
    Ok(v)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or_else(|| corrupt("valor truncado"))?;
        self.pos += 1;
        Ok(b)
    }

    /// A maioria dos valores (deltas, tf, posições) cabe em 1 byte.
    #[inline(always)]
    fn varint(&mut self) -> Result<u64, Error> {
        if let Some(&b) = self.buf.get(self.pos)
            && b < 0x80
        {
            self.pos += 1;
            return Ok(u64::from(b));
        }
        self.varint_slow()
    }

    #[cold]
    fn varint_slow(&mut self) -> Result<u64, Error> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("varint longo demais"))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        u32::try_from(self.varint()?).map_err(|_| corrupt("valor estourou u32"))
    }

    /// Pula `k` varints sem decodificar: conta os bytes de fim (bit alto zerado).
    #[inline]
    fn skip_varints(&mut self, mut k: u64) -> Result<(), Error> {
        while k > 0 {
            let b = *self.buf.get(self.pos).ok_or_else(|| corrupt("valor truncado"))?;
            self.pos += 1;
            k -= u64::from(b < 0x80);
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), Error> {
        if self.pos == self.buf.len() { Ok(()) } else { Err(corrupt("bytes sobrando no valor")) }
    }
}

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(doc: DocId, tf: u32) -> Posting {
        Posting::new(doc, tf)
    }

    fn pt(doc: DocId, tf: u32, title_tf: u32) -> Posting {
        Posting { doc, tf, title_tf }
    }

    fn pos_bytes(entries: &[(DocId, &[u32])]) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_positions_with(&mut buf, entries.len(), entries.iter().map(|&(d, ps)| (d, ps)));
        buf
    }

    #[test]
    fn node_roundtrip() {
        let mut node = TrieNode { is_end_of_word: true, ..Default::default() };
        node.children.insert('a', 1);
        node.children.insert('ç', 300);
        node.children.insert('z', u64::MAX);
        assert_eq!(TrieNode::decode(&node.encode()).unwrap(), node);
        assert_eq!(TrieNode::decode(&TrieNode::default().encode()).unwrap(), TrieNode::default());
    }

    #[test]
    fn postings_roundtrip_and_size() {
        let list: Vec<Posting> = (1000..2000).map(|d| p(d, 1 + (d % 3) as u32)).collect();
        let bytes = encode_postings(&list);
        assert!(bytes.len() < 2010, "delta e tf pequenos devem ocupar ~1 byte cada");
        assert_eq!(decode_postings(&bytes).unwrap(), list);
        assert_eq!(decode_docs(&bytes).unwrap(), (1000..2000).collect::<Vec<_>>());
        let edge = [p(0, 1), p(u64::MAX, u32::MAX)];
        assert_eq!(decode_postings(&encode_postings(&edge)).unwrap(), edge);
        assert_eq!(decode_docs(&encode_postings(&edge)).unwrap(), [0, u64::MAX]);
        assert!(decode_postings(&encode_postings(&[])).unwrap().is_empty());
    }

    #[test]
    fn title_tf_roundtrip_and_merge() {
        let list = [pt(1, 3, 1), p(2, 1), pt(5, 2, 2), p(9, 4), pt(u64::MAX, u32::MAX, u32::MAX)];
        let bytes = encode_postings(&list);
        assert_eq!(decode_postings(&bytes).unwrap(), list);
        assert_eq!(decode_docs(&bytes).unwrap(), [1, 2, 5, 9, u64::MAX]);
        // Sem título a entrada não cresce: Δdoc 1 byte + tf 1 byte.
        assert_eq!(encode_postings(&[p(1, 1)]).len(), 3);
        assert_eq!(encode_postings(&[pt(1, 1, 1)]).len(), 4);
        // Merge rápido (pedaços em ordem) e lento (sobrepostos), a última versão vence.
        let a = encode_postings(&[pt(1, 2, 1), p(4, 1)]);
        let b = encode_postings(&[pt(7, 1, 1)]);
        let fast = merge_postings([a.as_slice(), b.as_slice()].into_iter()).unwrap();
        assert_eq!(decode_postings(&fast).unwrap(), [pt(1, 2, 1), p(4, 1), pt(7, 1, 1)]);
        let c = encode_postings(&[p(1, 5), pt(4, 3, 2)]);
        let slow = merge_postings([a.as_slice(), b.as_slice(), c.as_slice()].into_iter()).unwrap();
        assert_eq!(decode_postings(&slow).unwrap(), [p(1, 5), pt(4, 3, 2), pt(7, 1, 1)]);
        // tf do título maior que o tf, ou marcado e zero: corrompido.
        assert!(decode_postings(&[1, 1, (1 << 1) | 1, 2]).is_err());
        assert!(decode_postings(&[1, 1, (1 << 1) | 1, 0]).is_err());
        assert!(decode_postings(&[1, 1, 0]).is_err(), "tf 0");
    }

    #[test]
    fn positions_roundtrip_and_cursor() {
        let entries: Vec<(DocId, &[u32])> = vec![(3, &[0, 5, 9]), (10, &[2]), (11, &[0, 1, 300, 70_000]), (500, &[u32::MAX])];
        let bytes = pos_bytes(&entries);
        let decoded = decode_positions(&bytes).unwrap();
        assert_eq!(decoded.len(), 4);
        for ((doc, ps), (d, got)) in entries.iter().zip(&decoded) {
            assert_eq!((doc, ps.to_vec()), (d, got.clone()));
        }
        // encode_positions_into (formato do lote) dá os mesmos bytes.
        let mut via_batch = Vec::new();
        let flat: Vec<u32> = entries.iter().flat_map(|(_, ps)| ps.iter().copied()).collect();
        let pairs: Vec<(DocId, u32, u32)> = entries.iter().map(|(d, ps)| (*d, ps.len() as u32, 0)).collect();
        encode_positions_into(&mut via_batch, &pairs, &flat);
        assert_eq!(via_batch, bytes);

        let mut cursor = PositionsCursor::new(&bytes).unwrap();
        let mut out = Vec::new();
        assert!(!cursor.seek(1, &mut out).unwrap());
        assert!(cursor.seek(3, &mut out).unwrap());
        assert_eq!(out, [0, 5, 9]);
        assert!(!cursor.seek(4, &mut out).unwrap(), "entre dois documentos");
        assert!(cursor.seek(11, &mut out).unwrap(), "pula o 10 sem decodificar");
        assert_eq!(out, [0, 1, 300, 70_000]);
        assert!(cursor.seek(500, &mut out).unwrap());
        assert_eq!(out, [u32::MAX]);
        assert!(!cursor.seek(501, &mut out).unwrap());
        assert!(!cursor.seek(u64::MAX, &mut out).unwrap());
    }

    #[test]
    fn rejects_garbage() {
        assert!(TrieNode::decode(&[]).is_err());
        assert!(TrieNode::decode(&[0, 1, 0x61]).is_err()); // filho sem ID
        assert!(decode_postings(&[2, 1, 1]).is_err()); // diz 2, tem 1
        assert!(decode_postings(&[1, 1]).is_err()); // falta o tf
        assert!(decode_postings(&[0, 9]).is_err()); // sobra
        assert!(decode_docs(&[1, 1]).is_err());
        assert!(decode_postings(&[1, 1, 0x80, 0x80, 0x80, 0x80, 0x20]).is_err()); // tf > u32 (o tf vai deslocado 1 bit)
        assert!(get_varint(&[0xff; 11]).is_err());
        assert!(decode_positions(&[1, 1, 2, 5]).is_err()); // diz 2 posições, tem 1
        assert!(decode_positions(&[1, 1, 0]).is_err()); // documento sem posições
        assert!(decode_positions(&[1, 1, 2, 5, 0]).is_err()); // posição repetida
        assert!(decode_positions(&[1, 1, 1, 5, 7]).is_err()); // sobra
        assert!(NormBlock::decode(&[1, 0x80, 0x08, 1]).is_err()); // deslocamento 1024
        assert!(NormBlock::decode(&[2, 3, 1, 0, 0, 1, 0]).is_err()); // repetido
        assert!(NormBlock::decode(&[1, 3, 1, 2]).is_err()); // título maior que o documento
        assert!(merge_postings([&[2u8, 1][..]].into_iter()).is_none());
        assert!(merge_positions([&[1u8, 1, 2, 5][..]].into_iter()).is_none());
    }

    fn merged(inputs: &[Vec<Posting>]) -> Vec<Posting> {
        let encoded: Vec<Vec<u8>> = inputs.iter().map(|l| encode_postings(l)).collect();
        decode_postings(&merge_postings(encoded.iter().map(Vec::as_slice)).unwrap()).unwrap()
    }

    #[test]
    fn merge_postings_newest_wins() {
        // Sem sobreposição: concatenação (caminho rápido).
        assert_eq!(merged(&[vec![p(1, 1), p(5, 2)], vec![p(6, 1)], vec![], vec![p(300, 4)]]), [p(1, 1), p(5, 2), p(6, 1), p(300, 4)]);
        // Com sobreposição: o pedaço mais novo vence no mesmo documento.
        assert_eq!(merged(&[vec![p(1, 1), p(5, 2)], vec![p(2, 1), p(5, 7)]]), [p(1, 1), p(2, 1), p(5, 7)]);
        assert_eq!(merged(&[vec![p(9, 1)], vec![p(1, 3)]]), [p(1, 3), p(9, 1)]);
        assert_eq!(merged(&[vec![p(4, 1)], vec![p(4, 2)], vec![p(4, 3)]]), [p(4, 3)]);
        assert!(merged(&[vec![], vec![]]).is_empty());
        assert!(merged(&[]).is_empty());
    }

    /// Unir em qualquer agrupamento dá o mesmo resultado (o RocksDB faz
    /// merges parciais de pedaços consecutivos, na ordem).
    #[test]
    fn merge_is_associative() {
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for _ in 0..300 {
            let pieces: Vec<Vec<(DocId, Vec<u32>)>> = (0..1 + next(5))
                .map(|_| {
                    let mut docs: Vec<DocId> = (0..next(6)).map(|_| next(20)).collect();
                    docs.sort_unstable();
                    docs.dedup();
                    docs.into_iter()
                        .map(|d| {
                            let mut ps: Vec<u32> = (0..1 + next(4)).map(|_| next(50) as u32).collect();
                            ps.sort_unstable();
                            ps.dedup();
                            (d, ps)
                        })
                        .collect()
                })
                .collect();
            // Resultado esperado: mapa doc -> (tf, posições) com o mais novo vencendo.
            let mut expected: BTreeMap<DocId, Vec<u32>> = BTreeMap::new();
            for piece in &pieces {
                for (d, ps) in piece {
                    expected.insert(*d, ps.clone());
                }
            }
            let post: Vec<Vec<u8>> = pieces
                .iter()
                .map(|l| encode_postings(&l.iter().map(|(d, ps)| p(*d, ps.len() as u32)).collect::<Vec<_>>()))
                .collect();
            let pos: Vec<Vec<u8>> = pieces
                .iter()
                .map(|l| pos_bytes(&l.iter().map(|(d, ps)| (*d, ps.as_slice())).collect::<Vec<_>>()))
                .collect();
            let all_post = merge_postings(post.iter().map(Vec::as_slice)).unwrap();
            let all_pos = merge_positions(pos.iter().map(Vec::as_slice)).unwrap();
            let want_post: Vec<Posting> = expected.iter().map(|(d, ps)| p(*d, ps.len() as u32)).collect();
            let want_pos: Vec<(DocId, Vec<u32>)> = expected.clone().into_iter().collect();
            assert_eq!(decode_postings(&all_post).unwrap(), want_post);
            assert_eq!(decode_positions(&all_pos).unwrap(), want_pos);
            // Agrupando em dois: primeiro os `split` mais antigos, depois o resto.
            let split = next(pieces.len() as u64 + 1) as usize;
            for (lists, full, positions) in [(&post, &all_post, false), (&pos, &all_pos, true)] {
                let slices: Vec<&[u8]> = lists.iter().map(Vec::as_slice).collect();
                let left = merge_lists(slices[..split].iter().copied(), positions).unwrap();
                let right = merge_lists(slices[split..].iter().copied(), positions).unwrap();
                let again = merge_lists([left.as_slice(), right.as_slice()].into_iter(), positions).unwrap();
                assert_eq!(&again, full);
            }
        }
    }

    #[test]
    fn norm_block_roundtrip_and_updates() {
        let mut block = NormBlock::default();
        assert_eq!(block.set(5, 100, 3), None);
        assert_eq!(block.set(900, 7, 0), None);
        assert_eq!(block.set(2, 0, 0), None, "insere no meio");
        assert_eq!(block.set(5, 40, 2), Some((100, 3)), "reindexar devolve os tamanhos antigos");
        assert_eq!(block.iter().collect::<Vec<_>>(), [(2, 0, 0), (5, 40, 2), (900, 7, 0)]);
        let decoded = NormBlock::decode(&block.encode()).unwrap();
        assert_eq!(decoded, block);
        assert_eq!(decoded.get(5), Some(40));
        assert_eq!(decoded.get_both(5), Some((40, 2)));
        assert_eq!(decoded.get_both(6), None);
        assert_eq!(decoded.get(6), None);
        assert_eq!(decoded.get(2), Some(0), "tamanho 0 existe");
        assert_eq!(decoded.get(800), None);
        assert_eq!(decoded.get(900), Some(7));
        assert_eq!(decoded.get(1023), None, "depois do último deslocamento");
        assert_eq!(decoded.len(), 3);
        assert!(NormBlock::decode(&NormBlock::default().encode()).unwrap().is_empty());
    }
}
