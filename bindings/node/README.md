# @fire-flower/node

O fire-flower no Node.js, no Bun e no Deno: busca full-text embutida, no
mesmo processo, sem servidor. É um addon nativo (napi-rs) fino sobre o core
em Rust: uma chamada por busca ou por lote, ~0,1 µs de fronteira numa
chamada vazia e ~1 µs numa busca. O plano e as metas de performance estão em
[docs/plano-bindings.md](../../docs/plano-bindings.md).

## Compilar

Precisa do Rust (o RocksDB é compilado do fonte; a primeira vez leva alguns
minutos) e do Node 18 ou mais novo:

```bash
cd bindings/node && npm run build && npm test
```

O `build` compila o crate `fire-flower-node` em release e copia a biblioteca
para `fire-flower.node`, que o `index.js` carrega.

## Uso

```javascript
const { Engine, q } = require("@fire-flower/node");

// Abre fora do event loop. O mesmo caminho devolve o mesmo banco nativo:
// worker_threads do processo compartilham índice e block cache.
const engine = await Engine.open("./indice", { languages: ["pt", "en"] });

// Um lote por chamada; o commit roda no pool de threads do libuv.
await engine.index([
  { id: 1, title: "Planeta Terra", text: "A Terra é o terceiro planeta…", label: "terra.md" },
  { id: 2, title: "Marte", text: Buffer.from("Marte é o planeta vermelho") }, // UTF-8, sem conversão
]);

// Caminho rápido, síncrono (µs).
const top = engine.searchTopSync("planeta verm*", { k: 10 });
top.total;  // quantos casaram
top.ids;    // number[]
top.scores; // number[]
for (const hit of top) console.log(hit.id, hit.score);

// Consultas pesadas (prefixo curto, frase frequente) no pool de threads,
// com os rótulos lidos num MultiGet só.
const heavy = await engine.searchTop('"sistema solar" pla*', { k: 20, labels: true });
heavy.labels; // (string | null)[]

engine.suggestSync("pla", 10);                     // ["planeta", …]
engine.searchSync("planeta");                      // todos os IDs, em ordem (Float64Array)
engine.searchTopSync(q.and(q.phrase("copa do mundo"), q.prefix("bra")));

await engine.index([{ id: 2, text: "Marte tem duas luas" }]); // o mesmo ID substitui
await engine.delete([1]);
await engine.compact();                            // limpa as versões mortas

engine.close(); // ou: using engine = Engine.openSync(…)
```

- **Síncrono ou `Promise`:** busca e autocompletar síncronos têm a latência
  nativa; `index`, `delete`, `compact`, `open` e `searchTop` vão para o pool
  de threads e não travam o event loop. `indexSync` e `deleteSync` existem
  para scripts e dão `FF_BUSY` se outro lote estiver aberto, em vez de
  esperar.
- **Gravação em fluxo:** `engine.writer({ batchSize })` acumula e grava a cada
  lote cheio (`await writer.add(doc)`, `await writer.flush()`).
- **Vários processos:** um processo grava; os outros abrem com
  `{ readOnly: true, refreshMs: 1000 }`, sem travar o banco, e reabrem sozinhos
  (no pool de threads) quando o banco muda, conferindo a pasta no máximo a cada
  `refreshMs`. `await engine.refresh()` confere na hora. Sem `refreshMs`, a
  visão é a da abertura.
- **IDs** são `number` inteiros de 0 a 2^40 − 1.
- **Erros** são `FireFlowerError` com `code` estável: `FF_OPEN`, `FF_BUSY`,
  `FF_READ_ONLY`, `FF_CLOSED`, `FF_CONFIG`, `FF_CORRUPT`, `FF_DB`,
  `FF_ARGUMENT` (argumento inválido) e `FF_PANIC` (um pânico do Rust pego na
  fronteira, em vez de derrubar o processo).
- **Métricas:** com `{ metrics: true }`, `engine.metrics()` devolve os
  contadores do motor (ver [docs/plano-monitoramento.md](../../docs/plano-monitoramento.md)).

## Medir

`python3 scripts/perf.py bindings --scale 100k` compara o binding com o
nativo no mesmo banco: latência de cada conjunto de consultas, chamada vazia,
rótulos, caminho assíncrono, leitores em `worker_threads` contra threads do
Rust e carga. O harness está em [`bench/node/`](../../bench/node/).
