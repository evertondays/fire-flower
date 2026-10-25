//! Camadas de índice (Trie) e armazenamento (RocksDB).
//!
//! Espaços de chave:
//! - `node:<id>`  -> [`TrieNode`] serializado (`node:0` é a raiz)
//! - `post:<id>`  -> lista de postagem do termo que termina no nó `<id>`
//! - `doc:<id>`   -> rótulo opcional do documento (ex.: caminho do arquivo)
//! - `meta:*`     -> contadores de ID

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rocksdb::{BlockBasedOptions, Cache, DB, DBCompressionType, MergeOperands, Options, WriteBatch};

use crate::codec::{self, TrieNode};
use crate::text::{index_terms, normalize_term, parse_query};
use crate::{DocId, Error, Result};

pub const ROOT_ID: u64 = 0;
const META_NEXT_NODE: &[u8] = b"meta:next_node_id";
const META_NEXT_DOC: &[u8] = b"meta:next_doc_id";

fn node_key(id: u64) -> String {
    format!("node:{id}")
}
fn post_key(id: u64) -> String {
    format!("post:{id}")
}
fn doc_key(id: DocId) -> String {
    format!("doc:{id}")
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Memória do block cache. Mantém a raiz e os primeiros níveis da Trie,
    /// que todas as buscas atravessam, sempre em RAM.
    pub block_cache_bytes: usize,
    /// Bits por chave do filtro de Bloom. 10 ≈ 1% de falsos positivos.
    pub bloom_bits_per_key: f64,
    /// Quantos termos uma busca por prefixo (`comp*`) expande no máximo.
    /// Termos além disso são ignorados; prefixos curtos podem casar milhares.
    pub max_prefix_terms: usize,
    /// Liga as estatísticas internas do RocksDB ([`Engine::rocksdb_stats`]).
    /// Custa um pouco de CPU; use em medições, não em produção.
    pub statistics: bool,
    /// Quantos nós da Trie o escritor mantém em memória entre um lote e
    /// outro (~200 bytes cada). Sem isso, todo lote relê do banco a raiz e os
    /// nós dos termos comuns. Passando do limite, saem primeiro os nós que o
    /// último lote não usou e depois os mais fundos. 0 desliga o cache.
    pub writer_cache_nodes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block_cache_bytes: 256 << 20,
            bloom_bits_per_key: 10.0,
            max_prefix_terms: 256,
            statistics: false,
            writer_cache_nodes: 1_000_000,
        }
    }
}

/// Motor de busca. É `Sync`: compartilhe via `Arc<Engine>`. Buscas rodam em
/// paralelo entre si e com a indexação; só um [`IndexBatch`] existe por vez.
pub struct Engine {
    db: DB,
    writer: Mutex<WriterState>,
    config: Config,
    /// Guardadas para ler as estatísticas depois do `open`.
    opts: Options,
}

struct WriterState {
    next_node_id: u64,
    next_doc_id: DocId,
    /// Nós da Trie dos lotes anteriores. Só o escritor altera a Trie, então
    /// depois de um commit o cache é igual ao banco. O lote o pega emprestado
    /// e só o devolve no commit: um lote descartado leva o cache junto, e
    /// nada que não foi gravado sobrevive.
    cache: NodeCache,
}

#[derive(Default)]
struct NodeCache {
    nodes: HashMap<u64, CachedNode>,
    /// Número do último lote gravado. Cada nó guarda o lote que o usou por último.
    generation: u64,
}

impl Engine {
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let mut table = BlockBasedOptions::default();
        table.set_bloom_filter(config.bloom_bits_per_key, false);
        table.set_block_cache(&Cache::new_lru_cache(config.block_cache_bytes));
        table.set_cache_index_and_filter_blocks(true);
        table.set_pin_l0_filter_and_index_blocks_in_cache(true);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_block_based_table_factory(&table);
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.set_merge_operator_associative("fire_flower_postings_union", merge_postings);
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        opts.increase_parallelism(cpus as i32);
        if config.statistics {
            opts.enable_statistics();
            opts.set_statistics_level(rocksdb::statistics::StatsLevel::ExceptTimers);
        }

        let db = DB::open(&opts, path)?;
        let next_node_id = read_counter(&db, META_NEXT_NODE)?.unwrap_or(ROOT_ID + 1);
        let next_doc_id = read_counter(&db, META_NEXT_DOC)?.unwrap_or(1);
        Ok(Self { db, writer: Mutex::new(WriterState { next_node_id, next_doc_id, cache: NodeCache::default() }), config, opts })
    }

    /// Inicia um lote de indexação. Bloqueia se outro lote estiver aberto
    /// (inclusive na mesma thread: não chame `index_document` com um lote aberto).
    pub fn batch(&self) -> IndexBatch<'_> {
        // Os contadores só mudam após um commit bem-sucedido, então o estado
        // continua válido mesmo se outra thread entrou em pânico com o lock.
        let mut state = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let cache = std::mem::take(&mut state.cache);
        IndexBatch {
            db: &self.db,
            next_node_id: state.next_node_id,
            next_doc_id: state.next_doc_id,
            state,
            nodes: cache.nodes,
            generation: cache.generation + 1,
            cache_limit: self.config.writer_cache_nodes,
            dirty: Vec::new(),
            pending: HashMap::new(),
            labels: Vec::new(),
            documents: 0,
        }
    }

    /// Atalho para indexar um único documento. Para muitos documentos, use [`Engine::batch`].
    pub fn index_document(&self, doc_id: DocId, text: &str) -> Result<()> {
        let mut batch = self.batch();
        batch.add_document(doc_id, text)?;
        batch.commit()
    }

    /// Busca full-text. Termos separados por espaço são combinados com E
    /// (o documento precisa conter todos). `termo*` casa por prefixo.
    /// Retorna os IDs dos documentos em ordem crescente.
    pub fn search(&self, query: &str) -> Result<Vec<DocId>> {
        self.view().search(query)
    }

    /// Documentos que contêm exatamente `term` (normalizado aqui).
    pub fn postings(&self, term: &str) -> Result<Vec<DocId>> {
        let Some(term) = normalize_term(term) else { return Ok(Vec::new()) };
        self.view().postings(&term)
    }

    /// Autocompletar: até `limit` palavras indexadas que começam com `prefix`,
    /// das mais curtas para as mais longas e em ordem alfabética.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let Some(prefix) = normalize_term(prefix) else { return Ok(Vec::new()) };
        let words = self.view().words_with_prefix(&prefix, limit)?;
        Ok(words.into_iter().map(|(word, _)| word).collect())
    }

    /// Compacta o banco inteiro: une os pedaços pendentes das listas de
    /// postagem e descarta versões antigas. Num índice grande leva minutos.
    pub fn compact(&self) {
        self.db.compact_range::<&[u8], &[u8]>(None, None);
    }

    /// Rótulo gravado com [`IndexBatch::add_labeled`] ou [`IndexBatch::set_label`].
    pub fn label(&self, doc_id: DocId) -> Result<Option<String>> {
        Ok(self
            .db
            .get_pinned(doc_key(doc_id))?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Uma busca faz várias leituras (um `Get` por caractere, mais as listas).
    /// O snapshot garante que todas vejam o mesmo momento do índice, mesmo que
    /// um commit aconteça no meio.
    fn view(&self) -> View<'_> {
        View { snap: self.db.snapshot(), config: &self.config }
    }
}

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
        Ok(match self.view().find_node(&term)? {
            Some((id, node)) if node.is_end_of_word => Some(id),
            _ => None,
        })
    }

    /// Lista de postagem pelo ID do nó terminal (ver [`Engine::for_each_term`]).
    pub fn postings_of_node(&self, node_id: u64) -> Result<Vec<DocId>> {
        self.view().load_postings(node_id)
    }

    /// Percorre todos os termos em ordem alfabética (busca em profundidade
    /// sobre um snapshot). `f` recebe o termo e o ID do nó terminal.
    pub fn for_each_term(&self, mut f: impl FnMut(&str, u64)) -> Result<()> {
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
                stack.push((next, view.require_node(child)?));
                ids.push(child);
            }
        }
        Ok(())
    }

    /// Percorre todas as chaves e valores do banco, em ordem de chave.
    /// Valores de `post:` já vêm com os merges pendentes aplicados.
    pub fn for_each_raw(&self, mut f: impl FnMut(&[u8], &[u8])) -> Result<()> {
        for item in self.db.iterator(rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            f(&key, &value);
        }
        Ok(())
    }
}

type Snapshot<'a> = rocksdb::SnapshotWithThreadMode<'a, DB>;

/// Leituras de uma única busca, todas sobre o mesmo snapshot.
struct View<'a> {
    snap: Snapshot<'a>,
    config: &'a Config,
}

impl View<'_> {
    fn search(&self, query: &str) -> Result<Vec<DocId>> {
        let mut lists: Vec<Vec<DocId>> = Vec::new();
        for term in parse_query(query) {
            let list = if term.prefix { self.prefix_postings(&term.text)? } else { self.postings(&term.text)? };
            if list.is_empty() {
                return Ok(Vec::new());
            }
            lists.push(list);
        }

        // Interseção começando pela lista menor: o resultado só encolhe.
        lists.sort_by_key(Vec::len);
        let mut lists = lists.into_iter();
        let Some(mut result) = lists.next() else { return Ok(Vec::new()) };
        for list in lists {
            result = intersect(&result, &list);
            if result.is_empty() {
                break;
            }
        }
        Ok(result)
    }

    /// `term` já deve estar normalizado.
    fn postings(&self, term: &str) -> Result<Vec<DocId>> {
        match self.find_node(term)? {
            Some((id, node)) if node.is_end_of_word => self.load_postings(id),
            _ => Ok(Vec::new()),
        }
    }

    fn prefix_postings(&self, prefix: &str) -> Result<Vec<DocId>> {
        let mut all = Vec::new();
        for (_, node_id) in self.words_with_prefix(prefix, self.config.max_prefix_terms)? {
            all.extend(self.load_postings(node_id)?);
        }
        all.sort_unstable();
        all.dedup();
        Ok(all)
    }

    /// Busca em largura a partir do nó do prefixo. Retorna (palavra, id do nó).
    fn words_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let Some((start, _)) = self.find_node(prefix)? else { return Ok(out) };
        let mut queue = VecDeque::from([(prefix.to_string(), start)]);
        while out.len() < limit {
            let Some((word, id)) = queue.pop_front() else { break };
            let node = self.require_node(id)?;
            for (&ch, &child) in &node.children {
                let mut next = word.clone();
                next.push(ch);
                queue.push_back((next, child));
            }
            if node.is_end_of_word {
                out.push((word, id));
            }
        }
        Ok(out)
    }

    /// Percorre a Trie caractere a caractere: um `Get` no RocksDB por nível.
    fn find_node(&self, term: &str) -> Result<Option<(u64, TrieNode)>> {
        let Some(mut node) = load_node(&self.snap, ROOT_ID)? else { return Ok(None) };
        let mut id = ROOT_ID;
        for ch in term.chars() {
            let Some(&child) = node.children.get(&ch) else { return Ok(None) };
            id = child;
            node = self.require_node(child)?;
        }
        Ok(Some((id, node)))
    }

    fn require_node(&self, id: u64) -> Result<TrieNode> {
        load_node(&self.snap, id)?.ok_or_else(|| missing_node(id))
    }

    fn load_postings(&self, node_id: u64) -> Result<Vec<DocId>> {
        match self.snap.get_bytes(&post_key(node_id))? {
            Some(v) => codec::decode_postings(&v),
            None => Ok(Vec::new()),
        }
    }
}

/// Lote de indexação. Acumula as mudanças em memória e grava tudo de uma vez,
/// atomicamente, em [`IndexBatch::commit`]. Se for descartado sem commit,
/// nada é gravado. Buscas concorrentes nunca veem um lote pela metade.
///
/// O lote é invertido: `add_document` só junta termo → documentos, e a Trie é
/// percorrida no commit, uma vez por termo distinto do lote (e não uma vez
/// por documento que contém o termo).
pub struct IndexBatch<'a> {
    db: &'a DB,
    state: MutexGuard<'a, WriterState>,
    next_node_id: u64,
    next_doc_id: DocId,
    /// Cache de nós emprestado do `WriterState`, mais os nós lidos ou criados
    /// neste lote. Nós criados aqui ainda não existem no banco, então as
    /// travessias seguintes precisam enxergá-los por este mapa.
    nodes: HashMap<u64, CachedNode>,
    generation: u64,
    cache_limit: usize,
    /// Nós alterados neste lote, na ordem em que mudaram.
    dirty: Vec<u64>,
    /// Termo -> documentos deste lote que o contêm.
    pending: HashMap<String, Vec<DocId>>,
    labels: Vec<(DocId, String)>,
    documents: usize,
}

struct CachedNode {
    node: TrieNode,
    dirty: bool,
    /// Lote que usou o nó por último.
    used: u64,
    /// Distância até a raiz. Os nós rasos são os mais compartilhados.
    depth: u8,
}

impl IndexBatch<'_> {
    /// Indexa `text` sob um ID escolhido por você.
    pub fn add_document(&mut self, doc_id: DocId, text: &str) -> Result<()> {
        for term in index_terms(text) {
            match self.pending.get_mut(term.as_str()) {
                Some(docs) => docs.push(doc_id),
                None => {
                    self.pending.insert(term, vec![doc_id]);
                }
            }
        }
        // IDs automáticos de `add_labeled` nunca colidem com IDs manuais.
        self.next_doc_id = self.next_doc_id.max(doc_id.saturating_add(1));
        self.documents += 1;
        Ok(())
    }

    /// Indexa `text` com um ID novo e grava `label` (ex.: caminho do arquivo).
    pub fn add_labeled(&mut self, label: &str, text: &str) -> Result<DocId> {
        let doc_id = self.next_doc_id;
        self.add_document(doc_id, text)?;
        self.labels.push((doc_id, label.to_string()));
        Ok(doc_id)
    }

    /// Grava (ou troca) o rótulo de um documento junto com o commit do lote.
    pub fn set_label(&mut self, doc_id: DocId, label: &str) {
        self.labels.push((doc_id, label.to_string()));
    }

    /// Documentos adicionados desde o início do lote. Use para decidir quando
    /// fazer commit: o lote guarda em memória os termos de todos eles.
    pub fn len(&self) -> usize {
        self.documents
    }

    pub fn is_empty(&self) -> bool {
        self.documents == 0
    }

    /// Grava o lote num único `WriteBatch` atômico.
    pub fn commit(mut self) -> Result<()> {
        // Termos em ordem: a criação de nós fica determinística (o mesmo
        // corpus com o mesmo tamanho de lote gera sempre os mesmos IDs).
        let mut terms: Vec<(String, Vec<DocId>)> = std::mem::take(&mut self.pending).into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut wb = WriteBatch::default();
        // `merge` em vez de ler + reescrever a lista: o RocksDB une os
        // fragmentos na leitura e na compactação (ver `merge_postings`).
        for (term, mut docs) in terms {
            let id = self.insert_term(&term)?;
            docs.sort_unstable();
            docs.dedup();
            wb.merge(post_key(id), codec::encode_postings(&docs));
        }
        for id in &self.dirty {
            wb.put(node_key(*id), self.nodes[id].node.encode());
        }
        for (doc_id, label) in &self.labels {
            wb.put(doc_key(*doc_id), label.as_bytes());
        }
        wb.put(META_NEXT_NODE, varint(self.next_node_id));
        wb.put(META_NEXT_DOC, varint(self.next_doc_id));

        self.db.write(wb)?;

        // Gravado: o cache volta a ser igual ao banco e fica para o próximo lote.
        for id in std::mem::take(&mut self.dirty) {
            if let Some(cached) = self.nodes.get_mut(&id) {
                cached.dirty = false;
            }
        }
        let mut nodes = std::mem::take(&mut self.nodes);
        evict(&mut nodes, self.generation, self.cache_limit);
        self.state.cache = NodeCache { nodes, generation: self.generation };
        self.state.next_node_id = self.next_node_id;
        self.state.next_doc_id = self.next_doc_id;
        Ok(())
    }

    /// Cria (se preciso) o caminho de `term` na Trie e devolve o nó terminal.
    fn insert_term(&mut self, term: &str) -> Result<u64> {
        let mut id = ROOT_ID;
        let mut depth = 0u8;
        for ch in term.chars() {
            let next_free = self.next_node_id;
            let generation = self.generation;
            let parent = self.node_mut(id, depth)?;
            id = match parent.node.children.get(&ch) {
                Some(&child) => child,
                None => {
                    parent.node.children.insert(ch, next_free);
                    if !parent.dirty {
                        parent.dirty = true;
                        self.dirty.push(id);
                    }
                    self.next_node_id += 1;
                    self.dirty.push(next_free);
                    let node = CachedNode { node: TrieNode::default(), dirty: true, used: generation, depth: depth + 1 };
                    self.nodes.insert(next_free, node);
                    next_free
                }
            };
            // Termos têm no máximo MAX_TOKEN_CHARS (64) caracteres.
            depth = depth.saturating_add(1);
        }
        let leaf = self.node_mut(id, depth)?;
        if !leaf.node.is_end_of_word {
            leaf.node.is_end_of_word = true;
            if !leaf.dirty {
                leaf.dirty = true;
                self.dirty.push(id);
            }
        }
        Ok(id)
    }

    /// Nó do cache, lendo do banco na primeira vez.
    fn node_mut(&mut self, id: u64, depth: u8) -> Result<&mut CachedNode> {
        let generation = self.generation;
        match self.nodes.entry(id) {
            Entry::Occupied(e) => {
                let cached = e.into_mut();
                cached.used = generation;
                Ok(cached)
            }
            Entry::Vacant(e) => {
                let (node, dirty) = match load_node(self.db, id)? {
                    Some(node) => (node, false),
                    // Banco novo: a raiz nasce no primeiro commit.
                    None if id == ROOT_ID => (TrieNode::default(), true),
                    None => return Err(missing_node(id)),
                };
                if dirty {
                    self.dirty.push(id);
                }
                Ok(e.insert(CachedNode { node, dirty, used: generation, depth }))
            }
        }
    }
}

/// Mantém o cache de nós dentro de `limit`: primeiro saem os nós que o lote
/// `generation` não usou; se ainda passar, saem os mais fundos.
fn evict(nodes: &mut HashMap<u64, CachedNode>, generation: u64, limit: usize) {
    if nodes.len() <= limit {
        return;
    }
    nodes.retain(|_, c| c.used == generation);
    if nodes.len() <= limit {
        return;
    }
    let mut per_depth = [0usize; 256];
    for c in nodes.values() {
        per_depth[c.depth as usize] += 1;
    }
    // Maior profundidade que ainda cabe inteira no limite.
    let mut total = 0;
    let mut max_depth = None;
    for (depth, &n) in per_depth.iter().enumerate() {
        total += n;
        if total > limit {
            break;
        }
        max_depth = Some(depth);
    }
    match max_depth {
        Some(d) => nodes.retain(|_, c| c.depth as usize <= d),
        None => nodes.clear(),
    }
}

thread_local! {
    /// Leituras (`get`) feitas pelo motor nesta thread. Usado pelo harness
    /// para medir "Gets por consulta" sem depender do PerfContext do RocksDB.
    static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Total de `get`s que o motor fez na thread atual (só cresce).
pub fn reads_on_this_thread() -> u64 {
    READS.with(|r| r.get())
}

fn count_read() {
    READS.with(|r| r.set(r.get() + 1));
}

/// O banco direto (usado pelo lote, único escritor) ou um snapshot (buscas).
trait Source {
    fn get_bytes(&self, key: &str) -> Result<Option<rocksdb::DBPinnableSlice<'_>>>;
}

impl Source for DB {
    fn get_bytes(&self, key: &str) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        count_read();
        Ok(self.get_pinned(key)?)
    }
}

impl Source for Snapshot<'_> {
    fn get_bytes(&self, key: &str) -> Result<Option<rocksdb::DBPinnableSlice<'_>>> {
        count_read();
        Ok(self.get_pinned(key)?)
    }
}

fn load_node(src: &impl Source, id: u64) -> Result<Option<TrieNode>> {
    src.get_bytes(&node_key(id))?.map(|v| TrieNode::decode(&v)).transpose()
}

fn missing_node(id: u64) -> Error {
    Error::Corrupt(format!("nó {id} referenciado mas ausente"))
}

fn read_counter(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    db.get_pinned(key)?.map(|v| codec::get_varint(&v)).transpose()
}

fn varint(v: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    codec::put_varint(&mut buf, v);
    buf
}

/// Operador de merge das listas de postagem: união ordenada sem repetições.
/// É associativo, então o RocksDB pode combinar fragmentos em qualquer ordem.
/// Devolver `None` (dado corrompido) faz a leitura falhar em vez de mentir.
fn merge_postings(_key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    union_encoded(existing, operands.iter())
}

/// União de listas codificadas: o trabalho do merge operator.
pub fn union_encoded<'a>(
    existing: Option<&[u8]>,
    operands: impl Iterator<Item = &'a [u8]>,
) -> Option<Vec<u8>> {
    let mut all = match existing {
        Some(bytes) => codec::decode_postings(bytes).ok()?,
        None => Vec::new(),
    };
    for op in operands {
        all.extend(codec::decode_postings(op).ok()?);
    }
    all.sort_unstable();
    all.dedup();
    Some(codec::encode_postings(&all))
}

/// Interseção de duas listas ordenadas.
pub fn intersect(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}
