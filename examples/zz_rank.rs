// Temporário: decodificar a lista vs. ranquear, por consulta de termo frequente.
use std::time::Instant;
use fire_flower::{Config, Engine};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let engine = Engine::open(&a[1], Config::default()).unwrap();
    let qs: Vec<String> = std::fs::read_to_string(&a[2]).unwrap().lines().map(String::from).collect();
    for _ in 0..2 { for q in &qs { engine.term_postings(q).unwrap(); engine.search_top(q, 10).unwrap(); } }
    let (mut d, mut r, mut p) = (vec![], vec![], vec![]);
    for q in &qs {
        let t = Instant::now(); std::hint::black_box(engine.postings(q).unwrap()); p.push(t.elapsed().as_micros());
        let t = Instant::now(); std::hint::black_box(engine.term_postings(q).unwrap()); d.push(t.elapsed().as_micros());
        let t = Instant::now(); std::hint::black_box(engine.search_top(q, 10).unwrap()); r.push(t.elapsed().as_micros());
    }
    for v in [&mut d, &mut r, &mut p] { v.sort(); }
    println!("só IDs p50 {} | lista com tf p50 {} | top10 p50 {}", p[p.len()/2], d[d.len()/2], r[r.len()/2]);
}
