//! Diagnóstico e medição: usados pelo harness de performance (`examples/wiki.rs`).

use crate::engine::view::Source;
use crate::Result;
use crate::engine::view::missing_node;
use crate::codec::TrieNode;
use crate::engine::keys::ROOT_ID;
use crate::engine::view::load_node;
use crate::codec::NORM_BLOCK_DOCS;
use crate::codec::NormBlock;
use crate::Error;
use crate::engine::keys::NORM;
use crate::codec;
use crate::engine::keys::POS;
use crate::engine::keys::Key;
use crate::Posting;
use crate::DocId;
use crate::analysis::normalize_term;
use crate::engine::keys::META_NEXT_NODE;
use crate::engine::meta::read_counter;
use crate::Config;
use crate::Engine;

/// Diagnóstico e medição: usados pelo harness de performance (`examples/wiki.rs`).
impl Engine {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Estatísticas internas do RocksDB, se `Config::statistics` estiver ligado.
    pub fn rocksdb_stats(&self) -> Option<String> {
        self.opts.get_statistics()
    }

    /// Uma propriedade do RocksDB, como `rocksdb.stats`,
    /// `rocksdb.estimate-num-keys` ou `rocksdb.total-sst-files-size`.
    pub fn property(&self, name: &str) -> Option<String> {
        self.db.property_value(name).ok().flatten()
    }

    /// Nós da Trie já criados (o próximo ID livre).
    pub fn trie_node_count(&self) -> Result<u64> {
        Ok(read_counter(&self.db, META_NEXT_NODE)?.unwrap_or(0))
    }

    /// ID do nó terminal de `term` (também é o ID da lista em `post:<id>`).
    pub fn term_node(&self, term: &str) -> Result<Option<u64>> {
        let Some(term) = normalize_term(term) else { return Ok(None) };
        self.view().term_node(&term)
    }

    /// IDs da lista de postagem pelo ID do nó terminal (ver [`Engine::for_each_term`]).
    pub fn postings_of_node(&self, node_id: u64) -> Result<Vec<DocId>> {
        self.view().load_docs(node_id)
    }

    /// Lista de postagem com tf, pelo ID do nó terminal.
    pub fn term_postings_of_node(&self, node_id: u64) -> Result<Vec<Posting>> {
        self.view().load_postings(node_id)
    }

    /// Posições do termo em cada documento, pelo ID do nó terminal. Vazio
    /// num banco sem posições.
    pub fn positions_of_node(&self, node_id: u64) -> Result<Vec<(DocId, Vec<u32>)>> {
        match self.view().snap.get_bytes(Key::new(POS, node_id).as_ref())? {
            Some(v) => codec::decode_positions(&v),
            None => Ok(Vec::new()),
        }
    }

    /// Percorre os tamanhos de todos os documentos (em ordem de bloco):
    /// `f(documento, tamanho inteiro, tamanho do título)`.
    pub fn for_each_doc_len(&self, mut f: impl FnMut(DocId, u32, u32)) -> Result<()> {
        let mode = rocksdb::IteratorMode::From(NORM, rocksdb::Direction::Forward);
        for item in self.db.iterator(mode) {
            let (key, value) = item?;
            let Some(block) = key.strip_prefix(NORM) else { break };
            let block: u64 = std::str::from_utf8(block)
                .ok()
                .and_then(|b| b.parse().ok())
                .ok_or_else(|| Error::Corrupt("chave norm: inválida".into()))?;
            for (offset, l) in NormBlock::decode(&value)?.iter() {
                f(block * NORM_BLOCK_DOCS + u64::from(offset), l.len, u32::from(l.title_len));
            }
        }
        Ok(())
    }

    /// Percorre todos os termos em ordem alfabética (busca em profundidade
    /// sobre um snapshot). `f` recebe o termo e o ID do nó terminal.
    pub fn for_each_term(&self, mut f: impl FnMut(&str, u64)) -> Result<()> {
        // Lê direto do snapshot: percorrer tudo não deve encher o cache das buscas.
        let view = self.view();
        let Some(root) = load_node(&view.snap, ROOT_ID)? else { return Ok(()) };
        let mut stack: Vec<(String, TrieNode)> = vec![(String::new(), root)];
        let mut ids = vec![ROOT_ID];
        while let (Some((word, node)), Some(id)) = (stack.pop(), ids.pop()) {
            if node.is_end_of_word {
                f(&word, id);
            }
            // Empilha ao contrário para visitar os filhos em ordem alfabética.
            for (&ch, &child) in node.children.iter().rev() {
                let mut next = word.clone();
                next.push(ch);
                stack.push((next, load_node(&view.snap, child)?.ok_or_else(|| missing_node(child))?));
                ids.push(child);
            }
        }
        Ok(())
    }

    /// Percorre todas as chaves e valores do banco, em ordem de chave.
    /// Valores de `post:` e `pos:` já vêm com os merges pendentes aplicados.
    pub fn for_each_raw(&self, mut f: impl FnMut(&[u8], &[u8])) -> Result<()> {
        for item in self.db.iterator(rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            f(&key, &value);
        }
        Ok(())
    }
}
