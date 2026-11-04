// Peças comuns ao harness do binding Node: argumentos, percentis e o JSON no
// formato do perf.py (examples/wiki/env.rs).
"use strict";

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");

/** `--chave valor` e `--chave v1 v2` viram { chave: valor | [v1, v2] }. */
function args(argv = process.argv.slice(2)) {
  const out = {};
  let key = null;
  for (const a of argv) {
    if (a.startsWith("--")) {
      key = a.slice(2);
      out[key] = true;
    } else if (key) {
      out[key] = out[key] === true ? a : [].concat(out[key], a);
    }
  }
  return out;
}

function list(v) {
  return v === undefined || v === true ? [] : [].concat(v);
}

/** O addon do binding: o compilado em bindings/node, ou FIRE_FLOWER_NODE. */
function binding() {
  return require(process.env.FIRE_FLOWER_NODE || path.join(ROOT, "bindings", "node"));
}

/** Percentil `p` de nanossegundos ordenados, em µs (como o wiki bench). */
function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  return Number(sorted[Math.round((sorted.length - 1) * p)]) / 1000;
}

/** Os arquivos de consulta: `s-*` usam suggest, `rank-*` os 10 melhores. */
function readSets(paths) {
  const files = paths.flatMap((p) =>
    fs.statSync(p).isDirectory()
      ? fs.readdirSync(p).filter((f) => f.endsWith(".txt")).sort().map((f) => path.join(p, f))
      : [p],
  );
  return files.map((file) => {
    const name = path.basename(file, ".txt");
    const mode = name.startsWith("s-") ? "suggest" : name.startsWith("rank-") ? "top10" : "search";
    const queries = fs.readFileSync(file, "utf8").split("\n").filter((l) => l.trim() !== "");
    return { name, mode, queries };
  });
}

function commit() {
  const git = (...a) => execFileSync("git", a, { cwd: ROOT, encoding: "utf8" }).trim();
  try {
    const head = git("rev-parse", "--short", "HEAD");
    return git("status", "--porcelain", "--untracked-files=no") ? `${head}-modificado` : head;
  } catch {
    return "sem-git";
  }
}

/** Grava `data` em FF_BENCH_DIR/results, como o wiki faz. */
function saveResult(kind, label, data) {
  const dir = path.join(process.env.FF_BENCH_DIR || path.join(ROOT, "..", "fire-flower-bench"), "results");
  fs.mkdirSync(dir, { recursive: true });
  const when = new Date().toISOString().slice(0, 19).replace(/:/g, "-");
  const c = commit();
  const machine = { cores: os.cpus().length, node: process.version };
  const file = path.join(dir, `${when}-${c}-${kind}-${label}.json`);
  fs.writeFileSync(file, JSON.stringify({ ...data, kind, label, commit: c, when, machine }, null, 1));
  console.error(`resultado: ${file}`);
}

module.exports = { ROOT, args, list, binding, percentile, readSets, saveResult };
