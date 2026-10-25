# Resultados de performance (escala 10k)

Gerado por `python3 scripts/perf.py relatorio --escala 10k` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-25T20-39-46-eec4683-sizes-ptwiki-10k.json`
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
| 1k | 1.8s | 562 | 11,4 | 118,9 MB | 8,3 MB | 0,41 | 294.366 | 114.480 | 0,00 | 0,0s |
| 10k | 10.5s | 955 | 8,9 | 159,9 MB | 22,4 MB | 0,24 | 728.633 | 288.493 | 0,54 | 0,0s |
| 100k | 1min23s | 1.206 | 5,7 | 264,7 MB | 86,8 MB | 0,18 | 2.118.335 | 871.185 | 0,80 | 0,0s |

### Experimentos (10k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 1000) | 955 | — | 159,9 MB | 22,4 MB |
| lote-100 | 592 | -38% | 126,1 MB | 36,3 MB |
| lote-5000 | 1.177 | +23% | 226,1 MB | 27,4 MB |
| lote-10000 | 1.201 | +26% | 301,8 MB | 24,7 MB |
| ids-remapeados | 1.018 | +7% | 161,8 MB | 21,7 MB |
| cache-64mb | 1.009 | +6% | 161,9 MB | 22,4 MB |
| cache-1024mb | 1.019 | +7% | 160,8 MB | 22,4 MB |
| streaming-1k | 163 | — | 71,4 MB | 23,0 MB |

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
| `and-2` | 113 µs | 138 µs | 170 µs | 241 µs | 8.474 | 41,3 | 18,6 | 0,00 |
| `and-3` | 203 µs | 328 µs | 387 µs | 527 µs | 4.368 | 7,2 | 28,0 | 0,00 |
| `and-5` | 274 µs | 312 µs | 376 µs | 455 µs | 3.556 | 0,3 | 46,6 | 0,00 |
| `and-freq` | 219 µs | 296 µs | 362 µs | 465 µs | 4.865 | 4.220,8 | 14,3 | 0,00 |
| `and-raro-freq` | 117 µs | 158 µs | 205 µs | 407 µs | 8.005 | 1,8 | 15,7 | 0,00 |
| `misto` | 99 µs | 306 µs | 1,1 ms | 2,2 ms | 7.356 | 320,4 | 30,3 | 0,00 |
| `p-1` | 1,7 ms | 2,3 ms | 3,0 ms | 3,8 ms | 566 | 6.862,9 | 639,0 | 0,00 |
| `p-2` | 2,3 ms | 3,9 ms | 4,8 ms | 7,8 ms | 453 | 4.169,1 | 844,3 | 0,00 |
| `p-4` | 292 µs | 2,0 ms | 3,3 ms | 9,2 ms | 1.879 | 1.443,7 | 178,8 | 0,00 |
| `p-raro` | 71 µs | 123 µs | 147 µs | 399 µs | 12.421 | 14,7 | 15,8 | 0,00 |
| `s-1` | 64 µs | 90 µs | 123 µs | 163 µs | 14.047 | 10,0 | 12,8 | 0,00 |
| `s-2` | 86 µs | 138 µs | 199 µs | 280 µs | 10.479 | 10,0 | 23,4 | 0,00 |
| `s-4` | 95 µs | 164 µs | 258 µs | 457 µs | 9.603 | 8,7 | 30,1 | 0,00 |
| `s-raro` | 63 µs | 88 µs | 113 µs | 265 µs | 14.695 | 2,6 | 13,2 | 0,00 |
| `t-freq` | 60 µs | 79 µs | 103 µs | 185 µs | 15.405 | 6.344,3 | 5,7 | 0,00 |
| `t-len-15` | 73 µs | 125 µs | 152 µs | 387 µs | 11.489 | 61,9 | 17,0 | 0,00 |
| `t-len-3` | 48 µs | 63 µs | 73 µs | 208 µs | 18.943 | 969,0 | 5,0 | 0,00 |
| `t-len-8` | 57 µs | 72 µs | 88 µs | 137 µs | 16.191 | 300,8 | 10,0 | 0,00 |
| `t-medio` | 56 µs | 71 µs | 86 µs | 113 µs | 16.649 | 415,5 | 9,4 | 0,00 |
| `t-miss` | 49 µs | 66 µs | 83 µs | 118 µs | 18.647 | 0,0 | 6,0 | 0,00 |
| `t-raro` | 58 µs | 90 µs | 101 µs | 140 µs | 15.220 | 2,2 | 9,9 | 0,00 |
| `titulos` | 109 µs | 254 µs | 367 µs | 486 µs | 7.867 | 143,6 | 16,3 | 0,00 |

<sub>2026-10-25T20-32-57-b983465-bench-10k-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 85 µs | 60 µs | 172 µs | 103 µs | 2,0 |
| `titulos` | 120 µs | 109 µs | 590 µs | 367 µs | 3,0 |
| `and-freq` | 286 µs | 219 µs | 491 µs | 362 µs | 2,5 |

Compactação: 0.5s, banco de 17,7 MB → 16,3 MB. Abrir o banco + 1 busca: 32 ms antes, 30 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 893 µs | 56 µs | 58 µs |
| `titulos` | 1,4 ms | 109 µs | 107 µs |
| `misto` | 1,2 ms | 99 µs | 88 µs |
| `p-2` | 85,9 ms | 2,3 ms | 2,0 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 4.889 | 1,0× | 126 µs | 1,7 ms |
| 2 | 7.592 | 1,6× | 125 µs | 1,8 ms |
| 4 | 14.047 | 2,9× | 133 µs | 1,5 ms |
| 8 | 24.261 | 5,0× | 152 µs | 2,0 ms |
| 12 | 30.586 | 6,3× | 156 µs | 2,5 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 104 µs | 106 µs | 1,2 ms | 1,2 ms | 1.327 | 0,0s |
| 4 | 100 µs | 103 µs | 993 µs | 1,0 ms | 1.238 | 0,0s |
| 8 | 120 µs | 121 µs | 1,4 ms | 1,5 ms | 871 | 0,0s |

## Metas (linha de base em 10k)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de 10k | 955 | |
| Banco ÷ texto bruto | 0,24 | |
| p99 de `titulos` (quente) | 367 µs | |
| p99 de `t-freq` (quente) | 103 µs | |
| p99 de `misto` com indexação em paralelo | 1,0 ms | |
| Abrir o banco + 1 busca (após a carga) | 32 ms | |
