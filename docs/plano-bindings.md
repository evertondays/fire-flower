# Plano: bindings para JavaScript e PHP

Origem: o documento "fire-flower: plano de bindings para JS e PHP", escrito
sobre o commit `a03cca2` (27/10/2026). Este plano o adapta à `v0.2-release`,
que já tem parte do que ele pedia (edição e remoção, `open_read_only`, as
métricas) e registra as decisões tomadas ao implementar.

## Resumo

- **Bindings nativos no mesmo processo**, sem servidor: napi-rs para Node,
  Bun e Deno, e uma extensão PHP em Rust (ext-php-rs), finas sobre o core. Um
  servidor HTTP somaria dezenas a centenas de µs por consulta, mais que a
  própria busca (`t-mid` tem p50 de ~15 µs na Wikipédia inteira).
- **Custo da fronteira em sub-µs por chamada:** poucas chamadas grossas (uma
  por busca, uma por lote), resultados em typed arrays sem cópia, rótulos só
  quando pedidos, num `MultiGet`.
- **Caminho síncrono para buscas rápidas** e assíncrono para commit,
  compactação, abertura e consultas pesadas, para não travar o event loop.
- **Vários processos:** um escritor e leitores só leitura (PHP-FPM, cluster
  do Node), que reabrem quando o banco muda, no máximo a cada N ms.
- **Navegador (WASM) fica para depois:** o RocksDB não compila para wasm32.

## O que o código tem hoje e o que falta

| Achado do plano original | Situação na `v0.2-release` |
|---|---|
| `IndexBatch<'a>` guarda `&Engine` e um `MutexGuard`: não é `Send` nem `'static` | **Fase 0** |
| `Engine::batch()` bloqueia; falta `try_batch` | **Fase 0** |
| `label(doc)` faz um `get` por documento | **Fase 0**: `labels(&[DocId])` |
| O cache de época só é invalidado por commits do mesmo processo | cada reabertura do leitor nasce com caches vazios (**Fase 2**) |
| clap e ratatui são dependências da biblioteca | **Fase 0**: a CLI vira um crate do workspace |
| Nenhum `catch_unwind` | **Fase 1**: os bindings pegam o pânico na fronteira |
| Sem remoção nem atualização | já existe (plano A, formato v7) |
| Sem leitor só leitura | já existe (`Engine::open_read_only`) |
| Erros só com mensagem | **Fase 0**: `Error::code()` estável |

## Arquitetura

```text
fire-flower (core, na raiz)      Engine, OwnedBatch, Error::code, métricas
├── crates/fire-flower-cli       a CLI (clap, ratatui): tui, index, monitor…
├── bindings/node                napi-rs (cdylib) + camada TypeScript
└── bindings/php                 ext-php-rs (depois)
```

Cada binding chama o core direto em Rust, sem C ABI no meio. Um
`fire-flower-ffi` com cabeçalho C fica para quando houver demanda (PHP FFI,
Python, Go).

## Fase 0: core pronto para bindings

| Onde | Mudança | Por quê |
|---|---|---|
| `Cargo.toml` | workspace: o core continua na raiz; a CLI vai para `crates/fire-flower-cli` com clap e ratatui | bindings sem as dependências da CLI |
| `src/engine/writer/` | o lote pega o estado do escritor para si e o devolve no `Drop` (`WriterSlot`), em vez de guardar uma trava; `Engine::try_batch`; `Engine::owned_batch` e `try_owned_batch` devolvem um `OwnedBatch` (`Send + 'static`) | um lote vivendo num objeto JS/PHP e fazendo commit no pool de threads |
| `src/engine/mod.rs` | `labels(&[DocId])` com `MultiGet` | 1 leitura em lote em vez de k |
| `src/error.rs` | `#[non_exhaustive]`, `Error::code()` (`FF_DB`, `FF_CORRUPT`, `FF_CONFIG`, `FF_OPEN`, `FF_READ_ONLY`, `FF_BUSY`, `FF_CLOSED`), variantes `Busy` e `Closed` | exceções com código, independentes do idioma da mensagem |

**Decisões:**

- **O core fica na raiz:** o plano original o movia para `crates/fire-flower`.
  Mantê-lo na raiz não muda `src/`, `tests/`, `benches/`, `examples/` nem o
  `perf.py`; o efeito para os bindings é o mesmo.
- **serde e serde_json ficam no core:** o snapshot das métricas é JSON.
- **Sem `search_top_into`:** o binding monta os typed arrays tomando posse de
  dois `Vec` de tamanho k (k é pequeno: a conversão custa O(k) e nenhuma
  cópia do lado do JS). Volta se o harness do Node mostrar a alocação.
- **Sem `add_titled` com `&[u8]`:** o binding valida o UTF-8 de um `Buffer`
  (`str::from_utf8`, sem cópia) e chama a API de `&str`.

## Fase 1: Node (napi-rs)

`bindings/node`, pacote `@fire-flower/node`. O mesmo `.node` roda no Bun
(Node-API) e no Deno (`npm:`).

```typescript
import { Engine, q } from "@fire-flower/node";

const engine = await Engine.open("./indice", { languages: ["pt", "en"] });
await engine.index([{ id: 1, title: "Planeta", text: "..." }]); // um cruzamento, commit fora do event loop
const r = engine.searchTopSync("coração bras*", { k: 10 });
r.total; r.ids; r.scores;            // number, Float64Array, Float64Array (sem cópia)
for (const hit of r) hit.id;         // iterador sobre os typed arrays
const heavy = await engine.searchTop('"revolução francesa" 1789', { k: 20, labels: true });
engine.suggestSync("prog", 10);
engine.searchTopSync(q.and(q.term("copa"), q.phrase("copa do mundo")), { k: 10 });
```

- Busca e `suggest` síncronos (latência nativa); `Promise` para abrir,
  indexar, remover, compactar e consultas pesadas.
- **IDs como `number`:** os IDs vão até 2^40 − 1 (formato v7), abaixo de
  2^53: não precisa de `bigint`.
- Registro global por caminho: `worker_threads` do mesmo processo
  compartilham o `Engine` nativo e o block cache.
- `close()` e `Symbol.dispose`; depois de fechado, toda chamada dá `FF_CLOSED`.
- Erros: `FireFlowerError` com `code`.

## Fase 2: vários processos

- **`Reader` no core:** guarda o `Engine` só leitura atual e o troca ao
  reabrir; buscas em andamento terminam na instância antiga.
- **Quando reabrir:** *desvio do plano original*, que pedia um contador de
  commits publicado pelo escritor num arquivo. Todo commit já acrescenta ao
  WAL do RocksDB: o leitor compara o nome e o tamanho do WAL e do MANIFEST
  (uma listagem da pasta, no máximo a cada `refresh`) e reabre só se
  mudaram. O escritor não paga nada.
- Medir o custo de reabrir com WAL grande; testes com um escritor
  compactando enquanto leitores leem, em outro processo.

## Validação de performance

| Medida | Meta |
|---|---|
| Fase 0: A/B do core contra a `v0.2-release` | sem perda (seção 11 das diretrizes) |
| Chamada vazia (ping) no Node | ≤ 0,2 µs |
| `searchTopSync` k = 10, `t-mid` p50 | ≤ nativo + max(1 µs, 5%) |
| `rank-freq`, `phrase-freq` p50 | ≤ nativo + 2% |
| Rótulos de 10 resultados | ≤ 1 `MultiGet` nativo + 2 µs |
| Indexação pelo binding (lote 10k) | ≥ 97% do nativo |
| Busca assíncrona | medir e documentar o custo da troca de thread |
| Leitores em `worker_threads` | escala como o teste multi-thread do Rust |

O harness do Node (`bench/node/`) lê os mesmos `bench/queries/*/*.txt` e o
mesmo banco do `examples/wiki`, e grava JSON no formato do `perf.py`.

## Roadmap

1. **Fase 0:** core pronto para bindings; A/B sem perda.
2. **Fase 1:** Node (napi-rs), harness e metas medidas.
3. **Fase 2:** vários processos (`Reader`), reabertura com intervalo mínimo,
   testes entre processos.
4. **Fase 3:** PHP (ext-php-rs): `Engine::persistent`, iterador de hits,
   stubs, Composer, binários PIE. *Adiada: falta o PHP de desenvolvimento no
   ambiente.*
5. **Fase 4:** distribuição 1.0 (matriz de CI, npm, PIE, docs pt/en).
6. **Fase 5 (opcional):** armazenamento em trait, índice congelado e WASM.

## Perguntas em aberto

1. Quem escreve no índice em produção? Um processo dedicado resolve o `LOCK`
   do RocksDB; escrever a partir de um request PHP pede uma fila.
2. ~~IDs acima de 2^53?~~ Não: vão até 2^40 − 1.
3. Qual frescor os leitores precisam? Define o intervalo mínimo de reabertura
   (padrão: 1 s).
4. A verificar: leitor só leitura enquanto o escritor compacta e apaga SSTs,
   e o pico de memória durante a troca de instância.
5. ~~Remoção e atualização antes do 1.0?~~ Já existem.
6. Navegador é caso de uso real? Pede a maior mudança no core.
