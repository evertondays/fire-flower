# Plano: correção de erros de digitação — fire-flower

Hoje uma letra errada zera a busca. O termo exato precisa existir na Trie, e
todas as partes da consulta são obrigatórias (E):

| # | O que acontece | Por quê |
|---|---|---|
| 1 | **`revolucao fracesa` não acha nada**, nem a "Revolução Francesa" | `fracesa` não está na Trie: `term_node` devolve `None` e o E sai vazio na hora (seção 6.2 do [como-funciona.md](como-funciona.md)) |
| 2 | **No TUI, um erro no meio da palavra apaga resultados e sugestões** | a palavra sendo digitada vira prefixo (`brasl*`), e nenhum nó da Trie começa com `brasl` |
| 3 | **Ninguém fica sabendo do erro** | a resposta é "0 resultados", sem um "você quis dizer" |
| 4 | **Uma palavra que é ela mesma um erro do corpus "acerta"** | a Wikipédia tem erros de digitação indexados (df 1); a busca com o mesmo erro acha só esse artigo, e não os milhares com a palavra certa |

Acentos e maiúsculas já não contam (a normalização NFKD da seção 5.1 trata
`coração` = `coracao`), então este plano trata só de letras trocadas,
faltando, sobrando ou invertidas.

Fora do escopo: fonética (`x`/`ch`, `s`/`z` como regra, no estilo do
Soundex), palavras coladas ou separadas (`guardachuva` ↔ `guarda chuva`),
correção dentro de frase e sinônimos.

---

## 1. Decisões (com a recomendação)

| # | Decisão | Recomendação | Por quê |
|---|---|---|---|
| T1 | Distância | **Damerau-Levenshtein** (inserção, remoção, troca e **transposição de vizinhas** valem 1), sobre o termo já normalizado | transposição é o erro mais comum de quem digita rápido (`pretno`); o Lucene e o Elasticsearch contam assim por padrão (`transpositions: true`) |
| T2 | Quantos erros, pelo tamanho | **0 até 4 letras e 1 a partir de 5**. Dois erros ficam fora da primeira versão (a Fase 0 mediu 7,5 ms no p50 percorrendo a Trie); voltam se um dicionário em memória (B) for medido e compensar | palavras curtas têm vizinhos demais e todos frequentes (`casa`: cada, caso, cama, capa, cara, casas): corrigir vira ruído e lê listas enormes. O Meilisearch usa 5 e 9; o `AUTO` do Elasticsearch, 3 e 6 |
| T3 | Primeira letra | **Fixa** (o `prefix_length: 1` do Lucene); a Fase 0 mede também sem fixar | erro na primeira letra é raro, e o leque da raiz (37 KB, milhares de filhos) é o que mais custa num percurso com erros. O Meilisearch conta um erro na primeira letra como dois |
| T4 | Como achar os candidatos | **(A), decidido na Fase 0** (seção 2). A comparação era entre **(A)** percorrer a Trie com um autômato de Levenshtein, sem mudar o formato, e **(B)** um dicionário em memória (FST, como o Tantivy e o Lucene) | A: carga e banco iguais, funciona em bancos que já existem e nos leitores só leitura, mas custa os nós lidos abaixo do cache (profundidade 4). B: microssegundos, mas o vocabulário muda a cada commit e o FST é imutável (precisa de reconstrução ou de um delta em memória), e abrir o banco fica mais caro. O SymSpell (remoções pré-calculadas) fica fora: com 3 milhões de termos e 2 erros seriam dezenas de milhões de chaves |
| T5 | Quais candidatos valem | **df ≥ `typo_min_df` (2)**; só os da **menor distância** que achou algum; ordem (distância, df decrescente, palavra) e no máximo **`max_typo_terms` (8)** | o df 1 corta os erros do próprio corpus (problema 4). Só a menor distância evita que `historya` (1 erro → `historia`) puxe também tudo a 2 erros. O desempate fixo deixa o resultado determinístico, que o oráculo precisa |
| T6 | Quando corrigir | **Só termos ausentes ou com df < `typo_min_df`, e sem flexões no índice** (`TypoMode::Missing`); a Fase 5 compara com `IfFewer(n)`, como nas flexões | custo zero em toda consulta cujas palavras existem (a maioria, e todos os conjuntos de hoje), e é exatamente o caso que hoje zera. `Always` mudaria a ordenação de tudo |
| T7 | Nota | **Grupo como o das flexões** (`Scorer::Group`): `tf~ = tf~_exata + γ · tf~_correções`, idf do maior df do grupo; γ por grade (ponto de partida 0,5) | reaproveita `groups.rs`, o E por união e o limite superior do top-k. Com γ < 1, se a palavra exata existir (`IfFewer`/`Always`), ela vence os empates |
| T8 | Frases | **Sem correção** | as aspas pedem as palavras como estão (a mesma regra das flexões, D5 do [plano-stemming.md](plano-stemming.md)) |
| T9 | Prefixo e autocompletar | **Fase 4, à parte**: só quando o prefixo não tem nenhuma completação, com 1 erro, primeira letra fixa e prefixo de 5 letras ou mais; depois, as completações dos nós achados | é o problema 2. Custa mais que o termo exato (o autômato aceita qualquer nó do caminho, não só fim de palavra), então fica medido à parte e pode ficar de fora |
| T10 | Mostrar a correção | **`TopHits` (e o `search`) devolvem as trocas feitas** (`fracesa` → `francesa`); o TUI mostra na linha "consulta:", a CLI imprime "corrigido: …" e os bindings expõem o campo | quem busca precisa saber por que achou outra coisa ("Mostrando resultados para…") |
| T11 | Configuração | **`Config::typos: TypoMode`** (`Off`, `Missing`, `IfFewer(n)`, `Always`), `typo_weight` (γ), `typo_min_df`, `max_typo_terms`, comprimentos de T2. Vale por busca, não fica gravado. Padrão `Off` até a Fase 5 | com (A), ligar não exige recarregar o banco. O padrão só muda depois de as metas fecharem |
| T12 | Com stemming e flexões | **Stemming na indexação:** corrige o radical (a Trie só tem radicais). **Flexões na consulta:** as correções não ganham flexões (um nível de expansão só) | expandir as flexões de cada correção multiplicaria as listas por consulta |
| T13 | Avaliação | **Conjuntos novos com o erro gerado e escrito à mão** (Fase 1); os de hoje como guarda | sem resposta esperada não dá para escolher γ nem os limites de T2 |

**Formato:** nada muda: o `FORMAT_VERSION` fica, e bancos já carregados
ganham a correção ao serem abertos com uma versão nova.

---

## 2. Fase 0 — Diagnóstico (antes de mudar o motor)

**Por quê:** a decisão T4 depende de quantos nós um percurso com erros lê na
Trie da Wikipédia inteira (7,3 milhões de nós, 3 milhões de termos), e isso
não dá para estimar no papel.

1. **Sonda** (`examples/typo_probe.rs`, com o autômato só no exemplo): para
   palavras tiradas do `rel-title` e do `t-mid`, com um erro sintético (as
   regras do `rel-typo`, Fase 1), percorrer a Trie com o autômato e medir:
   nós visitados, nós fora do cache, `Get`s, latência (p50/p99), quantos
   candidatos e em que posição fica a palavra original pela ordem de T5.
   Separado por distância (1, 2), primeira letra fixa ou não e tamanho da
   palavra (5–8, 9–12, 13+). O percurso anda nível a nível com `MultiGet`,
   como o `words_with_prefix`.
2. **df do vocabulário:** quantos dos 3 milhões de termos têm df 1, 2, 3–9 e
   10+. Decide o `typo_min_df` e o tamanho de um FST de (B).
3. **(B) fora do motor:** um FST (crate `fst`) das palavras com df ≥
   `typo_min_df`: tamanho, tempo para montar a partir da Trie e tempo de
   consulta com o mesmo autômato.

**Pronto quando:** uma tabela (A) × (B) com latência, memória e custo na
abertura, e a decisão T4 escrita aqui. Critério: fica (A) se o p99 de um
termo com até 2 erros na Wikipédia inteira couber na meta de `t-typo`
(seção 7); se só 1 erro couber, (A) com o segundo erro desligado até haver
(B).

### Resultado (04/11/2026)

`cargo run --release --example typo_probe -- <banco> <vocab.tsv>
bench/queries/full/{t-mid,titles,t-freq,and-2}.txt`, na Wikipédia inteira
(formato v7, cache quente, 1 thread), com 400 palavras reais (df ≥ 2, só
a–z) e um erro gerado fora da primeira letra. "Certa em 1º" é a ordem de T5
(df decrescente).

| Caso | p50 | p99 | Nós visitados (p50) | Lidos fora do cache (p50 / p99) | Candidatos (média) | Certa entre eles | Certa em 1º |
|---|---|---|---|---|---|---|---|
| 5–8 letras, 1 erro, 1ª fixa | 519 µs | 1,6 ms | 412 | 82 / 236 | 5,5 | 100% | 80% |
| 5–8 letras, 1 erro, 1ª livre | 2,9 ms | 4,1 ms | 6.692 | 103 / 302 | 6,3 | 100% | 79% |
| 9+ letras, 1 erro, 1ª fixa | 599 µs | 1,2 ms | 457 | 125 / 306 | 1,4 | 100% | 94% |
| 9+ letras, 2 erros, 1ª fixa | 7,5 ms | 13,8 ms | 6.029 | 2.326 / 4.665 | 2,5 | 87% | 76% |
| 9+ letras, 2 erros, 1ª livre | 35 ms | 56 ms | 55.928 | 4.141 / 8.755 | 2,7 | 87% | 74% |
| 5–8 letras, 1 erro, percurso com 2 | 7,7 ms | 15,2 ms | 5.940 | 2.209 / 4.933 | 5,5 | 100% | 80% |

- **1 erro com a primeira letra fixa cabe** (~0,5 ms); fixar a letra corta
  os nós visitados em ~16× e o tempo em ~6×.
- **2 erros não cabem na Trie**: milhares de leituras por palavra. Ficam
  fora da primeira versão (T2), até (B) ser medido.
- **df do vocabulário** (`vocab-full.tsv`): 63,0% dos 3.055.341 termos têm df
  1, 11,6% df 2, 14,7% df 3–9 e 10,7% df ≥ 10. `typo_min_df = 2` tira dos
  candidatos quase dois terços do vocabulário e deixa ~1,1 milhão de
  palavras para um eventual (B).
- (B) não foi medido: com 1 erro a Trie basta.

## 3. Fase 1 — Avaliação

| Conjunto | O que é | Acerto |
|---|---|---|
| `rel-typo` | gerado: as consultas do `rel-title` e do `rel-hub` com **um** erro numa palavra de 5 letras ou mais, fora da primeira letra: letra vizinha no teclado ABNT2, letra faltando, letra repetida ou duas vizinhas invertidas. Só entra se a palavra errada não existe no índice (ou tem df < 2) | top-1 e top-10 |
| `rel-typo-manual` | ~60 consultas com erros reais escritos à mão (`pesquiza`, `excessao`, `previlegio`, `beneficiente`, `ancioso`) e o artigo esperado, conferidas contra a fatia de 100k | top-3 |
| `rel-typo-typing` | o `rel-typing` com um erro antes do corte (`brasl*`), para a Fase 4 | top-1 |
| `t-typo` | latência: palavras do vocabulário com 1 erro gerado | p50/p99 |
| `t-miss-far` | palavras sem nenhum vizinho no índice (`xxbdbb`): o custo de procurar e não achar | p50/p99 |
| `s-typo` | autocompletar com erro (Fase 4) | p50/p99 |

O gerador fica no `wiki relevance`, com semente fixa, como os outros
conjuntos. Os de hoje (`rel-title`, `rel-typing`, `rel-hub`, `rel-body`,
`rel-manual`, `rel-plural`) viram **guarda**.

O `t-miss` muda de sentido: parte das palavras dele está a um erro de uma
palavra que existe (`ilegiveld`), e com a correção ligada elas passam a
achar resultados. Ele continua no `perf.py`, e o `t-miss-far` passa a medir
o caso "não achou nada".

**Oráculo:** o índice ingênuo do `wiki oracle` ganha a correção por força
bruta (a distância de T1 contra o vocabulário inteiro, com as regras de T2,
T3, T5 e T6) e precisa dar zero divergências no 10k com `TypoMode::Missing`
e `Always`.

**Pronto quando:** os conjuntos estão em `bench/relevance/100k/` e
`bench/queries/`, o `perf.py relevance` os mede com a correção desligada (a
linha de base: 0% no `rel-typo`, por construção) e o oráculo tem a correção.

## 4. Fase 2 — Correção na busca (T1–T8, T11, T12)

1. **Autômato:** `src/query/typos.rs`, puro (sem RocksDB): o estado é a
   linha da matriz de Damerau-Levenshtein, avançada um caractere por vez, com
   "ainda dá para casar com até *d* erros?" para podar. Testes no
   `src/query/tests.rs` contra a distância calculada direto.
2. **Percurso (se T4 = A):** `src/engine/typos.rs`, com a mesma leitura nível
   a nível do `words_with_prefix` (cache de nós até a profundidade 4,
   `MultiGet` no resto). Devolve os nós de fim de palavra aceitos; o df vem
   do cabeçalho de `post:` (uma leitura por candidato, no `MultiGet` das
   listas). Aplica T5.
3. **Busca:** em `search_with` e `search_ranked_with`, um termo exato que
   cai em T6 vira um grupo (`add_group`, `Member::Group`, `Scorer::Group`
   com γ no lugar de α). Um candidato cujas versões estão todas mortas
   (antes da compactação o df ainda as conta) dá lista vazia e é pulado.
4. **Resultado:** `TopHits::corrections` e um `Engine::search_corrected`
   (ou o mesmo campo num tipo novo) com as trocas feitas.
5. **Métricas:** termos corrigidos, nós visitados e consultas que acharam
   resultados só por causa da correção, no `metrics/` (o painel e o
   OpenMetrics ganham as séries).

**Pronto quando:** zero divergências no oráculo; `rel-typo` e
`rel-typo-manual` nas metas; a guarda idêntica; e o A/B completo contra a
`main` (`perf.py all --scale full`) com todos os conjuntos de hoje dentro do
ruído, menos o `t-miss` (decisão explícita, seção 3).

## 5. Fase 3 — Mostrar e configurar (T10, T11)

- CLI: `search --typos missing|if-fewer|always|off`; a saída diz
  "corrigido: fracesa → francesa".
- TUI: a linha "consulta:" mostra a troca (`revolucao fracesa → francesa`);
  `Ctrl+T` liga e desliga.
- Bindings: a opção em `search`/`searchTop` e o campo `corrections` no
  resultado (Node; o PHP quando existir).
- README e `docs/como-funciona.md`: seção 6.x "Erros de digitação" e as
  linhas novas da tabela de custos (seção 10).

## 6. Fase 4 — Prefixo e autocompletar com erro (T9)

1. O mesmo autômato em modo prefixo: aceita o nó quando o prefixo inteiro
   foi consumido com até 1 erro, sem exigir fim de palavra.
2. Só quando o prefixo exato não tem completação e tem 5 letras ou mais.
3. As completações dos nós aceitos, com o mesmo limite de
   `max_prefix_terms` somado entre eles; no `suggest`, as do prefixo
   corrigido mais frequente primeiro.

**Pronto quando:** `rel-typo-typing` e `s-typo` nas metas, e o `s-*` e o
`p-*` de hoje iguais (o caminho só roda quando o exato não acha nada). Se o
custo não couber, a fase fica registrada aqui e sai do escopo.

## 7. Fase 5 — Ajuste

Grade em 100k, como a do peso do título e a do α das flexões:

- γ ∈ {0,2; 0,3; 0,5; 0,7}
- comprimentos de T2 ∈ {(4, 8), (5, 9), (6, 10)}
- `typo_min_df` ∈ {1, 2, 3}
- modo ∈ {`Missing`, `IfFewer(10)`}

O ponto escolhido é conferido numa segunda amostra da Wikipédia. Depois
disso, a decisão de ligar a correção por padrão (`Missing`) ou deixar `Off`.

## 8. Metas

| Métrica | Hoje | Meta |
|---|---|---|
| `rel-typo` top-1 / top-10 | 0% / 0% (por construção) | ≥ 70% / ≥ 85% |
| `rel-typo-manual` top-3 | medir na Fase 1 | ≥ 70% |
| `rel-typo-typing` top-1 (Fase 4) | 0% | ≥ 60% |
| Guarda (`rel-title`, `rel-hub`, `rel-body`, `rel-typing`, `rel-manual`, `rel-plural`) | os do `RESULTADOS.md` | iguais |
| `t-typo` p50 / p99 (Wikipédia inteira) | — | ≤ 1 ms / ≤ 3 ms (a correção, ~0,5 ms, mais a busca das palavras corrigidas) |
| `t-miss-far` p50 | 1 µs (`t-miss`) | ≤ 100 µs |
| Os outros conjuntos de latência | `RESULTADOS.md` | dentro do ruído (±5%) |
| Carga e banco compactado (full) | 2min31s / 1.496 MB | sem mudança com (A) |
| Oráculo (10k) | 0 divergências | 0 divergências |

## 9. Riscos

- **Corrigir para a palavra errada.** `cidadae` → `cidade` é óbvio;
  `pernambco` tem um vizinho só; mas uma palavra de 5 letras pode ter vários
  vizinhos a 1 erro, todos frequentes. Mitigação: T2, a ordem por df de T5,
  γ < 1 e mostrar a troca (T10).
- **O custo de uma consulta errada vira o de uma certa.** `historya` passa a
  ler a lista de `historia` (403 KB no 100k). É o mesmo custo de digitar certo, mas o
  `t-miss`, que hoje leva 1 µs, deixa de ser grátis. Precisa ficar escrito no
  PR (seção 11 das diretrizes).
- **Pior caso do percurso (A).** Uma palavra longa com 2 erros, perto de um
  trecho denso da Trie, pode ler milhares de nós. Mitigação: o limite de T2,
  a primeira letra fixa, e um teto de nós por termo contado nas métricas. O
  teto precisa ser o mesmo no oráculo, senão as divergências não fecham.
- **Consultas de várias palavras.** Cada termo ausente é corrigido sozinho;
  duas correções independentes podem não aparecer juntas em nenhum documento
  (o E zera de novo). Fica medido no `rel-typo-manual`; corrigir em conjunto
  é outro plano.
- **Edições antes da compactação.** O df ainda conta as versões mortas
  (seção 5.6), então um candidato pode ter df alto e nenhum documento vivo:
  tratado na Fase 2 (lista vazia é pulada).
- **(B) e os leitores só leitura.** Um FST em memória teria de ser montado
  também no `Reader` de cada processo e refeito quando o banco muda; é
  boa parte do custo de (B).

## 10. Ordem e tamanho

| Fase | O quê | Tamanho |
|---|---|---|
| 0 | Diagnóstico: sonda na Trie, df do vocabulário, FST fora do motor | pequeno a médio (harness, sem mudar o motor) |
| 1 | Conjuntos de avaliação e oráculo | médio (gerador, ~60 consultas à mão, força bruta no oráculo) |
| 2 | Correção na busca | grande (autômato, percurso, grupos, métricas, A/B) |
| 3 | CLI, TUI, bindings e docs | pequeno |
| 4 | Prefixo e autocompletar | médio, opcional |
| 5 | Ajuste | médio (grade) |

A Fase 0 pode mudar a Fase 2 inteira (T4). A Fase 4 só começa se a 2 fechar
as metas com folga na latência.

## 11. Checklist

- [x] Fase 0: sonda na Wikipédia inteira (`examples/typo_probe.rs`); df do vocabulário; T4 decidida (FST não medido: com 1 erro a Trie basta)
- [ ] Fase 1: `rel-typo`, `rel-typo-manual`, `rel-typo-typing`, `t-typo`, `t-miss-far`, `s-typo`; oráculo com correção
- [ ] Fase 2: autômato, percurso, grupos na busca e no ranking, métricas; oráculo e A/B
- [ ] Fase 3: CLI, TUI, bindings, README e como-funciona
- [ ] Fase 4: prefixo e autocompletar com erro (ou registrado como fora)
- [ ] Fase 5: grade e decisão do padrão
