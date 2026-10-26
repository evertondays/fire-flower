# Plano de Validação de Performance — fire-flower (Rust + RocksDB)

Plano incremental para medir, validar e otimizar o **fire-flower** usando a
**Wikipédia em português** (1,1 milhão de artigos, já baixada) como carga
principal.

**O que mudou em relação à versão anterior**

- O corpus deixou de ser hipotético: a Wikipédia já está em disco, com fatias
  de 1k, 10k e 100k documentos para subir a carga aos poucos.
- Entrou uma fase de **corretude em escala** antes da de performance: o índice
  é comparado com um índice ingênuo em memória.
- As consultas vêm do próprio corpus, separadas pelo que custa caro *neste*
  motor: frequência do termo, comprimento e prefixo.
- Saíram itens que o motor ainda não tem (busca por frase, OR, update/delete),
  e entraram os riscos que ele tem: um `Get` por caractere, merge operator e
  limite de expansão de prefixo.
- O `db_bench` virou fase opcional, e os comandos foram corrigidos
  (`cargo install criterion` não existe; criterion é só dependência).
- Foram considerados os limites da máquina: WSL2, 7,7 GB de RAM e 12 núcleos.

---

## Como rodar

Tudo está implementado em `examples/wiki.rs` (o harness) e orquestrado por
`scripts/perf.py`. A escala define a maior fatia usada; comece pequeno, porque
a Wikipédia inteira leva mais de uma hora.

```bash
python3 scripts/perf.py tudo --escala 10k     # todas as fases, ~7 min
python3 scripts/perf.py pr --escala 10k       # checagem rápida contra a linha de base
python3 scripts/perf.py frases --escala 10k   # só frases e ranking (carga com/sem posições, oráculo, latência)
python3 scripts/perf.py relatorio --escala 10k
```

Ao salvar uma linha de base nova (`tudo`, ou `pr --salvar-baseline`), a
anterior, de outro commit, fica guardada como
`bench/baseline-pr-<escala>-<commit>.json`.

Os resultados ficam em `bench/results/` (um JSON por medição, com commit e
data) e o resumo legível em [`bench/RESULTADOS.md`](../bench/RESULTADOS.md).

## Achados da primeira linha de base (escala 10k, 25/10/2026)

Números em [`bench/RESULTADOS.md`](../bench/RESULTADOS.md).

1. **Corretude:** zero divergências do oráculo em todos os 288 mil termos, 5
   mil consultas AND, 2 mil prefixos e autocompletares e mil títulos.
   Idempotência, lotes vs. único e reabrir também batem.
2. **Prefixo incompleto é a regra, não a exceção:** cerca de metade dos
   prefixos sorteados casa mais de 256 termos, e o resultado de `pre*` fica
   incompleto. Já com 10k documentos.
3. **Custo por caractere confirmado:** Gets por consulta = comprimento + 2
   (`t-len-3` → 5, `t-len-8` → 10, `t-len-15` → 17). Prefixos curtos fazem de
   640 a 840 Gets e levam ~2 ms. É o argumento medido para o índice ordenado
   `term:<palavra>`.
4. **Merge pendente pesa:** antes da compactação, `t-freq` tem p50 de 85 µs;
   depois, 60 µs (−30%). Reindexar a fatia dobra o banco (17,6 → 27,6 MB) até
   compactar (16,3 MB).
5. **Risco para a escala 1,1M, o block cache:** com 64 MB, toda leitura vai ao
   disco (~1 block read por Get, p50 15× pior), mesmo com o banco de 16 MB
   cabendo no cache. O cache LRU se divide em 64 partes, e o filtro de Bloom
   do SST (~1,3 MB num bloco só) não cabe numa parte de 1 MB. Com 256 MB as
   partes têm 4 MB e cabe. Na Wikipédia inteira os SSTs são maiores e o filtro
   pode passar de 4 MB. **Confirmado e corrigido (25/10):** na carga inteira a
   velocidade caía de ~5.000 para ~200 artigos/s em 200k, 500k ou 600k
   artigos (quando a compactação gerava um SST de chaves `node:` com filtro de
   ~6 MB); o RocksDB chegou a contar 22 TB de filtro inserido no cache. Agora
   filtros e índices são particionados (blocos de 4 KB). Cenário de teste:
   `perf.py carga-longa` (com `--reproduzir`, roda antes a config antiga, que
   tem que colapsar) e o teste `bloom_filters_larger_than_a_cache_shard_...`.
6. **Lotes maiores indexam mais rápido:** lote de 10.000 dá +26% docs/s, mas
   o pico de RSS vai de 160 para 300 MB.
7. **Leitura escala:** `misto` vai de 4,9 mil qps (1 thread) a 30,6 mil qps
   (12 threads), 6,3×. Indexar ao mesmo tempo não mexeu no p99 nesta escala,
   mas o volume escrito foi pequeno (1.000 docs).
8. **Ruído:** docs/s variou ~10% entre execuções iguais (953 vs. 860). O
   limite de 10% do `pr` pode dar falso alarme na carga; o p50 das buscas foi
   mais estável.

## 1. O que estamos testando

O motor guarda os termos numa Trie persistida no RocksDB (um registro por nó)
e as listas de postagem com merge operator. Os testes miram o custo real de
cada operação:

| Operação | Caminho no código | Custo esperado | O que pode dar errado em escala |
|---|---|---|---|
| Indexar lote | `IndexBatch::add_document` + `commit` | tokenizar + percorrer a Trie por termo único + 1 `merge` por termo | o cache de nós do lote é descartado a cada commit, então os nós quentes são lidos e decodificados de novo em todo lote |
| Termo exato | `View::postings` → `find_node` | **1 `Get` por caractere** + 1 `Get` da lista | a raiz (`node:0`) é decodificada em toda consulta e pode ter milhares de filhos (a Wikipédia tem muitos alfabetos) |
| Termo frequente | `load_postings` | decodificar ~1 milhão de varints ("de", "a", "o") | listas de MB; operandos de merge ainda não compactados são unidos *em toda leitura* |
| AND (`a b c`) | `search` + `intersect` | começa pela lista menor | sempre carrega todas as listas inteiras, mesmo a de "de" |
| Prefixo (`port*`) | `prefix_postings` → busca em largura na Trie | 1 `Get` por nó visitado | `a*` para em 256 termos (`max_prefix_terms`): **resultado incompleto** |
| Autocompletar | `suggest` | idem, até `limit` | busca em largura em prefixos curtos |
| Abrir o banco | `Engine::open` | leitura de 2 contadores | tempo de recuperação do WAL após carga grande |

**Fora do escopo por enquanto** (o motor não tem): OR, remoção e
atualização. Busca por frase e ranking BM25 já existem (ver
[plano-frases-e-ranking.md](plano-frases-e-ranking.md)) e têm conjuntos
próprios (`frase-*`, `rank-*`).

---

## 2. Corpus: Wikipédia em português

| Arquivo (`~/datasets/ptwiki/`) | Artigos | Tamanho | Uso |
|---|---|---|---|
| `ptwiki-1000.jsonl` | 1.000 | 21 MB | smoke test, iteração rápida |
| `ptwiki-10000.jsonl` | 10.000 | 93 MB | oráculo completo, por PR |
| `ptwiki-100000.jsonl` | 100.000 | 471 MB | profiling, varreduras de configuração |
| `ptwiki.jsonl` | 1.112.246 | 2,7 GB (2,65 GB de texto) | carga total, concorrência |

- **Formato:** um JSON por linha, `{"id": u64, "title", "url", "text"}`.
- **Fonte:** Hugging Face `wikimedia/wikipedia`, snapshot `20231101.pt`, com o
  texto já limpo (sem wikitext). Os parquets originais ficam em `raw/`.
- **Como regerar:** `scripts/ptwiki_corpus.py`.
- **Fatias:** cada uma é um prefixo da seguinte (`head -n N`), então os
  resultados das fatias menores são comparáveis com os das maiores.
- **IDs:** o `id` é o page id da Wikipédia, único, mas **esparso**. Isso afeta
  o tamanho das listas, que guardam deltas (ver Fase 3).
- **O que indexar:** `title + "\n" + text`. Assim, buscar o título de um
  artigo tem que trazer o próprio artigo, o que serve de checagem de
  revocação.
- **Pendente:** confirmar que os IDs são únicos. A checagem foi interrompida e
  será refeita pelo `wiki vocab`, que lê o corpus inteiro de qualquer forma.

O corpus está em disco e não toca o código; o resto do plano depende dele.

---

## 3. Regras do ambiente

A máquina é um WSL2 com 7,7 GB de RAM, 12 núcleos e disco ext4 em VHD.
Números de outra máquina não são comparáveis com os daqui.

- **Compilar com poucos jobs.** Com 12 jobs, compilar o RocksDB em C++ chega
  perto de estourar a RAM. Use `.cargo/config.toml` com `[build] jobs = 4`.
- **Nada pesado em paralelo** durante as medições: outra compilação, outra
  sessão rodando `cargo check`, Jellyfin transcodificando.
- **Banco e corpus no disco Linux** (`~/…`), nunca em `/mnt/c`, que é lento
  (9p) e distorce I/O.
- **Sempre build release**, num commit fixo. Registre o hash do commit, a
  `Config` usada e o resultado de `free -h` em cada rodada.
- **Cache quente vs. frio.** Para frio, reabra o banco depois de
  `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches`. Para quente, descarte a
  primeira passada.
- **Teto de memória:** a carga total precisa caber sem usar swap. Uso de swap
  conta como regressão.

---

## 4. Fase 0 — Ferramentas e harness

### Ferramentas

```bash
cargo install hyperfine        # comparar comandos inteiros
cargo install samply           # profiler de CPU
sudo apt install heaptrack     # perfil de alocações
```

- `/usr/bin/time -v` dá o pico de RSS ("Maximum resident set size").
- `cargo flamegraph` precisa do `perf`, e o `perf` do apt não corresponde ao
  kernel do WSL2. Use o **samply**; se ele falhar no WSL, compile o `perf` a
  partir do fonte do kernel da Microsoft.
- Criterion **não** se instala com `cargo install`: ele entra como
  dev-dependency.

### Cargo.toml

```toml
[dev-dependencies]
tempfile = "3"
criterion = { version = "0.5", features = ["html_reports"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[[bench]]
name = "micro"
harness = false

# Release com símbolos, só para profiling. Não altera o `release` normal.
[profile.profiling]
inherits = "release"
debug = true
```

`lto` e `codegen-units = 1` ficam como **experimento separado** (Fase 6). Eles
deixam a compilação mais lenta e mais pesada, e o ganho precisa ser medido,
não assumido.

### Ganchos no motor (mudanças pequenas)

O `Engine` descarta as `Options` depois do `open`, então hoje não há como ler
as estatísticas do RocksDB. É preciso:

1. `Config::statistics: bool`. Quando ligado, chamar `opts.enable_statistics()`
   e `opts.set_statistics_level(StatsLevel::ExceptTimers)`, e guardar as
   `Options` dentro do `Engine`.
2. `Engine::rocksdb_stats() -> Option<String>`, que devolve
   `opts.get_statistics()`.
3. `Engine::property(name) -> Option<String>`, que repassa
   `db.property_value(...)` (`rocksdb.stats`, `rocksdb.estimate-num-keys`,
   `rocksdb.total-sst-files-size`, `rocksdb.num-files-at-level0` etc.).
4. `Engine::compact()`, que chama `db.compact_range::<&[u8], &[u8]>(None, None)`.

### Harness: `examples/wiki.rs`

Um único binário com subcomandos, todos lendo o JSONL:

| Subcomando | O que faz | Saída |
|---|---|---|
| `load --db D --corpus F [--batch N] [--skip K] [--take N] [--remap-ids]` | indexa e imprime progresso a cada 10k docs (docs/s, MB/s, RSS, tamanho do banco) | `results/load-*.json` |
| `vocab --corpus F` | tokeniza com `fire_flower::text::tokenize` e conta df (em quantos docs) e comprimento de cada termo; confere IDs únicos | `vocab.tsv` |
| `queries --vocab V --corpus F --seed 42` | gera os conjuntos da Fase 4 | `bench/queries/*.txt` |
| `oracle --db D --corpus F` | compara o motor com um índice ingênuo em memória (Fase 1) | divergências |
| `bench --db D --queries Q [--threads T] [--cold]` | roda as consultas e mede latência e PerfContext | `results/bench-*.json` |
| `sizes --db D` | percorre o banco somando chaves e valores por espaço (`node:`, `post:`, `meta:`) e lista os 20 maiores valores | tabela |

Usar `fire_flower::text::tokenize` no `vocab` e no `oracle` garante que o oráculo
normaliza exatamente como o motor.

---

## 5. Fase 1 — Corretude em escala

Velocidade não vale nada se o resultado estiver errado. Esta fase roda antes
de qualquer medição de performance, e o critério é **zero divergências**.

| Teste | Fatia | Como | Esperado |
|---|---|---|---|
| Listas de postagem | 10k | índice ingênuo `HashMap<termo, BTreeSet<id>>`, comparando **todos** os termos | idênticas |
| Listas (amostra) | 100k | 50k termos sorteados, estratificados por df | idênticas |
| AND | 10k e 100k | 5k consultas de 2 a 5 termos; o oráculo faz a interseção | idênticas |
| Prefixo | 10k | o oráculo filtra o vocabulário por prefixo | idêntico quando ≤ 256 termos; quando passar disso, **registrar o tamanho da diferença** |
| Autocompletar | 10k | o oráculo ordena por (comprimento, alfabeto) | idêntico |
| Título → artigo | 100k | 1.000 títulos sorteados; o resultado tem que conter o próprio artigo | 100% |
| Normalização | qualquer | "São Paulo", "sao paulo" e "SÃO PAULO" | mesmos resultados |
| Reabrir | 100k | carregar, fechar, abrir e repetir as consultas; carregar mais 1k docs | contadores corretos, nada sobrescrito |
| Idempotência | 10k | indexar a fatia duas vezes | mesmos resultados; anotar o tamanho do banco antes e depois de `compact()` |
| Lotes vs. único | 1k | `--batch 1` vs. `--batch 1000` | bancos com o mesmo conteúdo lógico |

---

## 6. Fase 2 — Escada de indexação (bulk)

Indexar as fatias em ordem, cada uma num banco novo:

```bash
/usr/bin/time -v target/release/examples/wiki load \
  --db ~/bench-db/ptwiki-100k --corpus ~/datasets/ptwiki/ptwiki-100000.jsonl --batch 1000
```

| Fatia | Tempo | docs/s | MB/s | Pico RSS | Banco (MB) | Banco ÷ texto | Nós da Trie | Termos únicos | Write amp | Stalls |
|---|---|---|---|---|---|---|---|---|---|---|
| 1k | | | | | | | | | | |
| 10k | | | | | | | | | | |
| 100k | | | | | | | | | | |
| 1,1M | | | | | | | | | | |

- O número de nós sai de `meta:next_node_id`.
- Write amp e stalls saem de `rocksdb.stats`, na seção de compaction e no
  "Stall".

**O que observar:** docs/s deveria ficar aproximadamente constante entre as
fatias. Se cair à medida que o índice cresce, o gargalo está em algo que
cresce junto: leitura de nós da Trie, tamanho das listas no merge ou
compactação. Aí a Fase 6 entra direto na fatia em que a queda começou.

### Experimentos na fatia de 100k

| Experimento | Variações | Mede |
|---|---|---|
| Tamanho do lote | 100, 1k, 10k, 50k docs/commit | docs/s vs. pico de RSS (o lote guarda em memória todo nó que tocou) |
| IDs | page id original vs. `--remap-ids` (1..N) | tamanho do espaço `post:`; deltas menores devem caber em 1 byte |
| Streaming | `index_document` por doc, 10k docs | commits/s e overhead fixo por commit |
| Block cache | 64 MB, 256 MB, 1 GB | docs/s e RSS |
| LTO | release vs. `lto = "thin"` + `codegen-units = 1` | docs/s (só fica se ganhar mais que o ruído) |

### Depois da carga total

- **Tempo de abertura:** `hyperfine 'target/release/fire-flower search ~/bench-db/ptwiki-full xyz'`,
  medindo logo depois da carga e depois de um `compact()`.
- **`wiki sizes`:** quanto ocupa `node:` vs. `post:`, o tamanho de `node:0`
  e quanto ocupam as listas de "de", "a", "o".
- **Tempo de `compact()`** e espaço antes e depois.

---

## 7. Fase 3 — Conjuntos de consulta

O `wiki vocab` roda sobre o corpus inteiro; o `wiki queries` sorteia com seed
fixa e grava em `bench/queries/`. São arquivos pequenos, versionados no repo.
Cerca de 1.000 consultas por conjunto (sorteios com reposição quando o grupo é
pequeno).

| Conjunto | Como escolher | Exemplo | O que isola |
|---|---|---|---|
| `t-raro` | df 1–10 | termos raros | caminho mínimo: Trie + lista curta |
| `t-medio` | df 100–10k | "constituicao" | caso típico |
| `t-freq` | df ≥ 100k | "de", "em", "que" | decodificar listas de MB e merge |
| `t-len-3` / `t-len-8` / `t-len-15` | df parecido, comprimentos diferentes | "rio" / "portugal" / "inconstitucional" | **custo por caractere** (1 `Get` por nível) |
| `t-miss` | termos inexistentes, curtos e longos, incluindo os que divergem só no final | "constituicaox" | quanto custa não achar nada |
| `and-raro-freq` | 1 raro + 1 frequente | "pessoa de" | se a interseção a partir da menor lista funciona |
| `and-freq` | 2–3 frequentes | "de a o" | pior caso do AND |
| `and-n` | 2, 3 e 5 termos médios | | escala com o número de termos |
| `p-1` / `p-2` / `p-4` / `p-raro` | prefixos de 1, 2–3 e 4+ caracteres, e prefixo raro | "a*", "co*", "port*" | busca em largura e o corte de 256 |
| `s-*` | os mesmos prefixos no `suggest`, limite 10 | | autocompletar |
| `titulos` | 1.000 títulos reais | "Revolução Francesa" | mistura realista; também checa revocação |
| `misto` | 70% `titulos`/`t-medio`, 20% `and-*`, 10% `p-*` | | carga das Fases 4 e 5 |
| `frase-2` / `frase-3` | trechos reais de 2 e 3 palavras sem stopword | `"revolução francesa"` | conferência de posições |
| `frase-stop` | 3 palavras com stopword no meio | `"copa do mundo"` | o buraco da stopword |
| `frase-freq` | 2 palavras entre os 500 termos de maior df | `"sao paulo"` | muitos candidatos para conferir |
| `frase-miss` | `frase-2`/`frase-3` com a ordem trocada | `"francesa revolução"` | conferir sem achar |
| `rank-freq` | os termos de `t-freq`, top 10 | | custo do BM25 numa lista grande |
| `rank-and` | 2–3 termos (df ≥ 100) do mesmo artigo, top 10 | | BM25 depois do E |
| `rank-titulos` | os títulos de `titulos`, top 10 | | o caso real da CLI e do TUI |

Conjuntos `rank-*` são medidos com `search_ranked(q, 10)`; os demais com
`search` (todos os IDs), e os `s-*` com `suggest`.

---

## 8. Fase 4 — Latência de busca

Sobre o banco de 1,1M, com uma thread:

```bash
target/release/examples/wiki bench --db ~/bench-db/ptwiki-full --queries bench/queries/t-freq.txt
target/release/examples/wiki bench --db ~/bench-db/ptwiki-full --queries bench/queries/t-freq.txt --cold
```

Para cada conjunto, registrar: p50, p95, p99 e máximo (histograma), qps, média
de resultados por consulta e, via PerfContext, `Get`s e block reads por
consulta.

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | `Get`s/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `t-raro` | | | | | | | | |
| … | | | | | | | | |

### PerfContext por consulta

O PerfContext é por thread: meça na mesma thread que fez a busca.

```rust
use rocksdb::perf::{set_perf_stats, PerfContext, PerfMetric, PerfStatsLevel};

set_perf_stats(PerfStatsLevel::EnableTime); // EnableCount se o timer pesar
let mut ctx = PerfContext::default();

ctx.reset();
let docs = engine.search(query)?;
let gets = ctx.metric(PerfMetric::GetFromMemtableCount);
let cache_hits = ctx.metric(PerfMetric::BlockCacheHitCount);
let block_reads = ctx.metric(PerfMetric::BlockReadCount);
let merge_ns = ctx.metric(PerfMetric::MergeOperatorTimeNanos);
```

**Confirmar:** `t-len-*` deveria mostrar `Get`s ≈ comprimento + 2. Se o p50
crescer de forma linear com o comprimento, isso é o argumento medido para o
índice ordenado `term:<palavra>` citado no README.

### Experimentos específicos

| Experimento | Como | Hipótese a testar |
|---|---|---|
| Merge pendente | `t-freq` logo após a carga vs. após `compact()` | operandos não compactados deixam a leitura de termos frequentes muito mais lenta (ver `MergeOperatorTimeNanos`) |
| Decodificar a raiz | microbench de `TrieNode::decode` sobre o `node:0` real | toda consulta paga esse custo; se for relevante, guardar a raiz decodificada em memória |
| Block cache | 64 MB, 256 MB e 1 GB, com cache frio e quente | a partir de que tamanho a Trie quente fica toda em RAM |
| Tamanho do resultado | `t-freq` mede o tempo de `search` e quantos IDs devolve | devolver ~1M IDs é parte do custo; serve de argumento para uma API com `limit` |

### Microbenchmarks com Criterion (`benches/micro.rs`)

Com dados reais em vez de `format!("doc-{i}")`. Os bancos são construídos
**uma vez** (fatia de 100k), fora da medição.

- `tokenize` sobre 1.000 artigos reais (MB/s).
- `decode_postings` e `merge_postings` sobre a lista real de "de".
- `intersect` com listas reais (rara × frequente, frequente × frequente).
- `search` com 5 consultas fixas, uma de cada tipo, no banco de 100k.
- Indexar 1.000 artigos num banco vazio: o único bench com `TempDir` por
  iteração (`iter_batched`, `BatchSize::PerIteration`).

```bash
cargo bench --bench micro -- --save-baseline main
cargo bench --bench micro -- --baseline main     # depois de uma mudança
```

---

## 9. Fase 5 — Concorrência

O `Engine` é `Sync`: há um escritor por vez, e as buscas rodam em paralelo
sobre snapshots.

1. **Só leitura:** `misto` com 1, 2, 4, 8 e 12 threads. qps deveria escalar
   até perto do número de núcleos.
2. **Leitura durante escrita:** carregar `--take 1000000` e depois, em
   paralelo, `load --skip 1000000` (os 112k restantes) + `bench misto` com 1,
   4 e 8 threads.
   - p99 das buscas vs. o mesmo p99 sem escrita;
   - docs/s da indexação vs. o mesmo docs/s sem leitores;
   - stalls e compactações em `rocksdb.stats` durante a janela.
3. **Commit no meio da busca:** o snapshot garante consistência. O teste
   checa que uma consulta nunca vê metade de um lote. Nenhum documento do lote
   em andamento pode aparecer sem os outros: rodar o oráculo do lote na thread
   leitora.

---

## 10. Fase 6 — Profiling direcionado

```bash
cargo build --profile profiling --example wiki
samply record target/profiling/examples/wiki load --db /tmp/p100k --corpus ~/datasets/ptwiki/ptwiki-100000.jsonl
samply record target/profiling/examples/wiki bench --db ~/bench-db/ptwiki-full --queries bench/queries/t-freq.txt
heaptrack target/profiling/examples/wiki load --db /tmp/p100k --corpus ~/datasets/ptwiki/ptwiki-100000.jsonl
```

Hipóteses a confirmar ou descartar no flamegraph (top 3 de funções na
indexação, na busca e na compactação):

- **Indexação:** NFKD caractere a caractere no `tokenize`; alocação de
  `HashSet<String>` por documento; `node_mut` relendo e redecodificando os
  mesmos nós em todo lote; `format!` montando chaves (`node_key`, `post_key`);
  rehash do `HashMap` do lote.
- **Busca:** `TrieNode::decode` (sobretudo da raiz); `decode_postings` em
  listas grandes; cópia de `String` na busca em largura do prefixo.
- **Compactação:** `merge_postings` decodifica, ordena e recodifica a lista
  inteira a cada merge, o que pode ser caro para os termos frequentes.

**Memória:** amostrar o RSS a cada segundo durante a carga total (um laço com
`ps -o rss=`) e plotar. Ele deve estabilizar; crescimento contínuo indica
vazamento ou um lote grande demais.

---

## 11. Fase 7 (opcional) — Baseline do RocksDB puro

Só vale se a Fase 4 mostrar que o `Get` do RocksDB em si é o gargalo, e não o
código à volta dele.

- O crate usa **RocksDB 10.4.2** (`librocksdb-sys 0.17.3+10.4.2`). O
  `db_bench` do apt é bem mais antigo; compile o da mesma versão:
  `git checkout v10.4.2 && make -j4 db_bench` (`-j4` por causa da RAM).
- Use chaves e valores parecidos com os nossos (chaves `node:N` de ~12 bytes,
  nós de poucos bytes) e as mesmas opções (LZ4, Bloom de 10 bits, cache de
  256 MB):

```bash
./db_bench --benchmarks=fillrandom,readrandom,seekrandom --num=20000000 \
  --key_size=16 --value_size=24 --compression_type=lz4 --bloom_bits=10 \
  --cache_size=268435456 --threads=4 --seek_nexts=10
```

- Compare o `readrandom` com o tempo por `Get` medido na Fase 4. A diferença é
  o overhead do fire-flower.

---

## 12. Fase 8 — Regressões

- **Por PR (alguns minutos):** oráculo na fatia de 10k + carga de 100k +
  `bench` de `t-medio`, `t-freq` e `p-2`. Falha se o oráculo divergir ou se o
  p50 piorar mais de 10% em relação ao baseline salvo.
- **Manual ou semanal:** a escada completa (Fase 2) e a concorrência (Fase 5).
- **Formato:** o `wiki` grava JSON em `bench/results/<data>-<commit>.json`, e
  um script compara dois arquivos.
- **Criterion:** `--save-baseline` / `--baseline` para os micros.
- Compare **só resultados da mesma máquina**.

---

## 13. Metas

Definidas depois do primeiro baseline. A estrutura:

| Métrica | Baseline | Meta |
|---|---|---|
| Divergências do oráculo | — | **0** (fixa) |
| Carga total sem swap | — | **sim** (fixa) |
| docs/s na carga total | | |
| Banco ÷ texto bruto | | |
| p99 de `titulos` (quente) | | |
| p99 de `t-freq` (quente) | | |
| p99 de `misto` com indexação em paralelo | | |
| Tempo de abertura após a carga total | | |

---

## 14. Checklist

- [x] Baixar e converter a Wikipédia PT (1,1M artigos + fatias)
- [x] `.cargo/config.toml` com `jobs = 4`
- [x] Ganchos no `Engine`: estatísticas, propriedades, `compact()`
- [x] `examples/wiki.rs`: `load`, `vocab`, `queries`, `oracle`, `bench`, `sizes` (+ `digest` e `concurrent`)
- [x] `scripts/perf.py`: fases, `pr`, `comparar` e `relatorio`, com `--escala`
- [ ] `vocab` sobre o corpus inteiro + confirmar IDs únicos (feito na escala 10k: 0 IDs repetidos)
- [x] Gerar e versionar `bench/queries/` (escala 10k)
- [x] **Fase 1:** oráculo com zero divergências (10k, todos os termos)
- [ ] **Fase 2:** escada 1k → 1,1M preenchida (1k, 10k e 100k feitos; falta 1,1M)
- [x] **Fase 4:** latência por conjunto + merge pendente vs. compactado (10k)
- [ ] **Fase 6:** flamegraph de carga e busca, top 3 de cada (precisa instalar o `samply`)
- [x] **Fase 5:** leitura durante escrita (10k)
- [x] Preencher as metas e salvar o baseline (10k: `bench/baseline-pr-10k.json`)
- [ ] (Opcional) **Fase 7:** `db_bench`
- [ ] Repetir na escala 100k e depois na 1,1M

**Ordem sugerida:** ganchos + `wiki load` + `oracle` primeiro. A corretude na
fatia de 10k sai no mesmo dia, e a escada de carga roda sozinha enquanto o
resto é escrito.
