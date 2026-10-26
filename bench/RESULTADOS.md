# Resultados de performance (escala full)

Gerado por `python3 scripts/perf.py relatorio --escala full` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das últimas rodadas do mesmo commit (até 5), com a faixa mín–máx entre parênteses. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-26T14-56-59-ad63e51-bench-pr-full.json`
- Máquina: 12 núcleos, 7.883,2 MB de RAM, kernel `6.6.87.2-microsoft-standard-WSL2`

## Fase 1: corretude

| Checagem | Resultado |
|---|---|
| `oraculo_10k` | ✅ ok |
| `idempotencia` | ✅ ok |
| `lotes_vs_unico` | ✅ ok |
| `oraculo_100k` | ✅ ok |
| `reabrir` | ✅ ok |

Idempotência (10k): banco de 51,4 MB após uma carga, 97,3 MB após duas e 51,5 MB depois de compactar.

**Oráculo `10k-todos`** (10.000 docs, 288.203 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 288.203 | 0 | 288203 termos no motor, 288203 no oráculo |
| posições | 288.203 | 0 |  |
| tamanhos e estatísticas | 10.004 | 0 |  |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1006 de 2000 prefixos passaram de 256 termos; 5960096 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1538 frases com resultado, 462 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo, frase e título; BM25F; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 99,3%, top-10 100,0%, MRR 0,996, fora do top-1000: 0.

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
| tamanhos e estatísticas | 100.004 | 0 |  |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1179 de 2000 prefixos passaram de 256 termos; 12748802 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1557 frases com resultado, 443 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo, frase e título; BM25F; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 99,3%, top-10 100,0%, MRR 0,996, fora do top-1000: 0.

**Oráculo `100k-reaberto`** (101.000 docs, 871.551 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas (doc, tf) | 14.306 | 0 | amostra de 14306 termos estratificada por df |
| posições | 14.306 | 0 |  |
| tamanhos e estatísticas | 101.004 | 0 |  |
| AND | 4.997 | 0 |  |
| prefixo | 2.000 | 0 | 1161 de 2000 prefixos passaram de 256 termos; 15306614 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |
| frases | 2.000 | 0 | 1558 frases com resultado, 442 sem (ordem trocada, em geral) |
| ranking BM25 (top 10) | 1.000 | 0 | termo frequente, termo qualquer, E, termo + prefixo, frase e título; BM25F; notas com diferença relativa < 1e-4 |

Relevância (título → artigo, 1.000 consultas): top-1 99,3%, top-10 100,0%, MRR 0,996, fora do top-1000: 0.

## Fase 2: escada de indexação

| Fatia | Commit | Rodadas | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1k | `ad63e51` | 3 | 0.7s | 1.368 (1.346–1.396) | 27,6 | 138,6 MB | 16,2 MB | 0,80 | 294.199 | 114.201 | 0,00 | 0,0s |
| 10k | `ad63e51` | 3 | 2.7s | 3.758 (3.711–3.823) | 34,8 | 422,1 MB | 59,8 MB | 0,65 | 728.495 | 288.203 | 0,00 | 0,0s |
| 100k | `ad63e51` | 1 | 15.4s | 6.481 | 30,5 | 734,8 MB | 267,4 MB | 0,57 | 2.118.236 | 870.885 | 1,28 | 0,0s |
| full | `ad63e51` | 2 | 1min57s | 9.509 (9.509–9.570) | 23,0 | 1.375,0 MB | 1.494,9 MB | 0,55 | 7.295.494 | 3.055.342 | 3,03 | 0,0s |

### Experimentos (100k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 10000) | 6.481 | — | 734,8 MB | 267,4 MB |
| lote-100 | 2.255 | -65% | 618,7 MB | 270,7 MB |
| lote-5000 | 5.880 | -9% | 731,5 MB | 283,6 MB |
| lote-10000 | 5.622 (5.622–5.980) | -13% | 696,4 MB | 479,8 MB |
| lote-50000 | 7.666 | +18% | 1.328,8 MB | 290,2 MB |
| ids-remapeados | 6.525 | +1% | 733,3 MB | 259,0 MB |
| cache-64mb | 6.612 | +2% | 735,2 MB | 267,4 MB |
| cache-1024mb | 6.533 | +1% | 734,1 MB | 267,4 MB |
| sem-cache-de-nos | 4.857 | -25% | 545,1 MB | 269,4 MB |
| sem-posicoes | 8.475 | +31% | 717,2 MB | 123,1 MB |
| streaming-10k | 623 | — | 221,2 MB | 80,7 MB |
| lote-1000 | 4.283 | -34% | 660,1 MB | 266,5 MB |

### Tamanhos (full)

| Espaço | Chaves | Chaves (MB) | Valores (MB) |
|---|---|---|---|
| `doc:` | 1.112.246 | 11,4 | 20,5 |
| `meta:` | 8 | 0,0 | 0,0 |
| `node:` | 7.295.494 | 82,4 | 47,0 |
| `norm:` | 7.025 | 0,1 | 4,7 |
| `pos:` | 3.055.342 | 31,5 | 857,5 |
| `post:` | 3.055.342 | 34,4 | 432,7 |

Raiz (`node:0`): 37.494 bytes. Listas: "anos" 474,6 KB, "area" 368,9 KB, "historia" 403,0 KB, "parte" 375,6 KB, "populacao" 248,0 KB

## Fase 4: latência de busca (full, cache quente, 1 thread)

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `and-2` | 33 µs | 59 µs | 84 µs | 130 µs | 26.844 | 3,4 | 9,3 | 0,00 |
| `and-3` | 48 µs | 74 µs | 93 µs | 137 µs | 19.200 | 0,0 | 13,9 | 0,00 |
| `and-5` | 81 µs | 112 µs | 125 µs | 234 µs | 11.726 | 0,0 | 23,5 | 0,00 |
| `and-freq` | 2,0 ms | 2,7 ms | 3,3 ms | 4,8 ms | 509 | 27.692,5 | 6,2 | 0,00 |
| `and-raro-freq` | 28 µs | 45 µs | 58 µs | 109 µs | 32.344 | 0,5 | 7,5 | 0,00 |
| `frase-2` | 496 µs | 5,4 ms | 10,9 ms | 20,2 ms | 733 | 6.420,9 | 9,6 | 3,19 |
| `frase-3` | 313 µs | 4,4 ms | 10,0 ms | 14,3 ms | 1.008 | 736,8 | 14,3 | 5,82 |
| `frase-freq` | 4,1 ms | 19,1 ms | 19,4 ms | 21,8 ms | 175 | 46.443,2 | 8,7 | 0,82 |
| `frase-miss` | 504 µs | 5,4 ms | 9,6 ms | 17,2 ms | 758 | 37,7 | 9,7 | 3,33 |
| `frase-stop` | 785 µs | 4,9 ms | 7,0 ms | 11,3 ms | 696 | 1.749,4 | 9,8 | 3,83 |
| `misto` | 36 µs | 1,6 ms | 3,1 ms | 6,5 ms | 4.196 | 2.847,8 | 58,8 | 0,05 |
| `p-1` | 3,4 ms | 8,7 ms | 26,6 ms | 26,9 ms | 253 | 160.037,6 | 256,0 | 0,00 |
| `p-2` | 3,2 ms | 6,0 ms | 7,1 ms | 10,5 ms | 308 | 78.483,4 | 596,9 | 61,91 |
| `p-4` | 834 µs | 4,4 ms | 6,3 ms | 9,1 ms | 692 | 21.832,8 | 419,1 | 20,05 |
| `p-raro` | 25 µs | 60 µs | 78 µs | 121 µs | 31.683 | 22,2 | 9,0 | 0,00 |
| `rank-and` | 412 µs | 2,3 ms | 4,0 ms | 9,4 ms | 1.391 | 4.296,9 | 12,4 | 0,03 |
| `rank-freq` | 917 µs | 2,4 ms | 3,5 ms | 4,3 ms | 896 | 149.212,6 | 5,5 | 0,00 |
| `rank-titulos` | 63 µs | 960 µs | 1,9 ms | 2,9 ms | 4.833 | 374,1 | 12,2 | 0,00 |
| `s-1` | 23 µs | 32 µs | 39 µs | 56 µs | 40.856 | 10,0 | 0,0 | 0,00 |
| `s-2` | 12 µs | 22 µs | 52 µs | 110 µs | 67.109 | 10,0 | 0,5 | 0,00 |
| `s-4` | 63 µs | 123 µs | 224 µs | 471 µs | 13.727 | 9,3 | 25,6 | 0,00 |
| `s-raro` | 17 µs | 43 µs | 62 µs | 133 µs | 46.048 | 2,3 | 6,7 | 0,00 |
| `t-freq` | 328 µs | 443 µs | 725 µs | 750 µs | 2.824 | 144.573,2 | 2,4 | 0,00 |
| `t-len-15` | 28 µs | 40 µs | 49 µs | 69 µs | 31.394 | 586,4 | 12,0 | 0,00 |
| `t-len-3` | 8 µs | 18 µs | 33 µs | 68 µs | 90.150 | 903,3 | 1,0 | 0,00 |
| `t-len-8` | 17 µs | 34 µs | 50 µs | 64 µs | 45.893 | 931,1 | 5,0 | 0,00 |
| `t-medio` | 18 µs | 35 µs | 53 µs | 67 µs | 46.661 | 880,9 | 4,7 | 0,00 |
| `t-miss` | 1 µs | 20 µs | 26 µs | 47 µs | 131.679 | 0,0 | 1,9 | 0,00 |
| `t-raro` | 14 µs | 26 µs | 37 µs | 59 µs | 59.221 | 1,7 | 5,0 | 0,00 |
| `titulos` | 51 µs | 802 µs | 1,5 ms | 1,9 ms | 5.866 | 374,1 | 9,2 | 0,00 |

<sub>2026-10-26T14-49-45-ad63e51-bench-full-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 399 µs | 328 µs | 902 µs | 725 µs | 4,5 |
| `titulos` | 95 µs | 51 µs | 1,7 ms | 1,5 ms | 9,0 |
| `and-freq` | 2,1 ms | 2,0 ms | 5,3 ms | 3,3 ms | 1,9 |
| `rank-freq` | 950 µs | 917 µs | 3,7 ms | 3,5 ms | 0,7 |
| `frase-2` | 640 µs | 496 µs | 12,1 ms | 10,9 ms | 3,0 |
| `frase-freq` | 4,5 ms | 4,1 ms | 25,7 ms | 19,4 ms | 3,2 |

Compactação: 5.3s, banco de 1.445,5 MB → 1.434,0 MB. Abrir o banco + 1 busca: 42 ms antes, 28 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 15 µs | 18 µs | 15 µs |
| `titulos` | 63 µs | 51 µs | 43 µs |
| `misto` | 52 µs | 36 µs | 32 µs |
| `p-2` | 3,4 ms | 3,2 ms | 2,5 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 4.512 | 1,0× | 31 µs | 2,8 ms |
| 2 | 5.445 | 1,2× | 31 µs | 2,7 ms |
| 4 | 9.608 | 2,1× | 32 µs | 2,9 ms |
| 8 | 15.386 | 3,4× | 39 µs | 3,5 ms |
| 12 | 19.521 | 4,3× | 52 µs | 4,5 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 48 µs | 96 µs | 3,8 ms | 5,7 ms | 4.392 | 0,0s |
| 4 | 46 µs | 88 µs | 2,9 ms | 4,2 ms | 3.442 | 0,0s |
| 8 | 66 µs | 116 µs | 3,7 ms | 5,5 ms | 2.584 | 0,0s |

## Frases e ranking (formato v2)

Plano em [docs/plano-frases-e-ranking.md](../docs/plano-frases-e-ranking.md). A linha de base é a última carga no formato v1 (sem tf nem posições) com o mesmo rótulo.

| Métrica | Linha de base (v1) | Agora | Meta |
|---|---|---|---|
| Carga full com posições | — | 9.509 (9.509–9.570) docs/s | queda ≤ 30% |
| Banco ÷ texto logo após a carga, com posições | — | 0,55 | registrar |
| Banco compactado ÷ texto, com posições | — | 0,53 (1.434,0 MB) | registrar |
| Pico de RSS na carga, com posições | — | 1.375,0 MB | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | — | 2,80× | ≤ 2× |
| p50 de `frase-2` | — | 496 µs | registrar |
| Título → artigo (MRR) | — | 0,996 (top-1 99,3%, top-10 100,0%) | registrar e acompanhar |

### Espaço por tipo de chave (full, valores lógicos)

| Espaço | Com posições (MB) | Sem posições (MB) |
|---|---|---|
| `doc:` | 31,9 | 2,3 |
| `meta:` | 0,0 | 0,0 |
| `node:` | 129,5 | 35,4 |
| `norm:` | 4,7 | 0,4 |
| `pos:` | 889,0 | — |
| `post:` | 467,1 | 72,5 |

### Ranking: top 10 vs. todos os IDs (full, p50)

| Consulta | Sem ranking | Top 10 (BM25) | Razão |
|---|---|---|---|
| `t-freq` / `rank-freq` | 328 µs | 917 µs | 2,80× |
| `and-2` / `rank-and` | 33 µs | 412 µs | 12,51× |
| `titulos` / `rank-titulos` | 51 µs | 63 µs | 1,23× |

### Frases (full)

| Conjunto | p50 | p95 | p99 | resultados (média) | Gets/consulta |
|---|---|---|---|---|---|
| `frase-2` | 496 µs | 5,4 ms | 10,9 ms | 6.420,9 | 9,6 |
| `frase-3` | 313 µs | 4,4 ms | 10,0 ms | 736,8 | 14,3 |
| `frase-stop` | 785 µs | 4,9 ms | 7,0 ms | 1.749,4 | 9,8 |
| `frase-freq` | 4,1 ms | 19,1 ms | 19,4 ms | 46.443,2 | 8,7 |
| `frase-miss` | 504 µs | 5,4 ms | 9,6 ms | 37,7 | 9,7 |

## Relevância (100k)

Consultas com resposta conhecida ([docs/plano-relevancia.md](../docs/plano-relevancia.md)). Acerto: top-1 em título, digitando e hub; top-10 em corpo; top-3 nas consultas à mão. Entre parênteses, a linha de base (ranking antes do plano).

| Conjunto | Consultas | top-1 | top-3 | top-10 | MRR | fora do top-1000 |
|---|---|---|---|---|---|---|
| `rel-titulo` | 1000 | 99,3% (81,3%) | 100,0% (92,8%) | 100,0% (97,1%) | 0,996 (0,875) | 0 |
| `rel-digitando` | 1000 | 84,4% (64,6%) | 94,2% (81,2%) | 98,0% (90,1%) | 0,897 (0,741) | 0 |
| `rel-hub` | 1000 | 97,9% (38,6%) | 99,9% (68,2%) | 100,0% (89,0%) | 0,989 (0,560) | 0 |
| `rel-corpo` | 1000 | 49,5% (49,8%) | 68,4% (68,7%) | 81,5% (81,4%) | 0,610 (0,612) | 1 |
| `rel-manual` | 36 | 66,7% (8,3%) | 94,4% (36,1%) | 100,0% (55,6%) | 0,811 (0,251) | 0 |

Exemplos de erro (até 3 por conjunto):

| Conjunto | Consulta | Esperado | Posição | Os 3 primeiros |
|---|---|---|---|---|
| `rel-titulo` | `Linha Nova` | Linha Nova | 2º | Nova linha · Linha Nova · Três Corações (Nova Iguaçu) |
| `rel-titulo` | `Tora! Tora! Tora!` | Tora! Tora! Tora! | 2º | Tora · Tora! Tora! Tora! · Torá |
| `rel-titulo` | `Sé (Bragança)` | Sé (Bragança) | 2º | Bragança · Sé (Bragança) · Bragança (Pará) |
| `rel-digitando` | `astron*` | Astronomia | 3º | Astrônomo · Astronáutica · Astronomia |
| `rel-digitando` | `after the funer*` | After the Funeral | 3º | Funerária · Funeral · After the Funeral |
| `rel-digitando` | `corn*` | Cornes | 12º | Cornualha · Cornucópia · Corno |
| `rel-hub` | `Getúlio Vargas` | Getúlio Vargas | 2º | Getúlio Vargas (desambiguação) · Getúlio Vargas · Carta-testamento de Getúlio Vargas |
| `rel-hub` | `Cruz` | Cruz | 2º | Santa Cruz · Cruz · Cruz de Ferro |
| `rel-hub` | `Luís Manuel da Câmara` | Luís Manuel da Câmara | 2º | Manuel Luís Baltazar da Câmara · Luís Manuel da Câmara · Manuel Luís Baltazar da Câmara, 1.º Conde da Ribeira Grande |
| `rel-corpo` | `baden wurttemberg` | Berg (Ravensburg) | 358º | Baden-Württemberg · Waldenburg (Baden-Württemberg) · Wies (Baden-Württemberg) |
| `rel-corpo` | `cantao ticino` | Carona (Tessino) | 76º | Fontana · Curio (Ticino) · Brione |
| `rel-corpo` | `maritimo sena` | Pommeréval | 256º | Beaurepaire (Sena Marítimo) · Bénouville (Sena Marítimo) · Boos (Sena Marítimo) |
| `rel-manual` | `brasi*` | Brasil | 5º | Brasileiros · Brasiguaios · Brasília |
| `rel-manual` | `lula` | Luiz Inácio Lula da Silva | 6º | Lula (desambiguação) · Lula-colossal · Lula-gigante |

<sub>2026-10-26T14-45-03-ad63e51-relevancia-100k.json</sub>

## Metas (linha de base em full)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de full | 9.509 | |
| Banco ÷ texto bruto | 0,55 | |
| p99 de `titulos` (quente) | 1,5 ms | |
| p99 de `t-freq` (quente) | 725 µs | |
| p99 de `misto` com indexação em paralelo | 4,2 ms | |
| Abrir o banco + 1 busca (após a carga) | 42 ms | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | 2,80× | **≤ 2×** |
| p50 de `frase-2` | 496 µs | |
