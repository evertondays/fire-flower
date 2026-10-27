//! `wiki sizes`: tamanho de cada espaço de chave e das maiores listas.

use std::collections::BTreeMap;
use std::path::Path;

use fire_flower::{Config, Engine};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde_json::json;

use crate::AnyResult;
use crate::corpus::stem;
use crate::env::{mb, save_result};

pub(crate) fn sizes(db: &Path) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let watched: Vec<(&str, Option<u64>)> = ["area", "populacao", "historia", "anos", "parte"].iter().map(|t| Ok((*t, engine.term_node(t)?))).collect::<AnyResult<_>>()?;

    #[derive(Default)]
    struct Space {
        keys: u64,
        key_bytes: u64,
        value_bytes: u64,
    }
    let mut spaces: BTreeMap<String, Space> = BTreeMap::new();
    let mut largest: Vec<(usize, String)> = Vec::new();
    let mut watched_sizes: HashMap<String, usize> = HashMap::default();
    let mut watched_pos: HashMap<String, usize> = HashMap::default();
    let mut root_size = 0;
    engine.for_each_raw(|k, v| {
        let key = String::from_utf8_lossy(k);
        let space = key.split(':').next().unwrap_or("").to_string();
        let s = spaces.entry(space).or_default();
        s.keys += 1;
        s.key_bytes += k.len() as u64;
        s.value_bytes += v.len() as u64;
        if key == "node:0" {
            root_size = v.len();
        }
        if largest.len() < 20 || v.len() > largest.last().unwrap().0 {
            largest.push((v.len(), key.to_string()));
            largest.sort_by(|a, b| b.0.cmp(&a.0));
            largest.truncate(20);
        }
        for (prefix, map) in [("post:", &mut watched_sizes), ("pos:", &mut watched_pos)] {
            if let Some(id) = key.strip_prefix(prefix).and_then(|i| i.parse::<u64>().ok()) {
                for (term, node) in &watched {
                    if *node == Some(id) {
                        map.insert(term.to_string(), v.len());
                    }
                }
            }
        }
    })?;

    // Termos dos maiores valores (só nós terminais têm termo).
    let wanted: HashSet<u64> = largest.iter().filter_map(|(_, k)| k.split_once(':').and_then(|(_, id)| id.parse().ok())).collect();
    let mut names: HashMap<u64, String> = HashMap::default();
    engine.for_each_term(|term, node| {
        if wanted.contains(&node) {
            names.insert(node, term.to_string());
        }
    })?;

    println!("| espaço | chaves | chaves (MB) | valores (MB) |");
    println!("|---|---|---|---|");
    for (name, s) in &spaces {
        println!("| `{name}:` | {} | {:.1} | {:.1} |", s.keys, mb(s.key_bytes), mb(s.value_bytes));
    }
    println!("\nraiz (`node:0`): {root_size} bytes; nós da Trie: {}; termos: {}", engine.trie_node_count()?, spaces.get("post").map_or(0, |s| s.keys));
    for (term, _) in &watched {
        if let Some(size) = watched_sizes.get(*term) {
            let pos = watched_pos.get(*term).map_or(String::new(), |p| format!(", posições {:.1} KB", *p as f64 / 1024.0));
            println!("lista de \"{term}\": {:.1} KB{pos}", *size as f64 / 1024.0);
        }
    }
    println!("\n20 maiores valores:");
    let mut top = Vec::new();
    for (size, key) in &largest {
        let id = key.split_once(':').and_then(|(_, id)| id.parse::<u64>().ok());
        let term = id.and_then(|i| names.get(&i)).map_or(String::new(), |t| format!(" ({t})"));
        println!("  {size:>10} B  {key}{term}");
        top.push(json!({ "key": key, "bytes": size, "term": term.trim().trim_matches(['(', ')']) }));
    }
    save_result(
        "sizes",
        &stem(db),
        json!({
            "db": db,
            "spaces": spaces.iter().map(|(n, s)| json!({"space": n, "keys": s.keys, "key_bytes": s.key_bytes, "value_bytes": s.value_bytes})).collect::<Vec<_>>(),
            "root_bytes": root_size, "trie_nodes": engine.trie_node_count()?,
            "lists": watched_sizes, "pos_lists": watched_pos, "largest": top,
            "positions": engine.has_positions(),
        }),
    )?;
    Ok(())
}
