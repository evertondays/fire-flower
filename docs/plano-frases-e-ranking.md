# Plano: busca por frase e ranking BM25 — fire-flower

Duas funcionalidades que hoje faltam e que mexem no mesmo lugar, as listas de
postagem:

- **Busca por frase:** `"revolução francesa"` acha só os documentos com as
  palavras em sequência. Precisa das **posições** de cada termo em cada
  documento.
- **Ranking BM25:** os resultados saem ordenados por relevância, não por ID.
  Precisa da **frequência do termo no documento** (tf), do **tamanho de cada
  documento** e de estatísticas do índice (total de documentos, tamanho
  médio).

As duas mudam o formato do banco. Por isso este plano faz **uma mudança de
formato só** (Fase A), que grava tudo o que as duas precisam, e depois cada
funcionalidade é só leitura e consulta (Fases B a D).

---

## 1. Onde estamos

| Hoje | Problema |
|---|---|
| `post:<nó>` = lista de IDs de documento (deltas em varint) | sem tf, sem posições |
| nenhum dado por documento além do rótulo (`doc:<id>`) | sem tamanho do documento |
| `meta:` guarda só contadores de ID e idiomas | sem total de documentos nem tamanho médio |
| `search` devolve IDs em ordem crescente | sem relevância |
| consulta = termos com E, `termo*` por prefixo | sem frase |
| título e texto entram juntos como um texto só | o título não pesa mais que o resto |

## 2. Decisões (com a recomendação)

| # | Decisão | Recomendação | Por quê |
|---|---|---|---|
| D1 | Posições obrigatórias ou opcionais | **Opcionais por banco, ligadas por padrão** (`Config::positions`, `--sem-posicoes`), gravadas em `meta:positions` | quem não precisa de frase não paga o espaço; tf e tamanhos são baratos e ficam sempre |
| D2 | Onde ficam as posições | **Chave separada `pos:<nó>`**, `post:<nó>` continua só com doc + tf | buscas comuns não leem posições (é o que o Lucene faz com `.doc` e `.pos`) |
| D3 | Tamanho do documento | **Exato (u32), em blocos** `norm:<id ÷ 1024>`, com cache em memória | 1,1 M docs × 4 bytes ≈ 4 MB; exato deixa o oráculo trivial. Quantizar em 1 byte (como o Lucene) fica para se o espaço pesar |
| D4 | O que conta no tamanho | **Tokens indexados** (sem stopwords) | o BM25 compara documentos pelo que foi indexado |
| D5 | Como as posições contam | **Toda palavra do `tokenize` conta, inclusive stopwords**; tokens descartados por tamanho (> 64) não | preserva os buracos: `"casa de papel"` vira `casa@0 papel@2` |
| D6 | Reindexar o mesmo ID | **A última gravação vence** (tf, posições e tamanho), e o total de documentos não conta duas vezes | mantém a idempotência que o `perf.py` já confere |
| D7 | API | **`search` continua igual** (IDs em ordem, usado pelo oráculo); **novo `search_top(query, k) -> Vec<Hit>`** com `Hit { doc, score }` | não quebra quem usa; CLI e TUI passam a usar o ranqueado |
| D8 | Prefixo no BM25 | ~~**Nota do melhor termo expandido** que o documento contém (máximo, não soma)~~ **Substituída pela R1 de [plano-relevancia.md](plano-relevancia.md):** o prefixo é um termo só (idf do conjunto, tf somado) | com o máximo, cada completação usava o próprio idf e uma completação rara ("nortel") passava na frente da palavra digitada ("norte") |
| D9 | Parâmetros | `k1 = 1.2`, `b = 0.75` em `Config` | valores padrão do Lucene e do Elasticsearch |
| D10 | Desempate | **Nota decrescente, depois ID crescente** | resultado determinístico, comparável pelo oráculo |
| D11 | Semântica da consulta | **Continua E** (todos os termos obrigatórios); OR fica fora | o BM25 ordena o que o E já filtrou |
| D12 | Título com peso maior (campos) | **Fora deste plano** (seção 10) | precisa de campos separados no índice; é a próxima melhoria de relevância para a Wikipédia |

## 3. Formato v2 do banco

| Chave | Valor | Novo? |
|---|---|---|
| `node:<id>` | nó da Trie | igual |
| `post:<nó>` | `n`, depois `n × (Δdoc, tf)` em varint | **muda**: ganha tf |
| `pos:<nó>` | `n`, depois `n × (Δdoc, k, k × Δposição)` em varint | **novo** (só com posições ligadas) |
| `norm:<bloco>` | pares `(Δid, tamanho)` dos documentos com `id ÷ 1024 = bloco` | **novo** |
| `doc:<id>` | rótulo | igual |
| `meta:format` | `2` | **novo** |
| `meta:positions` | `0` ou `1` | **novo** |
| `meta:doc_count`, `meta:total_len` | total de documentos e soma dos tamanhos (para o tamanho médio) | **novo** |
| `meta:languages`, `meta:next_*` | | igual |

- **Merge operator:** `post:` e `pos:` passam a unir as listas **por
  documento**, com a última gravação vencendo (D6). Isso continua associativo,
  então o RocksDB pode combinar os pedaços em qualquer ordem.
- **Blocos de `norm:`:** o escritor mantém os blocos tocados em memória (como
  o cache de nós) e grava o bloco inteiro no commit. Na leitura, um cache de
  blocos evita um `Get` por documento ao ranquear listas grandes.
- **Banco v1:** abrir um banco sem `meta:format` dá `Error::Config("formato
  antigo; recarregue o banco")`. Os bancos que temos são de benchmark e
  recarregam em minutos; não vale escrever migração.

**Custo estimado (a medir na Fase E):** cada termo aparece em média ~2 vezes
por artigo, então `pos:` deve ficar 2 a 3 vezes maior que `post:` (hoje ~30%
do banco). O tf custa ~1 byte por entrada em `post:`. No banco de 10k, algo
como 22 MB → 35–45 MB com posições, ~25 MB sem.

## 4. Fase A — Formato v2 (tudo o que vai para o disco)

O objetivo é gravar tf, posições, tamanhos e estatísticas, mantendo a busca
exatamente como está. Ao fim desta fase nada muda para quem busca, mas o banco
já tem tudo o que as Fases B a D precisam.

1. **Tokenização com posição:** `tokenize` ganha uma variante que devolve
   `(termo, posição)` (D5). `index_terms` passa a devolver, por termo, a lista
   de posições e o tamanho do documento (D4).
2. **Lote:** `pending` passa de `termo → [doc]` para
   `termo → [(doc, posições)]`. O tf é o número de posições.
3. **Codec:** `encode/decode` de `post:` v2 e de `pos:`, com testes de ida e
   volta e de dados corrompidos (como os atuais).
4. **Merge operator** por documento (D6), com testes de associatividade:
   unir em qualquer ordem dá o mesmo resultado.
5. **Normas e estatísticas:** blocos `norm:`, `meta:doc_count` e
   `meta:total_len`, atualizados no mesmo `WriteBatch` atômico do lote.
6. **Versão:** `meta:format = 2`, `meta:positions`, erro claro em banco v1.
7. **Harness:** `digest` inclui tf e posições (a checagem `lotes_vs_unico`
   continua valendo); `sizes` mostra o espaço de `pos:` e `norm:`;
   `wiki load --sem-posicoes`.
8. **Oráculo:** o índice em memória guarda tf e posições e confere, para
   todos os termos, a lista completa de (doc, tf, posições), mais os tamanhos
   e as estatísticas.

**Pronto quando:** oráculo de 10k com zero divergências (inclusive tf,
posições e tamanhos); idempotência e `lotes_vs_unico` ok; o `perf.py tudo`
mede o custo novo de carga e de espaço.

## 5. Fase B — Ranking BM25

Para cada termo *t* da consulta e documento *d*:

```text
idf(t)     = ln(1 + (N − df + 0,5) / (df + 0,5))
nota(t, d) = idf(t) · tf · (k1 + 1) / (tf + k1 · (1 − b + b · |d| / média))
nota(d)    = soma das notas dos termos (prefixo: um termo só, R1 de plano-relevancia.md)
```

*N* = `meta:doc_count`; df = tamanho da lista do termo; média =
`total_len / N`.

1. `search_top(query, k)`: faz o E pelas listas como hoje, calcula a nota de
   cada documento que sobrou e mantém os *k* melhores num heap (D10).
2. `Config { bm25_k1, bm25_b }` (D9).
3. **CLI:** `search` ordena por nota e mostra a nota; `--por-id` mantém o
   comportamento antigo.
4. **TUI:** resultados por nota; a nota aparece ao lado do título.
5. **Oráculo:** calcula o BM25 em memória para as mesmas consultas e compara
   os 10 primeiros (mesma ordem, notas com diferença relativa < 1e-4).
6. **Métrica de relevância** (acompanhada, não é aprovação): consultar o
   título de 1.000 artigos e ver em que posição o próprio artigo aparece:
   acerto no top-1, no top-10 e MRR. É o número que mostra se uma mudança
   melhorou ou piorou a relevância.

**Pronto quando:** oráculo de ranking com zero divergências; p50 de
`t-freq` ranqueado no máximo 2× o não ranqueado (a medir); métrica de títulos
registrada como linha de base.

## 6. Fase C — Busca por frase

1. **Sintaxe:** aspas no `parse_query`. Uma frase vira uma lista de termos com
   a distância ao primeiro, sem as stopwords, que ficam como buraco:

   ```text
   "casa de papel"            →  casa@0, papel@2
   brasil "copa do mundo" 2014 →  brasil E (copa@0, mundo@2) E 2014
   "papel"                    →  termo comum
   ```

   Aspas sem fechamento vão até o fim da consulta. Prefixo dentro de frase
   (`"revolução franc*"`) fica fora desta versão.
2. **Execução:** E dos termos da frase pelas listas `post:` (rápido), depois,
   para cada candidato, conferir nas listas `pos:` se existe *p* com cada
   termo na posição *p + distância*.
3. **Banco sem posições:** frase dá `Error::Config("banco sem posições")`.
4. **TUI:** a frase aparece como uma unidade na linha "consulta:" e o
   destaque marca as palavras dela.
5. **Oráculo:** índice com posições em memória; frases tiradas de trechos
   reais dos artigos (2 a 4 palavras, com e sem stopword no meio), mais frases
   que não existem (palavras reais em ordem trocada).

**Pronto quando:** oráculo de frases com zero divergências; conjuntos
`frase-*` medidos.

**Limitações conhecidas, que ficam documentadas:**

- Stopword dentro da frase só conta a distância: `"casa de papel"` também
  casa "casa da papel". O Lucene com filtro de stopwords faz o mesmo.
- Frase só de stopwords (`"to be or not to be"`) continua vazia.

## 7. Fase D — Frase no ranking

- O tf da frase é o número de ocorrências da frase no documento; o idf é a
  soma dos idfs dos termos (como o Lucene). A frase entra na soma da nota como
  se fosse um termo.
- O oráculo de ranking passa a incluir consultas com frase.

## 8. Fase E — Medição e ajustes

Novos conjuntos em `bench/queries/` (gerados pelo `wiki queries`):

| Conjunto | O que mede |
|---|---|
| `frase-2`, `frase-3` | frases reais de 2 e 3 palavras |
| `frase-stop` | frases com stopword no meio (`"copa do mundo"`) |
| `frase-freq` | frases de termos frequentes (muitos candidatos para conferir) |
| `frase-miss` | frases que não existem |
| `rank-freq` | top-10 de termo frequente (nota em centenas de milhares de docs na carga total) |
| `rank-and` | top-10 de E de 2 a 3 termos |

| Métrica | Linha de base | Meta inicial |
|---|---|---|
| Carga 10k com posições | 1.586 docs/s | queda ≤ 30% |
| Carga 10k sem posições | 1.586 docs/s | queda ≤ 10% |
| Banco ÷ texto, com posições | 0,23 | registrar |
| p50 de `rank-freq` vs. `t-freq` | — | ≤ 2× |
| p50 de `frase-2` | — | registrar |
| Título → artigo (MRR) | — | registrar e acompanhar |

Otimizações que só entram se a medição pedir:

- **Blocos com índice de saltos** nas listas grandes, para conferir posições
  e notas sem decodificar a lista inteira.
- **Block-max WAND** para o top-k de termos frequentes: pula blocos que não
  podem entrar no top-k.
- **Normas em 1 byte** (D3), se `norm:` pesar.

## 9. Ordem e tamanho

| Fase | Depende de | Tamanho | Banco precisa ser recarregado? |
|---|---|---|---|
| A — formato v2 | — | grande (codec, lote, merge, oráculo) | **sim, uma vez** |
| B — BM25 | A | médio | não |
| C — frase | A | médio | não |
| D — frase no ranking | B e C | pequeno | não |
| E — medição | todas | pequeno | não |

B e C são independentes depois da A. Começar pela B entrega valor em toda
busca; começar pela C entrega a funcionalidade nova. Cada fase é um PR, com o
oráculo e o `perf.py pr` passando.

## 10. Fora deste plano (próximos passos)

- **Campos, com título de peso maior (BM25F).** Na Wikipédia é o que mais
  deve melhorar a relevância: buscar "Brasil" deveria trazer o artigo
  "Brasil" primeiro, e o BM25 só no texto corrido não garante isso. Precisa
  separar título e texto no índice. A métrica de títulos da Fase B mostra
  quanto falta.
- **Prévia e trechos com destaque na Wikipédia:** ponteiro do artigo no
  `.jsonl` + `text::match_ranges` na biblioteca (discutido à parte).
- **OR**, frase com prefixo, proximidade (`NEAR`), stemming.
- **Remoção e atualização de documentos** (o tf e as estatísticas tornam isso
  um pouco mais urgente: reindexar com outro texto deixa termos antigos).

## 11. Checklist

- [x] Decisões D1–D12: implementadas como recomendado (notas na seção 12)
- [x] Fase A: tokenização com posição, `post:` v2, `pos:`, `norm:`, estatísticas, versão
- [x] Fase A: oráculo com tf, posições e tamanhos; `digest` e `sizes` atualizados
- [x] Fase B: `search_top`, BM25 com prefixo, CLI e TUI ranqueados
- [x] Fase B: oráculo de ranking; métrica título → artigo
- [x] Fase C: aspas no `parse_query`, conferência de posições, erro em banco sem posições
- [x] Fase C: oráculo de frases
- [x] Fase D: frase na nota
- [x] Fase E: conjuntos `frase-*` e `rank-*`; relatório com custo de espaço, carga e latência
- [ ] Fase E: rodada completa do `perf.py` (10k) com os números no `RESULTADOS.md` (fire-flower-bench)
- [x] README e `como-funciona.md` atualizados

## 12. Como ficou (25/10/2026)

**Detalhes de implementação que o plano não fixava:**

- `Config::positions` é `Option<bool>`, como `Config::languages`: `None` usa o
  que está no banco (ou liga, num banco novo), e `Some` diferente do gravado
  faz o `open` falhar. Assim um banco sem posições abre com a config padrão.
- Além de `search_top(query, k) -> Vec<Hit>`, existe
  `search_ranked(query, k) -> TopHits { hits, total }`: a CLI e o TUI mostram
  o total sem buscar duas vezes.
- `Hit::score` é `f64`; o oráculo compara notas com diferença relativa < 1e-4
  e só aceita documentos trocados num empate de verdade (< 1e-9).
- Banco v1 com merges pendentes no WAL falha já na recuperação do RocksDB (o
  merge operator novo não entende o formato antigo); o `open` reconhece o
  caso e devolve o mesmo erro "formato antigo".
- O merge operator tem um caminho rápido: com IDs crescentes (a carga da
  Wikipédia), os pedaços só são concatenados, sem decodificar.
- A tokenização foi reescrita para não alocar por token e pular as tabelas
  Unicode em ASCII (resultado idêntico, conferido por teste para todos os
  caracteres). Isso pagou o custo das posições na carga.
- **Top-k:** limite superior exato por tf (nota de um documento de tamanho 0,
  tabelada), e no termo único os documentos são visitados por nível de tf,
  parando quando o nível não alcança o pior guardado. Sem isso, `rank-freq`
  custava 2,7× o `t-freq`.
- Os blocos de `norm:` ficam num cache compartilhado que respeita snapshots
  (número de commit por bloco; ver `NormCache` em `engine.rs`).

**Primeiros números (10k, medições avulsas fora do `perf.py`; a rodada
oficial vai para o `RESULTADOS.md` de fire-flower-bench):**

| Métrica | v1 | v2 | Meta |
|---|---|---|---|
| Carga 10k com posições | ~1.600–1.700 docs/s | ~2.000–2.450 docs/s | queda ≤ 30% |
| Carga 10k sem posições | | ~3.000–3.300 docs/s | queda ≤ 10% |
| Banco compactado | 17 MB | 51 MB com posições, 21 MB sem | registrar |
| p50 `t-freq` (todos os IDs) | 56 µs | 53–57 µs | — |
| p50 `rank-freq` ÷ `t-freq` | — | ~1,8× | ≤ 2× |
| p50 `frase-2` | — | ~150 µs | registrar |
| Título → artigo | — | top-1 80,5%, top-10 96,2%, MRR 0,872 | linha de base |
| Oráculo 10k | 0 divergências | 0 divergências (tf, posições, tamanhos, frases, ranking) | 0 |

O espaço ficou acima da estimativa da seção 3 (35–45 MB): `pos:` tem 26 MB de
valores no 10k, porque o Δdoc e o k se repetem em cada entrada (o merge por
documento precisa deles). Se pesar, a opção é guardar as posições junto de
`post:`, em blocos (um Δdoc e um tf por entrada, em vez de dois). Normas em 1
byte não ajudam aqui: `norm:` tem só ~20 KB.
