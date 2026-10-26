# Relatório: rodada de performance 100k — formato v3 (BM25F)

Rodada `python3 scripts/perf.py tudo --escala 100k` de 26/10/2026, no commit
`d088f12` (formato v3, BM25F com peso de título 50). Comparada com a rodada de
100k de 25/10, no commit `4fe734a` (formato v2, antes do cache de nós, do
ranking sem reordenar, dos filtros particionados e do plano de relevância).

- Números completos: [RESULTADOS.md](https://github.com/evertondays/fire-flower-bench/blob/main/RESULTADOS.md) em fire-flower-bench (resultado
  `2026-10-26T03-54-34-d088f12-bench-pr-100k.json`).
- Máquina: WSL2, 12 núcleos (Ryzen 5 5600X, 6 físicos), 7,7 GB de RAM.
- Duração: 12 min 25 s. Memória disponível nunca abaixo de 5,3 GB; sem swap.

## Resumo

A rodada está saudável:

- **Corretude:** zero divergências em todos os oráculos.
- **Busca e ranking:** de 2× a 15× mais rápidos que na rodada de 100k de 25/10.
- **Cache pequeno:** a queda brusca com cache de 64 MB sumiu.
- **Relevância:** os números da Fase 4 do plano se repetiram.

Os pontos fracos são três:

- a escala com várias threads piorou em termos relativos;
- os prefixos curtos continuam sendo a consulta mais cara;
- a meta "ranking ≤ 2× a busca simples" segue sem ser atingida.

---

## 1. Corretude

| Checagem | Resultado |
|---|---|
| Oráculo 10k (todos os 288.203 termos) | ✅ zero divergências |
| Oráculo 100k (amostra de 30.958 termos) | ✅ zero divergências |
| Oráculo 100k reaberto (+1.000 documentos) | ✅ zero divergências |
| Idempotência (10k carregado duas vezes) | ✅ |
| Lotes vs. único (1k, lote 1 vs. 1000) | ✅ |

O oráculo confere listas (doc, tf, tf no título), posições, tamanhos,
estatísticas, E, prefixo, autocompletar, normalização, frases e o top 10 do
BM25F (diferença relativa < 1e-4), inclusive com títulos reais como consulta.

## 2. Indexação

| Fatia | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Write amp | Stalls |
|---|---|---|---|---|---|---|---|
| 1k | 1.267 | 25,6 | 140 MB | 16 MB | 0,79 | 0,00 | 0 |
| 10k | 2.438 | 22,6 | 284 MB | 68 MB | 0,73 | 0,38 | 0 |
| 100k | 3.463 | 16,3 | 473 MB | 261 MB | 0,55 | 1,38 | 0 |

- **Mesmo patamar de antes:** em 25/10 eram 3.284 docs/s e 15,4 MB/s. O formato
  v3 (título como campo) não custou nada na carga nem no tamanho: o banco
  compactado tem 0,50× o texto (237 MB).
- **As posições são 60% do banco:** `pos:` tem 147 MB, `post:` 71 MB e a Trie
  35 MB. Sem posições, o banco compactado tem 0,19× o texto.

### Experimentos

| Experimento | docs/s | vs. padrão | Pico RSS |
|---|---|---|---|
| lote 100 | 1.936 | −44% | 473 MB |
| lote 5.000 | 5.169 | +49% | 526 MB |
| lote 10.000 | 5.622 | +62% | 696 MB |
| lote 50.000 | 6.839 | +97% | 1.332 MB |
| IDs remapeados | 3.438 | −1% | 470 MB |
| cache de 64 MB | 3.426 | −1% | 474 MB |
| cache de 1 GB | 3.480 | 0% | 470 MB |
| sem cache de nós do escritor | 1.827 | −47% | 305 MB |
| sem posições | 4.365 | +26% | 429 MB |
| streaming (1 commit por doc, 10k) | 494 | — | 250 MB |

- **Cache de 64 MB:** antes custava −81% na carga e agora −1%. É efeito dos
  filtros de Bloom particionados (`a9009c7`).
- **Lote de 10 mil documentos:** +62% de velocidade a ~700 MB de RSS. Parece
  a melhor escolha para a carga da base completa.
- **Um commit por documento:** ~2 ms por commit (494 docs/s). Para edições
  uma a uma, os commits vão precisar ser agrupados.

## 3. Latência de busca (cache quente, 1 thread)

| Conjunto | p50 em 25/10 | p50 agora | p99 agora |
|---|---|---|---|
| `t-miss` | 120 µs | **8 µs** | 32 µs |
| `t-medio` | 127 µs | **21 µs** | 59 µs |
| `t-freq` | 162 µs | **51 µs** | 98 µs |
| `titulos` | 248 µs | **35 µs** | 214 µs |
| `and-2` | 249 µs | **39 µs** | 72 µs |
| `rank-freq` | 394 µs | **194 µs** | 432 µs |
| `rank-and` | 422 µs | **117 µs** | 517 µs |
| `rank-titulos` | 253 µs | **41 µs** | 235 µs |
| `frase-2` | 454 µs | **181 µs** | 1,4 ms |
| `frase-freq` | 926 µs | **719 µs** | 1,6 ms |
| `s-2` (autocompletar) | 155 µs | **44 µs** | 107 µs |
| `p-2` (prefixo de 2 letras) | 2,4 ms | 2,5 ms | 4,9 ms |
| `misto` | 246 µs | **31 µs** | 1,0 ms |

- **O que trouxe o ganho:** o cache da raiz e dos primeiros níveis da Trie
  (`3e1b2fe`) e o ranking de termo único sem reordenar a lista (`ab395fd`).
  O custo por letra do termo continua ~1 µs (`t-len-3`/`-8`/`-15`: 9, 21 e
  30 µs).
- **`s-1`** (autocompletar de 1 letra) faz zero leituras no RocksDB: tudo vem
  do cache de nós.

### Merge pendente vs. compactado

| Conjunto | p50 antes de compactar | p50 depois |
|---|---|---|
| `t-freq` | 96 µs | 51 µs |
| `titulos` | 48 µs | 35 µs |
| `and-freq` | 393 µs | 284 µs |
| `rank-freq` | 259 µs | 194 µs |
| `frase-2` | 311 µs | 181 µs |
| `frase-freq` | 1,1 ms | 719 µs |

A compactação leva 2,7 s (banco de 254 → 237 MB), e abrir o banco + 1 busca
cai de 32 ms para 18 ms.

### Block cache

| Conjunto | 64 MB | 256 MB | 1 GB |
|---|---|---|---|
| `t-medio` | 19 µs | 21 µs | 19 µs |
| `titulos` | 36 µs | 35 µs | 32 µs |
| `misto` | 37 µs | 31 µs | 32 µs |
| `p-2` | 2,9 ms | 2,5 ms | 2,5 ms |

Em 25/10, com 64 MB, eram 2,2 ms, 3,6 ms, 3,6 ms e 158 ms. Com os filtros
particionados, nenhum bloco de filtro passa do tamanho de uma parte do
cache, e um cache pequeno não derruba mais as buscas.

## 4. Concorrência

### Só leitura (`misto`)

| Threads | qps | Ganho vs. 1 | p50 | p99 |
|---|---|---|---|---|
| 1 | 11.493 | 1,0× | 31 µs | 1,1 ms |
| 2 | 14.199 | 1,2× | 34 µs | 1,2 ms |
| 4 | 24.298 | 2,1× | 36 µs | 1,1 ms |
| 8 | 34.333 | 3,0× | 45 µs | 1,4 ms |
| 12 | 46.111 | 4,0× | 50 µs | 1,7 ms |

A vazão absoluta quase dobrou: com 12 threads eram 23.899 consultas/s em
25/10 e agora são 46.111. O ganho relativo, porém, caiu de 5,8× para 4,0×, e
com 2 threads é só 1,2×. Ver o ponto de atenção 1.

### Leitura durante escrita (os últimos 10 mil artigos)

| Leitores | p50 sem / com escrita | p99 sem / com escrita | Indexação |
|---|---|---|---|
| 1 | 51 / 63 µs | 1,7 / 2,0 ms | 7.189 docs/s |
| 4 | 50 / 72 µs | 1,2 / 1,6 ms | 5.528 docs/s |
| 8 | 77 / 87 µs | 1,8 / 2,0 ms | 4.394 docs/s |

Nenhum stall. A janela de escrita ainda é curta (~1,5 s).

## 5. Relevância

Consultas com resposta conhecida
([plano-relevancia.md](plano-relevancia.md)). Entre parênteses, a linha de
base de antes do plano.

| Conjunto | top-1 | top-3 | top-10 | MRR |
|---|---|---|---|---|
| `rel-titulo` (título → artigo) | 99,3% (81,3%) | 100% (92,8%) | 100% (97,1%) | 0,996 (0,875) |
| `rel-digitando` (`coreia do nor*`) | 84,4% (64,6%) | 94,2% (81,2%) | 98,0% (90,1%) | 0,897 (0,741) |
| `rel-hub` (artigo principal em 1º) | 97,9% (38,6%) | 99,9% (68,2%) | 100% (89,0%) | 0,989 (0,560) |
| `rel-corpo` (busca pelo conteúdo) | 49,5% (49,8%) | 68,4% (68,7%) | 81,5% (81,4%) | 0,610 (0,612) |
| `rel-manual` (36 consultas à mão) | 66,7% (8,3%) | 94,4% (36,1%) | 100% (55,6%) | 0,811 (0,251) |

O peso do título melhorou tudo o que depende dele sem piorar a busca pelo
conteúdo.

### O que ainda erra

| Tipo | Exemplo | O que vem antes |
|---|---|---|
| Ordem das palavras no título | `Linha Nova` (2º) | "Nova linha" |
| Título repetido ou com parênteses | `Tora! Tora! Tora!` (2º), `Sé (Bragança)` (2º) | "Tora", "Bragança" |
| Página de desambiguação | `Getúlio Vargas` (2º) | "Getúlio Vargas (desambiguação)" |
| Prefixo curto | `astron*` (3º), `brasi*` (5º) | "Astrônomo", "Brasileiros" |
| Consulta ambígua | `lula` (6º) | "Lula (desambiguação)", "Lula-colossal" |

Os dois primeiros tipos se resolvem com um bônus para quando a consulta é
exatamente o título, na mesma ordem. É uma mudança pequena e não mexe no
formato.

---

## 6. Pontos de atenção

1. **A escala com threads piorou em termos relativos.** Como cada consulta
   agora leva ~30 µs, o que é compartilhado pesa mais. O principal suspeito são
   os caches compartilhados de nós e de tamanhos (`EpochCache`): toda leitura
   pega um `RwLock`, e o contador de leitores fica numa linha de cache
   disputada por todas as threads. Saídas possíveis: dividir o cache em partes
   ou dar uma cópia de leitura por thread, atualizada a cada commit.
2. **Os prefixos curtos são a consulta mais cara.** `p-1` e `p-2` levam
   1,9–2,5 ms, fazem 500 a 850 leituras na Trie e puxam o p99 do `misto`
   para 1 ms. O custo está nos níveis fundos da Trie, onde o cache não
   chega. Um índice ordenado de termos (`term:<palavra>`) resolveria o
   prefixo com uma varredura só.
3. **`rank-freq` custa 3,78× a busca simples** (194 µs contra 51 µs), e a meta
   é ≤ 2×. Em números absolutos está bom: a razão cresceu porque a busca
   simples ficou muito mais barata. Ou a meta passa a ser de latência
   absoluta, ou é preciso block-max no formato, para não decodificar a lista
   inteira.
4. **Compactar faz diferença:** sem compactação, as consultas de termos
   frequentes e de frases ficam 40–90% mais lentas. Em 100k há ~100 commits; na
   base inteira serão ~1.100, e a rodada completa vai mostrar se a
   compactação em segundo plano acompanha.
5. **Ranking com título custa mais que antes do BM25F.** Comparando bancos
   carregados do mesmo jeito, `rank-and`, `rank-titulos` e `misto` ficaram
   21–25% mais lentos que no v2, contra a meta de ≤ 20%. Com peso 50, o
   limite de um documento com o termo no título fica alto e o corte do top-k
   descarta menos. A busca sem ranking e o `rank-freq` não mudaram.

## 7. Cuidados ao ler o `RESULTADOS.md`

- **"Oráculo 10k-reaberto" é antigo** (`4fe734a`, top-1 de 80,3%). A rodada de
  100k reabre o banco de 100k, não o de 10k, e o relatório repete o último
  resultado com aquele rótulo. O "100k-reaberto", desta rodada, deu zero
  divergências e 99,3% de top-1.
- **"Frases e ranking (formato v2)"** tem esse nome por herança; os números
  são do v3.
- **O banco de 480 MB no lote de 10k** é um artefato da medição (WAL e
  compactação em andamento); os outros casos dão ~260 MB.
- **As cargas de 1k, 10k e 100k** incluem 2 rodadas extras do mesmo commit,
  de uma rodada da base completa interrompida logo no começo. Elas só entram
  na mediana.

## 8. Próximos passos sugeridos

1. Commitar esta rodada como linha de base do v3.
2. Concorrência: tirar o lock compartilhado dos caches de leitura.
3. Prefixos curtos: índice ordenado de termos.
4. Relevância: bônus para título idêntico à consulta.
5. Rodar a base completa (`perf.py tudo --escala full`) com lote de 10 mil
   documentos na carga.
