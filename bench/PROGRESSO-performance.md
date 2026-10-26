# Progresso de performance: `d088f12` → `ad63e51`

Branch `performance-issues-in-v3-version`. Duas rodadas oficiais de
`python3 scripts/perf.py tudo --escala full`, na mesma máquina (WSL2, 6
núcleos físicos, 7,7 GB de RAM) e com as mesmas consultas
(`bench/queries/full`):

| Rodada | Commit | O quê | Log |
|---|---|---|---|
| Linha de base | `d088f12` | formato v3 com BM25F | `~/bench-db/perf-v3-full.log` |
| Intermediária | `4d22de8` | formato v4 (tabela de saltos de blocos fixos) | `~/bench-db/perf-v4-full.log` |
| **Final** | `ad63e51` | formato v5 (blocos variáveis) | `~/bench-db/perf-v5-full.log` |

Os números vêm dos JSON em `bench/results/`. A análise da linha de base está
em [ANALISE-full-vs-100k.md](ANALISE-full-vs-100k.md), e o plano, em
[docs/plano-performance.md](../docs/plano-performance.md).

## Resumo

- **Carga da Wikipédia inteira: 241 s → 116 s (−52%)**, sem colapso, com pico
  de 1,37 GB (era 838 MB). A de 100k caiu de 27 s para 15 s.
- **Buscas:** termos, E, títulos, prefixos, autocompletar e frases ficaram 17%
  a 94% mais rápidos no p50. O maior ganho foi no E de um termo raro com um
  frequente (410 → 28 µs) e em frases (−38% a −72%).
- **Antes de compactar** (o estado normal durante indexação contínua), termo
  frequente, títulos e frases ficaram 41% a 57% mais rápidos.
- **Concorrência:** 19,5 mil consultas/s com 12 threads (eram 11,9 mil); com
  indexação em paralelo, o p99 das buscas caiu de 7,6 para 4,2 ms.
- **Sem mudança:** listas inteiras de termos muito frequentes (`t-freq`,
  `and-freq`, `frase-freq`) e o p99 das frases (~10 ms), que dependem de ler
  listas de MBs.
- **Corretude e relevância idênticas:** zero divergências em todos os
  oráculos, mesmos números de relevância.

## O que mudou (commits da branch)

| Commit | Mudança | Efeito medido |
|---|---|---|
| `0ca0b85` | Divisão do tempo do commit (`CommitStats`) e descarte do cache de nós por recência | descarte antigo jogava fora ~75% do cache ao encher; 500k: 108 → 96 s |
| `8289692` | Filhos da Trie em vetor ordenado; descida a partir do prefixo comum | −55 MB de memória no cache; −5 a −16% na descida da Trie nas buscas |
| `6036d73` | Padrões: lote de 10 mil, cache de 3 M nós, compactar no fim da carga | carga inteira 242 → 115 s |
| `7c7dd08` | Prefixo e autocompletar com `MultiGet` em lote; cache de nós das buscas até a profundidade 4 | `p-2` −28%, `s-2` −76% |
| `e6a4e7f` | Formato v4: tabela de saltos, E por saltos, frases saltando blocos | E raro × frequente −94%, frases −40 a −75% |
| `ad63e51` | Formato v5: blocos variáveis (o merge só concatena tabelas) | corrige a piora do v4 antes de compactar; compactação 17 → 5 s |

## Carga

| | `d088f12` | `ad63e51` | Δ |
|---|---|---|---|
| Wikipédia inteira (1.112.246 artigos) | 241,3 s (11,2 MB/s) | **116,2 s (23,2 MB/s)** | −52% |
| Os primeiros 90% (base da concorrência) | 217,6 s | 103,2 s | −53% |
| 100k | 27,1 s | 15,4 s | −43% |
| 10k | 3,8 s | 2,7 s | −29% |
| Pico de RSS (inteira) | 838 MB | 1.370 MB | +64% |
| Banco compactado | 1.411 MB | 1.434 MB | +1,6% (tabelas de saltos) |
| Compactar a inteira | 8,7 s | 5,3 s | −39% |
| Abrir + 1 busca (compactado) | 29 ms | 28 ms | — |

O pico de memória maior é o preço do lote de 10 mil e do cache de 3 milhões
de nós, dentro da meta de 1,5 GB.

Experimentos de carga (100k, `ad63e51`): lote de 1.000 leva 23,3 s, de 5.000
17,0 s e de 50.000 13,0 s; sem posições, 11,8 s; sem cache de nós, 20,6 s (era
51,3 s: o descarte novo e o vetor de filhos reduziram muito o custo de reler
nós).

## Latência (cache quente, 1 thread, compactado, p50)

| Conjunto | `d088f12` | `ad63e51` | Δ | p99 antes → depois |
|---|---|---|---|---|
| `t-miss` | 7 µs | 1 µs | −78% | 29 → 26 µs |
| `t-raro` | 18 µs | 14 µs | −26% | 45 → 37 µs |
| `t-medio` | 23 µs | 18 µs | −22% | 55 → 53 µs |
| `t-freq` | 329 µs | 328 µs | 0% | 722 → 725 µs |
| `and-2` / `and-3` / `and-5` | 44 / 65 / 110 µs | 33 / 48 / 81 µs | −25% | |
| `and-raro-freq` | 410 µs | **28 µs** | **−93%** | 995 → 58 µs |
| `and-freq` | 1,92 ms | 1,96 ms | +2% | 3,9 → 3,3 ms |
| `titulos` | 93 µs | 51 µs | −44% | 1,83 → 1,49 ms |
| `rank-titulos` | 107 µs | 63 µs | −41% | 1,92 → 1,86 ms |
| `rank-and` | 542 µs | 412 µs | −24% | 3,9 → 4,0 ms |
| `rank-freq` | 976 µs | 917 µs | −6% | 3,6 → 3,5 ms |
| `frase-2` | 928 µs | 496 µs | −47% | 10,7 → 10,9 ms |
| `frase-3` | 1,11 ms | 313 µs | **−72%** | 10,1 → 10,0 ms |
| `frase-stop` | 1,27 ms | 785 µs | −38% | 6,9 → 7,0 ms |
| `frase-miss` | 947 µs | 504 µs | −47% | 9,1 → 9,6 ms |
| `frase-freq` | 4,10 ms | 4,12 ms | 0% | 17,4 → 19,4 ms |
| `p-1` | 5,07 ms | 3,43 ms | −32% | 30,5 → 26,6 ms |
| `p-2` | 4,36 ms | 3,16 ms | −28% | 10,3 → 7,1 ms |
| `p-4` | 1,01 ms | 834 µs | −17% | 8,5 → 6,3 ms |
| `s-2` (autocompletar) | 52 µs | 12 µs | −76% | 97 → 52 µs |
| `s-4` | 71 µs | 63 µs | −11% | 263 → 224 µs |
| `misto` | 55 µs | 36 µs | −34% | 4,2 → 3,1 ms |

### Antes de compactar (logo após a carga, p50)

| Conjunto | `d088f12` | `4d22de8` (v4) | `ad63e51` (v5) |
|---|---|---|---|
| `t-freq` | 677 µs | 972 µs | **399 µs** |
| `titulos` | 168 µs | 153 µs | **95 µs** |
| `and-freq` | 2,29 ms | 3,15 ms | **2,11 ms** |
| `rank-freq` | 1,12 ms | 1,41 ms | **950 µs** |
| `frase-2` | 1,48 ms | 1,61 ms | **640 µs** |
| `frase-freq` | 6,16 ms | 7,86 ms | **4,46 ms** |

A rodada intermediária (v4) mostra por que o formato mudou duas vezes: com
blocos de tamanho fixo, o merge feito na leitura de uma lista com pedaços
pendentes recalculava a tabela de saltos inteira a cada leitura. Na v5 o
merge só concatena as tabelas, e ficou mais barato que o do v3.

### Block cache (p50)

| Conjunto | 64 MB antes → depois | 1 GB antes → depois |
|---|---|---|
| `titulos` | 105 → 63 µs | 83 → 43 µs |
| `misto` | 72 → 52 µs | 48 → 32 µs |
| `p-2` | 4,95 → 3,36 ms | 3,71 → 2,46 ms |

Com 1 GB (em vez dos 256 MB padrão), títulos, `misto` e `p-2` ganham mais
10–20%.

## Concorrência

| Threads (`misto`) | `d088f12` | `ad63e51` |
|---|---|---|
| 1 | 2.731 qps | 4.512 qps |
| 4 | 6.209 qps | 9.608 qps |
| 12 | 11.873 qps (4,3×) | **19.521 qps (4,3×)** |

| Leitura durante escrita (4 leitores) | `d088f12` | `ad63e51` |
|---|---|---|
| p50 sem → com escrita | 81 → 132 µs | 46 → 88 µs |
| p99 sem → com escrita | 5,4 → 7,6 ms | 2,9 → **4,2 ms** |
| Indexação com leitores | 3.203 artigos/s | 3.442 artigos/s |

O ganho relativo com 12 threads continua 4,3×. Medido à parte, o próprio
RocksDB (só `Get`s, sem o motor) escala 4,5× com 6 threads e 5,5× com 12
nesta máquina, e o `t-medio` já chega a 5,6×: o limite é a máquina (6
núcleos físicos com SMT, sob WSL2). O `RwLock` dos caches e o block cache
`HyperClock` foram testados e não mudam a escala.

## Corretude e relevância

| | `d088f12` | `ad63e51` |
|---|---|---|
| Oráculo 10k (todos os termos, frases, BM25F) | 0 divergências | 0 divergências |
| Oráculo 100k (amostra) e reaberto | 0 | 0 |
| Idempotência, lotes vs. único | ✅ | ✅ |
| Relevância (`rel-titulo` / `rel-manual`, top-1) | 99,3% / 66,7% | 99,3% / 66,7% |

## Metas do plano

| Meta | Linha de base | Agora | |
|---|---|---|---|
| Carga total ≤ 180 s | 242 s | 116 s | ✅ |
| Pico de RSS ≤ 1,5 GB | 838 MB | 1.370 MB | ✅ |
| p99 de `misto` com indexação (4 leitores) ≤ 5 ms | 7,6 ms | 4,2 ms | ✅ |
| p50 / p99 de `p-2` ≤ 1 / 5 ms | 4,4 / 10,3 ms | 3,2 / 7,1 ms | ❌ (−28% / −30%) |
| p99 de `titulos` ≤ 1 ms | 1,83 ms | 1,49 ms | ❌ (−19%) |
| p99 de `rank-titulos` ≤ 1 ms | 1,92 ms | 1,86 ms | ❌ |
| p50 de `rank-freq` ≤ 300 µs | 976 µs | 917 µs | ❌ |
| p50 de `frase-freq` ≤ 1 ms | 4,1 ms | 4,1 ms | ❌ |
| `misto` com 12 threads ≥ 7× | 4,3× | 4,3× | inviável nesta máquina |

## O que falta

As metas que faltam têm uma causa comum: **ler listas inteiras de termos
muito frequentes** (até 145 mil documentos, posições de até 2 MB).

1. **Block-max no top-k:** guardar o maior tf (e o menor tamanho) de cada
   bloco na tabela de saltos, para o ranking pular blocos que não podem entrar
   no top-k (`rank-freq`, `rank-titulos` com termos frequentes).
2. **Frases de termos frequentes:** começar a conferência pelo termo mais raro
   e intercalar as posições bloco a bloco; hoje `frase-freq` confere ~46 mil
   candidatos.
3. **Prefixos de 1–2 letras:** a união de até 256 listas domina o `p-1`; um
   corte por df (as completações mais frequentes primeiro) ou um resultado
   parcial ranqueado reduziria o trabalho, mas muda a semântica e precisa
   passar pela relevância.
