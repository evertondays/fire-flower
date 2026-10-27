//! `wiki digest`: resumo do conteúdo lógico do banco (para comparar cargas).

use std::path::Path;

use fire_flower::{Config, Engine};
use serde_json::json;

use crate::AnyResult;

/// FNV-1a de 64 bits: estável entre versões do Rust (o SipHash da std não garante isso).
pub(crate) struct Fnv(pub(crate) u64);

impl Fnv {
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Resumo do conteúdo lógico: termos com (doc, tf) e posições, tamanhos dos
/// documentos e estatísticas. Não depende dos IDs de nó, então bancos
/// carregados com lotes diferentes têm que dar o mesmo resumo.
pub(crate) fn digest(db: &Path) -> AnyResult {
    let engine = Engine::open(db, Config::default())?;
    let mut nodes = Vec::new();
    engine.for_each_term(|term, node| nodes.push((term.to_string(), node)))?;
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    let (mut postings, mut positions) = (0u64, 0u64);
    for (term, node) in &nodes {
        h.write(term.as_bytes());
        h.write(&[0]);
        for p in engine.term_postings_of_node(*node)? {
            h.write(&p.doc.to_le_bytes());
            h.write(&p.tf.to_le_bytes());
            h.write(&p.title_tf.to_le_bytes());
            postings += 1;
        }
        h.write(&[0xfe]);
        for (doc, list) in engine.positions_of_node(*node)? {
            h.write(&doc.to_le_bytes());
            h.write(&(list.len() as u32).to_le_bytes());
            for p in list {
                h.write(&p.to_le_bytes());
                positions += 1;
            }
        }
        h.write(&[0xff]);
    }
    let mut lens = Vec::new();
    engine.for_each_doc_len(|doc, len, title_len| lens.push((doc, len, title_len)))?;
    lens.sort_unstable();
    for (doc, len, title_len) in &lens {
        h.write(&doc.to_le_bytes());
        h.write(&len.to_le_bytes());
        h.write(&title_len.to_le_bytes());
    }
    let stats = engine.stats()?;
    h.write(&stats.doc_count.to_le_bytes());
    h.write(&stats.total_len.to_le_bytes());
    h.write(&stats.total_title_len.to_le_bytes());
    println!(
        "{}",
        json!({ "db": db, "terms": nodes.len(), "postings": postings, "positions": positions,
                "documents": stats.doc_count, "total_size": stats.total_len, "digest": format!("{:016x}", h.0) })
    );
    Ok(())
}
