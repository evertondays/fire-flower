# Resultados de performance (escala 10k)

Gerado por `python3 scripts/perf.py relatorio --escala 10k` a partir de `bench/results/`. Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das últimas rodadas do mesmo commit (até 5), com a faixa mín–máx entre parênteses. O plano está em [docs/plano-performance.md](../docs/plano-performance.md).

- Último resultado: `2026-10-25T21-23-23-c3e98df-oracle-10k-todos.json`
- Máquina: 12 núcleos, 7.883,2 MB de RAM, kernel `6.6.87.2-microsoft-standard-WSL2`

## Fase 1: corretude

| Checagem | Resultado |
|---|---|
| `oraculo_10k` | ✅ ok |
| `idempotencia` | ✅ ok |
| `lotes_vs_unico` | ✅ ok |
| `reabrir` | ✅ ok |

Idempotência (10k): banco de 17,4 MB após uma carga, 27,2 MB após duas e 16,3 MB depois de compactar.

**Oráculo `10k-todos`** (10.000 docs, 288.203 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas de postagem | 288.203 | 0 | 288203 termos no motor, 288203 no oráculo |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 999 de 2000 prefixos passaram de 256 termos; 5279247 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |

**Oráculo `10k-reaberto`** (10.100 docs, 289.796 termos): zero divergências

| Verificação | Casos | Divergências | Notas |
|---|---|---|---|
| listas de postagem | 15.573 | 0 | amostra de 15573 termos em 13 faixas de df |
| AND | 5.000 | 0 |  |
| prefixo | 2.000 | 0 | 1041 de 2000 prefixos passaram de 256 termos; 6280055 termos ignorados no total (resultado incompleto por projeto) |
| autocompletar | 2.000 | 0 |  |
| título → artigo | 1.000 | 0 |  |
| normalização | 8 | 0 |  |

## Fase 2: escada de indexação

| Fatia | Commit | Rodadas | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1k | `c3e98df` | 3 | 1.2s | 869 (828–871) | 17,6 | 109,2 MB | 8,3 MB | 0,41 | 294.199 | 114.201 | 0,00 | 0,0s |
| 10k | `c3e98df` | 3 | 6.3s | 1.586 (1.451–1.707) | 14,7 | 247,5 MB | 21,6 MB | 0,23 | 728.495 | 288.203 | 0,53 | 0,0s |
| 100k | `d314be1` | 1 | 1min23s | 1.206 | 5,7 | 264,7 MB | 86,8 MB | 0,18 | 2.118.335 | 871.185 | 0,80 | 0,0s |

### Experimentos (10k)

| Experimento | docs/s | vs. padrão | Pico RSS | Banco |
|---|---|---|---|---|
| padrão (lote 1000) | 1.586 (1.451–1.707) | — | 247,5 MB | 21,6 MB |
| lote-100 | 1.181 (1.166–1.211) | -26% | 218,5 MB | 35,7 MB |
| lote-5000 | 1.855 (1.704–1.885) | +17% | 236,5 MB | 27,0 MB |
| lote-10000 | 1.917 (1.860–1.923) | +21% | 238,7 MB | 24,3 MB |
| ids-remapeados | 1.592 (1.515–1.604) | +0% | 245,2 MB | 21,0 MB |
| cache-64mb | 1.578 (1.564–1.591) | -0% | 247,4 MB | 21,6 MB |
| cache-1024mb | 1.315 (1.145–1.379) | -17% | 247,8 MB | 21,6 MB |
| streaming-1k | 266 | — | 127,8 MB | 22,2 MB |
| sem-cache-de-nos | 1.415 (1.321–1.472) | -11% | 156,0 MB | 21,6 MB |

### Tamanhos (10k)

| Espaço | Chaves | Chaves (MB) | Valores (MB) |
|---|---|---|---|
| `doc:` | 10.000 | 0,1 | 0,1 |
| `meta:` | 3 | 0,0 | 0,0 |
| `node:` | 728.495 | 7,5 | 4,2 |
| `post:` | 288.203 | 3,0 | 6,4 |

Raiz (`node:0`): 4.991 bytes. Listas: "anos" 4,1 KB, "area" 5,0 KB, "historia" 4,3 KB, "parte" 4,1 KB, "populacao" 4,9 KB

## Fase 4: latência de busca (10k, cache quente, 1 thread)

| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |
|---|---|---|---|---|---|---|---|---|
| `and-2` | 107 µs | 127 µs | 147 µs | 227 µs | 8.952 | 41,1 | 18,8 | 0,00 |
| `and-3` | 162 µs | 209 µs | 260 µs | 314 µs | 5.868 | 5,7 | 28,3 | 0,00 |
| `and-5` | 268 µs | 345 µs | 449 µs | 506 µs | 3.571 | 0,2 | 47,1 | 0,00 |
| `and-freq` | 193 µs | 268 µs | 336 µs | 422 µs | 5.252 | 1.657,4 | 18,7 | 0,00 |
| `and-raro-freq` | 110 µs | 132 µs | 152 µs | 204 µs | 8.731 | 1,3 | 17,6 | 0,00 |
| `misto` | 102 µs | 291 µs | 998 µs | 2,7 ms | 7.505 | 339,4 | 33,5 | 0,00 |
| `p-1` | 1,4 ms | 1,8 ms | 2,0 ms | 2,6 ms | 708 | 4.568,6 | 641,2 | 0,00 |
| `p-2` | 1,8 ms | 2,6 ms | 3,5 ms | 5,2 ms | 596 | 3.552,7 | 866,2 | 0,00 |
| `p-4` | 212 µs | 1,3 ms | 2,3 ms | 3,3 ms | 2.746 | 1.437,3 | 183,3 | 0,00 |
| `p-raro` | 64 µs | 90 µs | 138 µs | 203 µs | 14.399 | 18,0 | 15,7 | 0,00 |
| `s-1` | 62 µs | 73 µs | 85 µs | 108 µs | 15.224 | 10,0 | 13,0 | 0,00 |
| `s-2` | 89 µs | 156 µs | 220 µs | 472 µs | 9.785 | 10,0 | 24,0 | 0,00 |
| `s-4` | 89 µs | 154 µs | 202 µs | 386 µs | 10.175 | 8,7 | 30,0 | 0,00 |
| `s-raro` | 62 µs | 95 µs | 124 µs | 175 µs | 14.557 | 2,6 | 13,1 | 0,00 |
| `t-freq` | 56 µs | 75 µs | 92 µs | 125 µs | 16.422 | 3.810,7 | 7,5 | 0,00 |
| `t-len-15` | 70 µs | 120 µs | 157 µs | 397 µs | 11.764 | 61,9 | 17,0 | 0,00 |
| `t-len-3` | 54 µs | 92 µs | 120 µs | 297 µs | 15.372 | 612,4 | 5,0 | 0,00 |
| `t-len-8` | 67 µs | 115 µs | 190 µs | 510 µs | 12.044 | 345,5 | 10,0 | 0,00 |
| `t-medio` | 55 µs | 71 µs | 90 µs | 264 µs | 16.931 | 380,0 | 9,5 | 0,00 |
| `t-miss` | 45 µs | 58 µs | 69 µs | 112 µs | 20.189 | 0,0 | 5,9 | 0,00 |
| `t-raro` | 53 µs | 67 µs | 80 µs | 154 µs | 17.527 | 2,1 | 10,0 | 0,00 |
| `titulos` | 105 µs | 205 µs | 275 µs | 324 µs | 9.418 | 139,9 | 15,3 | 0,00 |

<sub>2026-10-25T21-10-34-64ca0c2-bench-10k-t1.json</sub>

### Merge pendente vs. compactado

| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |
|---|---|---|---|---|---|
| `t-freq` | 70 µs | 56 µs | 102 µs | 92 µs | 2,0 |
| `titulos` | 107 µs | 105 µs | 350 µs | 275 µs | 3,4 |
| `and-freq` | 219 µs | 193 µs | 323 µs | 336 µs | 2,5 |

Compactação: 0.4s, banco de 17,5 MB → 16,3 MB. Abrir o banco + 1 busca: 19 ms antes, 20 ms depois (mediana de 5).

### Block cache (p50)

O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads por consulta no JSON).

| Conjunto | 64 MB | 256 MB | 1024 MB |
|---|---|---|---|
| `t-medio` | 888 µs | 55 µs | 52 µs |
| `titulos` | 1,4 ms | 105 µs | 101 µs |
| `misto` | 1,3 ms | 102 µs | 100 µs |
| `p-2` | 83,7 ms | 1,8 ms | 1,7 ms |

## Fase 5: concorrência

### Só leitura (`misto`)

| Threads | qps | ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 7.992 | 1,0× | 100 µs | 998 µs |
| 2 | 12.121 | 1,5× | 101 µs | 1,0 ms |
| 4 | 14.277 | 1,8× | 112 µs | 1,5 ms |
| 8 | 23.187 | 2,9× | 147 µs | 1,8 ms |
| 12 | 27.767 | 3,5× | 182 µs | 2,6 ms |

### Leitura durante escrita (carga dos últimos 10% da fatia)

| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |
|---|---|---|---|---|---|---|
| 1 | 108 µs | 116 µs | 1,2 ms | 1,4 ms | 1.442 | 0,0s |
| 4 | 109 µs | 116 µs | 995 µs | 1,1 ms | 1.257 | 0,0s |
| 8 | 123 µs | 153 µs | 1,3 ms | 2,0 ms | 927 | 0,0s |

## Metas (linha de base em 10k)

| Métrica | Linha de base | Meta |
|---|---|---|
| Divergências do oráculo | 0 | **0** |
| Carga sem swap (aumento < 64 MB) | sim | **sim** |
| docs/s na carga de 10k | 1.586 | |
| Banco ÷ texto bruto | 0,23 | |
| p99 de `titulos` (quente) | 275 µs | |
| p99 de `t-freq` (quente) | 92 µs | |
| p99 de `misto` com indexação em paralelo | 1,1 ms | |
| Abrir o banco + 1 busca (após a carga) | 19 ms | |
