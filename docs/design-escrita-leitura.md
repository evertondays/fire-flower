# Design — Escrita com um escritor, leitura multi-core

Status: **proposta, não implementada.**
Relacionado: [plano-performance.md](plano-performance.md), que diz como medir
cada passo.

## Resumo

- **Escrita:** existe sempre **um único escritor** (uma thread que mexe na
  Trie, distribui IDs e faz commit). A preparação dos documentos (strip HTML,
  tokenizar, deduplicar) roda em paralelo, antes do escritor.
- **Edição:** o motor ganha `update` e `delete`. Para isso, cada documento
  guarda a lista dos termos que tem (índice reverso), e o merge operator passa
  a aplicar adições **e remoções** em ordem.
- **Leitura:** quantas threads quiserem, cada busca sobre um snapshot (já é
  assim). O trabalho aqui é tirar os pontos de disputa: o cache de blocos, a
  compactação competindo por CPU e a raiz da Trie decodificada em toda busca.

Por que um escritor só: a Trie distribui IDs de nó em sequência e cada nó tem
a lista dos filhos. Com dois escritores, os dois poderiam criar o mesmo filho
ao mesmo tempo, e seria preciso lock por nó ou detecção de conflito. Com um
escritor, a ordem dos commits é a ordem das mudanças, e é isso que torna
`update`/`delete` simples de fazer certo.

---

## Estado atual (ponto de partida)

| Aspecto | Hoje |
|---|---|
| Escritores | um por vez: `Engine::batch()` pega `Mutex<WriterState>` |
| Preparação | `add_document` tokeniza dentro do lote, na thread do escritor |
| Cache de nós do escritor | só dentro do lote; descartado a cada commit |
| Listas de postagem | merge operator **associativo** de união; só adiciona |
| Update/delete | não existem; reindexar o mesmo ID só acrescenta termos |
| Leitura | `Engine: Sync`; cada busca abre um snapshot |
| Block cache | LRU de 256 MB |
| Compactação | `increase_parallelism(nº de CPUs)`, que disputa núcleos com as buscas |

---

## Parte 1 — Escrita: preparo em paralelo, um escritor

### 1.1 Separar preparo de aplicação

```rust
/// Documento já tokenizado. Pode ser criado em qualquer thread (`Send`).
pub struct PreparedDoc {
    id: Option<DocId>,      // None = o escritor distribui o ID (como `add_labeled`)
    label: Option<String>,
    terms: Vec<String>,     // normalizados, ordenados, sem repetição
}

impl PreparedDoc {
    pub fn new(id: DocId, text: &str) -> Self;
    pub fn labeled(label: &str, text: &str) -> Self;
    pub fn from_html(id: DocId, html: &str) -> Self;  // strip_html + tokenize
}

impl IndexBatch<'_> {
    pub fn add_prepared(&mut self, doc: PreparedDoc) -> Result<Option<DocId>>;
}
```

`add_document` e `add_labeled` continuam existindo, como atalhos para
`add_prepared(PreparedDoc::new(...))`. Nada muda para quem já usa a API.

IDs automáticos (`labeled`) são distribuídos **no escritor**, na ordem em que
os documentos chegam a ele. Por isso a ordem precisa ser preservada (1.2).

### 1.2 Pipeline pronto na biblioteca

```rust
pub struct PipelineOptions {
    pub workers: usize,          // threads de preparo; padrão: núcleos - 1
    pub docs_per_commit: usize,  // padrão: 1000
    pub chunk: usize,            // docs por mensagem no canal; padrão: 64
}

pub fn index_all<I>(engine: &Engine, docs: I, opts: PipelineOptions) -> Result<IndexStats>
where
    I: Iterator<Item = Input> + Send,   // Input = (id ou rótulo, texto, é_html)
```

```
leitor (thread atual) ──chunks numerados──▶ N workers ──PreparedDoc──▶ escritor
                        sync_channel(2·N)              sync_channel(2·N)
```

- Só `std`: `std::thread::scope` + `std::sync::mpsc::sync_channel`. Nenhuma
  dependência nova.
- **Canais com limite:** a memória fica limitada mesmo se o escritor for mais
  lento que os workers.
- **Ordem preservada:** cada chunk leva um número de sequência, e o escritor
  reordena com um `BTreeMap<seq, chunk>` antes de aplicar. Isso deixa o
  resultado determinístico (mesmos IDs de nó em toda rodada), o que facilita
  comparar bancos e repetir benchmarks. O buffer fica limitado pelos canais.
- **Erro em qualquer etapa:** fecha os canais, as outras threads terminam e o
  lote em andamento é descartado (sem commit). O que já foi commitado fica.

### 1.3 Cache de nós que sobrevive ao commit

Hoje cada lote começa com o cache vazio e relê do RocksDB (e redecodifica) os
nós quentes do topo da Trie. Com um escritor só, o cache pode morar no
`WriterState`, porque ninguém mais altera nós.

- **O que guardar:** nós até a profundidade `Config::writer_cache_depth`
  (padrão a medir; começar com 4). São os mais tocados, e cortar por
  profundidade evita precisar de LRU.
- **Copy-on-write:** o lote copia do cache do escritor o nó que vai alterar.
  Só depois de um `db.write` bem-sucedido as cópias alteradas voltam para o
  cache. Se o commit falhar, o cache continua igual ao banco.
- **Ao abrir o banco:** cache vazio. Ele aquece no primeiro lote.

### 1.4 Ajustes do RocksDB para o escritor

| Config nova | Padrão | Efeito |
|---|---|---|
| `background_jobs: Option<i32>` | `None` = comportamento atual | `set_max_background_jobs`; limita quantos núcleos flush e compactação usam |
| `max_subcompactions: Option<u32>` | `None` | `set_max_subcompactions` |
| `CommitOptions { wal: bool }` em `IndexBatch::commit_with` | `wal: true` | sem WAL, a carga em massa fica mais rápida, mas uma queda perde tudo desde o último `Engine::flush()`. Só para reconstrução do índice |

---

## Parte 2 — Edição: `update` e `delete`

### 2.1 API

```rust
impl IndexBatch<'_> {
    /// Indexa `doc`. Se o ID já existir, **substitui** o conteúdo anterior.
    pub fn add_prepared(&mut self, doc: PreparedDoc) -> Result<Option<DocId>>;
    /// Remove o documento. Não faz nada se o ID não existir.
    pub fn delete_document(&mut self, id: DocId) -> Result<()>;
}

impl Engine {
    /// Termos indexados de um documento (útil para depurar e para o oráculo).
    pub fn document_terms(&self, id: DocId) -> Result<Option<Vec<String>>>;
}
```

**Muda o comportamento:** reindexar um ID passa a substituir em vez de somar.
Isso elimina a primeira "limitação conhecida" do README.

### 2.2 Novas chaves

| Chave | Valor |
|---|---|
| `fwd:<doc>` | IDs dos **nós terminais** dos termos do documento, ordenados e em delta varint |
| `meta:format` | versão do formato em disco (`2`) |

Guardar IDs de nó, e não as palavras, deixa o `fwd:` menor. Isso funciona
porque nós nunca são apagados, então o ID de um nó é estável.

**Custo:** o `fwd:` tem uma entrada por par (documento, termo), o mesmo número
de entradas das listas de postagem, então o índice cresce de forma
comparável. O tamanho real sai da Fase 2 do plano de performance.
Decisão inicial: **sempre gravar** (um caminho de código só). Se o custo
pesar, vira `Config::track_documents`, e bancos sem ele recusam
`delete`/`update`.

### 2.3 Fluxo do escritor

Para cada documento que entra no lote (novo, editado ou removido):

1. Termos antigos: `fwd:<id>` do cache do lote (se o doc já passou por este
   lote) ou do banco. Se não existir, é documento novo.
2. Termos novos: `insert_term` para cada termo do `PreparedDoc`, gerando os
   IDs de nó (vazio para `delete`).
3. Diferença: `removidos = antigos − novos` e `adicionados = novos − antigos`.
   Termos que continuam não geram escrita.
4. Para cada nó em `removidos`, registrar "remove id"; para cada um em
   `adicionados`, registrar "adiciona id".
5. Guardar o novo `fwd:<id>` no lote (ou marcar a remoção).

**Dentro de um mesmo lote**, um documento pode ser adicionado, editado e
removido. O lote guarda as operações por nó como `(doc, adiciona|remove,
seq)`, e no commit, para cada (nó, doc), **a última operação vence**. O
resultado por nó é um par `(adições, remoções)` disjunto.

No commit, o mesmo `WriteBatch` atômico leva:
- `put node:*` dos nós alterados (como hoje);
- `merge post:*` com os pares (adições, remoções);
- `put fwd:<id>` ou `delete fwd:<id>`;
- `put`/`delete doc:<id>` (rótulo);
- os contadores.

**Leitura extra na carga em massa:** com upsert, todo `add` faz um `Get` de
`fwd:<id>`. Em carga nova quase sempre dá miss, e o filtro de Bloom deixa isso
barato, mas é preciso medir. Se pesar, existe
`IndexBatch::add_new(doc)`, que pula a checagem (com `debug_assert!` de que o
ID não existe).

### 2.4 Merge operator com remoção

O operador atual é uma união associativa: a ordem dos operandos não importa,
porque só se adiciona. Com remoção ela importa ("adiciona 5" e depois "remove
5" ≠ "remove 5" e depois "adiciona 5"). Daí a troca para
`Options::set_merge_operator(nome, full_merge, partial_merge)`, em que o
RocksDB entrega os operandos **em ordem**.

**Formato dos valores em `post:*`:** o primeiro byte é uma tag.

| Tag | Conteúdo | Onde aparece |
|---|---|---|
| `0x00` BASE | lista de IDs, como o `encode_postings` de hoje | valor completo, resultado de um full merge |
| `0x01` DELTA | `adições` (lista) + `remoções` (lista), disjuntas | operando gravado pelo commit |

**Full merge** (valor existente + operandos em ordem):
```
conjunto = existente (BASE) ou vazio
para cada operando DELTA, em ordem:
    conjunto = (conjunto − remoções) ∪ adições
retorna BASE(conjunto)
```

**Partial merge** (só operandos, sem valor existente): dois deltas em
sequência se combinam num só, o que mantém a compactação barata:
```
(A1, R1) seguido de (A2, R2)  =  ( (A1 − R2) ∪ A2 ,  (R1 − A2) ∪ R2 )
```
O resultado continua disjunto e equivale a aplicar os dois em ordem.

Um operando corrompido faz o merge devolver `None`, e a leitura falha em vez
de mentir (como hoje).

**Listas vazias:** quando todos os documentos de um termo são removidos, a
lista fica `BASE([])`, cerca de 2 bytes. Ela não é apagada: um compaction
filter que apague chaves com merge pendente em níveis inferiores é arriscado,
e o espaço é pequeno.

### 2.5 Efeitos na Trie e no autocompletar

- A Trie **nunca encolhe**: nós de termos que sumiram ficam. Para compactá-la,
  reconstrua o índice.
- `suggest` passa a **pular termos com lista vazia**, ao custo de um `Get` por
  candidato. Como a lista começa pela contagem (varint), basta ler o primeiro
  número, sem decodificar o resto.
- `search` não muda: lista vazia já resulta em zero documentos.

### 2.6 Formato em disco

Ainda não há versão publicada, então **não há migração**: grava-se
`meta:format = 2`, e `Engine::open` recusa bancos sem essa chave (ou com
outro valor) usando um erro novo, `Error::IncompatibleFormat { found,
expected }`. Para atualizar, basta reindexar.

---

## Parte 3 — Leitura multi-core

O modelo continua o de hoje: o `Engine` fica compartilhado num `Arc`, cada
thread chama `search`/`suggest`, e cada chamada abre o próprio snapshot. Não
há pool de threads dentro da biblioteca; quem chama decide quantas threads
usar.

### 3.1 Block cache

Toda busca lê `node:0` e os primeiros níveis da Trie, então todas as threads
disputam as mesmas entradas do cache. O cache LRU usa locks por shard, e isso
pode limitar a escala.

```rust
pub enum CacheKind {
    Lru,          // atual
    HyperClock,   // Cache::new_hyper_clock_cache(capacidade, 0); 0 = estimativa automática
}
```

`Config::cache_kind`, com padrão `Lru` até a Fase 5 do plano mostrar qual
escala melhor.

### 3.2 Compactação vs. leitura

`background_jobs` (1.4) também é um parâmetro de leitura: menos jobs deixam
mais núcleos livres para as buscas, mas a compactação atrasa, e os operandos
de merge acumulados deixam os termos frequentes mais lentos. É o mesmo
parâmetro, otimizado para o p99 de leitura com o escritor ativo.

### 3.3 Raiz e topo da Trie decodificados (condicional)

**Só implementar se o profiling mostrar que `TrieNode::decode` dos primeiros
níveis pesa na busca.**

Ideia: o escritor publica, a cada commit, um `Arc<HotTrie>` com os nós até
uma profundidade pequena, já decodificados, e as buscas usam esses nós em vez
de ler do RocksDB.

Cuidado de consistência: a busca usa o snapshot `S` para as listas e o
`HotTrie` `H` para o topo, e os dois precisam ser do mesmo commit.

- Se `H` for mais novo que `S`, um filho existente em `H` pode não existir em
  `S`, e a busca falha com "nó ausente".
- Se `H` for mais velho, a busca não acha termos novos daquele commit.

Solução: um `RwLock<Arc<HotTrie>>`.
- O commit pega o lock de escrita, faz `db.write` e troca o `Arc`.
- A busca pega o lock de leitura, cria o snapshot e clona o `Arc`, e solta o
  lock em seguida.

O `db.write` sem sync só insere na memtable, então o lock de escrita dura
pouco. Mesmo assim, medir o p99 de leitura durante commits grandes.

### 3.4 Paralelismo dentro de uma busca (não agora)

Carregar em paralelo as listas de um AND com 3 ou mais termos, ou dividir a
expansão de um prefixo curto, só ajuda nos casos pesados e disputa núcleos com
as outras buscas. Fica para depois, se a Fase 4 mostrar que esses casos
dominam o p99.

---

## Ordem de implementação

Cada passo é um PR separado, com testes e com a medição que o justifica.

| # | Passo | Valida com |
|---|---|---|
| 1 | `Config`: `background_jobs`, `max_subcompactions`, `cache_kind`; ganchos de estatísticas (do plano de performance) | testes existentes passam; Fase 5 com variações |
| 2 | Cache de nós do escritor entre commits (1.3) | oráculo da Fase 1 idêntico; docs/s da Fase 2 antes e depois |
| 3 | `PreparedDoc` + `add_prepared` (1.1) | testes existentes passam sem mudança |
| 4 | `index_all` com workers (1.2) | banco idêntico com 1 e com N workers; docs/s vs. número de workers |
| 5 | Formato v2: merge com tag BASE/DELTA, ainda só adições (2.4, 2.6) | oráculo idêntico; testes de merge (abaixo) |
| 6 | `fwd:` + upsert + `delete_document` (2.2, 2.3) | testes de edição; tamanho do `fwd:` na Fase 2 |
| 7 | `suggest` pulando termos vazios (2.5) | teste: remover o único doc de um termo o tira do autocompletar |
| 8 | (condicional) `HotTrie` (3.3) | só se o profiling pedir; p99 antes e depois |

## Testes novos

**Merge operator** (unitários, com seed fixa):
- sequências aleatórias de deltas aplicadas de três formas: (a) uma a uma, em
  full merge; (b) com partial merges intermediários em pontos aleatórios; (c)
  num `HashSet` de referência. As três têm que dar o mesmo resultado;
- operando corrompido → leitura falha;
- `BASE([])` codifica e decodifica.

**Edição** (integração):
- add → update → leitura: termos antigos não acham mais o doc, os novos acham;
- add + delete no **mesmo lote** = nada gravado para aquele doc;
- delete de ID inexistente não faz nada;
- reabrir o banco no meio de edições mantém tudo.

**Wikipédia** (integra a Fase 1 do plano de performance):
- carregar a fatia de 10k; depois, num segundo passo, para metade dos
  documentos (sorteados com seed), trocar o texto pelo de outro artigo da
  fatia de 100k; remover 10% dos documentos;
- o oráculo aplica as mesmas operações num índice em memória, e o motor tem
  que dar resultados idênticos;
- `document_terms(id)` é igual ao conjunto de termos do oráculo para todo doc.

**Concorrência:**
- durante um lote de updates, threads leitoras buscam um termo que só existe
  no texto antigo e um que só existe no novo. Nenhum documento pode aparecer
  nos dois ao mesmo tempo nem em nenhum dos dois (o lote é atômico);
- `index_all` com 1 e com 8 workers sobre a fatia de 10k gera bancos com o
  mesmo conteúdo lógico (e os mesmos IDs de nó, por causa da ordem
  preservada).

## Perguntas em aberto

- `writer_cache_depth`: qual profundidade cobre os nós quentes sem gastar
  memória demais? Medir quantos nós há por nível na Wikipédia (`wiki sizes`).
- `fwd:` sempre ou opcional? Depende do tamanho medido.
- A raiz pode ter milhares de filhos (vários alfabetos). Vale um formato de nó
  diferente para nós grandes (por exemplo, filhos em vetor ordenado com busca
  binária, em vez de `BTreeMap`)? Decidir junto com o 3.3.
- `search` devolve todos os IDs (cerca de 1M para "de"). Uma API com `limit`
  faz parte deste trabalho ou é separada?
