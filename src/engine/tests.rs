//! Testes unitários das partes internas do motor.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::{DocId, Hit, Posting};
use crate::codec::{DocLens, NormBlock, TrieNode};
use crate::engine::{EpochCache, Key};
use crate::engine::conjunction::seek;
use crate::engine::keys::{NODE, NORM, POST};
use crate::engine::phrase::count_phrase;
use crate::engine::ranking::topk::TopK;
use crate::engine::writer::trie::{CachedNode, evict};

fn cached(used: u64, depth: u8) -> CachedNode {
    CachedNode { node: TrieNode::default(), dirty: false, used, depth }
}

#[test]
fn evict_keeps_the_most_recent_and_shallowest() {
    let mut nodes: FxHashMap<u64, CachedNode> = FxHashMap::default();
    // Lote 10 (o atual): 40 nós; lote 9: 40; lote 3: 40, metade rasa.
    for i in 0..40 {
        nodes.insert(i, cached(10, 5));
        nodes.insert(100 + i, cached(9, 5));
        nodes.insert(200 + i, cached(3, if i < 20 { 1 } else { 8 }));
    }
    evict(&mut nodes, 10, 200); // abaixo do limite: nada sai
    assert_eq!(nodes.len(), 120);
    // Limite 100 → alvo 80: ficam os lotes 10 e 9 inteiros.
    evict(&mut nodes, 10, 100);
    assert_eq!(nodes.len(), 80);
    assert!(nodes.keys().all(|&k| k < 140));
    // Alvo 60 (limite 75): o lote 10 inteiro e 20 do lote 9, os mais rasos.
    for i in 0..20 {
        nodes.get_mut(&(100 + i)).unwrap().depth = 2;
    }
    evict(&mut nodes, 10, 75);
    assert_eq!(nodes.len(), 60);
    assert!((0..40).all(|k| nodes.contains_key(&k)));
    assert!((100..120).all(|k| nodes.contains_key(&k)));
    // O lote atual sozinho passa do alvo: ficam os mais rasos dele.
    evict(&mut nodes, 10, 10);
    assert!(nodes.len() <= 8);
}

#[test]
fn keys_without_allocation() {
    assert_eq!(Key::new(POST, 0).as_ref(), b"post:0");
    assert_eq!(Key::new(NORM, 1234567890).as_ref(), b"norm:1234567890");
    assert_eq!(Key::new(NODE, u64::MAX).as_ref(), format!("node:{}", u64::MAX).as_bytes());
}

#[test]
fn seek_gallops_and_stops_at_first_not_less() {
    let list: Vec<Posting> = [1u64, 3, 5, 7, 9, 11, 100, 101].iter().map(|&doc| Posting::new(doc, 1)).collect();
    let mut at = 0;
    assert!(seek(&list, &mut at, 1));
    assert!(!seek(&list, &mut at, 4));
    assert_eq!(list[at].doc, 5);
    assert!(seek(&list, &mut at, 100));
    assert!(seek(&list, &mut at, 100), "parado no alvo");
    assert!(!seek(&list, &mut at, 102));
    assert!(!seek(&list, &mut at, 103));
    assert!(!seek(&[], &mut 0, 1));
}

#[test]
fn phrase_counting() {
    let mut at = vec![0; 3];
    // casa@0 papel@2: "casa X papel" em 0 e 10; casa em 5 sem papel em 7.
    let pos = vec![vec![0, 5, 10], vec![2, 12]];
    assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 0), (2, 0));
    assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], true, 0), (1, 0));
    // Título com extensão 3: a ocorrência que começa em 0 está nele, a de 10 não.
    assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 3), (2, 1));
    // Termo repetido: novo@0 novo@2 em "novo de novo de novo" (0, 2, 4).
    let pos = vec![vec![0, 2, 4], vec![0, 2, 4]];
    assert_eq!(count_phrase(&[0, 2], &pos, &mut at[..2], false, 0), (2, 0));
    // Ordem trocada não casa.
    let pos = vec![vec![3], vec![2]];
    assert_eq!(count_phrase(&[0, 1], &pos, &mut at[..2], false, 0), (0, 0));
    // Três termos.
    let pos = vec![vec![1, 8], vec![2, 9], vec![4, 20]];
    assert_eq!(count_phrase(&[0, 1, 3], &pos, &mut at, false, 0), (1, 0));
}

#[test]
fn top_k_orders_by_score_then_id() {
    let mut top = TopK::new(3);
    for (doc, score) in [(1, 0.5), (2, 2.0), (3, 0.5), (4, 1.0), (5, 0.5), (6, 2.0)] {
        top.push(Hit { doc, score });
    }
    let got: Vec<(DocId, f64)> = top.into_sorted().into_iter().map(|h| (h.doc, h.score)).collect();
    assert_eq!(got, [(2, 2.0), (6, 2.0), (4, 1.0)]);
    let mut empty = TopK::new(0);
    empty.push(Hit { doc: 1, score: 1.0 });
    assert!(empty.into_sorted().is_empty());
}

#[test]
fn epoch_cache_respects_snapshots() {
    let cache = EpochCache::<NormBlock>::default();
    let a = Arc::new(NormBlock::default());
    let mut b = NormBlock::default();
    b.set(1, DocLens { len: 10, title_len: 0, title_span: 0 });
    let b = Arc::new(b);
    // Nunca mudou: qualquer busca usa.
    cache.offer(7, 0, &a);
    assert!(cache.get(7, 0).is_some());
    // O escritor vai mudar o bloco 7 no commit 1.
    cache.mark([7], 1);
    assert!(cache.get(7, 0).is_none(), "busca antiga relê do próprio snapshot");
    cache.offer(7, 0, &a);
    assert!(cache.get(7, 1).is_none(), "busca antiga não guarda o bloco velho");
    // Commit 1 publicado: uma busca nova guarda o que leu.
    cache.offer(7, 1, &b);
    assert_eq!(cache.get(7, 1).unwrap().get(1), Some(10));
    assert!(cache.get(7, 0).is_none());
    // Uma chave nunca marcada, oferecida por uma busca antiga, vale para todas.
    cache.offer(8, 0, &a);
    assert!(cache.get(8, 5).is_some());
}

/// Uma busca aberta antes de uma edição continua vendo a versão antiga, mesmo
/// que uma compactação rode no meio: a limpeza não tira das listas um morto
/// que um snapshot aberto ainda vê vivo, e só o tira na compactação seguinte.
#[test]
fn compaction_keeps_versions_that_an_open_snapshot_sees() {
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::Engine::open(dir.path(), crate::Config::default()).unwrap();
    engine.index_document(7, "velho comum").unwrap();
    let before = engine.view();
    engine.index_document(7, "novo comum").unwrap();
    assert_eq!(engine.compact().unwrap().purged, 0, "o snapshot aberto ainda vê a versão 7");
    assert_eq!(before.search("velho").unwrap(), [7]);
    assert_eq!(before.search("comum").unwrap(), [7]);
    assert!(before.search("novo").unwrap().is_empty());
    let during = engine.view();
    assert_eq!(during.search("comum").unwrap(), [7], "a versão nova, uma vez só");
    drop((before, during));
    assert_eq!(engine.compact().unwrap().purged, 1);
    assert!(engine.search("velho").unwrap().is_empty());
    assert_eq!(engine.search("novo comum").unwrap(), [7]);
}

/// Uma busca que começou antes de um commit que mata não filtra essas mortes,
/// mesmo pegando o conjunto de mortos depois do commit.
#[test]
fn deaths_after_the_snapshot_are_not_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::Engine::open(dir.path(), crate::Config::default()).unwrap();
    engine.index_document(3, "alfa").unwrap();
    engine.index_document(4, "alfa").unwrap();
    engine.delete_document(4).unwrap();
    let snap = engine.db.snapshot();
    let epoch = engine.epoch();
    engine.delete_document(3).unwrap();
    // O snapshot é do commit `epoch`, mas os mortos já incluem o 3.
    let view = super::View::new(snap, epoch, Some(epoch), engine.deaths.current(), &engine);
    assert_eq!(view.search("alfa").unwrap(), [3]);
    // Sem saber o commit exato, a busca o lê de `meta:epoch` no snapshot.
    let view = super::View::new(engine.db.snapshot(), epoch, None, engine.deaths.current(), &engine);
    assert!(view.search("alfa").unwrap().is_empty());
}

#[test]
fn translated_ids_merge_in_order() {
    use super::versions::{first_high, next_high, user_of, users_sorted};
    let (h1, h5, h7, h100) = (first_high(1), next_high(first_high(5)).unwrap(), first_high(7), first_high(100));
    assert_eq!((user_of(h1), user_of(h5), user_of(3)), (1, 5, 3));
    assert!(h1 < h7 && h7 < h100 && h100 < h5, "a faixa alta fica em ordem de versão e depois de documento");
    assert_eq!(users_sorted(vec![2, 6, 50, h1, h7, h100, h5]), [1, 2, 5, 6, 7, 50, 100]);
    assert_eq!(users_sorted(vec![h100]), [100]);
    assert_eq!(users_sorted(vec![3, 4]), [3, 4]);
    assert_eq!(users_sorted(vec![3, 4, h7]), [3, 4, 7]);
    // Poucos traduzidos numa lista grande: inserção por busca binária.
    let mut ids: Vec<u64> = (0..1000).map(|d| d * 2).collect();
    ids.extend([first_high(1), first_high(999), first_high(5000)]);
    let mut want: Vec<u64> = (0..1000).map(|d| d * 2).chain([1, 999, 5000]).collect();
    want.sort_unstable();
    assert_eq!(users_sorted(ids), want);
    let last = u64::MAX; // a última versão do maior documento
    assert_eq!(next_high(last), None, "versões esgotadas");
    assert_eq!(user_of(last), super::versions::MAX_DOC);
    // Versões diferentes formam trechos: a junção continua em ordem.
    let (a, b) = (next_high(first_high(2)).unwrap(), next_high(first_high(9)).unwrap());
    assert!(first_high(100) < a && a < b);
    assert_eq!(users_sorted(vec![4, first_high(1), first_high(100), a, b]), [1, 2, 4, 9, 100]);
}
