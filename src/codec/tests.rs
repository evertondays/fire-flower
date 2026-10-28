//! Testes dos formatos.

use crate::{DocId, Posting};
use std::collections::BTreeMap;

use crate::codec::{
    DocLens, ListKind, NormBlock, PositionsCursor, PostingsCursor, SKIP_BLOCK, TrieNode, decode_docs,
    decode_node_set, decode_positions, decode_postings, encode_node_set, encode_positions_into,
    encode_postings, encode_tombstones_into, get_varint, list_len, merge_lists, merge_node_sets, merge_positions,
    merge_postings,
};
use crate::codec::list::ListHeader;
use crate::codec::positions::encode_positions_with;

#[test]
fn node_sets_round_trip_and_merge() {
    let a = encode_node_set(&[3, 10, 700]);
    assert_eq!(decode_node_set(&a).unwrap(), [3, 10, 700]);
    assert_eq!(decode_node_set(&[]).unwrap(), Vec::<u64>::new());
    let b = encode_node_set(&[10, 11]);
    let merged = merge_node_sets([a.as_slice(), b.as_slice()].into_iter()).unwrap();
    assert_eq!(decode_node_set(&merged).unwrap(), [3, 10, 11, 700]);
    assert!(decode_node_set(&[5, 0]).is_err(), "repetido");
    assert!(merge_node_sets([&[0x80u8][..]].into_iter()).is_none(), "truncado");
}

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
        for (lists, full, kind) in [(&post, &all_post, ListKind::Postings), (&pos, &all_pos, ListKind::Positions)] {
            let slices: Vec<&[u8]> = lists.iter().map(Vec::as_slice).collect();
            let left = merge_lists(slices[..split].iter().copied(), kind, false).unwrap();
            let right = merge_lists(slices[split..].iter().copied(), kind, false).unwrap();
            let again = merge_lists([left.as_slice(), right.as_slice()].into_iter(), kind, true).unwrap();
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
    // Por remendo: os blocos que o pedaço novo não toca são copiados, e o
    // tamanho fica praticamente o da lista original.
    assert!(merged2.len() <= merged.len() + 16, "{} vs {}", merged2.len(), merged.len());
    let mut cursor = PostingsCursor::new(&merged2).unwrap();
    for p in want.iter().step_by(11) {
        assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p));
    }
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
    // Remover devolve os tamanhos e deixa o bloco igual ao decodificado.
    let mut block = decoded;
    assert_eq!(block.remove(900), Some(l(7, 0, 0)));
    assert_eq!(block.remove(900), None, "já removido");
    assert_eq!(block.remove(3), None, "nunca existiu");
    assert_eq!(block.len(), 2);
    assert_eq!(NormBlock::decode(&block.encode()).unwrap(), block);
    assert_eq!(block.remove(2), Some(l(0, 0, 0)));
    assert_eq!(block.remove(5), Some(l(40, 2, 5)));
    assert!(block.is_empty());
    assert_eq!(block, NormBlock::default());
}

fn tombstones(docs: &[DocId]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_tombstones_into(&mut buf, docs);
    buf
}

#[test]
fn tombstones_are_dropped_by_the_full_merge_only() {
    let t = tombstones(&[2, 9]);
    assert!(ListHeader::parse(&t, true).unwrap().tombstones);
    assert!(decode_postings(&t).is_err(), "uma busca nunca aceita lápide");
    assert!(decode_docs(&t).is_err());
    assert!(PostingsCursor::new(&t).is_err());
    let base = encode_postings(&[p(1, 1), p(2, 3), p(5, 1)]);
    assert_eq!(merged(&[vec![p(1, 1), p(2, 3), p(5, 1)]]), [p(1, 1), p(2, 3), p(5, 1)]);
    let full = merge_postings([base.as_slice(), t.as_slice()].into_iter()).unwrap();
    assert_eq!(decode_postings(&full).unwrap(), [p(1, 1), p(5, 1)]);
    assert!(!ListHeader::parse(&full, true).unwrap().tombstones);
    // Parcial: a lápide de 9 (que não está em nenhum pedaço) também fica,
    // porque pode haver um valor mais antigo com o 9.
    let partial = merge_lists([base.as_slice(), t.as_slice()].into_iter(), ListKind::Postings, false).unwrap();
    assert!(ListHeader::parse(&partial, true).unwrap().tombstones);
    assert_eq!(list_len(&partial).unwrap(), 4);
    // Lápide e depois a entrada de novo: o mais novo vence.
    let again = encode_postings(&[p(2, 7)]);
    let back = merge_postings([base.as_slice(), t.as_slice(), again.as_slice()].into_iter()).unwrap();
    assert_eq!(decode_postings(&back).unwrap(), [p(1, 1), p(2, 7), p(5, 1)]);
    // Todos removidos: lista vazia (2 bytes), não ausente.
    let gone = merge_postings([base.as_slice(), tombstones(&[1, 2, 5]).as_slice()].into_iter()).unwrap();
    assert_eq!(gone, [0, 0]);
    // As mesmas lápides servem para `pos:`.
    let pos = pos_bytes(&[(2, &[4, 8]), (3, &[1])]);
    let pos_full = merge_positions([pos.as_slice(), t.as_slice()].into_iter()).unwrap();
    assert_eq!(decode_positions(&pos_full).unwrap(), [(3, vec![1])]);
    // Uma lápide numa lista que não se diz com lápides é corrompida.
    let mut fake = encode_postings(&[p(1, 1)]);
    *fake.last_mut().unwrap() = 0;
    assert!(merge_postings([base.as_slice(), fake.as_slice()].into_iter()).is_none());
}

/// O que cada documento tem no termo: tf, tf do título e posições.
type Model = BTreeMap<DocId, (u32, u32, Vec<u32>)>;

/// Um pedaço gravado por um lote: entradas novas ou lápides.
enum Piece {
    Add(Model),
    Remove(Vec<DocId>),
}

impl Piece {
    fn encode(&self, kind: ListKind) -> Vec<u8> {
        match (self, kind) {
            (Piece::Add(m), ListKind::Postings) => {
                encode_postings(&m.iter().map(|(&d, &(tf, title, _))| pt(d, tf, title)).collect::<Vec<_>>())
            }
            (Piece::Add(m), ListKind::Positions) => {
                pos_bytes(&m.iter().map(|(&d, (_, _, ps))| (d, ps.as_slice())).collect::<Vec<_>>())
            }
            (Piece::Remove(docs), _) => tombstones(docs),
        }
    }

    fn apply(&self, model: &mut Model) {
        match self {
            Piece::Add(m) => model.extend(m.iter().map(|(&d, v)| (d, v.clone()))),
            Piece::Remove(docs) => docs.iter().for_each(|d| drop(model.remove(d))),
        }
    }
}

/// Une `lists` com merges parciais em pontos aleatórios e um completo no fim,
/// como o RocksDB pode fazer entre compactações e leituras.
fn merge_in_random_groups(lists: &[Vec<u8>], kind: ListKind, next: &mut impl FnMut(u64) -> u64) -> Vec<u8> {
    let mut level: Vec<Vec<u8>> = lists.to_vec();
    while level.len() > 1 && next(3) > 0 {
        let start = next(level.len() as u64) as usize;
        let end = start + 1 + next((level.len() - start) as u64) as usize;
        let group = merge_lists(level[start..end].iter().map(Vec::as_slice), kind, false).unwrap();
        level.splice(start..end, [group]);
    }
    merge_lists(level.iter().map(Vec::as_slice), kind, true).unwrap()
}

/// Sequências aleatórias de entradas e lápides, com bases grandes (caminho do
/// remendo) e pequenas: o merge completo direto, o merge depois de merges
/// parciais aleatórios e um `BTreeMap` de referência dão o mesmo conteúdo.
#[test]
fn merges_with_tombstones_match_a_reference_model() {
    let mut next = rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..250 {
        let universe = [20, 300, 3000][round % 3];
        let mut model = Model::new();
        let mut pieces = Vec::new();
        for i in 0..1 + next(6) {
            // O primeiro pedaço costuma ser grande: a base de um termo frequente.
            let size = if i == 0 { next(universe) + 1 } else { next(universe / 10 + 2) + 1 };
            let docs: Vec<DocId> = {
                let mut d: Vec<DocId> = (0..size).map(|_| next(universe)).collect();
                d.sort_unstable();
                d.dedup();
                d
            };
            let piece = if i > 0 && next(2) == 0 {
                Piece::Remove(docs)
            } else {
                let entries = docs.into_iter().map(|d| {
                    let tf = 1 + next(3) as u32;
                    let title = if next(4) == 0 { 1 } else { 0 };
                    (d, (tf, title, (0..tf).map(|k| k * 7 + next(5) as u32).collect()))
                });
                Piece::Add(entries.collect())
            };
            piece.apply(&mut model);
            pieces.push(piece);
        }
        let want_post: Vec<Posting> = model.iter().map(|(&d, &(tf, title, _))| pt(d, tf, title)).collect();
        let want_pos: Vec<(DocId, Vec<u32>)> = model.iter().map(|(&d, (_, _, ps))| (d, ps.clone())).collect();
        for kind in [ListKind::Postings, ListKind::Positions] {
            let lists: Vec<Vec<u8>> = pieces.iter().map(|p| p.encode(kind)).collect();
            let direct = merge_lists(lists.iter().map(Vec::as_slice), kind, true).unwrap();
            let grouped = merge_in_random_groups(&lists, kind, &mut next);
            for list in [&direct, &grouped] {
                assert!(!ListHeader::parse(list, true).unwrap().tombstones, "rodada {round}");
                match kind {
                    ListKind::Postings => {
                        assert_eq!(decode_postings(list).unwrap(), want_post, "rodada {round}");
                        let mut cursor = PostingsCursor::new(list).unwrap();
                        for p in want_post.iter().step_by(5) {
                            assert_eq!(cursor.seek(p.doc).unwrap(), Some(*p), "rodada {round}");
                        }
                    }
                    ListKind::Positions => {
                        assert_eq!(decode_positions(list).unwrap(), want_pos, "rodada {round}");
                        let mut cursor = PositionsCursor::new(list).unwrap();
                        let mut out = Vec::new();
                        for (d, ps) in want_pos.iter().step_by(5) {
                            assert!(cursor.seek(*d, &mut out).unwrap(), "rodada {round}");
                            assert_eq!(&out, ps);
                        }
                    }
                }
            }
        }
    }
}

/// O remendo só regrava os blocos que o pedaço novo toca: remover um
/// documento do meio de uma lista grande mantém os outros blocos iguais.
#[test]
fn patch_merge_copies_untouched_blocks() {
    let full = sample_list(3000, 21);
    let base = encode_postings(&full);
    let gone = full[1500].doc;
    let merged = merge_postings([base.as_slice(), tombstones(&[gone]).as_slice()].into_iter()).unwrap();
    let want: Vec<Posting> = full.iter().copied().filter(|p| p.doc != gone).collect();
    assert_eq!(decode_postings(&merged).unwrap(), want);
    let before = ListHeader::parse(&base, true).unwrap();
    let after = ListHeader::parse(&merged, true).unwrap();
    let untouched = |h: &ListHeader| h.entries.iter().filter(|e| e.count == SKIP_BLOCK as u64).count();
    assert_eq!(untouched(&after), untouched(&before) - 1, "só o bloco do documento removido muda");
    assert!(merged.len() < base.len());
    // Lista que cai para 256 entradas ou menos perde a tabela.
    let small = encode_postings(&full[..300]);
    let docs: Vec<DocId> = full[..60].iter().map(|p| p.doc).collect();
    let shrunk = merge_postings([small.as_slice(), tombstones(&docs).as_slice()].into_iter()).unwrap();
    assert!(ListHeader::parse(&shrunk, true).unwrap().blocks.is_empty());
    assert_eq!(decode_postings(&shrunk).unwrap(), full[60..300]);
}

/// A carga grava uma lista em pedaços crescentes; uma edição depois deles não
/// pode fazer o merge regravar a lista inteira: os pedaços viram a base do
/// remendo e só o bloco do documento editado muda.
#[test]
fn patch_over_a_list_loaded_in_pieces() {
    let full = sample_list(3000, 33);
    let mut pieces: Vec<Vec<u8>> = full.chunks(300).map(encode_postings).collect();
    let gone = full[1234].doc;
    pieces.push(tombstones(&[gone]));
    let merged = merge_postings(pieces.iter().map(Vec::as_slice)).unwrap();
    let want: Vec<Posting> = full.iter().copied().filter(|p| p.doc != gone).collect();
    assert_eq!(decode_postings(&merged).unwrap(), want);
    let concat = merge_postings(pieces[..10].iter().map(Vec::as_slice)).unwrap();
    let blocks = |b: &[u8]| ListHeader::parse(b, true).unwrap().entries;
    let (before, after) = (blocks(&concat), blocks(&merged));
    assert_eq!(before.len(), after.len());
    assert_eq!(before.iter().zip(&after).filter(|(a, b)| a != b).count(), 1, "só o bloco do documento removido muda");
}
