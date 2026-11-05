//! Sonda da Fase 0 do plano de correção de digitação: quantos nós da Trie um
//! percurso com erros (Damerau-Levenshtein) lê, e quanto tempo leva.
//!
//!   cargo run --release --example typo_probe -- <banco> <vocab.tsv> <palavras.txt>...

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use fire_flower::internals::TrieNode;
use rocksdb::{BlockBasedOptions, Cache, DB, Options};

/// Profundidade do cache de nós das buscas (`Config::reader_cache_depth`).
const CACHE_DEPTH: usize = 4;

struct Probe {
    db: DB,
    cache: HashMap<u64, Arc<TrieNode>>,
}

/// Um nó a visitar: id, palavra até ele, linha da matriz, a linha anterior e
/// o último caractere (para a transposição).
type State = (u64, String, Vec<u8>, Vec<u8>, char);

#[derive(Default)]
struct Walk {
    visited: usize,
    reads: usize,
    found: Vec<(String, u8)>,
}

impl Probe {
    fn nodes(&mut self, ids: &[u64], depth: usize, walk: &mut Walk) -> Vec<Arc<TrieNode>> {
        walk.visited += ids.len();
        let cached = depth <= CACHE_DEPTH;
        let mut out: Vec<Option<Arc<TrieNode>>> = ids.iter().map(|id| if cached { self.cache.get(id).cloned() } else { None }).collect();
        let missing: Vec<usize> = (0..ids.len()).filter(|&i| out[i].is_none()).collect();
        for chunk in missing.chunks(64) {
            let keys: Vec<Vec<u8>> = chunk.iter().map(|&i| format!("node:{}", ids[i]).into_bytes()).collect();
            walk.reads += keys.len();
            for (&i, v) in chunk.iter().zip(self.db.multi_get(&keys)) {
                let node = Arc::new(TrieNode::decode(&v.unwrap().expect("nó existe")).unwrap());
                if cached {
                    self.cache.insert(ids[i], node.clone());
                }
                out[i] = Some(node);
            }
        }
        out.into_iter().map(Option::unwrap).collect()
    }

    /// Palavras a até `d` erros de `q`, nível a nível (como `words_with_prefix`).
    fn walk(&mut self, q: &[char], d: u8, fixed_first: bool) -> Walk {
        let mut walk = Walk::default();
        let m = q.len();
        let row0: Vec<u8> = (0..=m as u8).collect();
        let mut level: Vec<State> = vec![(0, String::new(), row0, Vec::new(), '\0')];
        let mut depth = 0;
        while !level.is_empty() {
            let ids: Vec<u64> = level.iter().map(|e| e.0).collect();
            let nodes = self.nodes(&ids, depth, &mut walk);
            let mut next = Vec::new();
            for ((_, word, row, prev, pc), node) in level.into_iter().zip(nodes) {
                if node.is_end_of_word && row[m] <= d && depth > 0 {
                    walk.found.push((word.clone(), row[m]));
                }
                for (&c, &child) in &node.children {
                    if fixed_first && depth == 0 && c != q[0] {
                        continue;
                    }
                    let mut new = vec![0u8; m + 1];
                    new[0] = row[0] + 1;
                    for j in 1..=m {
                        let cost = u8::from(q[j - 1] != c);
                        let mut v = (row[j] + 1).min(new[j - 1] + 1).min(row[j - 1] + cost);
                        if j > 1 && !prev.is_empty() && c == q[j - 2] && pc == q[j - 1] {
                            v = v.min(prev[j - 2] + 1);
                        }
                        new[j] = v;
                    }
                    if *new.iter().min().unwrap() > d {
                        continue;
                    }
                    let mut w = word.clone();
                    w.push(c);
                    next.push((child, w, new, row.clone(), c));
                }
            }
            level = next;
            depth += 1;
        }
        walk
    }
}

/// xorshift: o mesmo erro em toda rodada.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ROWS: [&str; 3] = ["qwertyuiop", "asdfghjklc", "zxcvbnm"];

fn neighbor(c: char, rng: &mut Rng) -> char {
    for (r, row) in ROWS.iter().enumerate() {
        if let Some(i) = row.find(c) {
            let mut opts: Vec<char> = Vec::new();
            let row: Vec<char> = row.chars().collect();
            if i > 0 { opts.push(row[i - 1]) }
            if i + 1 < row.len() { opts.push(row[i + 1]) }
            for rr in [r.wrapping_sub(1), r + 1] {
                if let Some(other) = ROWS.get(rr) {
                    let other: Vec<char> = other.chars().collect();
                    if let Some(&o) = other.get(i) { opts.push(o) }
                }
            }
            return opts[rng.below(opts.len())];
        }
    }
    'e'
}

/// Um erro fora da primeira letra: vizinha do teclado, falta, repetida ou invertida.
fn typo(w: &[char], rng: &mut Rng) -> Vec<char> {
    let mut w = w.to_vec();
    let i = 1 + rng.below(w.len() - 1);
    match rng.below(4) {
        0 => w[i] = neighbor(w[i], rng),
        1 => { w.remove(i); }
        2 => w.insert(i, w[i]),
        _ if i + 1 < w.len() => w.swap(i, i + 1),
        _ => w.swap(i - 1, i),
    }
    w
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, vocab_path, word_files) = (&args[0], &args[1], &args[2..]);

    let mut df: HashMap<String, u32> = HashMap::new();
    for line in std::fs::read_to_string(vocab_path).unwrap().lines() {
        let mut it = line.split('\t');
        if let (Some(t), Some(d)) = (it.next(), it.next()) && let Ok(d) = d.parse() {
            df.insert(t.to_string(), d);
        }
    }

    let mut table = BlockBasedOptions::default();
    table.set_block_cache(&Cache::new_lru_cache(256 << 20));
    table.set_bloom_filter(10.0, false);
    let mut opts = Options::default();
    opts.set_block_based_table_factory(&table);
    let db = DB::open_for_read_only(&opts, path, false).unwrap();
    let mut probe = Probe { db, cache: HashMap::new() };

    let mut words: Vec<Vec<char>> = Vec::new();
    for f in word_files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            for w in fire_flower::analysis::tokenize(line) {
                if w.chars().all(|c| c.is_ascii_lowercase()) && df.get(&w).is_some_and(|&d| d >= 2) {
                    words.push(w.chars().collect());
                }
            }
        }
    }
    words.sort();
    words.dedup();

    // (rótulo, tamanhos, erros na palavra, d do percurso, primeira letra fixa)
    let cases: [(&str, std::ops::RangeInclusive<usize>, usize, u8, bool); 6] = [
        ("5–8 letras, 1 erro, d=1, 1ª fixa", 5..=8, 1, 1, true),
        ("5–8 letras, 1 erro, d=1, 1ª livre", 5..=8, 1, 1, false),
        ("9+ letras, 1 erro, d=1, 1ª fixa", 9..=64, 1, 1, true),
        ("9+ letras, 2 erros, d=2, 1ª fixa", 9..=64, 2, 2, true),
        ("9+ letras, 2 erros, d=2, 1ª livre", 9..=64, 2, 2, false),
        ("5–8 letras, 1 erro, d=2, 1ª fixa", 5..=8, 1, 2, true),
    ];
    println!("{} palavras de base (df ≥ 2, só a–z)\n", words.len());
    println!("| caso | consultas | p50 | p99 | nós visitados p50 / p99 | lidos fora do cache p50 / p99 | candidatos (df ≥ 2) média | certa entre eles | certa em 1º |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (label, lens, errors, d, fixed) in cases {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut queries: Vec<(Vec<char>, String)> = Vec::new();
        for w in words.iter().filter(|w| lens.contains(&w.len())) {
            let mut t = w.clone();
            for _ in 0..errors {
                t = typo(&t, &mut rng);
            }
            let ts: String = t.iter().collect();
            if df.get(&ts).copied().unwrap_or(0) < 2 {
                queries.push((t, w.iter().collect()));
            }
            if queries.len() == 400 {
                break;
            }
        }
        // Uma passada para aquecer os caches, outra medida.
        for (q, _) in &queries {
            probe.walk(q, d, fixed);
        }
        let (mut lat, mut vis, mut reads, mut cands) = (Vec::new(), Vec::new(), Vec::new(), 0.0);
        let (mut has, mut first) = (0, 0);
        for (q, right) in &queries {
            let t = Instant::now();
            let walk = probe.walk(q, d, fixed);
            lat.push(t.elapsed().as_secs_f64() * 1e6);
            vis.push(walk.visited as f64);
            reads.push(walk.reads as f64);
            let mut c: Vec<(u8, u32, &String)> =
                walk.found.iter().filter_map(|(w, dist)| df.get(w).filter(|&&x| x >= 2).map(|&x| (*dist, x, w))).collect();
            if let Some(&min) = c.iter().map(|(dist, _, _)| dist).min() {
                c.retain(|x| x.0 == min);
            }
            c.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
            cands += c.len() as f64;
            has += usize::from(c.iter().any(|x| x.2 == right));
            first += usize::from(c.first().is_some_and(|x| x.2 == right));
        }
        let n = queries.len() as f64;
        println!(
            "| {label} | {} | {:.0} µs | {:.0} µs | {:.0} / {:.0} | {:.0} / {:.0} | {:.1} | {:.0}% | {:.0}% |",
            queries.len(), pct(&mut lat, 0.5), pct(&mut lat, 0.99), pct(&mut vis, 0.5), pct(&mut vis, 0.99),
            pct(&mut reads, 0.5), pct(&mut reads, 0.99), cands / n, 100.0 * has as f64 / n, 100.0 * first as f64 / n,
        );
    }
}
