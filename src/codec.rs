//! Serialização compacta dos valores gravados no RocksDB (formato v5).
//!
//! Inteiros usam varint (LEB128): números pequenos ocupam 1–2 bytes em vez de
//! 8. Listas são ordenadas por documento e gravam a diferença (delta) entre IDs
//! consecutivos, então documentos próximos custam ~1 byte cada.
//!
//! | Chave | Valor |
//! |---|---|
//! | `node:<id>` | flags, `n`, `n × (caractere, filho)` |
//! | `post:<nó>` | `n`, saltos, `n × (Δdoc, tf << 1 \| tem título, [tf do título])` |
//! | `pos:<nó>` | `n`, saltos, `n × (Δdoc, k, k × Δposição)` (a primeira posição é absoluta) |
//!
//! **Saltos (v5):** listas com mais de `2 × SKIP_BLOCK` entradas guardam,
//! antes das entradas, uma tabela de blocos: `s`, `s × (Δúltimo doc, bytes,
//! entradas − 1)`. Listas menores gravam `s = 0`. As entradas são exatamente as
//! do v3 (os deltas continuam de um bloco para o outro), então decodificar a
//! lista inteira só pula a tabela; os cursores ([`PostingsCursor`],
//! [`PositionsCursor`]) a usam para ir direto ao bloco de um documento sem
//! decodificar os anteriores.
//!
//! O lote grava blocos de `SKIP_BLOCK` entradas, mas a tabela aceita blocos
//! de qualquer tamanho: assim o merge só concatena as tabelas dos pedaços (um
//! pedaço pequeno vira um bloco), sem percorrer a lista unida. No v4 os blocos
//! tinham tamanho fixo e o merge recalculava a tabela a cada leitura de uma
//! lista com pedaços pendentes (termo frequente antes de compactar: +43%).
//! | `norm:<bloco>` | `n`, `n × (Δdeslocamento, tamanho)` dos documentos do bloco |

use crate::{DocId, Error, Posting};

/// Nó da Trie, gravado em `node:<id>`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrieNode {
    /// Caractere -> ID do nó filho, em ordem de caractere (o que deixa o
    /// autocompletar em ordem alfabética).
    pub children: Children,
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
        // Cada filho ocupa pelo menos 2 bytes: limita a pré-alocação a dados reais.
        let mut children = Vec::with_capacity((n as usize).min(bytes.len() / 2));
        for _ in 0..n {
            let code = u32::try_from(r.varint()?).map_err(|_| corrupt("caractere inválido"))?;
            let ch = char::from_u32(code).ok_or_else(|| corrupt("caractere inválido"))?;
            if children.last().is_some_and(|&(prev, _)| prev >= ch) {
                return Err(corrupt("filhos fora de ordem"));
            }
            children.push((ch, r.varint()?));
        }
        r.finish()?;
        let children = Children(children);
        Ok(Self { children, is_end_of_word: flags & FLAG_END_OF_WORD != 0 })
    }
}

/// Filhos de um nó da Trie: um vetor ordenado por caractere.
///
/// Era um `BTreeMap`, que aloca um nó de ~200 bytes mesmo para um filho só, e
/// a maioria dos nós da Trie tem um filho só. Com o vetor, o cache de nós do
/// escritor e o das buscas guardam muito mais nós na mesma memória, e
/// decodificar um nó é uma alocação de 16 bytes por filho. A busca do filho é
/// binária; inserir no meio é O(n), mas só a raiz e os primeiros níveis têm
/// muitos filhos.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Children(Vec<(char, u64)>);

impl Children {
    #[inline]
    pub fn get(&self, ch: &char) -> Option<&u64> {
        match self.0.as_slice() {
            // A maioria dos nós tem um filho só.
            [(c, id)] => (c == ch).then_some(id),
            all => all.binary_search_by_key(ch, |e| e.0).ok().map(|i| &all[i].1),
        }
    }

    /// Insere ou troca o filho de `ch`; devolve o anterior.
    pub fn insert(&mut self, ch: char, id: u64) -> Option<u64> {
        match self.0.binary_search_by_key(&ch, |e| e.0) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, id)),
            Err(i) => {
                self.0.insert(i, (ch, id));
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Em ordem de caractere.
    pub fn iter(&self) -> ChildrenIter<'_> {
        self.0.iter().map(|(ch, id)| (ch, id))
    }
}

pub type ChildrenIter<'a> = std::iter::Map<std::slice::Iter<'a, (char, u64)>, fn(&'a (char, u64)) -> (&'a char, &'a u64)>;

impl<'a> IntoIterator for &'a Children {
    type Item = (&'a char, &'a u64);
    type IntoIter = ChildrenIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
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

/// Entradas por bloco da tabela de saltos.
pub const SKIP_BLOCK: usize = 128;

/// Grava `n`, a tabela de saltos (se a lista for grande) e as entradas.
/// `write` grava as entradas num buffer e anota, para cada bloco de
/// `SKIP_BLOCK` entradas (o último pode ser menor), o último documento e onde
/// o bloco termina (bytes desde o começo das entradas).
fn write_list(buf: &mut Vec<u8>, n: usize, write: impl FnOnce(&mut Vec<u8>, &mut Vec<(DocId, usize)>)) {
    put_varint(buf, n as u64);
    if n <= 2 * SKIP_BLOCK {
        // Lista pequena: sem tabela, e as entradas vão direto para `buf`.
        buf.push(0);
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
    put_skips(buf, &blocks);
    buf.extend_from_slice(&body);
}

/// Um bloco da tabela de saltos, como é gravado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkipEntry {
    /// Último documento do bloco (absoluto).
    last: DocId,
    /// Tamanho do bloco em bytes.
    bytes: usize,
    /// Entradas no bloco.
    count: u64,
}

/// `s`, depois `(Δúltimo doc, bytes, entradas − 1)` de cada bloco.
fn put_skips(buf: &mut Vec<u8>, blocks: &[SkipEntry]) {
    put_varint(buf, blocks.len() as u64);
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
struct Block {
    last: DocId,
    start: usize,
    first: u64,
}

/// Cabeçalho de uma lista: quantas entradas, a tabela de saltos e onde as
/// entradas começam.
struct ListHeader {
    n: u64,
    blocks: Vec<Block>,
    entries: Vec<SkipEntry>,
    body: usize,
}

impl ListHeader {
    /// `parse_skips = false` só pula a tabela (decodificação completa).
    fn parse(bytes: &[u8], parse_skips: bool) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let n = r.varint()?;
        let s = r.varint()?;
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
        Ok(Self { n, blocks, entries, body: r.pos })
    }
}

/// Quantas entradas a lista tem (o df do termo), sem decodificá-la.
pub fn list_len(bytes: &[u8]) -> Result<u64, Error> {
    Reader::new(bytes).varint()
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
    r: Reader<'a>,
    body: usize,
    n: u64,
    blocks: Vec<Block>,
    /// Bloco da próxima entrada a ler.
    block: usize,
    /// Entradas já lidas.
    read: u64,
    prev: DocId,
    /// A última entrada lida (a primeira com documento ≥ o último alvo).
    current: Option<Posting>,
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
    r: Reader<'a>,
    body: usize,
    n: u64,
    blocks: Vec<Block>,
    /// Bloco da próxima entrada a ler (a de `head` já foi lida).
    block: usize,
    remaining: u64,
    prev: DocId,
    /// Cabeçalho (doc, k) já lido, com as posições ainda por ler.
    head: Option<(DocId, u64)>,
}

impl<'a> PositionsCursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let ListHeader { n, blocks, body, .. } = ListHeader::parse(bytes, true)?;
        Ok(Self { r: Reader { buf: bytes, pos: body }, body, n, blocks, block: 0, remaining: n, prev: 0, head: None })
    }

    /// Se o bloco da próxima entrada termina antes de `target`, vai direto
    /// ao primeiro bloco que pode conter `target`.
    fn jump(&mut self, target: DocId) {
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
    fn next_doc(&mut self) -> Result<Option<DocId>, Error> {
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

// ------------------------------------------------------------------ merge

/// Uma lista codificada, pronta para o merge.
struct Scan {
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

// ------------------------------------------------------------------ norm:

/// Tamanhos dos documentos de um bloco de IDs (`norm:<id ÷ NORM_BLOCK_DOCS>`).
///
/// Para cada documento: o tamanho do documento inteiro e o do título (tokens
/// indexados, sem stopwords; o BM25F normaliza cada campo pelo seu), e a
/// extensão do título nas posições (stopwords inclusive): uma ocorrência de
/// frase que começa antes dela está no título.
///
/// No disco, só os documentos que existem: (Δdeslocamento, tamanho, título,
/// extensão) em ordem. Em memória o bloco é denso (um registro por
/// deslocamento, até o maior), para o ranking ler o tamanho de um documento
/// com um acesso direto: ele faz isso milhares de vezes por busca. Ocupa no
/// máximo 8 KB.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NormBlock {
    /// Tamanho por deslocamento; [`ABSENT`] onde não há documento.
    lens: Vec<u32>,
    /// (tamanho do título, extensão do título) por deslocamento.
    titles: Vec<(u16, u16)>,
    count: usize,
}

/// Os tamanhos de um documento.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DocLens {
    /// Tokens indexados do documento inteiro (título e texto).
    pub len: u32,
    /// Tokens indexados do título (já contados em `len`).
    pub title_len: u16,
    /// Posições ocupadas pelo título, stopwords inclusive.
    pub title_span: u16,
}

/// Documentos por bloco de `norm:`.
pub const NORM_BLOCK_DOCS: u64 = 1024;

/// Marca de "sem documento" no bloco denso. Nenhum documento tem 4 bilhões de tokens.
const ABSENT: u32 = u32::MAX;

impl NormBlock {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.count * 5);
        put_varint(&mut buf, self.count as u64);
        let mut prev = 0;
        for (off, l) in self.iter() {
            put_varint(&mut buf, u64::from(off - prev));
            put_varint(&mut buf, u64::from(l.len));
            put_varint(&mut buf, u64::from(l.title_len));
            put_varint(&mut buf, u64::from(l.title_span));
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
            let small = |v: u64| u16::try_from(v).map_err(|_| corrupt("tamanho de título inválido"));
            let title_len = small(r.varint()?)?;
            let title_span = small(r.varint()?)?;
            if len == ABSENT || u32::from(title_len) > len || title_len > title_span {
                return Err(corrupt("tamanho de documento inválido"));
            }
            block.set(off as u16, DocLens { len, title_len, title_span });
        }
        r.finish()?;
        Ok(block)
    }

    /// Tamanho do documento inteiro (título e texto).
    #[inline]
    pub fn get(&self, offset: u16) -> Option<u32> {
        self.get_lens(offset).map(|l| l.len)
    }

    /// Todos os tamanhos do documento.
    #[inline]
    pub fn get_lens(&self, offset: u16) -> Option<DocLens> {
        let i = usize::from(offset);
        match self.lens.get(i) {
            Some(&len) if len != ABSENT => {
                let (title_len, title_span) = self.titles[i];
                Some(DocLens { len, title_len, title_span })
            }
            _ => None,
        }
    }

    /// Grava os tamanhos e devolve os anteriores, se o documento já estava no bloco.
    pub fn set(&mut self, offset: u16, lens: DocLens) -> Option<DocLens> {
        debug_assert!(u64::from(offset) < NORM_BLOCK_DOCS);
        debug_assert!(lens.len != ABSENT && u32::from(lens.title_len) <= lens.len);
        let i = usize::from(offset);
        if i >= self.lens.len() {
            self.lens.resize(i + 1, ABSENT);
            self.titles.resize(i + 1, (0, 0));
        }
        let old = self.get_lens(offset);
        self.lens[i] = lens.len;
        self.titles[i] = (lens.title_len, lens.title_span);
        if old.is_none() {
            self.count += 1;
        }
        old
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// (deslocamento, tamanhos) em ordem crescente.
    pub fn iter(&self) -> impl Iterator<Item = (u16, DocLens)> + '_ {
        (0..self.lens.len()).filter_map(|i| self.get_lens(i as u16).map(|l| (i as u16, l)))
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
        node.children.insert('z', u64::MAX);
        node.children.insert('a', 1);
        node.children.insert('ç', 300);
        assert_eq!(node.children.insert('a', 2), Some(1), "troca devolve o anterior");
        assert_eq!(node.children.iter().map(|(c, _)| *c).collect::<String>(), "azç", "em ordem de caractere");
        assert_eq!(node.children.get(&'a'), Some(&2));
        assert_eq!(node.children.get(&'b'), None);
        let mut one = TrieNode::default();
        one.children.insert('x', 7);
        assert_eq!((one.children.get(&'x'), one.children.get(&'y')), (Some(&7), None));
        assert!(TrieNode::decode(&[0, 2, 0x62, 1, 0x61, 2]).is_err(), "filhos fora de ordem");
        assert_eq!(TrieNode::decode(&node.encode()).unwrap(), node);
        assert_eq!(TrieNode::decode(&TrieNode::default().encode()).unwrap(), TrieNode::default());
    }

    #[test]
    fn postings_roundtrip_and_size() {
        let list: Vec<Posting> = (1000..2000).map(|d| p(d, 1 + (d % 3) as u32)).collect();
        let bytes = encode_postings(&list);
        // ~1 byte por delta e por tf, mais a tabela de saltos (8 blocos, ~40 bytes).
        assert!(bytes.len() < 2050, "delta e tf pequenos devem ocupar ~1 byte cada: {}", bytes.len());
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
        // Sem título a entrada não cresce: n, s = 0, Δdoc 1 byte + tf 1 byte.
        assert_eq!(encode_postings(&[p(1, 1)]).len(), 4);
        assert_eq!(encode_postings(&[pt(1, 1, 1)]).len(), 5);
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
        assert!(NormBlock::decode(&[2, 3, 1, 0, 0, 0, 1, 0, 0]).is_err()); // repetido
        assert!(NormBlock::decode(&[1, 3, 1, 2, 2]).is_err()); // título maior que o documento
        assert!(NormBlock::decode(&[1, 3, 4, 2, 1]).is_err()); // título maior que a extensão dele
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
            let mut expected: std::collections::BTreeMap<DocId, Vec<u32>> = std::collections::BTreeMap::new();
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

    /// Gerador pequeno e determinístico para os testes.
    fn rng(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut x = seed;
        move |n| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        }
    }

    fn sample_list(n: usize, seed: u64) -> Vec<Posting> {
        let mut next = rng(seed);
        let mut doc = next(5);
        (0..n)
            .map(|_| {
                let span = if next(10) == 0 { 5000 } else { 30 };
                doc += 1 + next(span);
                let tf = 1 + next(4) as u32;
                Posting { doc, tf, title_tf: if next(5) == 0 { 1 } else { 0 } }
            })
            .collect()
    }

    #[test]
    fn skip_table_only_on_large_lists() {
        for n in [0, 1, 255, 256, 257, 1000, 5000] {
            let list = sample_list(n, n as u64 + 1);
            let bytes = encode_postings(&list);
            let header = ListHeader::parse(&bytes, true).unwrap();
            let expected = if n > 2 * SKIP_BLOCK { n.div_ceil(SKIP_BLOCK) } else { 0 };
            assert_eq!(header.blocks.len(), expected, "n = {n}");
            for (b, blk) in header.blocks.iter().enumerate() {
                assert_eq!(blk.first, (b * SKIP_BLOCK) as u64);
            }
            assert_eq!(list_len(&bytes).unwrap(), n as u64);
            assert_eq!(decode_postings(&bytes).unwrap(), list, "n = {n}");
            assert_eq!(decode_docs(&bytes).unwrap(), list.iter().map(|p| p.doc).collect::<Vec<_>>());
            // Cada bloco termina no documento certo.
            for (b, blk) in header.blocks.iter().enumerate() {
                assert_eq!(blk.last, list[((b + 1) * SKIP_BLOCK).min(n) - 1].doc);
            }
        }
    }

    #[test]
    fn postings_cursor_matches_a_reference_search() {
        for n in [0, 1, 200, 256, 257, 300, 1000, 5000] {
            let list = sample_list(n, 7 + n as u64);
            let bytes = encode_postings(&list);
            let max = list.last().map_or(10, |p| p.doc + 10);
            for seed in 0..20 {
                let mut next = rng(seed * 31 + 1);
                let mut cursor = PostingsCursor::new(&bytes).unwrap();
                assert_eq!(cursor.len(), n as u64);
                let mut target = 0;
                // Alvos crescentes, com passos pequenos e saltos grandes.
                while target <= max + 5 {
                    let got = cursor.seek(target).unwrap();
                    let want = list.iter().find(|p| p.doc >= target).copied();
                    assert_eq!(got, want, "n = {n}, alvo {target}");
                    target += if next(4) == 0 { next(max / 3 + 1) } else { next(40) };
                }
                // Repetir o mesmo alvo devolve a mesma entrada.
                if let Some(p) = list.last() {
                    let mut cursor = PostingsCursor::new(&bytes).unwrap();
                    assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p));
                    assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p));
                    assert_eq!(cursor.seek(p.doc + 1).unwrap(), None);
                }
            }
        }
    }

    #[test]
    fn positions_cursor_skips_blocks() {
        for n in [1, 256, 257, 1500] {
            let list = sample_list(n, 99 + n as u64);
            let entries: Vec<(DocId, Vec<u32>)> =
                list.iter().map(|p| (p.doc, (0..p.tf).map(|i| i * 3 + (p.doc % 7) as u32).collect())).collect();
            let bytes = pos_bytes(&entries.iter().map(|(d, ps)| (*d, ps.as_slice())).collect::<Vec<_>>());
            assert_eq!(decode_positions(&bytes).unwrap(), entries);
            let mut next = rng(5 + n as u64);
            let mut cursor = PositionsCursor::new(&bytes).unwrap();
            let mut out = Vec::new();
            let mut target = 0;
            let max = entries.last().unwrap().0 + 3;
            while target <= max {
                let found = cursor.seek(target, &mut out).unwrap();
                match entries.iter().find(|(d, _)| *d == target) {
                    Some((_, ps)) => {
                        assert!(found, "n = {n}, alvo {target}");
                        assert_eq!(&out, ps);
                    }
                    None => assert!(!found, "n = {n}, alvo {target}"),
                }
                // Às vezes um documento que existe, às vezes um salto.
                target = if next(2) == 0 {
                    entries.iter().map(|(d, _)| *d).find(|&d| d > target).unwrap_or(max + 1)
                } else {
                    target + 1 + next(max / 4 + 1)
                };
            }
        }
    }

    #[test]
    fn merging_large_lists_rebuilds_the_skip_table() {
        // Pedaços crescentes (caminho rápido) e sobrepostos (caminho completo),
        // atravessando o limite de 256 entradas.
        let full = sample_list(1200, 3);
        let pieces: Vec<&[Posting]> = vec![&full[..100], &full[100..250], &full[250..900], &full[900..]];
        let encoded: Vec<Vec<u8>> = pieces.iter().map(|p| encode_postings(p)).collect();
        let merged = merge_postings(encoded.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(decode_postings(&merged).unwrap(), full, "caminho rápido");
        // A tabela é a dos pedaços: 100 e 150 viram um bloco cada; os grandes
        // trazem os seus blocos de 128.
        let header = ListHeader::parse(&merged, true).unwrap();
        let counts: Vec<u64> = header.entries.iter().map(|e| e.count).collect();
        assert_eq!(&counts[..3], [100, 150, 128]);
        assert_eq!(counts.iter().sum::<u64>(), 1200);
        for (i, p) in full.iter().enumerate().step_by(7) {
            let mut cursor = PostingsCursor::new(&merged).unwrap();
            assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p), "entrada {i}");
        }
        let mut cursor = PostingsCursor::new(&merged).unwrap();
        for p in full.iter().step_by(13) {
            assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p));
        }
        // Unir de novo uma lista já unida com mais um pedaço continua certo.
        let extra = sample_list(300, 77).into_iter().map(|p| Posting { doc: p.doc + full.last().unwrap().doc, ..p }).collect::<Vec<_>>();
        let again = merge_postings([merged.as_slice(), encode_postings(&extra).as_slice()].into_iter()).unwrap();
        let mut want_all = full.clone();
        want_all.extend(extra.iter().copied());
        assert_eq!(decode_postings(&again).unwrap(), want_all);
        let mut cursor = PostingsCursor::new(&again).unwrap();
        assert_eq!(cursor.seek(extra[150].doc).unwrap(), Some(extra[150]));
        // Sobreposição: o pedaço mais novo troca o tf de documentos do meio.
        let newer: Vec<Posting> = full[500..520].iter().map(|p| Posting { tf: p.tf + 10, ..*p }).collect();
        let merged2 = merge_postings([merged.as_slice(), encode_postings(&newer).as_slice()].into_iter()).unwrap();
        let mut want = full.clone();
        want[500..520].copy_from_slice(&newer);
        assert_eq!(decode_postings(&merged2).unwrap(), want);
        assert_eq!(merged2, encode_postings(&want));
        // Posições também.
        let entries: Vec<(DocId, Vec<u32>)> = full.iter().map(|p| (p.doc, vec![1, 5])).collect();
        let pieces: Vec<Vec<u8>> = [&entries[..300], &entries[300..]]
            .iter()
            .map(|e| pos_bytes(&e.iter().map(|(d, ps)| (*d, ps.as_slice())).collect::<Vec<_>>()))
            .collect();
        let merged = merge_positions(pieces.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(decode_positions(&merged).unwrap(), entries);
        let mut cursor = PositionsCursor::new(&merged).unwrap();
        let mut out = Vec::new();
        assert!(cursor.seek(entries[700].0, &mut out).unwrap());
        assert_eq!(out, [1, 5]);
    }

    /// Com tabelas: unir em agrupamentos diferentes dá tabelas diferentes
    /// (blocos de tamanhos diferentes), mas o mesmo conteúdo, e os cursores
    /// acham tudo em qualquer uma delas.
    #[test]
    fn merge_with_skip_tables_is_associative_in_content() {
        let full = sample_list(3000, 11);
        let cuts = [0, 90, 400, 410, 1300, 2999, 3000];
        let pieces: Vec<Vec<u8>> = cuts.windows(2).map(|w| encode_postings(&full[w[0]..w[1]])).collect();
        let slices: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
        let all = merge_postings(slices.iter().copied()).unwrap();
        for split in 1..slices.len() {
            let left = merge_postings(slices[..split].iter().copied()).unwrap();
            let right = merge_postings(slices[split..].iter().copied()).unwrap();
            let grouped = merge_postings([left.as_slice(), right.as_slice()].into_iter()).unwrap();
            for list in [&all, &grouped] {
                assert_eq!(decode_postings(list).unwrap(), full, "split {split}");
                let mut cursor = PostingsCursor::new(list).unwrap();
                for p in full.iter().step_by(17) {
                    assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p), "split {split}");
                }
            }
        }
    }

    #[test]
    fn norm_block_roundtrip_and_updates() {
        let l = |len, title_len, title_span| DocLens { len, title_len, title_span };
        let mut block = NormBlock::default();
        assert_eq!(block.set(5, l(100, 3, 4)), None);
        assert_eq!(block.set(900, l(7, 0, 0)), None);
        assert_eq!(block.set(2, l(0, 0, 0)), None, "insere no meio");
        assert_eq!(block.set(5, l(40, 2, 5)), Some(l(100, 3, 4)), "reindexar devolve os tamanhos antigos");
        assert_eq!(block.iter().collect::<Vec<_>>(), [(2, l(0, 0, 0)), (5, l(40, 2, 5)), (900, l(7, 0, 0))]);
        let decoded = NormBlock::decode(&block.encode()).unwrap();
        assert_eq!(decoded, block);
        assert_eq!(decoded.get(5), Some(40));
        assert_eq!(decoded.get_lens(5), Some(l(40, 2, 5)));
        assert_eq!(decoded.get_lens(6), None);
        assert_eq!(decoded.get(6), None);
        assert_eq!(decoded.get(2), Some(0), "tamanho 0 existe");
        assert_eq!(decoded.get(800), None);
        assert_eq!(decoded.get(900), Some(7));
        assert_eq!(decoded.get(1023), None, "depois do último deslocamento");
        assert_eq!(decoded.len(), 3);
        assert!(NormBlock::decode(&NormBlock::default().encode()).unwrap().is_empty());
    }
}
