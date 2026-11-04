// Latência pelo binding Node, conjunto a conjunto, como o `wiki bench --plain`:
// mesmas consultas, mesma operação (search, suggest ou os 10 melhores), um
// aquecimento descartado. Mede também a chamada vazia, os rótulos e o custo
// do caminho assíncrono.
//
//   node bench/node/bench.js --db D --queries bench/queries/100k [--label L] [--passes 1]
"use strict";

const { args, list, binding, percentile, readSets, saveResult } = require("./common");

const opt = args();
const { Engine } = binding();
const engine = Engine.openSync(opt.db);
const passes = Number(opt.passes ?? 1);
// --mode: mede todo conjunto com a mesma operação, como o wiki bench --mode.
const forced = typeof opt.mode === "string" ? opt.mode : null;
const querySets = () => readSets(list(opt.queries)).map((s) => (forced ? { ...s, mode: forced } : s));

function run(set) {
  const samples = [];
  let results = 0;
  for (const q of set.queries) {
    const t = process.hrtime.bigint();
    if (set.mode === "suggest") results += engine.suggestSync(q, 10).length;
    else if (set.mode === "top10") results += engine.searchTopSync(q, { k: 10 }).total;
    else results += engine.searchSync(q).length;
    samples.push(process.hrtime.bigint() - t);
  }
  return { samples, results };
}

// A mesma consulta dentro do addon, no processo do Node: a diferença para a
// chamada pelo JS é a fronteira; para o wiki, o efeito de rodar dentro do Node.
const native = require(require("node:path").join(require("./common").ROOT, "bindings", "node", "fire-flower.node"));
const handle = native.openEngineSync(opt.db);
function inside(set) {
  native.nativeQueryNanos(handle, set.queries, set.mode); // aquecimento
  const ns = native.nativeQueryNanos(handle, set.queries, set.mode).map((x) => BigInt(Math.round(x)));
  return percentile(ns.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)), 0.5);
}

function summarize(set, samples, results, wall) {
  const sorted = samples.slice().sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const total = sorted.reduce((s, x) => s + x, 0n);
  return {
    set: set.name,
    mode: set.mode,
    queries: sorted.length,
    threads: 1,
    p50_us: percentile(sorted, 0.5),
    p95_us: percentile(sorted, 0.95),
    p99_us: percentile(sorted, 0.99),
    max_us: percentile(sorted, 1),
    mean_us: Number(total) / sorted.length / 1000,
    qps: sorted.length / wall,
    mean_results: results / sorted.length,
  };
}

const sets = [];
console.log("| conjunto | p50 µs | p95 µs | p99 µs | qps |");
console.log("|---|---|---|---|---|");
for (const set of querySets()) {
  run(set); // aquecimento
  let samples = [];
  let results = 0;
  const t = process.hrtime.bigint();
  for (let i = 0; i < passes; i++) {
    const r = run(set);
    samples = samples.concat(r.samples);
    results += r.results;
  }
  const s = summarize(set, samples, results, Number(process.hrtime.bigint() - t) / 1e9);
  s.inside_p50_us = inside(set);
  console.log(`| ${s.set} | ${s.p50_us.toFixed(1)} | ${s.p95_us.toFixed(1)} | ${s.p99_us.toFixed(1)} | ${s.qps.toFixed(0)} |`);
  sets.push(s);
}

// Uma chamada que não faz nada: o custo da fronteira em si.
function perCall(n, f) {
  for (let i = 0; i < n / 10; i++) f();
  const t = process.hrtime.bigint();
  for (let i = 0; i < n; i++) f();
  return Number(process.hrtime.bigint() - t) / n;
}
const noopNs = perCall(2_000_000, () => engine.noop());

// Rótulos dos 10 primeiros resultados de uma consulta frequente.
const top = engine.searchTopSync("brasil", { k: 10 });
const labelsUs = perCall(20_000, () => engine.labels(top.ids)) / 1000;

// O caminho assíncrono (pool do libuv) contra o síncrono, consulta a consulta.
async function asyncCost(set) {
  const sync = [];
  const asyncs = [];
  for (const q of set.queries) {
    let t = process.hrtime.bigint();
    engine.searchTopSync(q, { k: 10 });
    sync.push(process.hrtime.bigint() - t);
    t = process.hrtime.bigint();
    await engine.searchTop(q, { k: 10 });
    asyncs.push(process.hrtime.bigint() - t);
  }
  const p50 = (xs) => percentile(xs.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)), 0.5);
  return { set: set.name, sync_p50_us: p50(sync), async_p50_us: p50(asyncs) };
}

(async () => {
  const mid = querySets().find((s) => s.name === "t-mid");
  const async_ = mid ? await asyncCost(mid) : null;
  console.log(`chamada vazia: ${noopNs.toFixed(1)} ns · rótulos de ${top.length}: ${labelsUs.toFixed(2)} µs`);
  if (async_) console.log(`t-mid síncrono ${async_.sync_p50_us.toFixed(1)} µs · assíncrono ${async_.async_p50_us.toFixed(1)} µs`);
  saveResult("bench", opt.label ?? "node", {
    binding: "node",
    db: opt.db,
    threads: 1,
    passes,
    mode: forced,
    sets,
    noop_ns: noopNs,
    labels_10_us: labelsUs,
    async: async_,
  });
  engine.close();
})();
