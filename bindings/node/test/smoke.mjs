// Teste rápido para runtimes sem o node:test completo (Bun, Deno): abre,
// indexa, busca, remove e fecha. Sai com erro se algo não bater.
import { createRequire } from "node:module";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const { Engine, FireFlowerError } = require("../index.js");

function check(ok, what) {
  if (!ok) throw new Error(`falhou: ${what}`);
}

const engine = await Engine.open(join(mkdtempSync(join(tmpdir(), "ff-smoke-")), "db"));
await engine.index([
  { id: 1, title: "Planeta Terra", text: "o terceiro planeta" },
  { id: 2, title: "Marte", text: "o planeta vermelho" },
]);
const top = engine.searchTopSync("planeta", { k: 10 });
check(top.total === 2 && top.ids instanceof Float64Array, "busca ranqueada");
check(engine.suggestSync("verm", 5)[0] === "vermelho", "autocompletar");
await engine.delete([2]);
check([...engine.searchSync("planeta")].join() === "1", "remoção");
engine.close();
try {
  engine.searchSync("planeta");
  check(false, "busca depois de fechar");
} catch (e) {
  check(e instanceof FireFlowerError && e.code === "FF_CLOSED", "FF_CLOSED");
}
console.log("ok");
