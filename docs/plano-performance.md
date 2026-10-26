# Plano de performance — fire-flower (Rust + RocksDB)

Plano para medir, validar e otimizar o **fire-flower** usando a **Wikipédia em
português** (1,1 milhão de artigos) como carga principal.

**Estado em 26/10/2026 (commit `d088f12`, formato v3 com BM25F):** o harness, o
oráculo e as fases de medição rodam até a Wikipédia inteira. A carga completa
leva **4 minutos** (1.112.246 artigos em 242 s, sem colapso), e o
`perf.py tudo --escala full`, 31 minutos. A análise detalhada da última rodada
está em [ANALISE-full-vs-100k.md](https://github.com/evertondays/fire-flower-bench/blob/main/ANALISE-full-vs-100k.md).

**Progresso na branch `performance-issues-in-v3-version` (commit `ad63e51`,
formato v5):** carga inteira em **116 s** (−52%), E de termo raro com frequente
−93%, frases −38% a −72%, títulos −44%, 19,5 mil consultas/s com 12 threads,
zero divergências. P1, P2 e P5 (saltos) feitos; P3 parcial; P4 encerrado (o
limite é a máquina). Detalhes em
[PROGRESSO-performance.md](https://github.com/evertondays/fire-flower-bench/blob/main/PROGRESSO-performance.md). O que
falta tem uma causa comum: ler listas inteiras de termos muito frequentes
(block-max no top-k, frases de termos frequentes, prefixos de 1–2 letras).

---

## Como rodar

Tudo está em `examples/wiki.rs` (o harness) e é orquestrado por
`scripts/perf.py`. A escala define a maior fatia do corpus usada.

```bash
python3 scripts/perf.py tudo --escala 10k        # todas as fases, ~7 min
python3 scripts/perf.py tudo --escala 100k       # ~12 min
python3 scripts/perf.py tudo --escala full       # ~31 min
python3 scripts/perf.py pr --escala 100k         # checagem rápida contra a linha de base
python3 scripts/perf.py carga-longa --reproduzir # cenário do colapso da carga (ver achado 5)
python3 scripts/perf.py frases --escala 10k      # só frases e ranking
python3 scripts/perf.py relevancia --escala 100k # consultas com resposta conhecida
python3 scripts/perf.py relatorio --escala full  # regera o RESULTADOS.md de fire-flower-bench
```

- Resultados ficam no repositório separado
  [fire-flower-bench](https://github.com/evertondays/fire-flower-bench), clonado ao lado deste
  (`../fire-flower-bench`; `FF_BENCH_DIR` para mudar): um JSON por medição em
  `results/` (com commit e data) e o resumo legível em
  [`RESULTADOS.md`](https://github.com/evertondays/fire-flower-bench/blob/main/RESULTADOS.md). Depois de uma rodada, commite e suba
  esse repositório.
- Linha de base do `pr`: `baseline-pr-<escala>.json`, no mesmo repositório. Ao
  salvar uma nova, a anterior (de outro commit) fica como
  `baseline-pr-<escala>-<commit>.json`.
- Para medir com cache frio, rode `sudo -v` antes: sem isso o `perf.py` pula
  essa parte.

---

## 1. Onde está o custo hoje

Medido na Wikipédia inteira, cache quente, 1 thread, banco compactado.

| Operação | Custo medido (full) | De onde vem | Situação |
|---|---|---|---|
| Carga | 242 s, ~10 MB/s estável depois de 400k | **commit: 74–80% do tempo**, numa thread só; tokenização ~20% | aberto: falta saber o que dentro do commit (seção 8, P1) |
| Termo raro ou médio | 18–23 µs | descida na Trie, ~1 µs por letra | resolvido: não muda com a escala |
| Autocompletar | 22–71 µs | busca em largura nos níveis rasos (em cache) | resolvido |
| Termo frequente | 329 µs (145 mil docs) | decodificar a lista inteira, ~2 ns por documento | aberto (P5) |
| E de termos médios | 44–110 µs | listas curtas | resolvido |
| E com termo frequente | 0,4–1,9 ms | decodifica a lista frequente inteira | aberto (P5) |
| Prefixo curto (`p-1`, `p-2`) | p50 4,4–5,1 ms, p99 10–30 ms | 500–800 leituras na Trie, fora do cache de nós rasos; corte em 256 termos | **aberto: a consulta mais cara, e o TUI faz uma por tecla** (P3) |
| Frases | p50 0,9–4,1 ms, p99 ~10 ms | listas `pos:` inteiras (até 2 MB) e blocos fora do block cache | aberto (P5, P6) |
| Top 10 (BM25F) | 107 µs (títulos) a 976 µs (termo frequente) | decodifica as listas inteiras | aberto (P5) |
| Várias threads | 4,3× com 12 threads | suspeito: `RwLock` dos caches de leitura (`EpochCache`) | aberto (P4) |
| Abrir o banco | 38 ms após a carga, 29 ms compactado | | resolvido |

**Fora do escopo por enquanto** (o motor não tem): OR, remoção e atualização
(ver [design-escrita-leitura.md](design-escrita-leitura.md)).

---

## 2. Corpus: Wikipédia em português

| Arquivo (`~/datasets/ptwiki/`) | Artigos | Tamanho | Uso |
|---|---|---|---|
| `ptwiki-1000.jsonl` | 1.000 | 21 MB | smoke test |
| `ptwiki-10000.jsonl` | 10.000 | 93 MB | oráculo completo, por PR |
| `ptwiki-100000.jsonl` | 100.000 | 471 MB | experimentos de configuração |
| `ptwiki.jsonl` | 1.112.246 | 2,7 GB (2,69 GB de texto) | carga total, concorrência |

- **Formato:** um JSON por linha, `{"id": u64, "title", "url", "text"}`.
- **Fonte:** Hugging Face `wikimedia/wikipedia`, snapshot `20231101.pt`, texto
  já limpo. Os parquets originais ficam em `raw/`. Regerar com
  `scripts/ptwiki_corpus.py`.
- **Fatias:** cada uma é um prefixo da seguinte (`head -n N`). Os primeiros
  100k artigos da carga inteira dão, janela a janela, os mesmos números da
  carga de 100k.
- **IDs:** page ids da Wikipédia, únicos (confirmado no corpus inteiro: 0
  repetidos), crescentes no arquivo e **esparsos** (até 7,2 milhões para 1,1
  milhão de artigos).
- **Tamanho dos artigos:** os primeiros são longos (4,8 KB em média nos
  primeiros 100k) e os do fim, curtos (2,5 KB na média geral). Por isso
  artigos/s **sobe** na carga inteira; compare cargas por **MB/s**.

---

## 3. Regras do ambiente

WSL2, Ryzen 5 5600X (6 núcleos físicos, 12 threads), 7,7 GB de RAM, ext4 em
VHD. Números de outra máquina não são comparáveis com os daqui.

- **Compilar com poucos jobs:** `.cargo/config.toml` tem `[build] jobs = 4`
  (compilar o RocksDB com 12 jobs chega perto de estourar a RAM).
- **Nada pesado em paralelo** durante as medições (outra compilação, outra
  sessão com `cargo check`, Jellyfin transcodificando).
- **Banco e corpus no disco Linux** (`~/…`), nunca em `/mnt/c`.
- **Sempre build release**, num commit fixo. O harness grava commit, `Config`
  e estado da memória em cada resultado.
- **Teto de memória:** a carga total tem que caber sem swap (hoje: pico de
  838 MB, 6 GB livres).
- **Ruído:** a carga varia ~1% entre rodadas iguais na full e até ~10% no
  10k. O p50 das buscas é mais estável que o p99.

---

## 4. Ferramentas e harness

### Harness: `examples/wiki.rs`

| Subcomando | O que faz |
|---|---|
| `load` | indexa o corpus; a cada 10k artigos mostra MB/s, fração do tempo no commit, Gets por artigo, filtro de Bloom relido, stall, arquivos no L0, RSS e RAM livre, e acusa colapso (MB/s abaixo de 20% da mediana das 10 janelas anteriores). Opções de experimento: `--batch`, `--cache-mb`, `--cache-nos`, `--sem-posicoes`, `--filtro-inteiro`, `--remap-ids`, `--streaming` |
| `vocab` | df e comprimento de cada termo; confere IDs únicos |
| `queries` | gera os conjuntos de consulta (seção 6) com seed fixa |
| `oracle` | compara o motor com um índice ingênuo em memória (seção 5) |
| `bench` | latência por conjunto (p50/p95/p99/máx, qps, Gets e blocos lidos por consulta) |
| `sizes` | espaço por tipo de chave e os 20 maiores valores |
| `digest` | resumo do conteúdo lógico, para comparar bancos |
| `concurrent` | buscas com e sem indexação em paralelo |
| `relevancia` | consultas com resposta conhecida (ver [plano-relevancia.md](plano-relevancia.md)) |

O PerfContext é lido pelo relatório em texto: o enum `PerfMetric` do crate
`rocksdb` 0.23 está desalinhado com o RocksDB 10.4 embutido.

### Ferramentas

| Ferramenta | Para quê | Situação |
|---|---|---|
| `samply` (`cargo install samply`) | profiler de CPU (o `perf` do apt não bate com o kernel do WSL2) | **falta instalar**; necessário para P1 e P4 |
| `heaptrack` (`sudo apt install heaptrack`) | alocações | falta instalar |
| `hyperfine` | comparar comandos inteiros | opcional |
| Criterion (`benches/micro.rs`) | microbenchmarks com dados reais (banco de 100k) | pronto |
| `[profile.profiling]` no `Cargo.toml` | release com símbolos | pronto |

---

## 5. Fases de medição

| Fase | O que mede | Situação | Onde ver |
|---|---|---|---|
| 1 — corretude | oráculo, idempotência, lotes vs. único, reabrir | ✅ 0 divergências em 10k (todos os termos) e 100k (amostra) | `RESULTADOS.md` |
| 2 — escada de carga | 1k → 10k → 100k → 1,1M, mais experimentos no 100k | ✅ até 1,1M | `RESULTADOS.md`, análise seção 1 |
| 3 — conjuntos de consulta | ~1.000 consultas por conjunto, por escala | ✅ (ver ressalva abaixo) | `bench/queries/<escala>/` |
| 4 — latência | por conjunto; merge pendente; block cache de 64 MB / 256 MB / 1 GB | ✅ quente na full; ❌ frio nunca rodou | análise seção 4 |
| 5 — concorrência | `misto` com 1–12 threads; leitura durante escrita | ✅ na full | análise seção 5 |
| 6 — profiling | flamegraph de carga, busca e compactação | ❌ **nunca rodou** | seção 8 |
| 7 — `db_bench` | RocksDB puro vs. o motor | descartada: o `Get` do RocksDB nunca apareceu como gargalo | — |
| 8 — regressões | `pr`: oráculo 10k + carga + bench de 5 conjuntos | ✅ linhas de base de 100k e full | `baseline-pr-*.json` (fire-flower-bench) |

### Fase 1 — o que o oráculo confere

O índice ingênuo é construído só com `tokenize` e as stopwords (nada da
indexação do motor é reaproveitado) e compara: listas (doc, tf, tf no título),
posições, tamanhos e estatísticas, E, prefixo, autocompletar, título → artigo,
normalização, frases (tiradas de trechos reais e com a ordem trocada) e o top
10 do BM25F (diferença relativa < 1e-4). Critério: **zero divergências**.

| Teste | Fatia |
|---|---|
| Todos os termos | 10k |
| Amostra estratificada por df (~31 mil termos) | 100k |
| Reabrir e carregar mais 1.000 artigos | 100k |
| Idempotência (a fatia duas vezes) | 10k |
| Lotes vs. único (lote 1 vs. 1000) | 1k |

**Lacuna:** nada confere o banco de 1,1 milhão diretamente, porque o oráculo
completo da Wikipédia inteira não cabe na RAM (seção 8, H3).

### Fase 2 — experimentos de carga (100k, `d088f12`)

| Experimento | artigos/s | vs. padrão | Pico de RSS |
|---|---|---|---|
| padrão (lote 1.000) | 3.660 | — | 472 MB |
| lote 100 / 5.000 / 10.000 / 50.000 | 2.061 / 5.362 / 5.980 / 7.339 | −44% / +47% / +63% / +100% | 476 / 525 / 696 / 1.332 MB |
| sem cache de nós do escritor | 1.949 | −47% | 305 MB |
| sem posições | 4.835 | +32% | 429 MB |
| block cache 64 MB / 1 GB | 3.562 / 3.590 | −3% / −2% | 460 / 471 MB |
| IDs remapeados (1..N) | 3.755 | +3% | 471 MB |
| streaming (1 commit por artigo) | 519 | — | 248 MB |

---

## 6. Conjuntos de consulta

O `wiki vocab` roda sobre a fatia da escala e o `wiki queries` sorteia com seed
fixa, gravando em `bench/queries/<escala>/`.

**Ressalva:** os conjuntos são **por escala**. `t-freq` é "termo frequente
naquela escala" (25 mil documentos por consulta no 100k, 145 mil na full),
então "100k vs. full" mostra quanto cada tipo de consulta custa em cada
escala, não as mesmas consultas (seção 8, H2).

| Conjunto | Como escolher | O que isola |
|---|---|---|
| `t-raro` / `t-medio` / `t-freq` | df 1–10 / 100–10k / os mais frequentes | caminho mínimo / caso típico / listas grandes |
| `t-len-3` / `-8` / `-15` | mesmo df, comprimentos diferentes | custo por letra |
| `t-miss` | termos inexistentes | quanto custa não achar |
| `and-raro-freq` / `and-freq` / `and-2,3,5` | combinações | interseção |
| `p-1` / `p-2` / `p-4` / `p-raro` | prefixos de 1, 2–3, 4–6 letras e raros | busca em largura e o corte de 256 |
| `s-*` | os mesmos prefixos no `suggest` | autocompletar |
| `titulos` | 1.000 títulos reais | mistura realista |
| `misto` | 70% títulos/termos médios, 20% E, 10% prefixo | carga das fases 4 e 5 |
| `frase-2` / `-3` / `-stop` / `-freq` / `-miss` | trechos reais; com stopword; termos frequentes; ordem trocada | conferência de posições |
| `rank-freq` / `rank-and` / `rank-titulos` | top 10 (BM25F) | custo do ranking |

`rank-*` são medidos com `search_ranked(q, 10)`, `s-*` com `suggest` e os
demais com `search` (todos os IDs).

---

## 7. Achados

| # | Achado | Situação |
|---|---|---|
| 1 | Corretude: zero divergências em todos os oráculos, em todas as rodadas | mantido a cada PR |
| 2 | Prefixo incompleto é a regra: metade dos prefixos sorteados casa mais de 256 termos | aberto (por projeto); o índice ordenado mudaria a ordem (mais curtos primeiro) |
| 3 | Custo por letra confirmado (~1 µs por letra); prefixos curtos fazem 500–800 leituras | parcial: `MultiGet` por nível e cache até a profundidade 4 (`p-2` −28%) |
| 4 | Merge pendente pesa: sem compactar, termo frequente, títulos e frases ficam 30–50% mais lentos (full: `t-freq` 677 → 329 µs); compactar a full leva 8,7 s | ✅ compactar no fim por padrão; v5 deixou o merge pendente mais barato que o do v3 (`t-freq` antes de compactar 677 → 399 µs); compactar leva 5,3 s |
| 5 | **Colapso da carga inteira** (de ~5.000 para ~200 artigos/s em 200k, 500k ou 600k): filtro de Bloom de ~6 MB num bloco só, maior que uma parte do block cache, relido a cada leitura (22 TB contados) | ✅ resolvido com filtros particionados (`a9009c7`); vigiado por `perf.py carga-longa` e pelo teste `bloom_filters_larger_than_a_cache_shard_...` |
| 6 | Lotes maiores indexam muito mais rápido (lote de 10 mil: +63% no 100k) | ✅ lote padrão de 10 mil |
| 7 | A carga perde velocidade até ~400k e depois fica plana em ~10 MB/s; o commit sobe de 63% para 80% do tempo e as leituras do escritor, de 7 para 15–18 por artigo (Trie com 7,3 M nós, cache do escritor com 1 M) | ✅ descarte por recência + cache de 3 M nós: carga plana, 116 s |
| 8 | A concorrência escala pouco: 4,3× com 12 threads (era 6,3× no 10k do formato v2) | encerrado: o RocksDB puro escala 5,5× nesta máquina; 19,5 mil qps (4,3×) |
| 9 | Na full, o banco (1,4 GB) não cabe no block cache de 256 MB: frases leem 4–6 blocos de fora do cache por consulta e `p-2`, 112 | registrado: 1 GB tira mais 10–20%; padrão mantido em 256 MB |
| 10 | Listas longas custam ~2 ns por documento em qualquer escala (termo frequente, frases, ranking) | parcial: E e frases saltam blocos; listas inteiras (`t-freq`, `frase-freq`) iguais |
| 11 | Indexar em paralelo sobe o p50 das buscas em 60–75% na full; a indexação perde 18–47% de velocidade com leitores | registrar |
| 12 | Posições são 58% do banco (`pos:` 878 MB de 1,5 GB); o banco compactado tem 0,52× o texto | registrar |
| 13 | O detector de colapso dá falso positivo com lotes maiores que a janela (lote de 50 mil) | ✅ janelas agrupadas por lote |

---

## 8. Próximos passos

Cada passo é um PR com a medição que o justifica, comparado com
`baseline-pr-full.json` (ou a de 100k, para iterar mais rápido).

### P1 — O que dentro do commit custa (profiling)

**Por quê:** o commit é 74–80% da carga e é o que fica mais caro à medida que
o índice cresce. Não se sabe quanto é percorrer a Trie (e reler nós que saíram
do cache), codificar as listas ou gravar no RocksDB.

1. Instrumentar o commit (tempo em `insert_term`, em leituras de nó, em
   codificação de listas, em `norm:` e em `db.write`) e mostrar no monitor do
   `wiki load`.
2. `samply record` da carga de 100k e de 300k–500k (onde a velocidade cai).

**Pronto quando:** a carga tem uma divisão de tempo por etapa e um top 3 de
funções; dali saem os próximos PRs de carga.

### P2 — Ajustes baratos de carga e pós-carga

| Mudança | Hipótese | Como medir |
|---|---|---|
| Lote padrão de 10 mil (CLI, `carregar_wikipedia.py`, `perf.py`) | +50% na full com ~700 MB de RSS | carga da full, MB/s por janela |
| Cache de nós do escritor de 4 M nós (~800 MB) | menos releituras: Gets por artigo voltam para < 10 | `wiki load --cache-nos 4000000` na full |
| Compactar no fim da carga por padrão | −30–50% em termo frequente, títulos e frases, por 9 s | fase 4 antes/depois |

**Pronto quando:** carga da full ≤ 180 s sem passar de 1,5 GB de RSS.

### P3 — Prefixos curtos: índice ordenado de termos

**Por quê:** `p-1`/`p-2` são as consultas mais caras (p50 de 4–5 ms, p99 de
10–30 ms na full) e o TUI faz uma a cada tecla.

Ideia: uma chave `term:<palavra>` → nó terminal, lida com o iterador do
RocksDB. O prefixo vira uma varredura de faixa (sem busca em largura na Trie),
e o corte de 256 termos pode ser trocado por um corte por df.

**Pronto quando:** `p-1` e `p-2` com p50 ≤ 1 ms e p99 ≤ 5 ms na full, com o
oráculo de prefixo e autocompletar sem divergências. Medir também o custo em
espaço e na carga.

### P4 — Concorrência

**Por quê:** 4,3× com 12 threads. Suspeito: todo acesso aos caches de leitura
(nós rasos e tamanhos, `EpochCache`) pega um `RwLock` compartilhado.

1. `samply` do `bench misto --threads 12` para confirmar a disputa.
2. A/B: cache dividido em partes, ou cópia de leitura por thread trocada a cada
   commit; e o block cache `HyperClock` no lugar do LRU.

**Pronto quando:** ≥ 7× com 12 threads no `misto` da full.

### P5 — Listas longas

**Por quê:** termo frequente, frases e ranking decodificam a lista inteira
(~2 ns por documento): `frase-freq` com p50 de 4,1 ms, `rank-freq` de 976 µs
na full.

Ideia: listas em blocos com índice de saltos (e o máximo de tf por bloco, para
block-max no top-k), para pular o que não interessa sem decodificar. Muda o
formato do banco.

**Meta revista:** a meta antiga ("`rank-freq` ≤ 2× `t-freq`") deixa de valer: a
razão piora quando a busca simples melhora. No lugar, latência absoluta:
`rank-freq` ≤ 300 µs e `frase-freq` ≤ 1 ms de p50 na full.

### P6 — Block cache para a full

1 GB zera as leituras fora do cache e tira 10–15% do p50 de títulos, `misto` e
`p-2`. Decidir o padrão (ou um padrão proporcional ao banco) junto com o P4.

### Harness

| # | O quê |
|---|---|
| H1 | Detector de colapso: só comparar janelas quando o lote for menor que a janela (ou alinhar janelas aos commits) |
| H2 | Um conjunto fixo de consultas, igual em todas as escalas, para comparar 100k e full diretamente |
| H3 | Oráculo e relevância por amostra também na full (o oráculo em duas passadas, guardando só os termos sorteados) |
| H4 | Cache frio: rodar com `sudo -v` antes |
| H5 | Experimento de LTO (`lto = "thin"` + `codegen-units = 1`): só fica se ganhar mais que o ruído |

### Descartado ou adiado

- **Fase 7 (`db_bench`):** o `Get` do RocksDB nunca foi o gargalo.
- **Preparar documentos em paralelo** ([design-escrita-leitura.md](design-escrita-leitura.md),
  1.2): com o commit em 74–80% do tempo, o ganho máximo seria ~1,25× (Amdahl),
  menos que o lote maior. Volta a valer se o P1 deixar o commit barato.
- **Hipóteses antigas de profiling** (NFKD por caractere, `format!` nas chaves,
  `HashSet` por documento, raiz decodificada em toda busca, devolver 1 milhão
  de IDs): já resolvidas no código.

---

## 9. Metas

Linha de base: rodada `tudo --escala full` de 26/10/2026, commit `d088f12`.

| Métrica | Linha de base (full) | Meta | `ad63e51` |
|---|---|---|---|
| Divergências do oráculo | 0 | **0** (fixa) | 0 ✅ |
| Carga total sem swap e sem colapso | sim | **sim** (fixa) | sim ✅ |
| Tempo da carga total | 242 s (11,1 MB/s) | ≤ 180 s (P2) | 116 s ✅ |
| Pico de RSS na carga total | 838 MB | ≤ 1,5 GB | 1.370 MB ✅ |
| Banco compactado ÷ texto | 0,52 | ≤ 0,55 | 0,53 ✅ |
| p50 / p99 de `titulos` | 93 µs / 1,8 ms | p99 ≤ 1 ms | 51 µs / 1,5 ms ❌ |
| p50 / p99 de `rank-titulos` | 107 µs / 1,9 ms | p99 ≤ 1 ms | 63 µs / 1,9 ms ❌ |
| p50 / p99 de `t-freq` | 329 / 722 µs | registrar (P5) | 328 / 725 µs |
| p50 de `rank-freq` | 976 µs | ≤ 300 µs (P5) | 917 µs ❌ |
| p50 / p99 de `p-2` | 4,4 / 10,3 ms | ≤ 1 / 5 ms (P3) | 3,2 / 7,1 ms ❌ |
| p50 / p99 de `frase-freq` | 4,1 / 17,4 ms | p50 ≤ 1 ms (P5) | 4,1 / 19,4 ms ❌ |
| `misto` com 12 threads | 11.873 qps (4,3×) | ≥ 7× (P4) | 19.521 qps (4,3×); inviável aqui |
| p99 de `misto` com indexação em paralelo (4 leitores) | 7,6 ms | ≤ 5 ms | 4,2 ms ✅ |
| Abrir o banco + 1 busca | 38 ms (29 ms compactado) | registrar | 28 ms compactado |

---

## 10. Checklist

- [x] Wikipédia PT baixada e convertida (1,1M artigos + fatias)
- [x] Ganchos no `Engine`: estatísticas, propriedades, `compact()`
- [x] Harness (`load` com monitor por janela, `vocab`, `queries`, `oracle`, `bench`, `sizes`, `digest`, `concurrent`, `relevancia`)
- [x] `scripts/perf.py`: fases, `pr`, `carga-longa`, `frases`, `comparar`, `relatorio`
- [x] IDs únicos confirmados no corpus inteiro
- [x] Fase 1 (corretude) em 10k e 100k
- [x] Fase 2 (escada) até 1,1M
- [x] Fases 4 e 5 (latência quente e concorrência) na full
- [x] Linha de base salva (`baseline-pr-100k.json`, `baseline-pr-full.json`, em fire-flower-bench)
- [x] Colapso da carga inteira corrigido e com cenário de teste
- [ ] Latência com cache frio (H4)
- [x] P1: divisão do tempo do commit (`CommitStats`, no monitor do `wiki load`)
- [x] P2: lote, cache de nós e compactação no fim
- [ ] P3: prefixos curtos (parcial: `MultiGet` e cache até a profundidade 4)
- [x] P4: concorrência medida; limite é a máquina (RocksDB puro 5,5×)
- [x] P5: listas em blocos com saltos (formato v5)
- [ ] Block-max no top-k e frases de termos frequentes
- [x] H1: detector de colapso por lote
- [ ] H2–H3: conjunto fixo, oráculo e relevância na full
