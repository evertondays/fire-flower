// Testes do binding Node: busca, indexação, remoção, rótulos, erros e fechar.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { Worker } = require("node:worker_threads");

const { Engine, FireFlowerError, TopHits, q } = require("..");

function tempDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "ff-node-"));
}

const DOCS = [
  { id: 1, title: "Planeta Terra", text: "A Terra é o terceiro planeta do sistema solar", label: "terra.txt" },
  { id: 2, title: "Marte", text: "Marte é o planeta vermelho", label: "marte.txt" },
  { id: 3, title: "Copa do Mundo", text: "A copa do mundo de futebol" },
];

test("indexa, busca e devolve typed arrays", async () => {
  const dir = tempDir();
  const engine = await Engine.open(path.join(dir, "db"));
  const result = await engine.index(DOCS);
  assert.deepEqual(result.docs, 3);

  const top = engine.searchTopSync("planeta", { k: 10 });
  assert.ok(top instanceof TopHits);
  assert.equal(top.total, 2);
  assert.ok(Array.isArray(top.ids) && Array.isArray(top.scores));
  assert.deepEqual([...top.ids].sort(), [1, 2]);
  assert.ok(top.scores[0] >= top.scores[1]);
  assert.deepEqual([...top].map((h) => h.id), [...top.ids]);

  const labeled = await engine.searchTop("planeta", { k: 1, labels: true });
  assert.equal(labeled.length, 1);
  assert.equal(labeled.labels.length, 1);
  assert.ok(["terra.txt", "marte.txt"].includes([...labeled][0].label));

  assert.deepEqual([...engine.searchSync("planet*")], [1, 2]);
  assert.deepEqual([...(await engine.search('"copa do mundo"'))], [3]);
  assert.deepEqual(engine.suggestSync("pla", 5), ["planeta"]);
  assert.deepEqual(engine.labels([2, 3, 99]), ["marte.txt", null, null]);
  assert.deepEqual(engine.labels([]), []);
  assert.equal(engine.stats().docCount, 3);
  assert.ok(engine.contains(1) && !engine.contains(4));
  engine.close();
});

test("substitui, remove e compacta", async () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  engine.indexSync(DOCS);
  const replaced = await engine.index([{ id: 2, text: "Marte tem duas luas" }]);
  assert.equal(replaced.replaced, 1);
  assert.deepEqual([...engine.searchSync("vermelho")], []);
  assert.deepEqual([...engine.searchSync("luas")], [2]);
  const deleted = await engine.delete([1, 404]);
  assert.equal(deleted.deleted, 1);
  assert.deepEqual([...engine.searchSync("terra")], []);
  const compacted = await engine.compact();
  assert.equal(compacted.purged, 2);
  assert.ok(compacted.emptiedWords > 0);
  engine.close();
});

test("texto em Buffer, sem conversão para string", () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  engine.indexSync([{ id: 7, text: Buffer.from("coração do brasil", "utf8") }]);
  assert.deepEqual([...engine.searchSync("coracao")], [7]);
  assert.throws(
    () => engine.indexSync([{ id: 8, text: Buffer.from([0xff, 0xfe]) }]),
    (e) => e instanceof FireFlowerError && e.code === "FF_ARGUMENT",
  );
  engine.close();
});

test("erros com código estável", async () => {
  const dir = tempDir();
  await assert.rejects(
    Engine.open(path.join(dir, "nada"), { readOnly: true }),
    (e) => e instanceof FireFlowerError && e.code === "FF_OPEN",
  );
  const engine = Engine.openSync(path.join(dir, "db"), { positions: false });
  engine.indexSync(DOCS);
  assert.throws(() => engine.searchSync('"copa do mundo"'), (e) => e.code === "FF_CONFIG");
  assert.throws(() => engine.indexSync([{ id: -1, text: "x" }]), (e) => e.code === "FF_ARGUMENT");
  assert.throws(() => engine.indexSync([{ id: 2 ** 41, text: "x" }]), (e) => e.code === "FF_CONFIG");

  const reader = Engine.openSync(path.join(dir, "db"), { readOnly: true });
  assert.equal(reader.readOnly, true);
  assert.deepEqual([...reader.searchSync("marte")], [2]);
  assert.throws(() => reader.indexSync([{ id: 9, text: "x" }]), (e) => e.code === "FF_READ_ONLY");
  reader.close();

  engine.close();
  assert.equal(engine.closed, true);
  assert.throws(() => engine.searchSync("marte"), (e) => e.code === "FF_CLOSED");
});

test("FF_BUSY quando outro lote está aberto", async () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  const big = Array.from({ length: 20_000 }, (_, i) => ({ id: i + 1, text: `documento número ${i} com texto` }));
  const pending = engine.index(big);
  // O lote assíncrono está no pool de threads; um síncrono não espera.
  let busy = false;
  for (let i = 0; i < 1000 && !busy; i++) {
    try {
      engine.indexSync([{ id: 99_999, text: "agora" }]);
    } catch (e) {
      busy = e.code === "FF_BUSY";
      if (!busy) throw e;
    }
  }
  await pending;
  assert.ok(busy, "o lote síncrono viu o assíncrono aberto");
  engine.close();
});

test("o mesmo caminho devolve o mesmo banco nativo", () => {
  const db = path.join(tempDir(), "db");
  const a = Engine.openSync(db);
  const b = Engine.openSync(db); // sem esbarrar no LOCK do primeiro
  a.indexSync(DOCS);
  assert.deepEqual([...b.searchSync("marte")], [2]);
  a.close();
  assert.deepEqual([...b.searchSync("marte")], [2], "b continua aberto");
  b.close();
});

test("worker_threads compartilham o banco", async () => {
  const db = path.join(tempDir(), "db");
  const engine = Engine.openSync(db);
  engine.indexSync(DOCS);
  const code = `
    const { parentPort, workerData } = require("node:worker_threads");
    const { Engine } = require(workerData.module);
    const engine = Engine.openSync(workerData.db);
    parentPort.postMessage([...engine.searchSync("planeta")]);
    engine.close();
  `;
  const found = await new Promise((resolve, reject) => {
    const w = new Worker(code, { eval: true, workerData: { db, module: path.resolve(__dirname, "..") } });
    w.once("message", resolve);
    w.once("error", reject);
  });
  assert.deepEqual(found, [1, 2]);
  engine.close();
});

test("writer grava em lotes", async () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  const writer = engine.writer({ batchSize: 2 });
  for (const doc of DOCS) await writer.add(doc);
  assert.equal(engine.stats().docCount, 2, "o terceiro ainda está no lote");
  await writer.flush();
  assert.equal(engine.stats().docCount, 3);
  engine.close();
});

test("construtor de consultas", () => {
  assert.equal(q.and(q.term("copa"), q.phrase('copa "do" mundo'), q.prefix("bra*")), 'copa "copa  do  mundo" bra*');
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  engine.indexSync(DOCS);
  assert.deepEqual([...engine.searchTopSync(q.and(q.phrase("copa do mundo"), q.prefix("fut"))).ids], [3]);
  engine.close();
});

test("rótulos com qualquer caractere", () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"));
  engine.indexSync([
    { id: 1, text: "a", label: "" },
    { id: 2, text: "b", label: "com\0nulo" },
    { id: 3, text: "c", label: "\u0001" },
    { id: 4, text: "d", label: "ação 🌸" },
  ]);
  assert.deepEqual(engine.labels([1, 4, 5]), ["", "ação 🌸", null]);
  assert.deepEqual(engine.labels([2, 3, 5]), ["com\0nulo", "\u0001", null], "volta para a lista");
  engine.close();
});

test("métricas pelo binding", () => {
  const engine = Engine.openSync(path.join(tempDir(), "db"), { metrics: true });
  engine.indexSync(DOCS);
  engine.searchTopSync("planeta");
  const m = engine.metrics();
  assert.equal(m.writer.documents, 3);
  assert.equal(m.queries.find((x) => x.op === "search_ranked").count, 1);
  engine.close();
});

test("leitor com refreshMs acompanha um escritor em outro processo", async () => {
  const { spawn } = require("node:child_process");
  const db = path.join(tempDir(), "db");
  const first = Engine.openSync(db);
  first.indexSync([{ id: 1, text: "alfa comum" }]);
  first.close(); // solta o LOCK para o escritor do outro processo
  // O escritor é outro processo (o LOCK do RocksDB é por processo).
  const writer = spawn(process.execPath, ["-e", `
    const { Engine } = require(${JSON.stringify(path.resolve(__dirname, ".."))});
    const engine = Engine.openSync(${JSON.stringify(db)});
    (async () => {
      for (let i = 1; i <= 30; i++) {
        await engine.index([{ id: 1, text: "alfa comum versão " + i },
          { id: i * 2, text: "beta comum" }, { id: i * 2 + 1, text: "gama comum" }]);
        await new Promise((r) => setTimeout(r, 10));
      }
      engine.close();
    })();
  `]);
  const exited = new Promise((resolve) => writer.once("exit", resolve));
  const reader = await Engine.open(db, { readOnly: true, refreshMs: 20 });
  let done = false;
  exited.then(() => (done = true));
  let last = 1;
  while (!done) {
    const found = reader.searchSync("comum").length;
    assert.equal(found % 2, 1, `um commit pela metade: ${found}`);
    assert.ok(found >= last, "o leitor voltou no tempo");
    last = found;
    await new Promise((r) => setTimeout(r, 5));
  }
  assert.equal(await exited, 0);
  await reader.refresh();
  assert.equal(reader.searchSync("comum").length, 61);
  assert.ok(last > 1, "viu commits durante a escrita");
  reader.close();
});
