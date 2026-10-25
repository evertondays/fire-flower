//! Serialização compacta dos valores gravados no RocksDB.
//!
//! Inteiros usam varint (LEB128): IDs pequenos ocupam 1–2 bytes em vez de 8.
//! Listas de postagem são ordenadas e gravadas como deltas, então IDs de
//! documentos próximos custam ~1 byte cada.

use std::collections::BTreeMap;

use crate::Error;

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
        let mut r = Reader(bytes);
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

/// Codifica uma lista de IDs **ordenada e sem repetições**.
pub fn encode_postings(ids: &[u64]) -> Vec<u8> {
    debug_assert!(ids.windows(2).all(|w| w[0] < w[1]));
    let mut buf = Vec::with_capacity(1 + ids.len() * 2);
    put_varint(&mut buf, ids.len() as u64);
    let mut prev = 0;
    for &id in ids {
        put_varint(&mut buf, id - prev);
        prev = id;
    }
    buf
}

pub fn decode_postings(bytes: &[u8]) -> Result<Vec<u64>, Error> {
    let mut r = Reader(bytes);
    let n = r.varint()?;
    // Cada ID ocupa pelo menos 1 byte: limita a pré-alocação a dados reais.
    let mut ids = Vec::with_capacity((n as usize).min(bytes.len()));
    let mut prev: u64 = 0;
    for _ in 0..n {
        prev = prev.checked_add(r.varint()?).ok_or_else(|| corrupt("delta estourou u64"))?;
        ids.push(prev);
    }
    r.finish()?;
    Ok(ids)
}

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn get_varint(bytes: &[u8]) -> Result<u64, Error> {
    let mut r = Reader(bytes);
    let v = r.varint()?;
    r.finish()?;
    Ok(v)
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, Error> {
        let (&b, rest) = self.0.split_first().ok_or_else(|| corrupt("valor truncado"))?;
        self.0 = rest;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, Error> {
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

    fn finish(self) -> Result<(), Error> {
        if self.0.is_empty() { Ok(()) } else { Err(corrupt("bytes sobrando no valor")) }
    }
}

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let ids: Vec<u64> = (1000..2000).collect();
        let bytes = encode_postings(&ids);
        assert!(bytes.len() < 1010, "deltas de 1 devem ocupar ~1 byte cada");
        assert_eq!(decode_postings(&bytes).unwrap(), ids);
        assert_eq!(decode_postings(&encode_postings(&[0, u64::MAX])).unwrap(), [0, u64::MAX]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(TrieNode::decode(&[]).is_err());
        assert!(TrieNode::decode(&[0, 1, 0x61]).is_err()); // filho sem ID
        assert!(decode_postings(&[2, 1]).is_err()); // diz 2, tem 1
        assert!(decode_postings(&[0, 9]).is_err()); // sobra
        assert!(get_varint(&[0xff; 11]).is_err());
    }
}
