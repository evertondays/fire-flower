# Resultados de performance (escala 100k)

Gerado por `python3 scripts/perf.py relatorio --escala 100k` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das últimas rodadas do mesmo commit (até 5), com a faixa mín–máx entre parênteses. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-25T23-16-34-4fe734a-modificado-bench-pr-100k.json`
- Máquina: 12 núcleos, 7.883,2 MB de RAM, kernel `6.6.87.2-microsoft-standard-WSL2`

## Fase 1: corretude

| Checagem | Resultado |
|---|---|
| `oraculo_10k` | ✅ ok |
| `idempotencia` | ✅ ok |
| `lotes_vs_unico` | ✅ ok |
| `oraculo_100k` | ✅ ok |
| `reabrir` | ✅ ok |

Idempotência (10k): banco de 55,3 MB após uma carga, 50,0 MB após duas e 50,0 MB depois de compactar.

**Oráculo `10k-todos`** (10.000 docs, 288.203 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 288.203 | 0 | 288203 termos no motor, 288203 no oráculo |
| posições | 288.203 | 0 |  |
| tamanhos e estatísticas | 10.003 | 0 |  |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1033 de 2000 prefixos passaram de 256 termos; 5488778 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1554 frases com resultado, 446 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo e frase; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 80,5%, top-10 96,2%, MRR 0,872, fora do top-1000: 0.

**Oráculo `10k-reaberto`** (10.100 docs, 289.796 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 13.579 | 0 | amostra de 13579 termos estratificada por df |
| posições | 13.579 | 0 |  |
| tamanhos e estatísticas | 10.103 | 0 |  |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1020 de 2000 prefixos passaram de 256 termos; 5666586 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1564 frases com resultado, 436 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo e frase; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 80,3%, top-10 96,0%, MRR 0,870, fora do top-1000: 0.

**Oráculo `100k-amostra`** (100.000 docs, 870.885 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 30.958 | 0 | amostra de 30958 termos estratificada por df |
| posições | 30.958 | 0 |  |
| tamanhos e estatísticas | 100.003 | 0 |  |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1196 de 2000 prefixos passaram de 256 termos; 13980786 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1592 frases com resultado, 408 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo e frase; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 82,1%, top-10 98,4%, MRR 0,887, fora do top-1000: 1.

**Oráculo `100k-reaberto`** (101.000 docs, 871.551 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 14.306 | 0 | amostra de 14306 termos estratificada por df |
| posições | 14.306 | 0 |  |
| tamanhos e estatísticas | 101.003 | 0 |  |
| AND | 4.997 | 0 |  |
| prefixo | 2.000 | 0 | 1140 de 2000 prefixos passaram de 256 termos; 13442055 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1591 frases com resultado, 409 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo e frase; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 82,2%, top-10 98,4%, MRR 0,888, fora do top-1000: 1.

## Fase 2: escada de indexação

| Fatia | Commit | Rodadas | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1k | `4fe734a-modificado` | 5 | 0.8s | 1.319 (1.022–1.417) | 26,6 | 138,1 MB | 15,9 MB | 0,79 | 294.199 | 114.201 | 0,00 | 0,0s |
| 10k | `4fe734a-modificado` | 5 | 4.3s | 2.318 (2.226–2.611) | 21,5 | 288,7 MB | 67,2 MB | 0,72 | 728.495 | 288.203 | 0,38 | 0,0s |
| 100k | `4fe734a-modificado` | 2 | 30.4s | 3.284 (3.284–3.519) | 15,4 | 509,3 MB | 257,2 MB | 0,55 | 2.118.236 | 870.885 | 1,38 | 0,0s |

### Experimentos (100k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 1000) | 3.284 (3.284–3.519) | — | 509,3 MB | 257,2 MB |
| lote-100 | 1.876 | -43% | 533,7 MB | 253,0 MB |
| lote-5000 | 4.409 | +34% | 557,0 MB | 278,5 MB |
| lote-10000 | 5.344 | +63% | 709,3 MB | 473,9 MB |
| lote-50000 | 6.755 | +106% | 1.328,6 MB | 265,1 MB |
| ids-remapeados | 3.386 | +3% | 509,9 MB | 241,6 MB |
| cache-64mb | 629 | -81% | 491,8 MB | 257,2 MB |
| cache-1024mb | 3.468 | +6% | 509,1 MB | 257,2 MB |
| sem-cache-de-nos | 2.092 | -36% | 348,9 MB | 257,2 MB |
| sem-posicoes | 4.558 | +39% | 458,8 MB | 111,1 MB |
| streaming-10k | 525 | — | 257,6 MB | 86,1 MB |

### Tamanhos (100k)

| Espaço | Chaves | Chaves (MB) | Valores (MB) |
|---|---|---|---|
| `doc:` | 100.000 | 0,9 | 1,4 |
| `meta:` | 7 | 0,0 | 0,0 |
| `node:` | 2.118.236 | 23,2 | 12,2 |
| `norm:` | 241 | 0,0 | 0,2 |
| `pos:` | 870.885 | 8,7 | 134,9 |
| `post:` | 870.885 | 9,5 | 61,1 |

Raiz (`node:0`): 13.250 bytes. Listas: "anos" 54,6 KB, "area" 90,1 KB, "historia" 52,9 KB, "parte" 51,3 KB, "populacao" 48,8 KB

## Fase 4: latência de busca (100k, cache quente, 1 thread)

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `and-2` | 249 µs | 276 µs | 331 µs | 404 µs | 3.920 | 19,3 | 19,3 | 0,00 |
| `and-3` | 376 µs | 410 µs | 505 µs | 618 µs | 2.605 | 0,7 | 28,6 | 0,00 |
| `and-5` | 633 µs | 779 µs | 902 µs | 1,1 ms | 1.532 | 0,0 | 47,7 | 0,00 |
| `and-freq` | 592 µs | 842 µs | 1,0 ms | 1,3 ms | 1.646 | 8.310,1 | 18,6 | 0,00 |
| `and-raro-freq` | 299 µs | 396 µs | 500 µs | 584 µs | 3.179 | 0,9 | 17,3 | 0,00 |
| `frase-2` | 454 µs | 1,2 ms | 1,9 ms | 2,7 ms | 1.756 | 852,3 | 18,8 | 0,00 |
| `frase-3` | 580 µs | 1,2 ms | 1,5 ms | 2,2 ms | 1.513 | 162,1 | 27,5 | 0,07 |
| `frase-freq` | 926 µs | 1,5 ms | 2,0 ms | 2,9 ms | 1.032 | 5.335,9 | 18,7 | 0,00 |
| `frase-miss` | 441 µs | 1,1 ms | 1,7 ms | 2,6 ms | 1.841 | 8,0 | 18,9 | 0,00 |
| `frase-stop` | 494 µs | 1,4 ms | 2,0 ms | 3,8 ms | 1.609 | 654,8 | 19,6 | 0,05 |
| `misto` | 246 µs | 560 µs | 1,2 ms | 3,0 ms | 3.602 | 744,2 | 30,9 | 0,00 |
| `p-1` | 2,1 ms | 3,7 ms | 5,1 ms | 6,2 ms | 423 | 24.870,6 | 591,4 | 0,00 |
| `p-2` | 2,4 ms | 3,5 ms | 4,8 ms | 9,4 ms | 424 | 15.476,9 | 858,9 | 0,61 |
| `p-4` | 467 µs | 2,5 ms | 3,3 ms | 4,8 ms | 1.293 | 5.573,6 | 273,4 | 0,18 |
| `p-raro` | 143 µs | 222 µs | 278 µs | 2,1 ms | 6.385 | 14,0 | 15,4 | 0,00 |
| `rank-and` | 422 µs | 696 µs | 916 µs | 1,2 ms | 2.244 | 1.509,1 | 22,8 | 0,00 |
| `rank-freq` | 394 µs | 618 µs | 734 µs | 813 µs | 2.412 | 25.465,9 | 9,4 | 0,00 |
| `rank-titulos` | 253 µs | 524 µs | 699 µs | 1,1 ms | 3.737 | 153,7 | 17,8 | 0,00 |
| `s-1` | 142 µs | 185 µs | 245 µs | 289 µs | 6.576 | 10,0 | 12,3 | 0,00 |
| `s-2` | 155 µs | 202 µs | 247 µs | 475 µs | 6.151 | 10,0 | 19,2 | 0,00 |
| `s-4` | 163 µs | 217 µs | 281 µs | 454 µs | 5.868 | 9,2 | 30,2 | 0,00 |
| `s-raro` | 130 µs | 161 µs | 185 µs | 434 µs | 7.332 | 2,4 | 13,0 | 0,00 |
| `t-freq` | 162 µs | 211 µs | 264 µs | 418 µs | 5.827 | 25.453,7 | 7,5 | 0,00 |
| `t-len-15` | 133 µs | 161 µs | 187 µs | 268 µs | 7.167 | 485,6 | 17,0 | 0,00 |
| `t-len-3` | 120 µs | 193 µs | 283 µs | 719 µs | 7.352 | 482,0 | 5,0 | 0,00 |
| `t-len-8` | 127 µs | 159 µs | 181 µs | 400 µs | 7.431 | 599,0 | 10,0 | 0,00 |
| `t-medio` | 127 µs | 162 µs | 195 µs | 262 µs | 7.480 | 682,7 | 9,4 | 0,00 |
| `t-miss` | 120 µs | 164 µs | 213 µs | 270 µs | 7.830 | 0,0 | 6,2 | 0,00 |
| `t-raro` | 126 µs | 151 µs | 193 µs | 272 µs | 7.559 | 2,0 | 9,9 | 0,00 |
| `titulos` | 248 µs | 493 µs | 694 µs | 1,0 ms | 3.926 | 153,7 | 15,8 | 0,00 |

<sub>2026-10-25T23-08-16-4fe734a-modificado-bench-100k-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 190 µs | 162 µs | 276 µs | 264 µs | 3,0 |
| `titulos` | 262 µs | 248 µs | 803 µs | 694 µs | 4,6 |
| `and-freq` | 706 µs | 592 µs | 1,4 ms | 1,0 ms | 7,5 |
| `rank-freq` | 404 µs | 394 µs | 682 µs | 734 µs | 3,0 |
| `frase-2` | 516 µs | 454 µs | 2,2 ms | 1,9 ms | 11,1 |
| `frase-freq` | 1,3 ms | 926 µs | 2,9 ms | 2,0 ms | 12,0 |

Compactação: 2.6s, banco de 250,0 MB → 233,5 MB. Abrir o banco + 1 busca: 32 ms antes, 35 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 2,2 ms | 127 µs | 123 µs |
| `titulos` | 3,6 ms | 248 µs | 244 µs |
| `misto` | 3,6 ms | 246 µs | 235 µs |
| `p-2` | 158,4 ms | 2,4 ms | 2,1 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 4.108 | 1,0× | 234 µs | 919 µs |
| 2 | 6.863 | 1,7× | 238 µs | 947 µs |
| 4 | 12.801 | 3,1× | 237 µs | 1,1 ms |
| 8 | 19.229 | 4,7× | 258 µs | 1,4 ms |
| 12 | 23.899 | 5,8× | 287 µs | 1,8 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 256 µs | 274 µs | 1,7 ms | 2,0 ms | 6.774 | 0,0s |
| 4 | 253 µs | 263 µs | 1,7 ms | 1,8 ms | 6.182 | 0,0s |
| 8 | 269 µs | 281 µs | 1,8 ms | 2,0 ms | 5.749 | 0,0s |

## Frases e ranking (formato v2)

Plano em [docs/plano-frases-e-ranking.md](../docs/plano-frases-e-ranking.md). A linha de base é a última carga no formato v1 (sem tf nem posições) com o mesmo rótulo.

| Métrica | Linha de base (v1) | Agora | Meta |
|---|---|---|---|
| Carga 100k com posições | 1.206 docs/s | 3.284 (3.284–3.519) docs/s (+172%) | queda ≤ 30% |
| Carga 100k sem posições | 1.206 docs/s | 4.558 docs/s (+278%) | queda ≤ 10% |
| Banco ÷ texto logo após a carga, com posições | 0,18 | 0,55 | registrar |
| Banco ÷ texto logo após a carga, sem posições | 0,18 | 0,24 | |
| Banco compactado ÷ texto, com posições | — | 0,50 (233,5 MB) | registrar |
| Banco compactado ÷ texto, sem posições | — | 0,19 (90,2 MB) | |
| Pico de RSS na carga, com posições | 264,7 MB | 509,3 MB | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | — | 2,44× | ≤ 2× |
| p50 de `frase-2` | — | 454 µs | registrar |
| Título → artigo (MRR) | — | 0,888 (top-1 82,2%, top-10 98,4%) | registrar e acompanhar |

### Espaço por tipo de chave (100k, valores lógicos)

| Espaço | Com posições (MB) | Sem posições (MB) |
|---|---|---|
| `doc:` | 2,3 | 2,3 |
| `meta:` | 0,0 | 0,0 |
| `node:` | 35,4 | 35,4 |
| `norm:` | 0,2 | 0,2 |
| `pos:` | 143,6 | — |
| `post:` | 70,6 | 70,6 |

### Ranking: top 10 vs. todos os IDs (100k, p50)

| Consulta | Sem ranking | Top 10 (BM25) | Razão |
|---|---|---|---|
| `t-freq` / `rank-freq` | 162 µs | 394 µs | 2,44× |
| `and-2` / `rank-and` | 249 µs | 422 µs | 1,70× |
| `titulos` / `rank-titulos` | 248 µs | 253 µs | 1,02× |

### Frases (100k)

| Conjunto | p50 | p95 | p99 | resultados (média) | Gets/consulta |
|---|---|---|---|---|---|
| `frase-2` | 454 µs | 1,2 ms | 1,9 ms | 852,3 | 18,8 |
| `frase-3` | 580 µs | 1,2 ms | 1,5 ms | 162,1 | 27,5 |
| `frase-stop` | 494 µs | 1,4 ms | 2,0 ms | 654,8 | 19,6 |
| `frase-freq` | 926 µs | 1,5 ms | 2,0 ms | 5.335,9 | 18,7 |
| `frase-miss` | 441 µs | 1,1 ms | 1,7 ms | 8,0 | 18,9 |

## Metas (linha de base em 100k)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de 100k | 3.284 | |
| Banco ÷ texto bruto | 0,55 | |
| p99 de `titulos` (quente) | 694 µs | |
| p99 de `t-freq` (quente) | 264 µs | |
| p99 de `misto` com indexação em paralelo | 1,8 ms | |
| Abrir o banco + 1 busca (após a carga) | 32 ms | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | 2,44× | **≤ 2×** |
| p50 de `frase-2` | 454 µs | |
