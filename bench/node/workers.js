// Leitores em worker_threads: o mesmo banco nativo (um Engine por processo),
// N workers dividindo um conjunto, com a mesma operação do `wiki bench`
// (`mixed` é `search`). Compara com o `wiki bench --plain --threads N`.
//
//   node bench/node/workers.js --db D --queries bench/queries/100k/mixed.txt --threads 1 2 4 8 [--label L]
"use strict";

const { Worker, isMainThread, parentPort, workerData } = require("node:worker_threads");

if (isMainThread) {
  const { args, list, percentile, readSets, saveResult } = require("./common");
  const opt = args();
  const set = readSets(list(opt.queries))[0];
  const counts = list(opt.threads).map(Number);
  (async () => {
    const results = [];
    for (const n of counts.length ? counts : [1, 2, 4]) {
      const run = async () => {
        const started = process.hrtime.bigint();
        const parts = await Promise.all(
          Array.from({ length: n }, (_, i) =>
            new Promise((resolve, reject) => {
              const queries = set.queries.filter((_, j) => j % n === i);
              const w = new Worker(__filename, { workerData: { db: opt.db, mode: set.mode, queries } });
              w.once("message", resolve);
              w.once("error", reject);
            }),
          ),
        );
        const wall = Number(process.hrtime.bigint() - started) / 1e9;
        return { samples: parts.flat(), wall };
      };
      await run(); // aquecimento (e carga dos workers)
      const { samples, wall } = await run();
      const sorted = samples.map(BigInt).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
      const r = { set: set.name, threads: n, queries: sorted.length, p50_us: percentile(sorted, 0.5),
        p99_us: percentile(sorted, 0.99), qps: sorted.length / wall };
      console.log(`${n} worker(s): ${r.qps.toFixed(0)} consultas/s, p50 ${r.p50_us.toFixed(1)} µs`);
      results.push(r);
    }
    saveResult("workers", opt.label ?? "node", { binding: "node", db: opt.db, sets: results });
  })();
} else {
  const { binding } = require("./common");
  const { Engine } = binding();
  const engine = Engine.openSync(workerData.db);
  const samples = [];
  const { mode } = workerData;
  for (const q of workerData.queries) {
    const t = process.hrtime.bigint();
    if (mode === "suggest") engine.suggestSync(q, 10);
    else if (mode === "top10") engine.searchTopSync(q, { k: 10 });
    else engine.searchSync(q);
    samples.push(String(process.hrtime.bigint() - t));
  }
  engine.close();
  parentPort.postMessage(samples);
}
