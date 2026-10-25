# Resultados de performance (escala 10k)

Gerado por `python3 scripts/perf.py relatorio --escala 10k` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-25T20-47-30-80cad39-bench-pr-10k.json`
- Máquina: 12 núcleos, 7.883,2 MB de RAM, kernel `6.6.87.2-microsoft-standard-WSL2`

## Fase 1: corretude

| Checagem | Resultado |
|---|---|
| `oraculo_10k` | ✅ ok |
| `idempotencia` | ✅ ok |
| `lotes_vs_unico` | ✅ ok |
| `reabrir` | ✅ ok |

Idempotência (10k): banco de 17,6 MB após uma carga, 27,6 MB após duas e 16,3 MB depois de compactar.

**Oráculo `10k-todos`** (10.000 docs, 288.493 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas de postagem | 288.493 | 0 | 288493 termos no motor, 288493 no oráculo |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1035 de 2000 prefixos passaram de 256 termos; 5820267 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |

**Oráculo `10k-reaberto`** (10.100 docs, 290.087 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas de postagem | 14.689 | 0 | amostra de 14689 termos em 14 faixas de df |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1005 de 2000 prefixos passaram de 256 termos; 6006168 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |

## Fase 2: escada de indexação

| Fatia | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |
|---|---|---|---|---|---|---|---|---|---|---|
| 1k | 1.7s | 596 | 12,0 | 118,6 MB | 8,3 MB | 0,41 | 294.366 | 114.480 | 0,00 | 0,0s |
| 10k | 11.9s | 840 | 7,8 | 160,6 MB | 22,4 MB | 0,24 | 728.633 | 288.493 | 0,54 | 0,0s |
| 100k | 1min23s | 1.206 | 5,7 | 264,7 MB | 86,8 MB | 0,18 | 2.118.335 | 871.185 | 0,80 | 0,0s |

### Experimentos (10k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 1000) | 840 | — | 160,6 MB | 22,4 MB |
| lote-100 | 590 | -30% | 124,1 MB | 36,3 MB |
| lote-5000 | 1.171 | +39% | 226,0 MB | 27,4 MB |
| lote-10000 | 1.215 | +45% | 301,8 MB | 24,7 MB |
| ids-remapeados | 1.024 | +22% | 160,4 MB | 21,7 MB |
| cache-64mb | 991 | +18% | 160,0 MB | 22,4 MB |
| cache-1024mb | 958 | +14% | 160,0 MB | 22,4 MB |
| streaming-1k | 159 | — | 71,2 MB | 23,0 MB |

### Tamanhos (10k)

| Espaço | Chaves | Chaves (MB) | Valores (MB) |
|---|---|---|---|
| `doc:` | 10.000 | 0,1 | 0,1 |
| `meta:` | 2 | 0,0 | 0,0 |
| `node:` | 728.633 | 7,5 | 4,2 |
| `post:` | 288.493 | 3,0 | 6,7 |

Raiz (`node:0`): 4.959 bytes. Listas: "a" 9,1 KB, "de" 9,7 KB, "e" 9,3 KB, "em" 8,3 KB, "o" 8,1 KB

## Fase 4: latência de busca (10k, cache quente, 1 thread)

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `and-2` | 115 µs | 148 µs | 178 µs | 264 µs | 8.261 | 41,3 | 18,6 | 0,00 |
| `and-3` | 184 µs | 276 µs | 329 µs | 387 µs | 5.023 | 7,2 | 28,0 | 0,00 |
| `and-5` | 304 µs | 573 µs | 846 µs | 2,5 ms | 2.672 | 0,3 | 46,6 | 0,00 |
| `and-freq` | 216 µs | 263 µs | 353 µs | 577 µs | 4.986 | 4.220,8 | 14,3 | 0,00 |
| `and-raro-freq` | 117 µs | 157 µs | 243 µs | 327 µs | 7.951 | 1,8 | 15,7 | 0,00 |
| `misto` | 99 µs | 311 µs | 1,0 ms | 2,1 ms | 7.266 | 320,4 | 30,3 | 0,00 |
| `p-1` | 1,6 ms | 2,1 ms | 2,5 ms | 3,0 ms | 600 | 6.862,9 | 639,0 | 0,00 |
| `p-2` | 1,9 ms | 2,7 ms | 3,1 ms | 3,9 ms | 572 | 4.169,1 | 844,3 | 0,00 |
| `p-4` | 225 µs | 1,4 ms | 2,3 ms | 2,8 ms | 2.607 | 1.443,7 | 178,8 | 0,00 |
| `p-raro` | 67 µs | 90 µs | 108 µs | 171 µs | 14.109 | 14,7 | 15,8 | 0,00 |
| `s-1` | 61 µs | 81 µs | 103 µs | 148 µs | 14.884 | 10,0 | 12,8 | 0,00 |
| `s-2` | 90 µs | 170 µs | 217 µs | 495 µs | 9.707 | 10,0 | 23,4 | 0,00 |
| `s-4` | 100 µs | 196 µs | 321 µs | 450 µs | 8.698 | 8,7 | 30,1 | 0,00 |
| `s-raro` | 66 µs | 103 µs | 133 µs | 166 µs | 13.612 | 2,6 | 13,2 | 0,00 |
| `t-freq` | 59 µs | 82 µs | 106 µs | 178 µs | 15.336 | 6.344,3 | 5,7 | 0,00 |
| `t-len-15` | 68 µs | 95 µs | 110 µs | 370 µs | 13.202 | 61,9 | 17,0 | 0,00 |
| `t-len-3` | 47 µs | 61 µs | 77 µs | 107 µs | 19.252 | 969,0 | 5,0 | 0,00 |
| `t-len-8` | 55 µs | 67 µs | 78 µs | 136 µs | 17.036 | 300,8 | 10,0 | 0,00 |
| `t-medio` | 57 µs | 74 µs | 90 µs | 137 µs | 16.103 | 415,5 | 9,4 | 0,00 |
| `t-miss` | 54 µs | 80 µs | 108 µs | 122 µs | 16.836 | 0,0 | 6,0 | 0,00 |
| `t-raro` | 81 µs | 127 µs | 169 µs | 367 µs | 11.066 | 2,2 | 9,9 | 0,00 |
| `titulos` | 123 µs | 347 µs | 513 µs | 717 µs | 6.298 | 143,6 | 16,3 | 0,00 |

<sub>2026-10-25T20-43-41-80cad39-bench-10k-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 89 µs | 59 µs | 157 µs | 106 µs | 2,0 |
| `titulos` | 133 µs | 123 µs | 642 µs | 513 µs | 2,9 |
| `and-freq` | 291 µs | 216 µs | 522 µs | 353 µs | 2,5 |

Compactação: 0.4s, banco de 17,7 MB → 16,3 MB. Abrir o banco + 1 busca: 20 ms antes, 20 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 925 µs | 57 µs | 53 µs |
| `titulos` | 1,5 ms | 123 µs | 102 µs |
| `misto` | 1,3 ms | 99 µs | 100 µs |
| `p-2` | 83,3 ms | 1,9 ms | 1,8 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 7.980 | 1,0× | 83 µs | 934 µs |
| 2 | 10.610 | 1,3× | 104 µs | 1,1 ms |
| 4 | 21.304 | 2,7× | 109 µs | 1,1 ms |
| 8 | 34.304 | 4,3× | 113 µs | 1,2 ms |
| 12 | 37.818 | 4,7× | 131 µs | 1,7 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 89 µs | 98 µs | 1,0 ms | 1,1 ms | 1.447 | 0,0s |
| 4 | 102 µs | 102 µs | 998 µs | 1,0 ms | 1.154 | 0,0s |
| 8 | 113 µs | 116 µs | 1,2 ms | 1,3 ms | 989 | 0,0s |

## Metas (linha de base em 10k)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de 10k | 840 | |
| Banco ÷ texto bruto | 0,24 | |
| p99 de `titulos` (quente) | 513 µs | |
| p99 de `t-freq` (quente) | 106 µs | |
| p99 de `misto` com indexação em paralelo | 1,0 ms | |
| Abrir o banco + 1 busca (após a carga) | 20 ms | |
