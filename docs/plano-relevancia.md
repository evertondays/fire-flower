# Plano: ordenação dos resultados — prefixo e título — fire-flower

A ordenação dos resultados está ruim em dois pontos, os dois vistos no TUI
com `coreia do norte` na Wikipédia de 100k:

| # | O que acontece | Por quê |
|---|---|---|
| 1 | **"LG Corporation" em 1º lugar** (nota 21,0; os artigos da Coreia do Norte ficam em ~12,5) | o TUI busca `coreia norte*`; o prefixo vale a **maior nota entre as completações** (decisão D8), e cada completação usa o próprio idf. "nortel" aparece em 3 documentos, idf alto, e a LG cita a Nortel duas vezes: a nota é exatamente a de `coreia nortel` |
| 2 | **"Coreia do Norte" só em 7º** mesmo sem prefixo, empatado com "Bandeira da…", "Demografia da…", "Política da…" (12,49 a 12,38) | o título entra concatenado ao texto, como mais umas palavras do corpo. Um artigo cujo **título é a consulta** não ganha nada por isso |

Este plano corrige os dois e cria o que faltava para julgar ordenação:
**conjuntos de avaliação com resposta esperada**. Hoje só existe "título →
artigo", que mede a mesma coisa que o problema 2 esconde.

Fora do escopo: popularidade (links, visitas), stemming, sinônimos, OU.

---

## 1. Decisões (com a recomendação)

| # | Decisão | Recomendação | Por quê |
|---|---|---|---|
| R1 | Nota do prefixo | **Um termo só**: idf do **conjunto** de documentos que contêm alguma completação; tf = **soma** dos tf das completações no documento. Substitui D8 | é o que o Lucene faz com sinônimos (`SynonymQuery`). O conjunto é grande, então o idf é baixo e uma completação rara não vale mais que a palavra comum. `norte*` passa a valer o mesmo que `norte`, que é o que quem está digitando quer |
| R2 | Como guardar o título | **Campo próprio na mesma lista**: `post:<nó>` passa a ter `(doc, tf_corpo, tf_título)` | uma leitura por termo, como hoje; o E não muda; o merge por documento ("a última gravação vence") continua igual. Uma lista separada `tpost:` dobraria as leituras |
| R3 | Tamanho do título | **No mesmo bloco `norm:`**: (tamanho do corpo, tamanho do título) por documento; `meta:total_title_len` para a média | o ranking já lê esse bloco; um segundo espaço de chaves seria mais um `get` por bloco |
| R4 | Fórmula | **BM25F** (soma ponderada dos tf normalizados por campo, saturada uma vez só) | somar dois BM25 (um por campo) satura cada campo separadamente e deixa o título valer quase uma palavra a mais. No BM25F, o título multiplica o tf antes da saturação |
| R5 | Parâmetros do título | `Config::title_weight` e `Config::title_b`, com **padrão escolhido na Fase 4** pela avaliação (ponto de partida: peso 3, b 0,5) | não dá para escolher no escuro; a grade é pequena (seção 6) |
| R6 | Posições | **Título nas posições 0…T−1, corpo a partir de T + 100** | frase não casa atravessando o fim do título e o começo do texto (o Elasticsearch usa o mesmo vão de 100). O campo de uma ocorrência sai da posição: `p < T` é título |
| R7 | API | `IndexBatch::add_titled(id, título, texto)` novo; `add_document(id, texto)` continua (sem título) | não quebra quem usa. `index-jsonl`, `wiki load` e o CLI passam a usar o novo |
| R8 | `fire-flower index` (arquivos) | **Título = nome do arquivo sem extensão** | barato e útil (`README`, `engine`) |
| R9 | Formato | **v3**; bancos v2 são recusados com a mensagem de hoje ("recarregue") | sem versão publicada, sem migração |
| R10 | `search` (sem ranking) | **Mesmos documentos de hoje** | o título já era indexado (concatenado). Só muda a frase que atravessava título → texto, que deixa de casar (R6) |

### A fórmula (R1 + R4)

Para um termo `t` e um documento `d`, com os campos corpo (c) e título (t):

```
B_f(d)  = 1 − b_f + b_f · len_f(d) / média_f          (f = corpo, título)
tf~(d)  = tf_c(d) / B_c(d)  +  w_t · tf_t(d) / B_t(d)
nota    = idf(df) · tf~ · (k1 + 1) / (tf~ + k1)
```

- **Prefixo (R1):** `tf_f(d)` = soma de `tf_f` das completações que `d`
  contém; `df` = documentos distintos com alguma completação (até
  `max_prefix_terms` completações, como hoje).
- **Frase:** `tf_f` = ocorrências da frase no campo (pela posição, R6);
  idf = soma dos idfs dos termos, como hoje.
- **Sem título** (`add_document`): `tf_t = 0` e `len_t = 0`; a nota é
  exatamente o BM25 de hoje.
- **Limite superior** para o top-k: `B_f` mínimo é `1 − b_f` (documento de
  tamanho 0), então `tf~ ≤ tf_c/(1 − b_c) + w_t · tf_t/(1 − b_t)`. Exige
  `b < 1` nos dois campos (`Config` valida). O corte continua exato.

---

## 2. Formato v3

| Chave | v2 | v3 |
|---|---|---|
| `post:<nó>` | `n`, depois `(Δdoc, tf)` | `n`, depois `(Δdoc, tf_c << 1 \| tem_título, [tf_t])`: o tf do título só ocupa espaço quando existe |
| `pos:<nó>` | posições no texto concatenado | título em 0…T−1, corpo a partir de T + 100 (R6) |
| `norm:<bloco>` | `(Δdeslocamento, len)` | `(Δdeslocamento, len_c, len_t)` |
| `meta:total_title_len` | — | soma dos tamanhos de título |
| `meta:format` | 2 | 3 |

- Em memória, o `NormBlock` denso ganha um `Vec<u16>` com o tamanho do título
  (título com mais de 65.535 tokens é truncado na contagem). Até 6 KB por
  bloco.
- `tf_c` pode ser 0 (termo só no título); a invariante passa a ser
  `tf_c + tf_t ≥ 1`.
- Custo esperado: `post:` +~1 byte só nas entradas com título (poucas por
  termo), `norm:` +1–2 bytes por documento, `pos:` quase nada. O que medir
  está na seção 7.

---

## 3. Fase 1 — Avaliação de relevância (antes de mudar o motor)

Sem isso, "melhorou" vira opinião. Um subcomando `wiki relevancia` gera os
conjuntos do corpus (seed fixa), roda as consultas com `search_top` e grava
top-1, top-3, top-10, MRR e "fora do top-1000" em `bench/results/`. O
relatório ganha uma seção "Relevância".

| Conjunto | Consulta | Resposta esperada | O que mede |
|---|---|---|---|
| `rel-titulo` | o título inteiro ("Revolução Francesa") | o artigo | o que já existe hoje (fica para comparar) |
| `rel-digitando` | o título com a última palavra cortada em ~60% (≥ 3 letras) + `*` (`coreia do nor*`) | o artigo | **problema 1**: o que o TUI busca enquanto se digita |
| `rel-hub` | títulos que aparecem dentro de ≥ 3 outros títulos ("Coreia do Norte" ⊂ "Bandeira da Coreia do Norte", …) | o artigo do título exato em **1º** | **problema 2**: o artigo principal acima dos derivados |
| `rel-corpo` | 2–3 termos raros do **corpo** de um artigo, que não estão no título | o artigo no top-10 | proteção: o peso do título não pode afundar a busca pelo conteúdo |
| `rel-manual` | ~30 consultas escritas à mão em `bench/relevancia/manual.tsv` (consulta, IDs esperados, nota) | os IDs no top-3 | casos reais: `coreia do norte` → 3899, `coreia do nor` → 3899, `brasil`, `são paulo`, `segunda guerra mundial`, `revolução francesa`… |

- Os conjuntos `rel-*` são gerados de 100k e versionados em
  `bench/queries/100k/`, como os de latência.
- A primeira rodada é a **linha de base** com o ranking atual: dá o número de
  "horrível" (espero `rel-hub` e `rel-digitando` baixos).
- O oráculo continua conferindo que as notas do motor batem com a fórmula; a
  avaliação confere se a fórmula ordena bem. São coisas diferentes.

---

## 4. Fase 2 — Prefixo como um termo só (R1, sem mudar o formato)

1. `Scorer::Prefix`: um peso só (idf do conjunto) e tf somado por documento,
   no lugar de `weights`/`term_of` e do máximo. `doc_score` e `doc_bound`
   ficam iguais aos de um termo comum.
2. Oráculo (`wiki.rs`): mesma regra.
3. Testes: o caso `nortel` em miniatura (uma completação rara não supera a
   palavra comum); `norte*` com só `norte` no índice dá a mesma nota que
   `norte`; tf somado com duas completações no mesmo documento.
4. Atualizar D8 em `plano-frases-e-ranking.md` (substituída por R1) e
   `como-funciona.md`.
5. Medir: `rel-digitando` e `rel-manual` vs. a linha de base; latência de
   `p-*` e `misto` (deve ficar igual ou melhor: menos trabalho por documento).

Entrega sozinha: corrige o problema 1 antes do formato novo.

---

## 5. Fase 3 — Título como campo (R2, R3, R6–R10)

1. `Analyzer` com dois campos: tokeniza o título e o texto, com as posições
   da R6, e devolve por termo `(tf_c, tf_t, posições)` e os dois tamanhos.
2. `IndexBatch::add_titled`; `add_document` = `add_titled` sem título.
3. Codec v3: `post:`, `norm:` e merge operator (o merge por documento só
   carrega o campo novo). Testes de ida e volta e de associatividade, como
   os do v2.
4. `meta:total_title_len` e `IndexStats::total_title_len`.
5. Carga: `index-jsonl`, `wiki load` e `carregar_wikipedia.py` param de
   concatenar e passam o título; `fire-flower index` usa o nome do arquivo
   (R8).
6. `meta:format = 3`; abrir v2 dá o erro de formato antigo.
7. Oráculo: tf por campo, os dois tamanhos e as posições com o vão.
   Conferência extra: **`search` devolve os mesmos IDs que o v2** na fatia de
   10k (R10), exceto frases que atravessavam título → texto, contadas à
   parte.

Nesta fase o ranking ainda ignora o título (`w_t = 0`), para separar "o
formato está certo" de "a fórmula ordena bem".

---

## 6. Fase 4 — BM25F e ajuste dos parâmetros (R4, R5)

1. `Bm25` com os dois campos: fatores `B_c(d)` e `B_t(d)` por documento,
   `tf~` e a saturação. Termo, prefixo e frase usam a mesma conta.
2. Limite superior com os dois tf (seção 1). `top_single_term` agrupa por
   nível do limite (não mais só do tf), para manter o corte exato.
3. `Config::title_weight`, `Config::title_b` (validados: `b < 1`, peso ≥ 0)
   e as opções no `wiki` (`--peso-titulo`, `--b-titulo`) para a grade.
4. Oráculo: BM25F, diferença relativa < 1e-4 como hoje.
5. **Grade** em 100k: peso ∈ {1, 2, 3, 5, 8} × `b_título` ∈ {0,3; 0,5;
   0,75}, sobre `rel-hub`, `rel-digitando`, `rel-titulo` e `rel-corpo`.
   Escolher o melhor MRR médio sem piorar `rel-corpo`; **confirmar com outra
   seed** (amostra diferente) para não ajustar ao acaso. O vencedor vira o
   padrão do `Config`.
6. Testes: título igual à consulta vence o mesmo termo só no corpo; título
   curto vence título longo; sem título dá o BM25 de antes (nota idêntica).

---

## 7. Metas

Fixadas depois da linha de base (Fase 1); os valores abaixo são o ponto de
partida.

| Métrica (100k) | Meta |
|---|---|
| `rel-manual` | **todas** no top-3; `coreia do norte` e `coreia do nor` → "Coreia do Norte" em **1º** |
| `rel-hub` top-1 | ≥ 90% |
| `rel-digitando` MRR | ≥ 0,8 |
| `rel-titulo` top-1 | ≥ 95% (hoje 82%) |
| `rel-corpo` top-10 | ≥ linha de base − 2 pontos |
| Oráculo | 0 divergências (fixa) |
| p50 de `rank-freq`, `rank-titulos`, `misto` | ≤ +20% |
| Carga (MB/s) | ≤ −10% |
| Banco compactado | ≤ +10% |

---

## 8. Riscos

- **Ajustar demais à Wikipédia.** Títulos da Wikipédia são curtos e
  descritivos; em outro corpus o peso ideal muda. Por isso o peso é
  `Config`, e `rel-corpo` segura o exagero.
- **`rel-titulo`/`rel-hub` favorecem o título por construção.** As metas
  são lidas junto com `rel-corpo` e `rel-manual`, nunca sozinhas.
- **Corte do top-k.** Com dois campos, o limite fica mais frouxo (o título
  pode multiplicar o tf). O teste `top_k_pruning_is_exact` ganha casos com
  título, e a latência de `rank-freq` é medida na Fase 4.
- **Prefixo com `max_prefix_terms`.** O df do conjunto só conta as 256
  completações mais curtas; num prefixo de 1–2 letras o idf fica um pouco
  alto. Aceitável: esses prefixos casam milhares de documentos de qualquer
  jeito.

---

## 9. Ordem e tamanho

| Fase | Muda o formato? | Tamanho | Entrega sozinha |
|---|---|---|---|
| 1. Avaliação | não | médio (`wiki relevancia`, conjuntos, ~30 consultas à mão) | os números de hoje |
| 2. Prefixo | não | pequeno | corrige o problema 1 |
| 3. Título no formato | **sim (v3)** | grande | título gravado, ranking igual |
| 4. BM25F + grade | não (usa o v3) | médio | corrige o problema 2 |
| Rodada 100k + full | — | máquina ocupada | números finais |

Cada fase é um commit (ou mais), com testes, oráculo zerado e a avaliação
rodada.

---

## 10. Checklist

- [x] Fase 1: `wiki relevancia`, conjuntos `rel-*` em 100k, `manual.tsv`, seção no relatório
- [x] Fase 1: linha de base com o ranking atual
- [x] Fase 2: prefixo como um termo só + oráculo + testes
- [x] Fase 2: D8 substituída por R1 nos docs; avaliação e latência
- [x] Fase 3: analyzer com dois campos e posições com vão
- [x] Fase 3: codec v3 (`post:`, `norm:`, merge) + testes
- [x] Fase 3: `add_titled`, carga sem concatenar, `index` com nome do arquivo
- [x] Fase 3: oráculo v3 + `search` igual ao v2 em 10k
- [x] Fase 4: BM25F, limite com dois campos, `Config` validado
- [x] Fase 4: grade de peso × b, confirmação com outra seed, padrão escolhido
- [ ] Rodadas de performance 100k e full; metas preenchidas

---

## 11. Como ficou (26/10/2026)

Commits: `ad30369` (Fase 1), `9bd293b` (Fase 2), `fb63d23` (Fase 3) e o da
Fase 4.

### Relevância em 100k (linha de base → agora)

| Conjunto | top-1 | top-3 | top-10 | MRR | Meta |
|---|---|---|---|---|---|
| `rel-manual` (36 consultas) | 8,3% → **66,7%** | 36,1% → **94,4%** | 55,6% → 100% | 0,251 → 0,811 | todas no top-3 ✗ (34 de 36) |
| `rel-hub` | 38,6% → **97,9%** | 68,2% → 99,9% | 89,0% → 100% | 0,560 → 0,989 | top-1 ≥ 90% ✔ |
| `rel-digitando` | 64,6% → **84,4%** | 81,2% → 94,2% | 90,1% → 98,0% | 0,741 → **0,897** | MRR ≥ 0,8 ✔ |
| `rel-titulo` | 81,3% → **99,3%** | 92,8% → 100% | 97,1% → 100% | 0,875 → 0,996 | top-1 ≥ 95% ✔ |
| `rel-corpo` | 49,8% → 49,5% | 68,7% → 68,4% | 81,4% → **81,5%** | 0,612 → 0,610 | ≥ base − 2 ✔ |

As duas consultas à mão fora do top-3 são ambíguas: `lula` (o molusco; a
página de desambiguação vem em 1º) e `brasi*` (prefixo curto: "Brasileiros",
"Brasiguaios", "Brasília" antes de "Brasil"). `coreia do norte` e `coreia do
nor*` dão "Coreia do Norte" em 1º.

O que sobra de erro em `rel-hub` e `rel-digitando`: páginas "(desambiguação)"
antes do artigo principal e prefixos curtos que casam vários títulos
(`astron*` → "Astrônomo" antes de "Astronomia").

### Grade (Fase 4)

Peso do título × `b` do título em 100k, depois confirmada com outra amostra
(seed 7). A média do MRR de título, digitando e hub sobe até peso ~50 e
depois quase não muda (0,961 em 50; 0,965 em 1000), e `rel-corpo` não mexe
em nenhum ponto. Escolha: **peso 50, `b` do título 0,9** (0,9 em vez de 1,0:
mesma qualidade, e o limite do corte do top-k continua finito).

| Peso (b 0,9) | 1 | 8 | 24 | 32 | 50 | 100 |
|---|---|---|---|---|---|---|
| MRR médio (seed 42) | 0,736* | 0,841* | 0,946 | 0,954 | 0,961 | 0,964 |
| MRR médio (seed 7) | 0,741 | 0,874 | 0,942 | 0,953 | 0,961 | 0,963 |

<sub>* com b 0,5; a grade inicial ia de 1 a 8 com b ∈ {0,3; 0,5; 0,75}.</sub>

### Desvios do plano

- **`norm:` guarda também a extensão do título** (posições, stopwords
  inclusive), além do tamanho dele: é o que diz se uma ocorrência de frase
  está no título. O tamanho sem stopwords não basta.
- **Fase 3 sem `w = 0`:** o ranking da Fase 3 usou o tf e o tamanho
  totais, o que reproduz exatamente as notas do v2 (a avaliação deu os mesmos
  números da Fase 2). Mais forte que `w = 0`, que zeraria documentos que só
  têm o termo no título.
- **`b = 1` é aceito** (sem limite útil para o corte, mas exato); `Config`
  só recusa valores fora de [0, 1], peso negativo e NaN.

### Custo

Mesmo conteúdo, carregado do mesmo jeito (v2 da Fase 2 vs. v3 com BM25F,
100k, sem compactar, 2 rodadas):

| Conjunto | p50 v2 | p50 v3 | Diferença |
|---|---|---|---|
| `t-medio`, `titulos`, `and-2` (sem ranking) | 33 / 50 / 67 µs | 34 / 50 / 69 µs | ~0 |
| `rank-freq` | 258 µs | 259 µs | 0% |
| `rank-and` | 165 µs | 200 µs | +21% |
| `rank-titulos` | 59 µs | 73 µs | +25% |
| `misto` | 59 µs | 72 µs | +22% |

Acima da meta de +20% nos rankings com vários termos ou com título: com
peso 50 o limite de um documento com o termo no título fica alto e o corte
descarta menos. Carga e banco ficaram iguais (100k: ~3.500–3.700 docs/s,
0,55× o texto).

Uma medição anterior mostrava o v3 ~25% mais lento até na busca sem ranking.
Era o banco v2 de comparação: compactado em outra época (antes dos filtros
particionados), cada `get` dele custava ~0,45 µs a menos com os mesmos nós
byte a byte. Carregando os dois do zero, a diferença some.
