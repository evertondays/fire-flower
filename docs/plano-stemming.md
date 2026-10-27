# Plano: stemming sem perder ordenação nem velocidade — fire-flower

O stemming Snowball entrou como opção (`--stemming pt`, branch
`stemming-snowball`) e foi medido contra o mesmo binário sem stemming, na
Wikipédia inteira ([ANALISE-stemming.md](https://github.com/evertondays/fire-flower-bench/blob/main/ANALISE-stemming.md)).
Ele faz o que promete: uma consulta com outra flexão da palavra passa a achar
o artigo (top-10 de 0% para 55% no `rel-inflection`). Mas o preço é alto demais
para ligá-lo por padrão:

| # | Problema | Evidência (Wikipédia inteira; relevância em 100k) |
|---|---|---|
| 1 | **A ordenação piora em todos os conjuntos que já existiam** | `rel-hub` top-1 97,9% → 87,8%; `rel-body` top-10 81,5% → 65,2%; `rel-title` 99,3% → 97,3%; `rel-typing` 84,4% → 81,5% |
| 2 | **O Snowball junta palavras sem relação** (sobre-agrupamento) | `coreia` = `cor` = `cores`; `lage` = `lago`; `nove` = `nova`; `area` = `areia`; `parte` = `partir` = `partido`; `comuna` = `comunidade` = `comunista`. No 100k, 20 mil palavras com mais de 3 letras ficam com radical de até 3 |
| 3 | **E não junta o que devia** (sub-agrupamento) | singular e plural de -ção/-ão ficam separados: `eleicao` → `eleica`, `eleicoes` → `eleico`; `regiao`/`regioes`, `cancao`/`cancoes`; `lei`/`leis` |
| 4 | **Frases e E ficam mais lentos** | frases +70% a +105% no p50 (`phrase-2` 509 → 933 µs); `and-2` +48%; `rank-and` +83% |
| 5 | **Palavras raras viram listas longas** | p99 de consultas de uma palavra ~6× (`t-mid` 52 → 322 µs): "abandonarei" (4 docs) lê a lista de `abandon` (34.746 docs); `t-rare` passa de 1,7 para 295 resultados em média |
| 6 | **O autocompletar mostra radicais** | `brasileir`, `inform`, `corr` no lugar de palavras |
| 7 | **A escolha é fixa na criação do banco** | mudar exige recarregar; não dá para ligar por consulta |

A raiz dos problemas 1, 4, 5 e 6 é a mesma: **o índice só guarda o radical**.
A palavra exata deixa de existir, então não dá para preferi-la, as frases
conferem as posições de todas as flexões e o autocompletar só tem radicais
para mostrar. Os problemas 2 e 3 são do algoritmo e valem para qualquer
abordagem.

Fora do escopo: sinônimos, correção de erros de digitação, outros idiomas.

---

## 1. Decisões (com a recomendação)

| # | Decisão | Recomendação | Por quê |
|---|---|---|---|
| D1 | Onde aplicar o radical | **Na consulta**: o índice continua com as palavras, e a busca soma as flexões. O stemming na indexação (o de hoje) fica só como linha de comparação até a decisão final | resolve 1, 4, 5, 6 e 7 de uma vez: a palavra exata continua no índice e pode valer mais; frases e autocompletar não mudam; dá para ligar por consulta |
| D2 | Como achar as flexões de uma palavra | **Espaço novo `stem:<radical>` → nós das palavras com esse radical**, gravado na indexação (só quando uma palavra aparece pela primeira vez na Trie) | uma leitura por termo da consulta. No 100k, 88% dos radicais têm uma palavra só, a mediana é 1 e o p99 é 8 (máximo 63): o mapa é pequeno e a união é de poucas listas |
| D3 | Nota de uma palavra com flexões | **A palavra exata com peso 1 e as flexões com peso α < 1**, como um campo a mais do BM25F (`tf~ = tf_exata + α · tf_flexões`); α escolhido por grade (seção 6) | é o que o Elasticsearch faz com um campo de radicais ao lado do campo exato. Com α < 1, "Austral" continua acima de "Austrália" |
| D4 | Quando expandir | **Comparar "always" com "só se faltar"** (expande só quando a busca exata tem menos de `k` resultados) | "só se faltar" custa zero nas consultas que já acham bastante, que são a maioria; "always" muda a ordenação de todas |
| D5 | Frases | **Exatas** (as aspas pedem as palavras como estão) | é o que se espera de aspas, e elimina o problema 4 nas frases |
| D6 | Prefixos e autocompletar | **Sem stemming**, como antes do branch | com as palavras no índice, `bras*` e o autocompletar voltam a mostrar palavras (problema 6) |
| D7 | Sobre-agrupamento | **Radical mínimo de 4 letras** (se o Snowball deixar menos, fica a palavra) e **palavras de até 4 letras não são reduzidas**; medir também uma lista curta de exceções | `cor`, `lag`, `nov`, `are` somem como radicais; o custo é perder algumas flexões curtas (`pais`/`paises` ainda juntam) |
| D8 | Sub-agrupamento | **Normalizar o plural antes do Snowball**, com um mínimo de letras antes do sufixo: `-ções` → `-ção`, `-ões` → `-ão`, `-ães` → `-ão`; medir também `-ais` → `-al` e `-éis` → `-el` | junta `eleição`/`eleições`, a classe de substantivos mais comum do português, que o Snowball deixa separada. Os dois últimos são arriscados (`pais` viraria `pal`) |
| D9 | Alternativa ao Snowball | **Medir também um stemmer leve** (só plural e gênero, no estilo do de Savoy): `casas` → `casa`, `brasileiras` → `brasileiro`, mas `correndo` ≠ `corrida` | agrupa muito menos (menos problema 2) e cobre a variação mais comum nas consultas (plural) |
| D10 | Avaliação | **Conjuntos que não dependem do Snowball** (seção 3) e os atuais como guarda | o `rel-inflection` é gerado com o próprio Snowball: uma junção errada conta como acerto |

---

## 2. Fase 0 — Diagnóstico (antes de mudar o motor)

**Por quê:** saber quanto de cada perda vem do sobre-agrupamento (problema 2)
e quanto vem de não distinguir a palavra exata (problema 1). Isso decide se
basta corrigir o algoritmo (D7, D8) ou se é preciso a expansão na consulta
(D1).

1. Classificar os erros novos do `rel-hub` e do `rel-body` com stemming
   (~100 e ~160 casos no 100k): o 1º lugar casou por uma **flexão de verdade**
   ("Astronomia" → "Astrônomo") ou por uma **junção errada** ("Lage" →
   "Lago")? Script no `wiki`, com a classificação conferida à mão numa
   amostra de 50.
2. Distribuição dos grupos por radical na Wikipédia inteira: palavras por
   radical, df somado, quantos radicais têm ≤ 3 letras e quais grupos misturam
   palavras de sentidos diferentes (os 200 maiores, à mão).
3. Custo das frases: confirmar que a piora vem do tamanho das listas de
   posições (posições lidas por consulta com e sem stemming, com o `wiki
   bench`).

**Pronto quando:** uma tabela "erro novo → causa" com a fração de cada uma, e a
lista dos grupos problemáticos.

## 3. Fase 1 — Avaliação que não depende do Snowball

| Conjunto | O que é | Acerto |
|---|---|---|
| `rel-plural` | gerado: as consultas do `rel-body` com as palavras trocadas pelo plural ou singular **regular** (regras simples: `+s`, `-ção`/`-ções`, `-al`/`-ais`), só quando a outra forma existe no corpus e não está no artigo | top-10 |
| `rel-inflection-manual` | ~60 consultas escritas à mão com flexões reais e o artigo esperado (como "eleições presidenciais do brasil" → o artigo da eleição, ou "correr maratona" → Maratona), conferidas contra os artigos que existem na fatia | top-3 |
| `rel-inflection` | o de hoje (Snowball): fica, mas como indicador, não como meta | top-10 |
| `t-inflection` | conjunto de latência: palavras do vocabulário que têm outras flexões no índice, para medir o custo da expansão | p50/p99 |

Os conjuntos atuais (`rel-title`, `rel-typing`, `rel-hub`, `rel-body`,
`rel-manual`) viram **guarda**: nenhuma mudança pode piorá-los.

**Pronto quando:** os conjuntos novos estão versionados em
`bench/relevance/100k/` e o `perf.py stemming` os mede com e sem stemming.

## 4. Fase 2 — Corrigir o algoritmo (D7, D8, D9)

Barata e útil para qualquer abordagem: mexe só em `src/stem.rs`, e o A/B
continua sendo o `perf.py stemming` de hoje (stemming na indexação).

1. Plural antes do Snowball (D8) e radical mínimo / palavras curtas (D7), cada
   um medido separado.
2. Stemmer leve (D9) como `Stemming::Leve(Language)`.
3. Para cada variante: radicais distintos, grupos problemáticos da Fase 0 e
   os conjuntos da Fase 1.

**Pronto quando:** escolhida a variante com melhor `rel-plural` e
`rel-inflection-manual` sem piorar a guarda mais que hoje. Se só isso já zerar a
perda da guarda, a Fase 3 pode ser dispensada.

## 5. Fase 3 — Flexões na consulta (D1–D6)

1. **Índice:** espaço `stem:<radical>` → lista ordenada de nós (merge
   operator de união, como `post:`). O commit grava um item novo só quando uma
   palavra vira terminal pela primeira vez na Trie (o `insert_term` já sabe
   disso). Medir o custo na carga e no tamanho.
2. **Consulta:** um termo exato vira um grupo `{exata, flexões}`. No `search`
   (sem ranking), o grupo é a união das listas; no ranking, a nota do D3. O E
   continua sendo conduzido pela menor lista (a da palavra exata, se for a
   menor, ainda conduz quando a expansão é "só se faltar").
3. **Config:** `Config::inflections` (desligado, "always", "só se faltar"),
   por banco, e uma opção por consulta para o TUI e a CLI (`--inflections`).
   Exige o mapa `stem:` no banco, gravado desde a criação (como as posições).
4. **Oráculo:** o grupo de flexões e a nota com α, escritos de novo no
   `wiki oracle`.

**Pronto quando:** zero divergências no oráculo de 10k e o A/B completo
(carga, tamanho, latência, relevância) contra o stemming na indexação e contra
nenhum stemming.

## 6. Fase 4 — Ajuste (α e modo de expansão)

Grade em 100k, como a do peso do título no plano de relevância: α ∈ {0,1;
0,2; 0,3; 0,5; 0,7} × modo ∈ {sempre, só se faltar}, com a variante de stemmer
escolhida na Fase 2. Confirmar o ponto escolhido numa segunda amostra da
Wikipédia.

## 7. Metas

| Métrica | Sem stemming | Stemming na indexação (hoje) | Meta |
|---|---|---|---|
| `rel-title` / `rel-hub` top-1 | 99,3% / 97,9% | 97,3% / 87,8% | ≥ 99% / ≥ 97% |
| `rel-body` top-10 | 81,5% | 65,2% | ≥ 80% |
| `rel-typing` top-1 | 84,4% | 81,5% | ≥ 84% |
| `rel-plural` top-10 | — | — | ≥ 80% |
| `rel-inflection-manual` top-3 | — | — | ≥ 70% |
| `rel-inflection` top-10 (indicador) | 0% | 55% | ≥ 50% |
| p50 de frases | 509 µs (`phrase-2`) | +83% | sem mudança |
| p50 / p99 de `t-mid`, `and-2` | 18 / 52 µs; 33 / 83 µs | p99 ~6× | p50 ≤ +15%, p99 ≤ 2× |
| Carga da Wikipédia inteira | 116 s | 118 s | ≤ 122 s (+5%) |
| Banco compactado | 1.434 MB | 1.270 MB | ≤ 1.510 MB (+5%) |
| Autocompletar | palavras | radicais | palavras |
| Divergências do oráculo | 0 | 0 | **0** |

## 8. Riscos

- **α único pode não servir para todos os conjuntos**: o `rel-hub` quer α
  baixo e o `rel-plural`, alto. Mitigação: o modo "só se faltar", que não
  mexe na ordenação quando a palavra exata já acha resultados.
- **"Só se faltar" depende de `k`**: a mesma consulta pode dar resultados
  diferentes no `search_top(10)` e no `search_top(100)`. Precisa ficar
  documentado (ou usar um limiar fixo, como "menos de 10 documentos").
- **O mapa `stem:` muda o formato do banco**: bancos sem ele não têm flexões.
  Como as posições, é decidido na criação.
- **Regras de plural erradas** (D8) juntam palavras diferentes (`pais` →
  `pal` com `-ais` → `-al`): conferir os grupos que mudarem, à mão, antes de
  adotar.

## 9. Ordem e tamanho

| Fase | O quê | Tamanho |
|---|---|---|
| 0 | Diagnóstico | pequeno (scripts, sem mudar o motor) |
| 1 | Conjuntos de avaliação | pequeno (gerador + ~60 consultas à mão) |
| 2 | Correções do algoritmo | pequeno (`stem.rs`) |
| 3 | Flexões na consulta | grande (formato, consulta, ranking, oráculo) |
| 4 | Ajuste | médio (grade) |

As Fases 0 a 2 podem mostrar que corrigir o algoritmo basta; a Fase 3 só
começa se a guarda continuar caindo.

## 10. Checklist

- [x] Stemming na indexação como opção (`--stemming pt`), com oráculo e A/B (`perf.py stemming`)
- [x] Conjunto `rel-inflection` e análise ([ANALISE-stemming.md](https://github.com/evertondays/fire-flower-bench/blob/main/ANALISE-stemming.md))
- [x] Fase 0: erros novos classificados por causa; grupos problemáticos listados
- [x] Fase 1: `rel-plural`, `rel-inflection-manual`, `t-inflection`
- [x] Fase 2: plural antes do Snowball, radical mínimo (descartado), stemmer de plural (`pt-plural`)
- [x] Fase 3: espaço `stem:`, grupos de flexões na consulta, oráculo
- [x] Fase 4: grade de α e modo de expansão
- [ ] Decisão final: qual modo fica, qual sai, e o padrão

---

## 11. Como ficou (26/10/2026)

Branch `stemming-snowball`. Números da rodada `perf.py stemming --scale
full` (relevância em 100k), em [RESULTADOS.md](https://github.com/evertondays/fire-flower-bench/blob/main/RESULTADOS.md)
e [ANALISE-flexoes.md](https://github.com/evertondays/fire-flower-bench/blob/main/ANALISE-flexoes.md).

### Fase 0

- `rel-hub`: 102 consultas perdidas com o Snowball, 1 ganha. A maioria dos
  pares (palavra da consulta → palavra do título do 1º) não tem relação:
  nomes próprios e radicais curtos (`lage→lago`, `prato→prata`, `som→soma`).
  Os outros são derivações (`terror→terrorismo`) ou flexões de verdade
  (`torre→torres`), que também erram porque "Torre" e "Torres" são artigos
  diferentes. O radical mínimo (D7) separaria 42 dos 104 pares.
- `rel-body`: 165 perdidas, 2 ganhas. Quase todos os pares são flexões
  legítimas (`fisicas→fisica`); a perda vem de cada palavra casar 5,7× mais
  documentos (mediana), não de junções erradas. D7 separaria 7 dos 36.

Conclusão: corrigir o algoritmo ajuda, mas a palavra exata precisa valer
mais que as flexões.

### Fase 2 (100k, stemming na indexação)

| Variante | hub@1 | corpo@10 | plural@10 | flexão à mão@3 |
|---|---|---|---|---|
| sem stemming | 97,9% | 81,5% | 0% | 38,2% |
| Snowball | 87,8% | 65,2% | 41,4% | 86,3% |
| Snowball + plural antes (D8) | 87,7% | 65,1% | 46,3% | 90,8% |
| Snowball + radical mínimo (D7) | 90,8% | 68,0% | 34,2% | 81,7% |
| **só plural (`pt-plural`, D9)** | **96,3%** | **78,2%** | **71,1%** | **96,2%** |

D8 entrou no Snowball; D7 ficou de fora (perde recuperação). O `pt-plural`
ganhou também o "e" final depois de r/z ("mercadores" = "mercador"), que
levou os pares plural/singular separados de 12% para 7%.

### Fases 3 e 4

A primeira fórmula, `BM25(exata) + α · BM25(flexões)`, derrubava o `rel-hub`
para 77% mesmo com α = 0,1: a segunda parte saturada, com o idf da flexão,
desempatava a favor de artigos longos. A que ficou é a do D3: um termo só,
`tf~ = tf~_exata + α · tf~_flexões`, com o idf do maior df (como o
`SynonymQuery` do Lucene, sem decodificar as listas para contar a união). O
modo "só se faltar" não ajuda: "planetas" já acha documentos demais para
expandir, e o `rel-inflection-manual` fica em 38%. Padrão: α = 0,5, sempre.

### Resultado (Wikipédia inteira; relevância em 100k)

| Métrica | Sem stemming | Plural na indexação | Flexões na consulta | Meta |
|---|---|---|---|---|
| `rel-title` / `rel-hub` top-1 | 99,3% / 97,9% | 99,1% / 96,3% | **99,3% / 97,9%** | ≥ 99% / ≥ 97% |
| `rel-body` top-10 | 81,5% | 78,2% | 79,8% | ≥ 80% |
| `rel-typing` top-1 | 84,4% | 83,9% | 84,3% | ≥ 84% |
| `rel-plural` top-10 | 0% | **71,1%** | 65,0% | ≥ 80% |
| `rel-inflection-manual` top-3 | 38,2% | **96,2%** | 93,1% | ≥ 70% |
| p50 de frases | 506 µs (`phrase-2`) | +17% a +35% | **sem mudança** | sem mudança |
| p50 / p99 de `t-mid` | 18 / 53 µs | **+3% / +89%** | +61% / +245% | ≤ +15% / ≤ 2× |
| p50 / p99 de `and-2` | 33 / 86 µs | **+3% / +42%** | +58% / +113% | ≤ +15% / ≤ 2× |
| Carga | 118 s | **116 s** | 121 s | ≤ +5% |
| Banco compactado | 1.434 MB | **1.376 MB** | 1.467 MB | ≤ +5% |
| Autocompletar | palavras | palavras (singular) | palavras | palavras |
| Oráculo | 0 | 0 | 0 | 0 |

Nenhuma das duas cumpre todas as metas. O plural na indexação perde 1,6
ponto no `rel-hub` e 3,3 no `rel-body`, e as frases ficam 17–35% mais
lentas; em troca, é quase de graça no resto e o que mais acha flexões. As
flexões na consulta mantêm a ordenação de sem stemming, mas termos e E ficam
1,5–2,4× mais lentos (cada palavra lê também as listas das flexões).

**Recomendação:** `--stemming pt-plural` como a opção indicada (e candidata a
padrão). Manter as flexões na consulta como opção para quem precisa da
palavra exata na frente, e o Snowball como opção para verbos e derivados
(ele é o único que acha `rel-inflection`, 55%, mas perde 10 pontos no `rel-hub`).
