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
  // --mode: como o wiki bench --mode (top10 é o caminho de uma aplicação).
  if (typeof opt.mode === "string") set.mode = opt.mode;
  const counts = list(opt.threads).map(Number);
  (async () => {
    const results = [];
    for (const n of counts.length ? counts : [1, 2, 4]) {
      const run = async () => {
        // Cada worker carrega o addon e abre o banco antes; o relógio só
        // começa quando todos estão prontos (como as threads do wiki bench).
        const workers = Array.from({ length: n }, (_, i) => {
          const queries = set.queries.filter((_, j) => j % n === i);
          return new Worker(__filename, { workerData: { db: opt.db, mode: set.mode, queries } });
        });
        const message = (w) => new Promise((resolve, reject) => {
          w.once("message", resolve);
          w.once("error", reject);
        });
        await Promise.all(workers.map(message)); // prontos
        const started = process.hrtime.bigint();
        const done = workers.map(message);
        for (const w of workers) w.postMessage("go");
        const parts = await Promise.all(done);
        const wall = Number(process.hrtime.bigint() - started) / 1e9;
        for (const w of workers) w.terminate();
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
} else (async () => {
  const { binding } = require("./common");
  const { Engine } = binding();
  const engine = Engine.openSync(workerData.db);
  const samples = [];
  const { mode } = workerData;
  const run = (record) => {
    for (const q of workerData.queries) {
      const t = process.hrtime.bigint();
      if (mode === "suggest") engine.suggestSync(q, 10);
      else if (mode === "top10") engine.searchTopSync(q, { k: 10 });
      else engine.searchSync(q);
      if (record) samples.push(String(process.hrtime.bigint() - t));
    }
  };
  // Aquecimento dentro do worker: cada worker é um isolate novo do V8 (JIT
  // frio) e uma thread nova para o RocksDB.
  run(false);
  parentPort.postMessage("pronto");
  await new Promise((resolve) => parentPort.once("message", resolve));
  run(true);
  engine.close();
  parentPort.postMessage(samples);
})();
