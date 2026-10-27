//! Blocos de tamanhos dos documentos (`norm:<bloco>`), para o BM25.

use crate::Error;
use crate::codec::put_varint;
use crate::codec::varint::{Reader, corrupt};

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
    pub(crate) lens: Vec<u32>,
    /// (tamanho do título, extensão do título) por deslocamento.
    pub(crate) titles: Vec<(u16, u16)>,
    pub(crate) count: usize,
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
pub(crate) const ABSENT: u32 = u32::MAX;

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
