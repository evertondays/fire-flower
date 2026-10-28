// Carga pelo binding Node: lê o JSONL e indexa em lotes (engine.index, commit
// no pool de threads), como o `wiki load --plain`.
//
//   node bench/node/load.js --db D --corpus F [--take N] [--batch 10000] [--label L]
"use strict";

const fs = require("node:fs");
const readline = require("node:readline");

const { args, binding, saveResult } = require("./common");

const opt = args();
const { Engine } = binding();

(async () => {
  const take = opt.take ? Number(opt.take) : Infinity;
  const batchSize = Number(opt.batch ?? 10_000);
  const engine = await Engine.open(opt.db);
  const lines = readline.createInterface({ input: fs.createReadStream(opt.corpus), crlfDelay: Infinity });
  let batch = [];
  let docs = 0;
  let bytes = 0;
  let indexSecs = 0;
  const started = process.hrtime.bigint();
  const flush = async () => {
    const t = process.hrtime.bigint();
    await engine.index(batch);
    indexSecs += Number(process.hrtime.bigint() - t) / 1e9;
    batch = [];
  };
  for await (const line of lines) {
    if (docs >= take) break;
    if (!line.trim()) continue;
    const a = JSON.parse(line);
    batch.push({ id: a.id, title: a.title ?? "", text: a.text ?? "" });
    bytes += (a.title ?? "").length + (a.text ?? "").length;
    docs++;
    if (batch.length >= batchSize) await flush();
  }
  if (batch.length) await flush();
  const seconds = Number(process.hrtime.bigint() - started) / 1e9;
  engine.close();
  const result = { binding: "node", db: opt.db, corpus: opt.corpus, batch: batchSize, docs, bytes,
    seconds, docs_per_s: docs / seconds, index_seconds: indexSecs };
  console.log(`${docs} documentos em ${seconds.toFixed(1)} s: ${result.docs_per_s.toFixed(0)} docs/s (index ${indexSecs.toFixed(1)} s)`);
  saveResult("load", opt.label ?? "node", result);
})();
