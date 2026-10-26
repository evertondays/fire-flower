# Análise: Wikipédia inteira vs. 100k

Duas rodadas completas de `python3 scripts/perf.py tudo`, no **mesmo commit**
(`d088f12`, formato v3 com título como campo e BM25F), na mesma máquina e na
mesma noite (26/10/2026, WSL2, 12 núcleos, 7,7 GB de RAM):

| Rodada | Escala | Duração | Log |
|---|---|---|---|
| 1 | 100k (100.000 artigos, 470 MB de texto) | ~12 min | `bench/results/*d088f12*` até 03:54 UTC |
| 2 | full (1.112.246 artigos, 2.694 MB de texto) | 31min29s | `~/bench-db/perf-v3-full.log` |

Os números vêm dos JSON em `bench/results/`. Os primeiros 100k artigos da carga
inteira dão, janela a janela, exatamente os mesmos números da carga de 100k
(MB/s, Gets/doc, fração no commit), então a comparação é direta.

## Resumo

- **A carga inteira não colapsa mais.** 1.112.246 artigos em **241–243 s**
  (três cargas iguais na mesma rodada), 4.600 artigos/s, sem nenhum stall do
  RocksDB e com no máximo 4 arquivos no nível 0. A rodada que colapsava
  (25/10) chegou a 530k em 116 s e depois caiu para ~200 artigos/s; foi
  interrompida em 820k depois de 32 minutos.
- **A velocidade cai até ~400k e depois fica plana.** De 15,6 MB/s (0–100k)
  para ~10 MB/s, estável até 1,1 M. O que cresce é o tempo do commit (74% → 80%)
  e as leituras do escritor (7 → ~15 Gets por artigo): a Trie tem 7,3 milhões
  de nós e o cache do escritor guarda 1 milhão.
- **O banco cresce proporcional ao texto, não aos artigos:** 5,7× mais texto,
  5,9× mais banco (237 MB → 1.411 MB compactado, ~0,52× o texto). As posições
  são 58% do banco.
- **Corretude:** zero divergências em todos os oráculos (10k completo, 100k
  amostra, 100k reaberto), idempotência, lotes vs. único e reabrir. Mas nenhum
  oráculo nem a relevância rodam na escala full (só até 100k, por projeto).
- **Busca:** consultas que não dependem do tamanho das listas (termo raro,
  termo médio, autocompletar, E de termos médios) ficam iguais. As que dependem
  crescem com a lista: termo frequente 6,4×, frases ~5×, ranking de termo
  frequente 5×. O banco (1,4 GB) já não cabe no block cache de 256 MB, e frases
  e prefixos curtos passam a ler blocos de fora dele.
- **Pontos a acertar:** o detector de colapso dá falso positivo com lotes muito
  grandes; a meta de `rank-freq` ≤ 2× `t-freq` continua de fora (3,0×); a
  concorrência escala pouco (4,3× com 12 threads); e a carga deixa espaço para
  um cache de nós e lotes maiores (há 6 GB de RAM livres).

---

## 1. Carga

### 100k vs. full

| | 100k | full | full ÷ 100k |
|---|---|---|---|
| Artigos | 100.000 | 1.112.246 | 11,1× |
| Texto | 470 MB (4,8 KB/artigo) | 2.694 MB (2,5 KB/artigo) | 5,7× |
| Tempo | 26–29 s | 241–243 s | 8,7× |
| artigos/s | 3.460–3.810 | 4.590–4.610 | 1,3× |
| MB/s | 16,3–17,9 | 11,1–11,2 | 0,65× |
| Pico de RSS | 470 MB | 836–838 MB | 1,8× |
| Banco logo após a carga | 261 MB | 1.468 MB | 5,6× |
| Banco compactado | 237 MB | 1.411 MB | 5,95× |
| Nós da Trie | 2,12 M | 7,30 M | 3,4× |
| Termos | 871 mil | 3,06 M | 3,5× |
| Write amplification | 1,38 | 3,06 | 2,2× |
| Bytes escritos (usuário / flush / compactação) | 403 / 278 / 279 MB | 2.727 / 1.845 / 6.510 MB | |
| Stall do RocksDB | 0 s | 0 s | |
| Aumento de swap | 0 | 0–3 MB | |

Os artigos por segundo **sobem** na carga inteira porque os artigos do fim do
corpus são mais curtos (2,5 KB contra 4,8 KB em média nos primeiros 100k).
A medida justa é MB/s, que cai para 65%.

As três cargas inteiras da rodada (escada, latência e `pr`) deram 241,3 s,
242,3 s e 242,5 s: variação abaixo de 1%.

### Velocidade ao longo do corpus

Mediana das janelas de 10k artigos, por faixa de 100k (duas cargas inteiras):

| Faixa | MB/s | artigos/s | Tempo no commit | Gets/artigo | Filtro relido | L0 máx. | Stall |
|---|---|---|---|---|---|---|---|
| 0–100k | 15,6 | 3.903 | 74% | 7,2 | 10 MB | 4 | 0 |
| 100k–200k | 12,7 | 4.116 | 78% | 15,0 | 8 MB | 4 | 0 |
| 200k–300k | 11,3 | 4.641 | 78% | 10,4 | 4 MB | 4 | 0 |
| 300k–400k | 10,8 | 4.442 | 78% | 11,9 | 10 MB | 4 | 0 |
| 400k–500k | 9,2 | 4.753 | 81% | 11,2 | 7 MB | 4 | 0 |
| 500k–600k | 10,3 | 6.270 | 80% | 8,6 | 12 MB | 4 | 0 |
| 600k–700k | 10,3 | 4.405 | 79% | 13,1 | 11 MB | 4 | 0 |
| 700k–800k | 9,9 | 5.550 | 79% | 11,2 | 8 MB | 4 | 0 |
| 800k–900k | 10,2 | 5.122 | 79% | 11,9 | 5 MB | 4 | 0 |
| 900k–1M | 9,6 | 3.703 | 80% | 18,8 | 19 MB | 4 | 0 |
| 1M–1,11M | 10,0–10,7 | 3.465–4.189 | 78–79% | 13,6–18,2 | 9 MB | 4 | 0 |

Na carga inteira foram 154 MB de filtro de Bloom relidos no total. Na rodada
de 25/10, antes dos filtros particionados, o RocksDB contou 22 TB.

**Por que cai até 400k e depois estabiliza:**

1. **O commit fica mais caro.** A fração do tempo no commit passa de 63% (10k)
   para ~80%. O commit percorre a Trie uma vez por termo distinto do lote, e
   cada lote traz termos novos e mais fundos à medida que o vocabulário cresce.
2. **O cache de nós do escritor deixa de caber.** Ele guarda 1 milhão de nós
   (`Config::writer_cache_nodes`); a Trie passa disso entre 20k e 30k artigos
   (as leituras do escritor vão de 0 para 22 por artigo nessa janela) e
   termina com 7,3 milhões. Os nós que saem do cache são relidos do banco: as
   leituras do escritor vão de ~7 para ~11–18 por artigo. O experimento
   `sem-cache-de-nos` mostra o peso disso: sem o cache, a carga de 100k leva
   51 s em vez de 27 s.
3. **O resto do sistema está folgado.** Zero stall, L0 ≤ 4, compactação
   pendente de no máximo ~600 MB, 6,2 GB de RAM livre e RSS estável (284 MB →
   805 MB, crescendo com o block cache e o cache de nós até os limites).

A estabilização tem uma explicação provável, ainda não medida à parte: o
cache de nós está cheio desde cedo, e daí em diante o custo por termo para de
crescer, porque os termos novos passam a ser principalmente palavras raras na
cauda do vocabulário.

### Comparação com a rodada que colapsava (25/10)

| | 25/10 (filtro inteiro) | 26/10 (`d088f12`) |
|---|---|---|
| Até 530k | 116 s | 111 s |
| Depois de 540k | 76–374 artigos/s | 3.500–9.000 artigos/s |
| Até 820k | 1.910 s (interrompida aí) | 168 s |
| Carga inteira | não terminou | 242 s |

### Experimentos de carga (100k)

| Experimento | Tempo | artigos/s | vs. padrão | Pico de RSS | Banco |
|---|---|---|---|---|---|
| padrão (lote 1.000) | 27,3 s | 3.660 | — | 472 MB | 261 MB |
| lote 100 | 48,5 s | 2.061 | −44% | 476 MB | 257 MB |
| lote 5.000 | 18,7 s | 5.362 | +47% | 525 MB | 281 MB |
| lote 10.000 | 16,7 s | 5.980 | +63% | 696 MB | 265 MB |
| lote 50.000 | 13,6 s | 7.339 | +100% | 1.332 MB | 274 MB |
| IDs remapeados (1..N) | 26,6 s | 3.755 | +3% | 471 MB | 252 MB |
| block cache 64 MB | 28,1 s | 3.562 | −3% | 460 MB | 261 MB |
| block cache 1 GB | 27,9 s | 3.590 | −2% | 471 MB | 261 MB |
| sem cache de nós | 51,3 s | 1.949 | −47% | 305 MB | 262 MB |
| sem posições | 20,7 s | 4.835 | +32% | 429 MB | 112 MB (91 MB compactado) |
| streaming (1 commit por artigo, 10k) | 19,3 s | 519 | — | 248 MB | 65 MB |

O tamanho do lote é o que mais pesa: o commit percorre a Trie uma vez por
termo distinto do lote, então lotes maiores dividem esse custo por mais
artigos. O `carregar_wikipedia.py` usa 2.000 e o `perf.py`, 1.000.

---

## 2. Espaço

Valores lógicos por tipo de chave (iguais antes e depois de compactar):

| Espaço | 100k | full | full ÷ 100k | % do banco (full) |
|---|---|---|---|---|
| `pos:` | 146,7 MB | 878,1 MB | 6,0× | 58,5% |
| `post:` | 70,8 MB | 456,5 MB | 6,4× | 30,4% |
| `node:` | 35,4 MB | 129,5 MB | 3,7× | 8,6% |
| `doc:` | 2,3 MB | 31,9 MB | 13,9× | 2,1% |
| `norm:` | 0,4 MB | 4,7 MB | 11,8× | 0,3% |
| **Total** | **255,6 MB** | **1.500,7 MB** | **5,9×** | |

- Listas (`post:` e `pos:`) crescem com o texto (~6×). A Trie cresce menos
  (3,7×), porque os termos novos compartilham prefixos; `doc:` e `norm:`
  crescem com o número de artigos (~12–14×).
- Compactado, o banco inteiro tem 1.411 MB (0,52× o texto). A compactação
  levou 8,7 s e só tirou 23 MB, porque a carga já termina quase compactada.
- Os maiores valores são as posições dos termos frequentes: `sao` 2,0 MB,
  `ligacoes` 1,7 MB, `externas` 1,6 MB, `ser` 1,6 MB. A raiz da Trie tem
  37 KB (13 KB no 100k).
- Abrir o banco e fazer uma busca: 38 ms antes de compactar, 29 ms depois
  (32 → 18 ms no 100k).

---

## 3. Corretude

| Checagem | 100k | full |
|---|---|---|
| Oráculo 10k, todos os termos (tf, posições, tamanhos, AND, prefixo, autocompletar, frases, BM25) | ✅ 0 | ✅ 0 |
| Oráculo 100k, amostra de 31 mil termos | ✅ 0 | ✅ 0 |
| Reabrir (100k + 1.000) | ✅ 0 | ✅ 0 |
| Idempotência (10k duas vezes) | ✅ | ✅ |
| Lotes vs. único (1k) | ✅ | ✅ |

Na escala full, a corretude usa as mesmas fatias de 10k e 100k (o oráculo em
memória da Wikipédia inteira não cabe na RAM). **Nada confere o banco de 1,1 M
artigos diretamente**: o `pr` roda o oráculo só em 10k.

---

## 4. Latência de busca (cache quente, 1 thread, depois de compactar)

**Atenção:** os conjuntos de consulta são gerados por escala (`bench/queries/100k`
e `bench/queries/full`). `t-freq` é "termo frequente na escala": casa em média
25 mil documentos no 100k e 145 mil no full. A comparação mostra quanto cada
tipo de consulta custa em cada escala, e não as mesmas consultas.

| Conjunto | p50 100k | p50 full | × | p99 100k | p99 full | Resultados 100k → full |
|---|---|---|---|---|---|---|
| `t-raro` | 19 µs | 18 µs | 1,0 | 46 µs | 45 µs | 2 → 2 |
| `t-medio` | 21 µs | 23 µs | 1,1 | 59 µs | 55 µs | 683 → 881 |
| `t-miss` | 8 µs | 7 µs | 0,9 | 32 µs | 29 µs | 0 |
| `t-len-3` / `-8` / `-15` | 9 / 21 / 30 µs | 11 / 23 / 34 µs | 1,1–1,2 | | | |
| `t-freq` | 51 µs | 329 µs | 6,4 | 98 µs | 722 µs | 25 mil → 145 mil |
| `titulos` | 35 µs | 93 µs | 2,6 | 214 µs | 1,8 ms | 154 → 374 |
| `and-2` / `and-3` / `and-5` | 39 / 60 / 114 µs | 44 / 65 / 110 µs | 1,0–1,1 | | | |
| `and-raro-freq` | 84 µs | 410 µs | 4,9 | 193 µs | 995 µs | |
| `and-freq` | 284 µs | 1,9 ms | 6,7 | 534 µs | 3,9 ms | 8 mil → 28 mil |
| `p-raro` | 27 µs | 29 µs | 1,1 | | | |
| `p-4` | 381 µs | 1,0 ms | 2,6 | 3,4 ms | 8,5 ms | |
| `p-2` | 2,5 ms | 4,4 ms | 1,8 | 4,9 ms | 10,3 ms | |
| `p-1` | 1,9 ms | 5,1 ms | 2,7 | 4,6 ms | 30,5 ms | 25 mil → 160 mil |
| `s-1` / `s-2` / `s-4` / `s-raro` | 10 / 44 / 61 / 21 µs | 29 / 52 / 71 / 22 µs | 1,0–3,0 | | | 10 |
| `frase-2` | 181 µs | 928 µs | 5,1 | 1,4 ms | 10,7 ms | 852 → 6.421 |
| `frase-3` | 254 µs | 1,1 ms | 4,4 | 1,4 ms | 10,1 ms | |
| `frase-stop` | 204 µs | 1,3 ms | 6,2 | 1,4 ms | 6,9 ms | |
| `frase-freq` | 719 µs | 4,1 ms | 5,7 | 1,6 ms | 17,4 ms | 5 mil → 46 mil |
| `frase-miss` | 185 µs | 947 µs | 5,1 | 1,5 ms | 9,1 ms | 8 → 38 |
| `rank-freq` | 194 µs | 976 µs | 5,0 | 432 µs | 3,6 ms | |
| `rank-and` | 117 µs | 542 µs | 4,6 | 517 µs | 3,9 ms | |
| `rank-titulos` | 41 µs | 107 µs | 2,6 | 235 µs | 1,9 ms | |
| `misto` | 31 µs | 55 µs | 1,8 | 1,0 ms | 4,2 ms | |

**O que isso mostra:**

- **Termos pontuais não sentem a escala.** Termo raro, médio, inexistente,
  autocompletar e E de termos médios custam o mesmo com 11× mais artigos. O
  custo é a descida na Trie (1 `Get` por caractere), e o cache de nós rasos
  das buscas absorve o crescimento da raiz (37 KB).
- **O que lê listas inteiras cresce linearmente com a lista.** `t-freq` custa
  ~2 ns por documento nas duas escalas (51 µs / 25 mil e 329 µs / 145 mil).
  Frases, `and-freq` e `rank-freq` seguem a mesma regra. Para ir além disso as
  listas precisam de índice de saltos ou block-max no disco (Fase E do plano
  de frases e ranking).
- **Ranking:** `rank-freq ÷ t-freq` é 3,0× no full e 3,8× no 100k; a meta do
  plano (≤ 2×) continua de fora. Títulos ranqueados custam 107 µs no full.
- **O block cache deixou de bastar para frases e prefixos curtos.** Com o banco
  de 1,4 GB, as frases leem 3,7–6,5 blocos de fora do cache por consulta e o
  `p-2` lê 112. O sistema operacional ainda segura esses blocos na page cache
  (6 GB livres), mas o p99 das frases foi para ~10 ms.

### Tamanho do block cache (p50 / blocos lidos fora do cache por consulta)

| Conjunto | 100k: 64 MB | 256 MB | 1 GB | full: 64 MB | 256 MB | 1 GB |
|---|---|---|---|---|---|---|
| `t-medio` | 19 µs / 0 | 21 µs / 0 | 19 µs / 0 | 20 µs / 0 | 23 µs / 0 | 20 µs / 0 |
| `titulos` | 36 µs / 0 | 35 µs / 0 | 32 µs / 0 | 105 µs / 4,9 | 93 µs / 0 | 83 µs / 0 |
| `misto` | 37 µs / 0 | 31 µs / 0 | 32 µs / 0 | 72 µs / 26 | 55 µs / 0,3 | 48 µs / 0 |
| `p-2` | 2,9 ms / 55 | 2,5 ms / 0 | 2,5 ms / 0 | 5,0 ms / 419 | 4,4 ms / 112 | 3,7 ms / 0 |

No 100k, 256 MB basta. No full, 1 GB zera as leituras fora do cache e tira
10–15% do p50 de títulos, `misto` e `p-2`.

### Merge pendente (logo após a carga vs. depois de compactar, p50)

| Conjunto | 100k antes → depois | full antes → depois | Merges por consulta (antes, full) |
|---|---|---|---|
| `t-freq` | 96 → 51 µs | 677 → 329 µs | 5,0 |
| `titulos` | 48 → 35 µs | 168 → 93 µs | 10,0 |
| `and-freq` | 393 → 284 µs | 2,3 → 1,9 ms | 1,3 |
| `rank-freq` | 259 → 194 µs | 1,1 ms → 976 µs | 0,5 |
| `frase-2` | 311 → 181 µs | 1,5 ms → 928 µs | 2,8 |
| `frase-freq` | 1,1 ms → 719 µs | 6,2 → 4,1 ms | 3,1 |

Compactar depois da carga deixa termos frequentes, títulos e frases 30–50%
mais rápidos, e custa só 8,7 s no banco inteiro. Vale fazer sempre
(`carregar_wikipedia.py --compactar`).

---

## 5. Concorrência

### Só leitura (`misto`)

| Threads | 100k qps | ganho | full qps | ganho | p99 full |
|---|---|---|---|---|---|
| 1 | 11.493 | 1,0× | 2.731 | 1,0× | 3,9 ms |
| 2 | 14.199 | 1,2× | 3.283 | 1,2× | 5,0 ms |
| 4 | 24.298 | 2,1× | 6.209 | 2,3× | 4,3 ms |
| 8 | 34.333 | 3,0× | 10.377 | 3,8× | 4,8 ms |
| 12 | 46.111 | 4,0× | 11.873 | 4,3× | 6,7 ms |

O ganho com 12 threads é de 4,0–4,3× (era 6,3× no 10k do formato antigo). O
qps de 1 thread no full é ¼ do 100k porque o `misto` tem prefixos e títulos,
que crescem com a escala.

### Leitura durante a escrita (carga dos últimos 10% em paralelo)

| Leitores | Escala | p50 sem → com escrita | p99 sem → com | Indexação |
|---|---|---|---|---|
| 1 | 100k | 51 → 63 µs | 1,7 → 2,0 ms | 7.189 artigos/s (10 mil) |
| 4 | 100k | 50 → 72 µs | 1,2 → 1,6 ms | 5.528 artigos/s |
| 8 | 100k | 77 → 87 µs | 1,8 → 2,0 ms | 4.394 artigos/s |
| 1 | full | 79 → 139 µs | 5,9 → 8,6 ms | 3.767 artigos/s (111 mil) |
| 4 | full | 81 → 132 µs | 5,4 → 7,6 ms | 3.203 artigos/s |
| 8 | full | 105 → 168 µs | 7,2 → 10,0 ms | 2.427 artigos/s |

No full, indexar em paralelo sobe o p50 das buscas em 60–75% e o p99 em ~40%.
A indexação perde 18–47% de velocidade (de ~4.600 artigos/s sozinha). Não houve
stall em nenhum caso.

---

## 6. Relevância

Só existe na fatia de 100k, nas duas rodadas (`perf.py` usa no máximo 100k
para a relevância). A rodada full repetiu os mesmos números da rodada 100k.

| Conjunto | Linha de base (`00552d4`) | BM25F (`d088f12`) |
|---|---|---|
| `rel-titulo` (top-1 / MRR) | 81,3% / 0,875 | 99,3% / 0,996 |
| `rel-digitando` | 64,6% / 0,741 | 84,4% / 0,897 |
| `rel-hub` | 38,6% / 0,560 | 97,9% / 0,989 |
| `rel-corpo` (acerto até o 10º) | 81,4% / 0,612 | 81,5% / 0,610 |
| `rel-manual` (36 consultas) | 8,3% / 0,251 | 66,7% / 0,811 |

Ainda erram no conjunto manual, por exemplo, `brasi*` (o artigo "Brasil" vem
em 5º, atrás de "Brasileiros", "Brasiguaios" e "Brasília") e `lula` (o
presidente vem em 6º, atrás de "Lula (desambiguação)" e dos moluscos). **Falta
medir a relevância na Wikipédia inteira**, onde o idf e o tamanho médio mudam.

---

## 7. Linha de base do `pr`

O `pr` mede logo após a carga, sem compactar, então os números são mais altos
que os da latência acima (p50):

| Conjunto | 100k `4fe734a` (v2) | 100k `d088f12` (v3) | full `d088f12` |
|---|---|---|---|
| `t-medio` | 132 µs | 31 µs | 46 µs |
| `t-freq` | 190 µs | 98 µs | 655 µs |
| `p-2` | 2,9 ms | 3,7 ms | 4,9 ms |
| `rank-freq` | 416 µs | 254 µs | 1,1 ms |
| `frase-2` | 518 µs | 309 µs | 1,5 ms |
| Carga (artigos/s) | 3.737 | 3.596 | 4.587 |

Entre `4fe734a` e `d088f12` quase tudo melhorou (cache de nós rasos, ranking),
mas o `p-2` piorou 30%. O `p-2` varia bastante entre rodadas, então vale
confirmar com A/B antes de tratar como regressão; uma suspeita é o campo de
título do formato v3 aumentando o trabalho da expansão de prefixo.

`bench/baseline-pr-full.json` foi salvo: os próximos `perf.py pr --escala full`
comparam contra ele.

---

## 8. Problemas encontrados e próximos passos

| # | O quê | Evidência | Sugestão |
|---|---|---|---|
| 1 | **Falso positivo do detector de colapso** com lotes grandes | `exp-100k-lote-50000` marcado como "colapso em 50k" com 0 MB de filtro, 0 Gets e 0 stall: a janela de 10k que contém o commit de 50 mil artigos parece lenta | comparar janelas só quando o lote ≤ janela, ou usar janelas alinhadas aos commits |
| 2 | **Cache de nós do escritor pequeno para o full** | 7,3 M nós contra 1 M no cache; Gets por artigo 7 → 15–18; sem cache, o 100k fica 2× mais lento | medir `--cache-nos 4000000` (~800 MB) na carga inteira; há 6 GB livres |
| 3 | **Lote padrão pequeno** | lote 10.000 dá +63% no 100k (RSS 696 MB) | medir lotes de 5.000 e 10.000 na carga inteira e mudar o padrão do `carregar_wikipedia.py` |
| 4 | **`rank-freq` ≤ 2× `t-freq` não atingido** | 3,0× no full | índice de saltos e block-max nas listas (Fase E) |
| 5 | **Frases e prefixos curtos com p99 de ~10–30 ms** no full | blocos fora do cache; `pos:` de termos frequentes com até 2 MB | block cache de 1 GB para o full; listas em blocos |
| 6 | **Concorrência escala 4,3× com 12 threads** | `misto`, 100k e full | investigar contenção (trava do cache de nós rasos, `NormCache`, alocação) |
| 7 | **Nada confere o banco de 1,1 M diretamente** | oráculo e relevância só até 100k | oráculo por amostra lendo o corpus em duas passadas; relevância no full |
| 8 | **Cache frio não medido** | "pulando cache frio: precisa de `sudo -n`" | rodar `sudo -v` antes do `perf.py` |
| 9 | **Possível regressão no `p-2`** | 2,9 → 3,7 ms entre `4fe734a` e `d088f12` (100k, `pr`) | A/B dos dois commits |
