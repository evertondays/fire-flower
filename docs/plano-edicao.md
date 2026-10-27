# Plano — Edição e remoção de documentos

Status: **proposta (27/10/2026).** Substitui a Parte 2 de
[design-escrita-leitura.md](design-escrita-leitura.md), escrita para o formato
v2 (merge com tags BASE/DELTA), que já não bate com o formato v5 (listas em
blocos com tabela de saltos, merge por documento).

Medição: as mesmas regras e ferramentas do
[plano-performance.md](plano-performance.md) (`perf.py`, oráculo, linhas de
base em fire-flower-bench).

## Resumo

- **API:** `add_*` com um ID que já existe passa a **substituir** o documento
  (hoje só troca tf/posições e deixa os termos antigos). Entra
  `IndexBatch::delete_document(id)`.
- **Como:** cada documento guarda os nós dos seus termos (`fwd:<doc>`). Editar
  ou remover grava, no mesmo `WriteBatch` atômico, uma **lápide** (entrada
  com tf 0) nas listas dos termos que saíram. O merge operator descarta as
  lápides quando junta a lista inteira.
- **Por que a busca não perde nada:** toda leitura de lista passa pelo merge
  completo do RocksDB, então a busca nunca vê uma lápide. **O caminho de
  leitura não muda.** O custo aparece só no estado "merge pendente" (entre a
  edição e a compactação), e o plano ataca isso com um merge que só reescreve
  os blocos afetados.
- **Assíncrono onde paga:** (1) uma fila de escrita com commit em grupo, para
  edições pequenas e frequentes (hoje um commit por documento faz ~520
  docs/s); (2) manutenção em segundo plano: tirar do autocompletar palavras que
  ficaram sem documento e consolidar listas quentes.
- **Custo principal:** o `fwd:`. Estimativa na Wikipédia inteira: +0,4–0,5 GB
  (+30–35% do banco) e alguns % na carga. Medido na Fase 2; se pesar, fica
  opcional (`--no-edits`) ou vai para uma família de colunas própria.

---

## 1. Ponto de partida (v5)

| Aspecto | Hoje |
|---|---|
| Reindexar o mesmo ID | troca tf, posições e tamanho; termos que saíram continuam apontando para o doc |
| Merge de `post:`/`pos:` | associativo (`set_merge_operator_associative`), "o mais novo vence" por documento |
| Caminho rápido do merge | pedaços que não se sobrepõem: concatena bytes e tabelas de saltos (é o caso da carga) |
| Caminho lento | pedaços que se sobrepõem: decodifica tudo, ordena, regrava. **Reindexar um doc antigo já cai aqui**, em cada termo dele, até a compactação |
| Saber os termos de um doc | impossível: não há índice reverso |
| `norm:` | `NormBlock::set`, sem remoção |
| Trie | nunca encolhe; `is_end_of_word` nunca é desligado |

Números de referência (full, `ef6ec21`): banco compactado 1,43 GB, `post:`
453 MB, `pos:` 899 MB, 7,3 M nós, 3,06 M termos. Carga 116 s. `t-freq` 328 µs
compactado e 399 µs com merge pendente. Um commit por artigo: 519 artigos/s
(100k).

---

## 2. Desenho

### 2.1 Por que lápides nas listas, e não "documentos apagados" na leitura

A alternativa clássica (Lucene) é marcar o documento como apagado num bitmap e
filtrar na busca, limpando as listas depois. Aqui ela não serve sozinha:

- **Edição não é remoção.** Ao editar o doc 5, ele continua vivo, mas não pode
  mais aparecer na lista de um termo que saiu do texto. O Lucene resolve
  dando um ID interno novo ao doc editado; aqui o ID é do usuário e as listas
  são ordenadas por ele. Um filtro por documento não esconde um par (termo,
  doc).
- O filtro entraria em todos os caminhos de busca (termo, E, prefixo, frase,
  ranking), e o df/idf ficaria errado até a limpeza.

Com lápides, a remoção de um par (termo, doc) é só mais uma entrada na lista,
e o merge, que já decide "o mais novo vence", passa a decidir também "apagado
vence". O commit continua sendo um `WriteBatch` só: a busca vê o documento
inteiro antes ou inteiro depois.

### 2.2 Chaves novas (formato v6)

| Chave | Valor |
|---|---|
| `fwd:<doc>` | nós terminais dos termos do documento, ordenados, em delta varint |
| `meta:edits` | `1` se o banco grava `fwd:` (como `meta:positions`) |
| `meta:maybe_empty` | nós que receberam lápide e podem ter ficado sem documento (conjunto de nós, unido pelo merge) |

`fwd:` guarda IDs de nó e não palavras: nós nunca são apagados, então o ID é
estável, e é mais curto. O formato passa a v6 e o merge operator a
`fire_flower_v6`: como nos outros formatos, um banco v5 é recusado e precisa
ser recarregado.

**Lápides:** em `post:`, uma entrada com `tf << 1 | título` = `0`; em `pos:`,
uma entrada com `k = 0` posições. As duas são inválidas hoje (o decodificador
trata como corrompido), então não há ambiguidade.

### 2.3 Merge operator: completo e parcial

Troca de `set_merge_operator_associative` para
`set_merge_operator(nome, full, partial)` (crate `rocksdb` 0.23, já disponível):

- **parcial** (só operandos, pode haver valor mais antigo em outro nível):
  "o mais novo vence" por documento, **mantendo** as lápides;
- **completo** (com o valor base, ou com a certeza de que não há base: leitura
  ou compactação no último nível): o mesmo, e **descarta** as lápides.

Todo `Get`, `MultiGet` e iterador faz o merge completo. Por isso as listas que
a busca recebe nunca têm lápides e todo o código de leitura fica como está.
Uma lista que perde todos os documentos vira uma lista vazia (2 bytes) e não é
apagada.

**Merge por remendo (o ponto de performance).** O caminho lento de hoje
decodifica e regrava a lista inteira. Com edições, toda lista de termo comum de
um documento editado passaria por ele em cada leitura, até a compactação. O
merge novo junta antes os operandos (pequenos) num "remendo" e o aplica sobre a
base usando a tabela de saltos:

1. para cada documento do remendo, acha o bloco da base que o contém (busca
   binária nas entradas da tabela);
2. blocos sem documento do remendo são **copiados byte a byte** (só o delta da
   primeira entrada é refeito, como no caminho rápido de hoje);
3. blocos tocados são decodificados, recebem as entradas/lápides e são
   regravados (um bloco que fica vazio some);
4. documentos depois do fim da base são anexados (o caminho rápido atual vira
   um caso particular);
5. a tabela é remontada a partir dos blocos; se a lista cair para ≤ 256
   entradas, perde a tabela, e se uma lista pequena passar de 256, é regravada
   inteira (é pequena).

O custo fica parecido com o do caminho rápido de hoje (copiar os bytes da
lista), em vez de decodificar e ordenar tudo. Isso também melhora o caso
"reindexar um ID antigo", que já existe.

### 2.4 Fluxo do escritor

Para cada documento do lote (novo, editado ou removido):

1. **Já existia?** O commit já lê os blocos `norm:` tocados (`write_lengths`);
   a leitura passa para antes da travessia. Documento novo: nenhuma leitura a
   mais. Por isso a carga de um corpus novo não faz `Get` de `fwd:`.
2. **Termos antigos:** só para quem já existia, um `MultiGet` de `fwd:`.
3. **Termos novos:** saem da travessia que o commit já faz (`insert_term`
   devolve o nó). O lote anota, em cada entrada de `TermAcc`, o índice do
   documento no lote (`u32`), para montar o `fwd:` sem tabela de hash.
4. **Diferença:** `antigos − novos` recebe lápide em `post:` e `pos:`.
   Termos que continuam já são sobrescritos pela entrada nova (o mais novo
   vence).
5. **Grava:** `fwd:<doc>` novo (ou `delete`), `norm:` com o tamanho novo (ou
   `NormBlock::remove`), `doc:<id>` (ou `delete`), contadores (`doc_count`,
   `total_len` e `total_title_len` descontam o que saiu) e os nós que
   receberam lápide em `meta:maybe_empty`.

**Dentro de um lote** a última operação de cada documento vence: `add` → `add`
fica com o segundo texto inteiro (hoje um termo que só estava no primeiro
sobra), `add` → `delete` não grava nada, `delete` → `add` é uma edição. Cada
`add`/`delete` recebe um número de sequência; o `normalize` de `TermAcc` só
filtra quando algum documento apareceu mais de uma vez no lote (o caso comum,
cada documento uma vez, continua sem custo).

### 2.5 Palavras que ficam sem documento

A Trie não encolhe, e `suggest` e a expansão de prefixo olham só
`is_end_of_word`. Uma palavra cujo último documento foi removido continuaria
sendo sugerida e ocuparia uma das 256 vagas do prefixo.

- **Na leitura:** o `Engine` mantém em memória os nós de `meta:maybe_empty`.
  Conjunto vazio (todo banco sem remoções): nenhum custo. Senão, um candidato
  que está no conjunto tem a lista conferida (só o cabeçalho, `list_len`)
  antes de entrar no resultado. Fica correto desde o commit.
- **Em segundo plano:** a manutenção (2.6) confere esses nós; os que têm lista
  vazia perdem `is_end_of_word` (os nós rasos são marcados no `EpochCache`) e
  saem do conjunto. Uma palavra que volta a ser indexada recebe o flag de novo
  pelo `insert_term`, como hoje.

### 2.6 O que roda de forma assíncrona

A edição em si não precisa ser assíncrona para ficar barata: o commit não lê
listas, só grava entradas pequenas. Três coisas ganham com o assíncrono:

| Peça | Problema | Proposta |
|---|---|---|
| Fila de escrita (`AsyncWriter`) | um commit por edição custa caro (~520 docs/s); aplicação que edita um documento por vez | thread do escritor recebe `upsert`/`delete` por um canal com limite e faz commit em grupo (a cada N docs ou T ms). Devolve um número de commit; `wait(n)` dá leitura-das-próprias-escritas |
| Palavras vazias | ver 2.5 | a mesma thread, entre commits, quando a fila está ociosa |
| Listas quentes com merge pendente | depois de muitas edições, termos frequentes pagam o merge em toda leitura até a compactação | experimentos na Fase 4: `max_successive_merges`, compactação disparada por contagem de edições, ou "materializar" (o escritor lê a lista consolidada e grava com `put`) |

A fila é opcional: `engine.batch()` continua existindo e é síncrono.

---

## 3. A/B: como medir

Igual ao que o projeto já faz, com dois cuidados a mais.

1. **Dois binários alternados sobre os mesmos bancos:** `main` numa worktree e
   a branch, 5 rodadas alternadas, mediana (como na
   ANALISE-refatoracao-vs-main). Os dois compilados com
   `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`, para tirar o ruído de alinhamento
   de código (±6% no `rank-freq` naquela análise) sem mudar o `Cargo.toml`.
2. **No mesmo binário**, quando a diferença é uma opção (`fwd:` ligado ou não,
   família de colunas, fila ou não), como o `perf.py stemming` faz.

Nova fase `perf.py edits --scale <escala>`:

| Etapa | O que mede |
|---|---|
| Plano de edições | `wiki edit` aplica, com seed fixa, sobre a fatia carregada: troca o texto de 50% dos documentos pelo de outro artigo da fatia seguinte, remove 10% e readiciona 1% dos removidos. Lotes de 1, 100 e 10 mil |
| Oráculo | o `wiki oracle --edits` aplica o mesmo plano no índice em memória: **0 divergências** |
| Equivalência | `digest` do banco editado e compactado = `digest` de um banco carregado direto com o estado final (o digest passa a ignorar listas vazias) |
| Vazão | edições/s e remoções/s por tamanho de lote; síncrono vs. `AsyncWriter` |
| Leitura depois | `PRE_SETS` + `s-*`/`p-*` no banco editado antes de compactar, depois de compactar e no banco limpo |
| Concorrência | `wiki concurrent` com um editor no lugar do indexador: p99 do `mixed` |

---

## 4. Fases

Cada fase é um PR com a medição que a justifica.

### Fase 0 — Linha de base e harness (sem mexer no motor)

- `wiki edit`, `wiki oracle --edits`, `digest` ignorando listas vazias, fase
  `edits` no `perf.py` (as etapas que dependem do motor ficam puladas até a
  Fase 2).
- `baseline-pr-full.json` e a rodada `all --scale full` da `main` atual com
  `codegen-units = 1`, para servir de A.

**Pronto quando:** a linha de base com `codegen-units = 1` está salva e a
variação entre duas rodadas iguais é conhecida (o "ruído" usado nas metas).

### Fase 1 — Merge operator v6 (lápides + remendo)

Só o codec e o merge; o escritor ainda não grava lápides.

- `merge_lists` em versões completa e parcial; lápides em `post:` e `pos:`;
  merge por remendo.
- Testes do codec (seed fixa): sequências aleatórias de pedaços com entradas e
  lápides, aplicadas (a) uma a uma no merge completo, (b) com merges parciais
  em pontos aleatórios, (c) num `BTreeMap` de referência: os três iguais. Lista
  que cruza 256 entradas nos dois sentidos; lista que fica vazia; operando
  corrompido faz a leitura falhar.
- Microbenchmark (`benches/micro.rs`): base de 145 mil entradas + 1 operando
  que se sobrepõe, caminho lento atual vs. remendo, em `post:` e `pos:`.

**Pronto quando:** oráculo 10k e 100k com 0 divergências; A/B contra a `main`
sem diferença além do ruído na carga e nos conjuntos antes e depois de
compactar (o caminho rápido não pode piorar); o remendo ≥ 5× mais rápido que o
caminho lento no microbenchmark.

### Fase 2 — `fwd:`, substituição e remoção

- Formato v6, `meta:edits`, `fwd:`, `delete_document`, `NormBlock::remove`,
  estatísticas, rótulos, regra "a última operação vence" no lote.
- `Engine::contains(id)` e, só para o harness, os nós de um documento
  (`#[doc(hidden)]`).
- Testes de integração: add → edição → os termos antigos não acham mais o doc
  e os novos acham; add + delete no mesmo lote não grava nada; delete de ID
  inexistente não faz nada; estatísticas e `norm:` batem depois de remover;
  reabrir no meio das edições; concorrência: durante um lote de edições, um
  termo só do texto antigo e um só do novo nunca acham o doc ao mesmo tempo
  nem nenhum dos dois.
- A/B no mesmo binário: `fwd:` desligado vs. ligado na família padrão vs.
  ligado numa família de colunas própria (ZSTD, sem filtro de Bloom: só é lido
  para documentos que já existem).

**Pronto quando:** oráculo com edições 0 divergências e digest equivalente;
busca sem edições igual à `main` (±ruído); carga e tamanho dentro das metas
(seção 5). Se não couber, `fwd:` vira opcional (`--no-edits` na CLI,
`Config::edits`, gravado como as posições) e um banco sem ele recusa
`delete_document` com erro de configuração.

### Fase 3 — Palavras vazias

- Conjunto `maybe_empty` em memória, filtro em `words_with_prefix` e na
  expansão de prefixo, limpeza no escritor.
- Testes: remover o único documento de uma palavra a tira do `suggest` e do
  prefixo na hora; readicioná-la a traz de volta; o mesmo depois de reabrir.

**Pronto quando:** `s-*` e `p-*` iguais à `main` num banco sem remoções, e o
custo com 10% removidos (antes da limpeza) medido e registrado.

### Fase 4 — Leitura depois de muitas edições

Medir o estado pendente (Fase 2 já mede) e, se passar da meta, testar em A/B:
`max_successive_merges`, compactação disparada por número de edições e
materialização das listas mais lidas. Escolher pelo p50/p99 dos `PRE_SETS` e
pelo custo no escritor.

**Pronto quando:** as metas de "depois de editar" da seção 5 batem, ou a
Fase 4 registra por que não vale a pena.

### Fase 5 — `AsyncWriter` (condicional)

Só se a Fase 2 mostrar que edições de um documento por vez são o gargalo para
o uso esperado. Fila com limite, commit em grupo, `wait`, erros devolvidos no
próximo `wait` ou `flush`, encerramento que grava o que está na fila.

**Pronto quando:** edições de 1 documento por chamada ≥ 10× mais rápidas que um
commit por edição, sem piorar o p99 dos leitores no `concurrent`.

### Fase 6 — CLI e docs

- `fire-flower delete <banco> <id>...`.
- `index-jsonl`: linhas `{"op": "delete", "id": ...}` (sem `op` continua sendo
  adicionar/substituir).
- `index --sync`: reindexar uma pasta atualiza arquivos alterados e remove os
  que sumiram (precisa de `lbl:<caminho>` → ID; hoje `index` de novo duplica
  os documentos, porque `add_labeled` gera ID novo).
- README (tira a primeira limitação conhecida), como-funciona (chaves, merge,
  fluxo de edição), design-escrita-leitura (Parte 2 aponta para este plano).

---

## 5. Metas

"Ruído" é a variação medida na Fase 0 entre rodadas iguais.

| Métrica | Meta |
|---|---|
| Divergências do oráculo (com e sem edições) | **0** (fixa) |
| Busca em banco sem edições, todos os conjuntos, p50 | igual à `main` (± ruído) (fixa) |
| Carga total com `fwd:` | ≤ +10% de tempo, sem colapso, pico de RSS ≤ 1,5 GB |
| Banco compactado com `fwd:` | ≤ +35% |
| Carga total com `--no-edits` | igual à `main` (± ruído) |
| Depois de editar 10% dos docs, antes de compactar: `t-freq`, `titles`, `rank-freq` | p50 ≤ +25% do banco limpo (hoje o merge pendente da carga dá +22%) |
| Depois de editar e compactar | igual ao banco limpo (± ruído) |
| Remover 1 documento (commit próprio) | registrar; com `AsyncWriter`, ≥ 10× a vazão síncrona |
| p99 do `mixed` com um editor em paralelo (4 leitores) | ≤ 5 ms (a mesma meta da indexação em paralelo) |

---

## 6. Perguntas em aberto

- `fwd:` sempre ou opcional? Depende da Fase 2. Proposta: ligado por padrão,
  `--no-edits` para cargas que só acrescentam.
- O `AsyncWriter` é necessário para o uso previsto (edição de um documento por
  vez vinda de uma aplicação) ou edições sempre chegam em lote?
- Família de colunas para `fwd:`: vale a complexidade (o projeto hoje só usa
  a padrão)? Decide o A/B da Fase 2.
- Lápides de documentos que nunca existiram naquele termo (não acontece pelo
  fluxo, mas um `fwd:` corrompido geraria): o merge completo as descarta sem
  erro; vale um contador de diagnóstico?

## 7. Checklist

- [ ] Fase 0: harness de edição, linha de base com `codegen-units = 1`
- [ ] Fase 1: merge v6 com lápides e remendo
- [ ] Fase 2: `fwd:`, substituição, `delete_document`, formato v6
- [ ] Fase 3: palavras vazias fora do autocompletar e do prefixo
- [ ] Fase 4: leitura depois de muitas edições
- [ ] Fase 5: `AsyncWriter` (se a Fase 2 pedir)
- [ ] Fase 6: CLI e docs
