# Análise — Como bater as metas de edição (e o que os outros fazem)

Status: **análise (28/10/2026), depois do [plano-edicao.md](plano-edicao.md).**
Números do A/B em
[ANALISE-edicao.md](https://github.com/evertondays/fire-flower-bench/blob/main/ANALISE-edicao.md).

## Resumo

- **Onde ficamos:** busca sem edições igual à `main`; carga +6%, banco +20%
  e compactação +26% por causa do `fwd:`; e, antes da compactação, buscas de
  termos frequentes 7× mais lentas depois de 1.000 edições.
- **A causa:** o desenho coloca a remoção **dentro das listas** (lápides), e o
  RocksDB une as listas **na leitura**. Com edições espalhadas, cada leitura
  regrava a lista inteira. É o único dos motores olhados que paga a edição
  na leitura dessa forma.
- **O que os outros fazem:** ou marcam o documento como morto e filtram na
  busca, consolidando em segundo plano (Lucene, Tantivy, RediSearch, GIN do
  PostgreSQL), ou pagam a edição na escrita, com ler-alterar-gravar (Meilisearch,
  Xapian).
- **Proposta:** IDs internos de versão + conjunto de documentos mortos (o
  modelo do Lucene, adaptado para manter os IDs do usuário e o merge do
  RocksDB). Sem lápide e sem `fwd:`: carga e banco iguais aos da `main`,
  edição só acrescenta, e a busca antes da compactação paga um teste de bit
  por documento em vez de regravar listas.

---

## 1. Onde as metas não fecharam

| Meta | Resultado | Por quê |
|---|---|---|
| Busca antes da compactação ≤ +25% depois de editar | `t-freq` ~7×, autocompletar ~17× | o merge na leitura regrava as listas tocadas |
| Carga com `fwd:` ≤ +10% | ✅ +6% (de +6% a +11%) | anotar e gravar os termos de cada documento |
| Banco ≤ +35% | ✅ +20% | o `fwd:` (311 MB) |
| (não era meta) compactação | +26% | proporcional aos dados a mais |

A meta de leitura é a que importa: as outras passaram, mas com custo
permanente (carga, espaço) para uma funcionalidade que muitos bancos nem usam.

## 2. A causa, em detalhe

Uma lista `post:` é a base (blocos de 128 documentos com tabela de saltos) e,
até a compactação, os pedaços mais novos. Cada `Get` faz o merge completo. Sem
edições, os pedaços se concatenam com um `memcpy` (~2 ns por entrada). Com
edições, o remendo copia os blocos não tocados e regrava os tocados, entrada
por entrada (~15 ns cada).

O problema é a **probabilidade de tocar um bloco**. Um termo com 25 mil
documentos tem ~200 blocos; 1.000 edições espalhadas tocam, no termo, algumas
centenas de documentos, e quase todo bloco recebe pelo menos um. Daí a lista
inteira é regravada a cada leitura. Nenhum ajuste do merge muda isso:
`max_successive_merges` e flush mediram igual, e copiar trechos entre as
mudanças (sem regravar entrada por entrada) daria talvez 2–3×, longe de +25%.

Há uma segunda causa estrutural: os IDs são do usuário e as listas são
ordenadas por eles. Por isso uma edição precisa mexer **no meio** das listas,
e não no fim, onde o merge só concatenaria.

## 3. Como outros motores resolvem

| Motor | Remover | Atualizar | Quem paga | Consolidação |
|---|---|---|---|---|
| **Lucene / Elasticsearch / OpenSearch** | marca um bit num bitset do segmento; a busca pula | remove + adiciona com ID interno novo | a busca testa o bit; estatísticas (df) contam os removidos até o merge | merge de segmentos em segundo plano; o `TieredMergePolicy` prefere segmentos com mais removidos |
| **Tantivy** | por termo (não há chave primária); no commit grava um bitset imutável de removidos por segmento | idem Lucene | idem | merges |
| **RediSearch** | marca o documento como removido | idem | a busca filtra | GC em processo forkado varre o índice e limpa as listas |
| **PostgreSQL (GIN, `fastupdate`)** | a tupla fica morta; o índice confere a visibilidade | entra numa lista pendente | a busca varre a lista pendente além do índice | limpeza quando passa de `gin_pending_list_limit`, no vacuum ou à mão; a documentação recomenda deixar para o autovacuum |
| **Meilisearch** (LMDB, um bitmap de documentos por palavra) | até a 1.5: remoção lógica e coleta de lixo; desde a 1.6: compara a versão velha e a nova do documento e aplica só a diferença nos bitmaps | idem (diferença) | a escrita (ler-alterar-gravar os bitmaps) | toda escrita é uma tarefa assíncrona, agrupada em lotes |
| **Xapian** | precisa da tabela de termos por documento (*termlist*) | idem | a escrita | — |
| **fire-flower (hoje)** | lápide em cada termo (`fwd:` diz quais) | lápides nos termos que saíram + entradas novas | **a leitura, até a compactação** | compactação (`Maintenance`) |

Três padrões:

1. **Marcar e filtrar** (Lucene, Tantivy, RediSearch, GIN). A remoção custa um
   bit; a busca paga um teste por documento, que custa pouco perto de
   decodificar a lista. No Lucene, com 50% dos documentos removidos, a vazão
   cai 18–46% conforme a consulta, e esse é o pior caso. As estatísticas ficam
   aproximadas até o merge, e o Lucene aceita isso.
2. **Diferença na escrita** (Meilisearch desde a 1.6, Xapian). Precisa saber
   os termos antigos do documento (o `fwd:`, a *termlist* do Xapian) e paga a
   edição na escrita. Funciona porque o LMDB e as B-trees alteram o valor no
   lugar: a leitura seguinte já lê a lista pronta. O fire-flower copiou a
   metade "saber os termos antigos", mas não a metade "alterar no lugar":
   no RocksDB, a alteração vira um pedaço pendente que a leitura une.
3. **Pendência limitada e limpa em segundo plano** (GIN, merges do Lucene,
   GC do RediSearch). É o que a `Maintenance` faz. Todos limitam o tamanho
   da pendência e preferem limpar em segundo plano.

O Meilisearch teve um caminho parecido com o nosso: a remoção lógica com
coleta de lixo (até a 1.5) gastava disco e trazia bugs, e eles migraram para
a diferença na escrita, que funciona para eles porque o LMDB altera no lugar.

## 4. O que poderíamos fazer

### A. IDs internos de versão + documentos mortos (recomendado)

O modelo do Lucene, adaptado para manter os IDs do usuário e o merge do
RocksDB.

- **ID interno = ID do usuário na primeira versão.** A carga não muda nada:
  nenhuma chave nova, nenhum `fwd:`.
- **Uma atualização ganha um ID interno novo**, numa faixa alta e crescente
  (`2^63 + n`), e o antigo vai para o conjunto de **mortos**. As entradas
  novas caem sempre depois do fim das listas: o merge só concatena, como na
  carga. Uma readição depois de remover também ganha ID novo: **um ID interno
  nunca volta**.
- **Remover** = pôr o ID interno nos mortos (um bitmap em memória, gravado em
  blocos, como o `norm:`). Não há lápide em termo nenhum, então o `fwd:` não
  é necessário.
- **A busca filtra os mortos** com um teste de bit por documento, só quando o
  conjunto não está vazio. Sem remoções, custo zero (o mesmo atalho do
  `MaybeEmpty`). Os IDs da faixa alta são traduzidos de volta (um mapa pequeno:
  só os documentos editados). O resultado em ordem de ID é a junção de dois
  trechos já ordenados, a faixa normal e a traduzida, em O(n).
- **A limpeza cabe no merge.** Como um ID interno nunca volta, descartar um
  morto é seguro em qualquer merge, parcial ou completo, e em qualquer ordem: o
  merge operator recebe o conjunto de mortos (um `Arc` na closure) e os tira
  das listas. A compactação comum do RocksDB limpa sem passo extra.
- **Estatísticas:** `doc_count` e os tamanhos ficam exatos (`norm:` por ID
  interno); o df conta os mortos até a compactação, como no Lucene.

| | Hoje | Com A (estimativa, a medir) |
|---|---|---|
| Carga | +6% | igual à `main` |
| Banco | +20% | igual à `main`, mais um bitmap de mortos (~1 MB) e o mapa dos editados |
| Remover um documento | lápides em ~300 termos | um bit e um contador |
| Atualizar | lápides nos termos que saíram + entradas | entradas novas no fim das listas |
| Busca antes da compactação | 7× (`t-freq`) | +1 teste de bit por documento lido: até ~+50% numa lista inteira, menos no E e no top-k; independe do número de edições |
| Compactação | +26% | igual à `main` |

**Custos e riscos:**

- o resultado precisa traduzir os IDs da faixa alta (mapa em memória) e
  juntar dois trechos;
- o idf conta os removidos até a compactação, então o ranking muda um pouco;
- as palavras sem documento não são detectadas no commit (sem `fwd:` não se
  sabe quais termos o documento tinha): é preciso uma varredura de fundo das
  listas que ficaram vazias, como o GC do RediSearch. Até lá, o autocompletar
  pode sugerir uma palavra que não acha nada;
- muda o formato (v7) e o desenho do commit; o oráculo e o digest continuam
  servindo para provar a equivalência.

### B. Pendência separada, como o GIN

Manter o desenho atual, mas gravar as edições numa chave à parte
(`edit:<nó>`, pequena) em vez de pedaços de merge da lista. A busca lê a base
**sem merge** (cópia zero, como hoje sem edições) e aplica a pendência num
cursor, pulando e trocando entradas enquanto percorre, sem regravar nada. Um
"dobrador" em segundo plano incorpora a pendência na base (ler + `put`) e
apaga a chave.

- Custo na busca: um `Get` a mais por termo com pendência (um conjunto em
  memória diz quais têm, então os sem edição não pagam nada) mais
  O(tamanho da pendência).
- Mantém o `fwd:` (carga +6%, banco +20%).
- Menos mudança que A, mas a meta de carga e espaço continua como está.

### C. Paliativos no desenho atual

| Mudança | Ganho esperado | Limite |
|---|---|---|
| Remendo copiando trechos entre as mudanças, sem regravar entrada por entrada | 2–3× no estado degradado | ainda percorre os blocos tocados; não chega a +25% |
| Materializar listas quentes no escritor (ler o merge e gravar com `put`) depois de N edições no termo | a busca volta ao normal para esses termos | a escrita paga por termo frequente (~0,5 ms cada na fatia de 100k); amplificação de escrita |
| `Maintenance` por custo, não por contagem (amostrar o tempo de merge das buscas) | compacta quando dói | continua compactando o banco inteiro |
| `fwd:` numa família de colunas com compressão melhor (zstd; precisa da feature do crate) ou compactação menos agressiva | reduz parte dos +20% e da escrita extra | não resolve a leitura |

### D. Fila de escrita assíncrona (como o Meilisearch)

Agrupar edições pequenas em lotes melhora a vazão de escrita (hoje, ~700
commits de um documento por segundo). Não melhora a leitura, que é o
problema. Vale depois de A, se aparecer uma aplicação que edita muito.

## 5. Recomendação

**A**, em fases, cada uma com o A/B do jeito de sempre:

1. IDs internos e mortos no escritor, `fwd:` removido, busca filtrando (ainda
   sem limpeza): oráculo com edições e digest; carga e banco iguais aos da
   `main`.
2. Limpeza dos mortos no merge operator: medir a busca antes e depois da
   compactação com 1.000 edições contínuas (meta: ≤ +25%) e com o plano
   inteiro.
3. Varredura de fundo das palavras sem documento (substitui o `MaybeEmpty`).
4. Revisar a `Maintenance`: com A, ela só serve para recuperar espaço, não
   para salvar a latência.

Se mudar o modelo de IDs for demais, **B** é o segundo melhor: resolve a
leitura e mantém o resto como está. As opções de **C** cabem como paliativo em
qualquer caso.

## Fontes

- [Lucene's Handling of Deleted Documents](https://www.elastic.co/blog/lucenes-handling-of-deleted-documents)
  (bitset por segmento, atualização = remoção + adição, estatísticas com os
  removidos, vazão 18–46% menor com 50% removidos, `TieredMergePolicy`).
- [Discussão sobre `deletes_pct_allowed`](https://discuss.elastic.co/t/why-is-segment-not-merged-when-deleted-over-33/353537)
  (33% por padrão no Elasticsearch 7.x).
- [Arquitetura do Tantivy](https://github.com/opencollector/tantivy/blob/main/ARCHITECTURE.md)
  (remoção por termo, bitset imutável gravado no commit).
- [Meilisearch is too slow](https://www.meilisearch.com/blog/meilisearch-is-too-slow)
  (remoção lógica até a 1.5, diferença na escrita desde a 1.6, tarefas
  assíncronas com auto-batching).
- [GIN: dicas](https://www.postgresql.org/docs/16/gin-tips.html)
  (lista pendente, `gin_pending_list_limit`, limpeza em segundo plano).
- [Garbage collection no RediSearch 1.4.1](https://redis.io/blog/increased-garbage-collection-performance-redisearch-1-4-1/)
  (remoção marcada, GC em processo forkado).
- [Xapian, ticket #181](https://trac.xapian.org/ticket/181)
  (sem a *termlist*, não dá para remover nem substituir documentos).
- [Compaction filter em operandos de merge](https://git.nextgraph.org/NextGraph/rocksdb/commit/d80ce7f99a1f875e3d43e20e6d5810f359ec2fbe)
  (`FilterMergeOperand`, para limpar operandos na compactação).
