//! Nó da Trie (`node:<id>`): os filhos e se o caminho até ele é uma palavra.

use crate::codec::varint::corrupt;
use crate::codec::varint::Reader;
use crate::Error;
use crate::codec::put_varint;

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

pub(crate) const FLAG_END_OF_WORD: u8 = 1;

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
