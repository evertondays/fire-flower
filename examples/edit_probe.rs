//! Sonda da regressão de leitura depois de editar, sem o corpus da Wikipédia.
//!
//! Mede duas coisas:
//! - no codec, quanto o merge custa conforme a fração de blocos tocados e o
//!   número de operandos, contra decodificar a lista e contra filtrar um
//!   remendo enquanto decodifica;
//! - no motor, a latência de buscar e autocompletar uma lista frequente
//!   antes das edições, depois de 1.000 edições espalhadas e depois de
//!   compactar. Cada consulta é repetida para ver se o RocksDB guarda o
//!   resultado do merge.
//!
//!   DOCS=20000 EDITS=1000 cargo run --release --example edit_probe

use std::hint::black_box;
use std::time::{Duration, Instant};

use fire_flower::analysis::stem::Stemming;
use fire_flower::internals::{MergeTrace, decode_docs, encode_postings, encode_tombstones_into, merge_postings, set_merge_trace, take_merge_trace};
use fire_flower::{Config, Engine, Posting};

fn main() {
    if let Err(e) = run() {
        eprintln!("erro: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    codec_bench();
    engine_bench()?;
    Ok(())
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn ns_per(total: Duration, n: u64) -> f64 {
    total.as_nanos() as f64 / n as f64
}

/// Lista densa `1..=n`, o caso de um termo que está em todo documento.
fn dense_list(n: u64) -> Vec<u8> {
    let posts: Vec<Posting> = (1..=n).map(|doc| Posting::new(doc, 1)).collect();
    encode_postings(&posts)
}

fn tombstones(docs: &[u64]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_tombstones_into(&mut buf, docs);
    buf
}

fn time_it(iters: usize, mut f: impl FnMut()) -> Duration {
    f();
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f();
        samples.push(t.elapsed());
    }
    median(&mut samples)
}

fn codec_bench() {
    let n = env_u64("LIST", 100_000);
    let base = dense_list(n);
    let blocks = n.div_ceil(128);
    println!("codec: lista de {n} documentos, {blocks} blocos, {} KB", base.len() / 1024);

    let decode = time_it(30, || {
        black_box(decode_docs(&base).unwrap());
    });
    let copy = time_it(30, || {
        black_box(base.clone());
    });
    let identity = time_it(20, || {
        black_box(merge_postings(std::iter::once(base.as_slice())).unwrap());
    });
    println!(
        "  decodificar {:>8.1} µs ({:.2} ns/entrada)   copiar bytes {:>8.1} µs ({:.2} ns/entrada)   merge sem remendo {:>8.1} µs ({:.2} ns/entrada)",
        decode.as_secs_f64() * 1e6,
        ns_per(decode, n),
        copy.as_secs_f64() * 1e6,
        ns_per(copy, n),
        identity.as_secs_f64() * 1e6,
        ns_per(identity, n),
    );

    // Uma lápide por bloco tocado, num operando só.
    for &frac in &[0.01_f64, 0.05, 0.10, 0.25, 0.50, 1.0] {
        let touched = ((blocks as f64) * frac).round().max(1.0) as u64;
        let docs: Vec<u64> = (0..touched).map(|b| 1 + b * 128 + 64).filter(|&d| d <= n).collect();
        let op = tombstones(&docs);
        let dt = time_it(15, || {
            black_box(merge_postings([base.as_slice(), op.as_slice()].into_iter()).unwrap());
        });
        println!(
            "  merge, {:>4} blocos tocados ({:>5.1}%)  {:>8.1} µs ({:.2} ns/entrada)  {:>5.1}× o merge sem remendo",
            docs.len(),
            100.0 * docs.len() as f64 / blocks as f64,
            dt.as_secs_f64() * 1e6,
            ns_per(dt, n),
            dt.as_secs_f64() / identity.as_secs_f64(),
        );
    }

    // Mesmas 1.000 lápides espalhadas: um operando contra mil operandos.
    let spread: Vec<u64> = {
        let step = (n / 1000).max(1);
        (0..1000).map(|i| 1 + i * step).filter(|&d| d <= n).collect()
    };
    let one = tombstones(&spread);
    let many: Vec<Vec<u8>> = spread.iter().map(|d| tombstones(&[*d])).collect();
    let refs: Vec<&[u8]> = std::iter::once(base.as_slice()).chain(many.iter().map(Vec::as_slice)).collect();
    let as_one = time_it(15, || {
        black_box(merge_postings([base.as_slice(), one.as_slice()].into_iter()).unwrap());
    });
    let as_many = time_it(10, || {
        black_box(merge_postings(refs.iter().copied()).unwrap());
    });
    let clustered: Vec<u64> = (1..=1000).collect();
    let cluster_op = tombstones(&clustered);
    let as_cluster = time_it(15, || {
        black_box(merge_postings([base.as_slice(), cluster_op.as_slice()].into_iter()).unwrap());
    });
    println!(
        "  1000 lápides espalhadas, 1 operando   {:>8.1} µs    1000 operandos {:>8.1} µs ({:.1}×)    1000 lápides no começo {:>8.1} µs",
        as_one.as_secs_f64() * 1e6,
        as_many.as_secs_f64() * 1e6,
        as_many.as_secs_f64() / as_one.as_secs_f64(),
        as_cluster.as_secs_f64() * 1e6,
    );

    // O que a busca pagaria se aplicasse o remendo enquanto decodifica, sem regravar.
    let merged = merge_postings([base.as_slice(), one.as_slice()].into_iter()).unwrap();
    let merge_then_decode = time_it(15, || {
        let bytes = merge_postings([base.as_slice(), one.as_slice()].into_iter()).unwrap();
        black_box(decode_docs(&bytes).unwrap());
    });
    let filter = time_it(20, || {
        black_box(filter_docs(&base, &spread));
    });
    let decode_merged = time_it(20, || {
        black_box(decode_docs(&merged).unwrap());
    });
    println!(
        "  decodificar o resultado já mesclado {:>8.1} µs    merge + decodificar {:>8.1} µs    filtrar lápides na decodificação {:>8.1} µs ({:.2}× decodificar)",
        decode_merged.as_secs_f64() * 1e6,
        merge_then_decode.as_secs_f64() * 1e6,
        filter.as_secs_f64() * 1e6,
        filter.as_secs_f64() / decode.as_secs_f64(),
    );
    let got = filter_docs(&base, &spread);
    let expect = decode_docs(&merged).unwrap();
    assert_eq!(got, expect, "filtrar na decodificação divergiu do merge");
    println!();
}

/// Decodifica `base` e tira os documentos de `drop` (ordenado). É o custo de
/// um cursor que aplica lápides sem regravar a lista.
fn filter_docs(base: &[u8], drop: &[u64]) -> Vec<u64> {
    let docs = decode_docs(base).unwrap();
    let mut out = Vec::with_capacity(docs.len());
    let mut i = 0;
    for doc in docs {
        while i < drop.len() && drop[i] < doc {
            i += 1;
        }
        if i < drop.len() && drop[i] == doc {
            i += 1;
        } else {
            out.push(doc);
        }
    }
    out
}

fn text(id: u64, salt: u64, keep_fa: bool) -> String {
    let mut s = String::with_capacity(48);
    if keep_fa {
        s.push_str("fa fa ");
    }
    s.push_str(&format!("c{} m{} v{id}{salt}", (id + salt) % 30, (id + salt * 3) % 400));
    s
}

fn engine_bench() -> Result<(), Box<dyn std::error::Error>> {
    let docs = env_u64("DOCS", 20_000);
    let edits = env_u64("EDITS", 1_000);
    let dir = tempfile::tempdir()?;
    let config = Config {
        positions: Some(false),
        edits: Some(true),
        languages: Some(Vec::new()),
        stemming: Some(Stemming::Off),
        statistics: true,
        block_cache_bytes: 64 << 20,
        writer_cache_nodes: 200_000,
        ..Config::default()
    };
    let engine = Engine::open(dir.path(), config)?;
    println!("motor: {docs} documentos, posições desligadas, fwd: ligado");
    let t = Instant::now();
    let mut id = 1;
    while id <= docs {
        let mut batch = engine.batch();
        let end = (id + 999).min(docs);
        for doc in id..=end {
            batch.add_document(doc, &text(doc, 0, true))?;
        }
        batch.commit()?;
        id = end + 1;
    }
    println!("  carga em {:.2} s", t.elapsed().as_secs_f64());
    print_db(&engine, "depois da carga");
    measure(&engine, "limpa, antes de compactar");

    let step = (docs / edits).max(1);
    let mut ids = Vec::with_capacity(edits as usize);
    let mut doc = 1;
    while ids.len() < edits as usize && doc <= docs {
        ids.push(doc);
        doc += step;
    }
    let (p50, tombstones) = apply_edits(&engine, &ids, true);
    let dropped = ids.len() / 5;
    println!(
        "  {n} edições espalhadas (passo {step}): commit p50 {p50:.0} µs, {tombstones} lápides, {dropped} documentos sem fa",
        n = ids.len()
    );
    print_db(&engine, "depois das edições");
    let fa = measure(&engine, "espalhadas, antes de compactar");
    let expect = docs - dropped as u64;
    assert_eq!(fa, expect, "busca de fa depois das edições");

    // Termo que saiu de vários documentos: o autocompletar confere a lista.
    let suggest_fa = engine.suggest("f", 10)?;
    println!("  suggest(\"f\") = {suggest_fa:?}");

    let t = Instant::now();
    engine.compact();
    println!("  compactação em {:.2} s", t.elapsed().as_secs_f64());
    print_db(&engine, "depois de compactar");
    let fa = measure(&engine, "espalhadas, compactado");
    assert_eq!(fa, expect, "busca de fa depois de compactar");

    // Controle: edições concentradas no começo da lista, sobre a base já compactada.
    let clustered: Vec<u64> = (1..=edits.min(docs)).collect();
    let (p50, tombstones) = apply_edits(&engine, &clustered, false);
    println!("  {edits_n} edições nos primeiros documentos (fa continua): commit p50 {p50:.0} µs, {tombstones} lápides", edits_n = clustered.len());
    measure(&engine, "concentradas, antes de compactar");
    Ok(())
}

fn apply_edits(engine: &Engine, ids: &[u64], drop_some_fa: bool) -> (f64, usize) {
    let mut samples = Vec::with_capacity(ids.len());
    let mut tombstones = 0;
    for (i, &doc) in ids.iter().enumerate() {
        let keep = !(drop_some_fa && i % 5 == 0);
        let t = Instant::now();
        let mut batch = engine.batch();
        batch.add_document(doc, &text(doc, 1, keep)).unwrap();
        tombstones += batch.commit_with_stats().unwrap().tombstones;
        samples.push(t.elapsed());
    }
    let p50 = median(&mut samples).as_secs_f64() * 1e6;
    (p50, tombstones)
}

fn print_db(engine: &Engine, when: &str) {
    let mem = engine.property("rocksdb.cur-size-all-mem-tables").unwrap_or_default();
    let sst = engine.property("rocksdb.total-sst-files-size").unwrap_or_default();
    let files = engine.property("rocksdb.num-files-at-level0").unwrap_or_default();
    println!("  banco ({when}): memtable {mem} B, sst {sst} B, arquivos no L0 {files}");
}

/// Repete a consulta e mostra, em cada repetição, o tempo e o que o merge fez.
/// Devolve quantos documentos `fa` tem.
fn measure(engine: &Engine, label: &str) -> u64 {
    println!("  [{label}]");
    let mut fa_len = 0;
    for i in 0..4 {
        fa_len = time_one(engine, i, "search fa", || engine.search("fa").unwrap().len());
        time_one(engine, i, "search c0", || engine.search("c0").unwrap().len());
        time_one(engine, i, "search zz", || engine.search("zz").unwrap().len());
        time_one(engine, i, "suggest f", || engine.suggest("f", 10).unwrap().len());
        time_one(engine, i, "ranked fa", || engine.search_ranked("fa", 10).unwrap().total);
    }
    fa_len as u64
}

fn time_one(_engine: &Engine, iter: usize, name: &str, f: impl Fn() -> usize) -> usize {
    set_merge_trace(true);
    take_merge_trace();
    let t = Instant::now();
    let n = f();
    let dt = t.elapsed();
    let tr = take_merge_trace();
    set_merge_trace(false);
    println!(
        "    {name:<12} #{iter} {:>8.1} µs  resultados {n:<6}  {}",
        dt.as_secs_f64() * 1e6,
        fmt_trace(&tr),
    );
    n
}

fn fmt_trace(tr: &MergeTrace) -> String {
    if tr.full_calls == 0 && tr.partial_calls == 0 {
        return "merge 0".into();
    }
    format!(
        "full {} ({:.0} µs, base {}, ops {}, máx {}, saída {} KB)  parcial {} ({:.0} µs, ops {})",
        tr.full_calls,
        tr.full_ns as f64 / 1e3,
        tr.full_with_base,
        tr.full_operands,
        tr.max_operands,
        tr.full_out_bytes / 1024,
        tr.partial_calls,
        tr.partial_ns as f64 / 1e3,
        tr.partial_operands,
    )
}
