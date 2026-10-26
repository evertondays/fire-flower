// Temporário: separa o custo de um prefixo em achar os termos e carregar as listas.
use std::time::Instant;
use fire_flower::{Config, Engine};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let engine = Engine::open(&a[1], Config::default()).unwrap();
    for set in &a[2..] {
        let qs: Vec<String> = std::fs::read_to_string(set).unwrap().lines().map(|l| l.trim_end_matches('*').to_string()).collect();
        for _ in 0..2 { for q in &qs { engine.suggest(q, 256).unwrap(); engine.search(&format!("{q}*")).unwrap(); } }
        let (mut t_bfs, mut t_all, mut t_top) = (vec![], vec![], vec![]);
        for q in &qs {
            let t = Instant::now(); std::hint::black_box(engine.suggest(q, 256).unwrap()); t_bfs.push(t.elapsed().as_micros());
            let t = Instant::now(); std::hint::black_box(engine.search(&format!("{q}*")).unwrap()); t_all.push(t.elapsed().as_micros());
            let t = Instant::now(); std::hint::black_box(engine.search_top(&format!("{q}*"), 10).unwrap()); t_top.push(t.elapsed().as_micros());
        }
        for v in [&mut t_bfs, &mut t_all, &mut t_top] { v.sort(); }
        let p = |v: &Vec<u128>, f: f64| v[((v.len() - 1) as f64 * f) as usize];
        println!("{set}: achar 256 termos p50 {} µs p99 {} | search p50 {} p99 {} | top10 p50 {} p99 {}", p(&t_bfs,0.5), p(&t_bfs,0.99), p(&t_all,0.5), p(&t_all,0.99), p(&t_top,0.5), p(&t_top,0.99));
    }
}
