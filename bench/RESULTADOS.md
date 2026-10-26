# Resultados de performance (escala full)

Gerado por `python3 scripts/perf.py relatorio --escala full` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das últimas rodadas do mesmo commit (até 5), com a faixa mín–máx entre parênteses. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-26T04-44-21-d088f12-bench-pr-full.json`
- Máquina: 12 núcleos, 7.883,2 MB de RAM, kernel `6.6.87.2-microsoft-standard-WSL2`

## Fase 1: corretude

| Checagem | Resultado |
|---|---|
| `oraculo_10k` | ✅ ok |
| `idempotencia` | ✅ ok |
| `lotes_vs_unico` | ✅ ok |
| `oraculo_100k` | ✅ ok |
| `reabrir` | ✅ ok |

Idempotência (10k): banco de 55,9 MB após uma carga, 50,6 MB após duas e 50,6 MB depois de compactar.

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
| 1k | `d088f12` | 5 | 0.7s | 1.415 (1.380–1.445) | 28,6 | 139,3 MB | 16,0 MB | 0,79 | 294.199 | 114.201 | 0,00 | 0,0s |
| 10k | `d088f12` | 5 | 3.7s | 2.682 (2.514–2.702) | 24,9 | 284,1 MB | 67,7 MB | 0,73 | 728.495 | 288.203 | 0,38 | 0,0s |
| 100k | `d088f12` | 4 | 27.3s | 3.658 (3.463–3.807) | 17,2 | 472,0 MB | 260,9 MB | 0,55 | 2.118.236 | 870.885 | 1,38 | 0,0s |
| full | `d088f12` | 2 | 4min02s | 4.590 (4.590–4.609) | 11,1 | 837,5 MB | 1.468,0 MB | 0,54 | 7.295.494 | 3.055.342 | 3,06 | 0,0s |

### Experimentos (100k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 1000) | 3.658 (3.463–3.807) | — | 472,0 MB | 260,9 MB |
| lote-100 | 1.936 (1.936–2.061) | -47% | 472,8 MB | 257,0 MB |
| lote-5000 | 5.169 (5.169–5.362) | +41% | 525,7 MB | 283,2 MB |
| lote-10000 | 5.622 (5.622–5.980) | +54% | 696,4 MB | 479,8 MB |
| lote-50000 | 6.839 (6.839–7.339) | +87% | 1.332,2 MB | 268,8 MB |
| ids-remapeados | 3.438 (3.438–3.755) | -6% | 470,1 MB | 252,4 MB |
| cache-64mb | 3.426 (3.426–3.562) | -6% | 474,0 MB | 260,9 MB |
| cache-1024mb | 3.480 (3.480–3.590) | -5% | 470,2 MB | 260,9 MB |
| sem-cache-de-nos | 1.827 (1.827–1.949) | -50% | 305,3 MB | 260,9 MB |
| sem-posicoes | 4.365 (4.365–4.835) | +19% | 429,3 MB | 111,8 MB |
| streaming-10k | 494 (494–519) | — | 249,6 MB | 64,7 MB |

### Tamanhos (full)

| Espaço | Chaves | Chaves (MB) | Valores (MB) |
|---|---|---|---|
| `doc:` | 1.112.246 | 11,4 | 20,5 |
| `meta:` | 8 | 0,0 | 0,0 |
| `node:` | 7.295.494 | 82,4 | 47,0 |
| `norm:` | 7.025 | 0,1 | 4,7 |
| `pos:` | 3.055.342 | 31,5 | 846,6 |
| `post:` | 3.055.342 | 34,4 | 422,0 |

Raiz (`node:0`): 37.485 bytes. Listas: "anos" 465,4 KB, "area" 361,9 KB, "historia" 395,3 KB, "parte" 368,4 KB, "populacao" 243,2 KB

## Fase 4: latência de busca (full, cache quente, 1 thread)

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `and-2` | 44 µs | 75 µs | 93 µs | 150 µs | 20.560 | 3,4 | 13,1 | 0,00 |
| `and-3` | 65 µs | 98 µs | 124 µs | 197 µs | 14.394 | 0,0 | 19,7 | 0,00 |
| `and-5` | 110 µs | 146 µs | 165 µs | 297 µs | 8.719 | 0,0 | 33,1 | 0,00 |
| `and-freq` | 1,9 ms | 2,8 ms | 3,9 ms | 6,4 ms | 518 | 27.692,5 | 10,2 | 0,00 |
| `and-raro-freq` | 410 µs | 566 µs | 995 µs | 1,1 ms | 2.274 | 0,5 | 11,1 | 0,00 |
| `frase-2` | 928 µs | 5,4 ms | 10,7 ms | 20,3 ms | 626 | 6.420,9 | 13,2 | 3,69 |
| `frase-3` | 1,1 ms | 5,9 ms | 10,1 ms | 14,1 ms | 572 | 736,8 | 19,5 | 6,45 |
| `frase-freq` | 4,1 ms | 17,0 ms | 17,4 ms | 24,7 ms | 180 | 46.443,2 | 12,0 | 0,76 |
| `frase-miss` | 947 µs | 5,3 ms | 9,1 ms | 17,9 ms | 631 | 37,7 | 13,3 | 4,04 |
| `frase-stop` | 1,3 ms | 4,8 ms | 6,9 ms | 10,7 ms | 592 | 1.749,4 | 13,6 | 4,33 |
| `misto` | 55 µs | 1,9 ms | 4,2 ms | 12,0 ms | 2.665 | 2.847,8 | 62,3 | 0,34 |
| `p-1` | 5,1 ms | 12,5 ms | 30,5 ms | 31,5 ms | 175 | 160.037,6 | 504,9 | 0,00 |
| `p-2` | 4,4 ms | 8,1 ms | 10,3 ms | 13,4 ms | 212 | 78.483,4 | 797,0 | 112,15 |
| `p-4` | 1,0 ms | 5,5 ms | 8,5 ms | 11,5 ms | 538 | 21.832,8 | 421,4 | 44,69 |
| `p-raro` | 29 µs | 63 µs | 81 µs | 118 µs | 28.409 | 22,2 | 12,0 | 0,00 |
| `rank-and` | 542 µs | 2,4 ms | 3,9 ms | 9,0 ms | 1.235 | 4.296,9 | 16,9 | 0,06 |
| `rank-freq` | 976 µs | 2,5 ms | 3,6 ms | 4,2 ms | 845 | 149.212,6 | 7,1 | 0,00 |
| `rank-titulos` | 107 µs | 1,1 ms | 1,9 ms | 2,9 ms | 3.341 | 374,1 | 16,8 | 0,00 |
| `s-1` | 29 µs | 40 µs | 49 µs | 169 µs | 32.705 | 10,0 | 0,0 | 0,00 |
| `s-2` | 52 µs | 79 µs | 97 µs | 135 µs | 17.856 | 10,0 | 13,8 | 0,00 |
| `s-4` | 71 µs | 138 µs | 263 µs | 525 µs | 12.402 | 9,3 | 26,6 | 0,00 |
| `s-raro` | 22 µs | 48 µs | 68 µs | 98 µs | 37.114 | 2,3 | 9,8 | 0,00 |
| `t-freq` | 329 µs | 445 µs | 722 µs | 745 µs | 2.816 | 144.573,2 | 4,0 | 0,00 |
| `t-len-15` | 34 µs | 48 µs | 63 µs | 89 µs | 25.972 | 586,4 | 14,0 | 0,00 |
| `t-len-3` | 11 µs | 23 µs | 38 µs | 69 µs | 69.944 | 903,3 | 2,0 | 0,00 |
| `t-len-8` | 23 µs | 40 µs | 55 µs | 72 µs | 36.625 | 931,1 | 7,0 | 0,00 |
| `t-medio` | 23 µs | 41 µs | 55 µs | 79 µs | 38.372 | 880,9 | 6,6 | 0,00 |
| `t-miss` | 7 µs | 23 µs | 29 µs | 55 µs | 87.627 | 0,0 | 3,4 | 0,00 |
| `t-raro` | 18 µs | 33 µs | 45 µs | 70 µs | 45.547 | 1,7 | 6,9 | 0,00 |
| `titulos` | 93 µs | 1,1 ms | 1,8 ms | 2,3 ms | 3.610 | 374,1 | 13,8 | 0,00 |

<sub>2026-10-26T04-32-45-d088f12-bench-full-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 677 µs | 329 µs | 1,5 ms | 722 µs | 5,0 |
| `titulos` | 168 µs | 93 µs | 3,2 ms | 1,8 ms | 10,0 |
| `and-freq` | 2,3 ms | 1,9 ms | 5,2 ms | 3,9 ms | 1,3 |
| `rank-freq` | 1,1 ms | 976 µs | 4,1 ms | 3,6 ms | 0,5 |
| `frase-2` | 1,5 ms | 928 µs | 14,6 ms | 10,7 ms | 2,8 |
| `frase-freq` | 6,2 ms | 4,1 ms | 25,3 ms | 17,4 ms | 3,1 |

Compactação: 8.7s, banco de 1.433,7 MB → 1.411,4 MB. Abrir o banco + 1 busca: 38 ms antes, 29 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 20 µs | 23 µs | 20 µs |
| `titulos` | 105 µs | 93 µs | 83 µs |
| `misto` | 72 µs | 55 µs | 48 µs |
| `p-2` | 5,0 ms | 4,4 ms | 3,7 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 2.731 | 1,0× | 52 µs | 3,9 ms |
| 2 | 3.283 | 1,2× | 62 µs | 5,0 ms |
| 4 | 6.209 | 2,3× | 58 µs | 4,3 ms |
| 8 | 10.377 | 3,8× | 74 µs | 4,8 ms |
| 12 | 11.873 | 4,3× | 85 µs | 6,7 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 79 µs | 139 µs | 5,9 ms | 8,6 ms | 3.767 | 0,0s |
| 4 | 81 µs | 132 µs | 5,4 ms | 7,6 ms | 3.203 | 0,0s |
| 8 | 105 µs | 168 µs | 7,2 ms | 10,0 ms | 2.427 | 0,0s |

## Frases e ranking (formato v2)

Plano em [docs/plano-frases-e-ranking.md](../docs/plano-frases-e-ranking.md). A linha de base é a última carga no formato v1 (sem tf nem posições) com o mesmo rótulo.

| Métrica | Linha de base (v1) | Agora | Meta |
|---|---|---|---|
| Carga full com posições | — | 4.590 (4.590–4.609) docs/s | queda ≤ 30% |
| Banco ÷ texto logo após a carga, com posições | — | 0,54 | registrar |
| Banco compactado ÷ texto, com posições | — | 0,52 (1.411,4 MB) | registrar |
| Pico de RSS na carga, com posições | — | 837,5 MB | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | — | 2,97× | ≤ 2× |
| p50 de `frase-2` | — | 928 µs | registrar |
| Título → artigo (MRR) | — | 0,996 (top-1 99,3%, top-10 100,0%) | registrar e acompanhar |

### Espaço por tipo de chave (full, valores lógicos)

| Espaço | Com posições (MB) | Sem posições (MB) |
|---|---|---|
| `doc:` | 31,9 | 2,3 |
| `meta:` | 0,0 | 0,0 |
| `node:` | 129,5 | 35,4 |
| `norm:` | 4,7 | 0,4 |
| `pos:` | 878,1 | — |
| `post:` | 456,5 | 70,8 |

### Ranking: top 10 vs. todos os IDs (full, p50)

| Consulta | Sem ranking | Top 10 (BM25) | Razão |
|---|---|---|---|
| `t-freq` / `rank-freq` | 329 µs | 976 µs | 2,97× |
| `and-2` / `rank-and` | 44 µs | 542 µs | 12,42× |
| `titulos` / `rank-titulos` | 93 µs | 107 µs | 1,15× |

### Frases (full)

| Conjunto | p50 | p95 | p99 | resultados (média) | Gets/consulta |
|---|---|---|---|---|---|
| `frase-2` | 928 µs | 5,4 ms | 10,7 ms | 6.420,9 | 13,2 |
| `frase-3` | 1,1 ms | 5,9 ms | 10,1 ms | 736,8 | 19,5 |
| `frase-stop` | 1,3 ms | 4,8 ms | 6,9 ms | 1.749,4 | 13,6 |
| `frase-freq` | 4,1 ms | 17,0 ms | 17,4 ms | 46.443,2 | 12,0 |
| `frase-miss` | 947 µs | 5,3 ms | 9,1 ms | 37,7 | 13,3 |

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

<sub>2026-10-26T04-25-37-d088f12-relevancia-100k.json</sub>

## Metas (linha de base em full)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de full | 4.590 | |
| Banco ÷ texto bruto | 0,54 | |
| p99 de `titulos` (quente) | 1,8 ms | |
| p99 de `t-freq` (quente) | 722 µs | |
| p99 de `misto` com indexação em paralelo | 7,6 ms | |
| Abrir o banco + 1 busca (após a carga) | 38 ms | |
| p50 de `rank-freq` ÷ p50 de `t-freq` | 2,97× | **≤ 2×** |
| p50 de `frase-2` | 928 µs | |
