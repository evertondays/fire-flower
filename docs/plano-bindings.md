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

## Resultado da Fase 0 (28/10/2026)

A/B completo (`perf.py all --scale full` e `edits --scale 100k`) contra a
`v0.2-release`, uma variante depois da outra: zero divergências nos cinco
oráculos.

| Medida | `v0.2-release` | `bindings-core` |
|---|---|---|
| Carga da Wikipédia inteira | 9.204 docs/s | 9.174 (−0,3%) |
| Carga de 100k | 6.299 docs/s | 6.401 (+1,6%) |
| Carga de 1 documento por commit (alternando os binários) | 355 docs/s | 356 |
| Edições, lote de 10 mil / de 1 | 7.209 / 1.046 ops/s | 7.228 / 1.050 |
| p99 de `mixed` com indexação (4 leitores) | 4.293 µs | 4.290 µs |
| Compactação da Wikipédia inteira | 4,98 s | 5,12 s (+2,7%) |

O `t-freq` apareceu de +7% a +10% em todos os estágios, e alternando os
binários continuou em +8,6% (301,6 → 327,6 µs; `and-freq` +3,6%). Com builds
de alinhamento fixo (`-align-all-functions=6 -align-all-nofallthru-blocks=6`)
a diferença some: 315,7 → 318,0 µs (+0,7%), `and-freq` +0,3%. É alinhamento
do código, como no A/B do monitoramento; a Fase 0 não perde nada.

## Resultado da Fase 1 (28/10/2026)

`perf.py bindings --scale full`: o binding contra o `wiki` (`--plain`, sem
PerfContext nem estatísticas), no mesmo banco, 3 rodadas alternadas.

| Medida | Meta | Resultado |
|---|---|---|
| Chamada vazia | ≤ 0,2 µs | 0,099 µs |
| `searchTopSync` k = 10, `t-mid` p50 | ≤ nativo + max(1 µs, 5%) | 56 µs contra 55 µs (+0,24 µs) |
| `rank-freq`, `phrase-freq` p50 (top 10) | ≤ nativo + 2% | +0,9% e −2,7% |
| Rótulos de 10 resultados | ≤ `MultiGet` nativo + 2 µs | +1,6 µs no mesmo processo; +3,2 µs contra o `wiki` |
| Carga 100k (lote 10k) | ≥ 97% | 89% a 94% (ver abaixo) |
| Busca assíncrona | medir | +50 µs (ida e volta ao pool do libuv): só para consultas de ms |
| Leitores em `worker_threads` (top 10 do `mixed`) | escala como o Rust | 1, 2, 4 e 8 workers: −7%, −4%, −4% e −18% do nativo |

O caminho até aí:

- **Método de classe do napi-rs: ~240 ns por chamada** (desembrulhar o
  objeto e conferir a classe). Com a feature `napi8`, mais ~120 ns de type
  tag. O addon expõe funções soltas sobre um handle (`External` lido direto,
  conferido por um número mágico): 364 → 99 ns.
- **Criar um `Float64Array` custa ~550 ns no Node 24**, inclusive no próprio
  JS (o buffer fica fora do heap, com contabilidade no GC), e o do napi-rs
  ainda é externo, com finalizador. `searchTopSync` escreve num buffer de
  trabalho do `Engine` JS e o `TopHits` copia os k resultados para `number[]`
  (~30 ns). **Desvio do plano original:** `TopHits.ids` e `scores` são
  `number[]`, não `Float64Array`. `searchSync` (todos os IDs) e as promessas
  continuam em `Float64Array`, com memória do V8.
- **Rótulos numa string só** (separados por `\0`): uma string criada na
  fronteira em vez de dez.
- **Carga:** com o próximo lote montado enquanto o anterior grava, o parse
  do JSON e a conversão das strings (1,5 s em 100k) ficam escondidos; o
  event loop passa 13,8 s de 18 s esperando o commit. O que falta para 97% é
  o próprio commit (o mesmo código Rust) rodar ~5% mais devagar dentro do
  processo do Node, disputando a CPU com o V8 e o GC; uma arena só do malloc
  piora (19,4 s). Fica como investigação.
- **Fronteira contra processo:** o harness roda cada consulta também dentro
  do addon, no processo do Node. A diferença para a chamada pelo JS (de 1,3
  a 3,8 µs nas consultas curtas) inclui o trabalho JS entre as buscas
  disputando o cache; a diferença para o `wiki` varia de rodada para rodada
  em ±5 µs.
