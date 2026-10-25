# Como o fire-flower funciona

Este documento explica o projeto de ponta a ponta: o que é o RocksDB, como os
dados ficam guardados nele e o caminho que um texto percorre até virar um
resultado de busca. Os exemplos usam dados reais, extraídos de um banco criado
pelo próprio motor.

**Sumário**

1. [A ideia em uma página](#1-a-ideia-em-uma-página)
2. [O que é o RocksDB](#2-o-que-é-o-rocksdb)
3. [Conceitos de busca: índice invertido e Trie](#3-conceitos-de-busca-índice-invertido-e-trie)
4. [Como os dados ficam no RocksDB](#4-como-os-dados-ficam-no-rocksdb)
5. [Fluxo de indexação](#5-fluxo-de-indexação)
6. [Fluxo de busca](#6-fluxo-de-busca)
7. [Concorrência: um escritor, muitos leitores](#7-concorrência-um-escritor-muitos-leitores)
8. [A CLI interativa](#8-a-cli-interativa)
9. [Mapa do código](#9-mapa-do-código)
10. [Custos e limites](#10-custos-e-limites)
11. [Glossário](#11-glossário)

---

## 1. A ideia em uma página

O fire-flower responde a perguntas como "quais documentos contêm a palavra
*coração* e alguma palavra começando com *bras*?". Para responder rápido, ele
não lê os documentos na hora da busca. Ele monta antes um **índice**: para cada
palavra, a lista dos documentos onde ela aparece.

As palavras ficam organizadas numa **Trie** (árvore de prefixos), e tanto a
árvore quanto as listas ficam gravadas no **RocksDB**, um banco chave-valor que
roda dentro do próprio programa. Não há servidor: o índice é uma pasta no
disco.

```mermaid
flowchart LR
    T["Texto<br/>'O Coração do Brasil'"] --> P["1. Processamento<br/>text.rs<br/>tokenizar e normalizar"]
    P -->|"coracao, brasil, do, o"| I["2. Índice<br/>engine.rs<br/>Trie + listas"]
    I -->|"node:*, post:*"| R[("3. RocksDB<br/>pasta no disco")]
    Q["Consulta<br/>'coração bras*'"] --> P2["mesma normalização"]
    P2 --> I
    R --> I
    I --> D["Documentos<br/>[1, 7, 42]"]
```

Três camadas, cada uma com uma responsabilidade:

| Camada | Onde | Faz |
|---|---|---|
| Processamento de texto | `src/text.rs` | transforma texto livre em termos padronizados |
| Índice | `src/engine.rs` | Trie, listas de documentos, busca, autocompletar |
| Armazenamento | RocksDB + `src/codec.rs` | grava tudo em disco de forma durável e compacta |

---

## 2. O que é o RocksDB

### Em uma frase

O RocksDB é uma **biblioteca** (não um servidor) que guarda pares
**chave → valor** em disco, ordenados pela chave, e que é muito boa em
escrever rápido.

Foi criado pelo Facebook em 2012, a partir do LevelDB do Google, e hoje está
por baixo de sistemas como TiKV (o armazenamento do TiDB), MyRocks (MySQL) e
Kafka Streams. Para nós, ele é um
"`HashMap` que mora no disco, sobrevive a reinícios e não precisa caber na
RAM".

### O que ele oferece

| Operação | O que faz | Onde usamos |
|---|---|---|
| `put(chave, valor)` | grava ou sobrescreve | nós da Trie, rótulos, contadores |
| `get(chave)` | lê um valor | cada passo da travessia da Trie |
| `merge(chave, pedaço)` | anota um pedaço para combinar depois com o valor atual | listas de documentos |
| `WriteBatch` | agrupa várias escritas; ou entram todas, ou nenhuma | todo commit de indexação |
| `snapshot` | uma "foto" do banco num instante | toda busca |
| iterador | percorre as chaves em ordem | (ainda não usamos) |

Chaves e valores são **bytes** sem significado para o RocksDB. Dar estrutura a
eles (o que é um nó, o que é uma lista) é trabalho nosso.

### Como ele funciona por dentro (LSM-tree)

O RocksDB organiza os dados numa **LSM-tree** (*Log-Structured Merge tree*). A
ideia central é **nunca alterar um arquivo no lugar**: toda escrita vai para o
fim de algo, e a arrumação é feita depois, em segundo plano.

```mermaid
flowchart TB
    W["escrita<br/>put / merge / WriteBatch"] --> WAL["WAL<br/>log em disco, só anexa<br/>(recuperação após queda)"]
    W --> MEM["Memtable<br/>em RAM, ordenada"]
    MEM -->|"cheia (≈64 MB): flush"| L0["Nível 0<br/>arquivos SST"]
    L0 -->|compactação| L1["Nível 1<br/>SST, ~10× maior"]
    L1 -->|compactação| L2["Nível 2 …<br/>SST, ~10× maior"]
```

**Caminho da escrita:**

1. A escrita é anexada ao **WAL** (*write-ahead log*), um arquivo de log. Se
   o processo cair, o RocksDB relê o WAL ao abrir e não perde nada.
2. A escrita entra na **memtable**, uma estrutura ordenada em RAM.
3. Quando a memtable enche, ela vira um arquivo **SST** (*Sorted String
   Table*) no nível 0. Um SST é imutável: chaves ordenadas, divididas em
   blocos de ~4 KB, comprimidos (usamos LZ4).
4. Em segundo plano, a **compactação** junta SSTs de um nível com os do
   nível seguinte, descarta versões antigas e combina os `merge` pendentes.

Por isso escrever é barato: é só anexar ao log e inserir em RAM.

**Caminho da leitura (`get`):** o RocksDB procura do mais novo para o mais
velho e para no primeiro que tiver a chave.

```mermaid
flowchart LR
    G["get(chave)"] --> M{"memtable?"}
    M -->|não| F0{"filtro de Bloom<br/>dos SST do L0"}
    F0 -->|"talvez"| B0["lê bloco<br/>(block cache ou disco)"]
    F0 -->|"com certeza não"| F1{"filtro de Bloom<br/>do SST do L1"}
    B0 -->|não achou| F1
    F1 -->|...| N["próximos níveis"]
    M -->|sim| OK["valor"]
    B0 -->|achou| OK
```

Duas peças tornam esse caminho rápido, e as duas estão ligadas no nosso
`Engine::open`:

- **Filtro de Bloom.** Cada SST tem um resumo probabilístico das suas chaves
  que responde "com certeza não está aqui" ou "talvez esteja". Com 10 bits por
  chave, o "talvez" erra cerca de 1% das vezes. Na Trie isso importa: buscar
  uma palavra que não existe consulta chaves que não existem, e o filtro evita
  ler disco à toa.
- **Block cache.** Blocos lidos ficam em RAM (256 MB, política LRU). A raiz e
  os primeiros níveis da Trie são lidos por *toda* busca, então ficam sempre
  no cache e custam microssegundos.

### Três recursos que o projeto usa de forma especial

**WriteBatch (atomicidade).** Um commit de indexação grava centenas de nós e
listas. Se só metade entrasse, a Trie poderia apontar para um nó que não
existe. O `WriteBatch` vai para o WAL como um único registro: ou entra tudo, ou
nada.

**Merge operator (anexar sem ler).** A lista de documentos de "de" pode ter um
milhão de IDs. Sem merge, adicionar um documento exigiria ler a lista inteira,
acrescentar um ID e regravá-la. Com merge, gravamos só o pedaço novo
(`merge("post:1", [42])`) e registramos uma função que sabe unir pedaços (a
`merge_postings`). O RocksDB chama essa função quando alguém lê a chave e
durante a compactação, quando consolida os pedaços de vez.

**Snapshot (consistência de leitura).** Toda escrita recebe um número de
sequência crescente. Um snapshot "congela" um número: leituras feitas por ele
ignoram tudo que foi escrito depois. Uma busca faz dezenas de `get`s; com o
snapshot, todas enxergam o mesmo momento do índice, mesmo que um commit
aconteça no meio.

### Um detalhe que surpreende: a ordem das chaves

O RocksDB ordena as chaves **byte a byte**, como texto. Por isso `node:10`
vem antes de `node:2` (o caractere `1` é menor que `2`). Para nós isso não
importa, porque sempre acessamos os nós por chave exata. Mas é o que você vê ao
listar o banco (seção 4).

---

## 3. Conceitos de busca: índice invertido e Trie

### Índice invertido

Um documento "normal" vai de documento → palavras. O índice **inverte** isso:
palavra → documentos.

| Termo | Lista de postagem (IDs de documentos) |
|---|---|
| `azul` | 1, 2 |
| `carro` | 2 |
| `casa` | 1, 2 |

Buscar `casa carro` é pegar as duas listas e calcular a interseção: `[2]`.
A lista de documentos de um termo se chama **lista de postagem** (*postings
list*).

### Trie (árvore de prefixos)

Precisamos encontrar o termo `casa` para chegar à lista dele. Uma Trie guarda
as palavras caractere a caractere, e palavras com o mesmo começo
compartilham o mesmo caminho:

```mermaid
flowchart TB
    R(("raiz")) -->|c| C(("c"))
    C -->|a| CA(("ca"))
    CA -->|s| CAS(("cas"))
    CAS -->|a| CASA(["casa ✓"])
    CA -->|r| CAR(("car"))
    CAR -->|r| CARR(("carr"))
    CARR -->|o| CARRO(["carro ✓"])
```

O ✓ marca "fim de palavra": `cas` é só um caminho, `casa` é um termo indexado.

A vantagem sobre um simples dicionário de palavras é o **prefixo**: para
autocompletar `ca`, basta descer até o nó `ca` e listar tudo que está abaixo
dele. É isso que alimenta o `suggest` e as buscas `termo*`.

---

## 4. Como os dados ficam no RocksDB

### Os espaços de chave

Como o RocksDB só conhece chave → valor, separamos os tipos de dado por um
prefixo na chave:

| Chave | Valor | Exemplo |
|---|---|---|
| `node:<id>` | um nó da Trie: "é fim de palavra?" + filhos (`caractere → id`) | `node:0` é sempre a raiz |
| `post:<id>` | lista de postagem do termo que **termina** no nó `<id>` | `post:8` = documentos com `casa` |
| `doc:<id>` | rótulo do documento (a CLI grava o caminho do arquivo) | `doc:42` → `/home/.../notas.txt` |
| `meta:next_node_id` | próximo ID livre para nós | |
| `meta:next_doc_id` | próximo ID livre para documentos | |

A lista de postagem usa o mesmo número do nó onde a palavra termina. Assim
não precisamos de um segundo contador nem de guardar o ID da lista dentro do
nó.

### Um banco real, byte a byte

Indexando dois documentos:

```text
documento 1: "A casa azul"
documento 2: "O carro azul e a Casa"
```

O banco resultante tem exatamente estas chaves (valores em hexadecimal, na
ordem em que o RocksDB as guarda):

```text
meta:next_doc_id       03
meta:next_node_id      0e
node:0                 00 04 61 01 63 05 65 0c 6f 0d
node:1                 01 01 7a 02
node:10                00 01 6f 0b
node:11                01 00
node:12                01 00
node:13                01 00
node:2                 00 01 75 03
node:3                 00 01 6c 04
node:4                 01 00
node:5                 00 01 61 06
node:6                 00 02 72 09 73 07
node:7                 00 01 61 08
node:8                 01 00
node:9                 00 01 72 0a
post:1                 02 01 01
post:11                01 02
post:12                01 02
post:13                01 02
post:4                 02 01 01
post:8                 02 01 01
```

A Trie que esses bytes descrevem:

```mermaid
flowchart TB
    N0(("node:0<br/>raiz")) -->|a| N1(["node:1<br/>a ✓"])
    N0 -->|c| N5(("node:5<br/>c"))
    N0 -->|e| N12(["node:12<br/>e ✓"])
    N0 -->|o| N13(["node:13<br/>o ✓"])
    N1 -->|z| N2(("node:2<br/>az"))
    N2 -->|u| N3(("node:3<br/>azu"))
    N3 -->|l| N4(["node:4<br/>azul ✓"])
    N5 -->|a| N6(("node:6<br/>ca"))
    N6 -->|r| N9(("node:9<br/>car"))
    N6 -->|s| N7(("node:7<br/>cas"))
    N7 -->|a| N8(["node:8<br/>casa ✓"])
    N9 -->|r| N10(("node:10<br/>carr"))
    N10 -->|o| N11(["node:11<br/>carro ✓"])
```

Repare:

- `Casa` e `casa` viraram o mesmo termo (normalização, seção 5.1).
- `a` é ao mesmo tempo uma palavra (✓) e o começo de `azul`.
- `casa` e `carro` compartilham os nós `c` e `ca`.

### Lendo um nó: `node:6`

```text
00          flags: 0 = não é fim de palavra
02          2 filhos
72 09       'r' (0x72) → node:9
73 07       's' (0x73) → node:7
```

Os filhos ficam em ordem de caractere (é um `BTreeMap`), o que deixa o
autocompletar em ordem alfabética sem esforço extra.

### Lendo uma lista: `post:8` (o termo `casa`)

```text
02          2 documentos
01          primeiro ID: 0 + 1 = 1
01          segundo ID:  1 + 1 = 2
```

Duas técnicas deixam isso pequeno:

- **Varint:** um número ocupa só os bytes de que precisa. Cada byte carrega 7
  bits do número; o bit mais alto diz "tem mais um byte". `1` ocupa 1 byte,
  `300` ocupa 2 (`ac 02`), e o máximo de 64 bits ocupa 10.
- **Deltas:** a lista é ordenada, então guardamos a *diferença* para o ID
  anterior. `[1000, 1001, 1005]` vira `1000, 1, 4`: 2 + 1 + 1 bytes em vez de
  24. Por isso IDs próximos uns dos outros deixam o índice menor.

Os contadores também usam varint: `meta:next_node_id = 0e` significa 14 (os
nós 0 a 13 existem).

---

## 5. Fluxo de indexação

Visão geral do que acontece em `fire-flower index <banco> <pasta>`:

```mermaid
sequenceDiagram
    participant CLI as main.rs (index)
    participant B as IndexBatch
    participant T as text.rs
    participant DB as RocksDB

    CLI->>CLI: coleta arquivos, ignora ocultos e binários
    CLI->>B: engine.batch() (trava o escritor)
    loop cada arquivo
        CLI->>T: strip_html (se .html)
        CLI->>B: add_labeled(caminho, texto)
        B->>T: index_terms(texto)
        T-->>B: termos normalizados, sem stopwords
        B->>B: anota doc_id em pending[termo]
        alt 1000 documentos no lote
            loop cada termo distinto do lote, em ordem
                B->>DB: get node:ID (só se o nó não estiver no cache)
                B->>B: cria nós que faltam, marca fim de palavra
            end
            B->>DB: commit: um WriteBatch
            CLI->>B: engine.batch() de novo (o cache de nós continua)
        end
    end
    B->>DB: commit final
```

### 5.1 Processamento de texto (`text::tokenize`)

O objetivo é que `Ação`, `ACAO` e `acao` virem o mesmo termo. Para cada
caractere:

1. **Decomposição NFKD.** Caracteres acentuados viram letra-base + acento
   separado: `ç` → `c` + `◌̧`, `ã` → `a` + `◌̃`. A forma "K" também desfaz
   variantes tipográficas: a ligadura `ﬁ` vira `fi`, e o `１` de largura total
   vira `1`.
2. **Descarta os acentos soltos** (marcas combinantes).
3. **Letras e dígitos** (de qualquer alfabeto) entram no termo atual, em
   minúsculas.
4. **Qualquer outra coisa** (espaço, pontuação, `-`, `_`) encerra o termo.
5. Termos com mais de 64 caracteres são descartados (hashes, base64, lixo)
   para não criar caminhos enormes e inúteis na Trie.

```text
"Ação, CORAÇÃO! São-Paulo e-mail"
 → ["acao", "coracao", "sao", "paulo", "e", "mail"]
```

Para HTML, `strip_html` roda antes: remove as tags (cada tag vira um espaço,
senão `<p>a</p><p>b</p>` viraria `ab`), descarta `<script>`/`<style>`
inteiros e decodifica entidades comuns (`&amp;`, `&nbsp;`…).

A **mesma** função é usada na busca. Se as duas normalizações divergissem, os
termos buscados nunca bateriam com os indexados.

**Stopwords.** Depois de normalizar, `index_terms` descarta as palavras
comuns em português e inglês (listas `STOPWORDS_PT` e `STOPWORDS_EN`, baseadas
no Snowball/NLTK). Elas aparecem em quase todo documento, então só inchariam o
índice: na Wikipédia, "de" está em 99% dos artigos. Na busca, `parse_query`
descarta as mesmas palavras quando exatas, senão `casa de papel` exigiria um
termo que não existe e voltaria vazia. Como prefixo (`de*`) elas continuam
valendo. "são" e "era" ficam fora da lista de propósito: são também nomes
(São Paulo, Era Vargas).

### 5.2 Lote invertido

`add_document` não toca na Trie. Ele tira os termos repetidos do documento (um
documento que diz "café" dez vezes entra uma vez só na lista de `cafe`) e
anota o `doc_id` em `pending[termo]`. No commit, cada termo distinto do lote
é inserido **uma vez**, em ordem alfabética. Num lote de 1.000 artigos, "brasil"
aparece em centenas deles, mas a Trie é percorrida uma vez só para ele.

A ordem alfabética deixa a criação de nós determinística: o mesmo corpus com
o mesmo tamanho de lote gera sempre os mesmos IDs. Com outro tamanho de lote
os IDs mudam, mas o conteúdo (termo → documentos) é o mesmo.

### 5.3 Inserção na Trie (`IndexBatch::insert_term`)

Para o termo `carro`, começando em `node:0`:

1. Pega o nó atual com `node_mut`. Se ele já está no cache (lido ou criado
   antes, neste lote ou num anterior), usa a cópia em memória; senão, lê do
   RocksDB e decodifica.
2. Procura o próximo caractere nos filhos.
   - **Existe:** desce para o filho.
   - **Não existe:** reserva o próximo ID (`next_node_id`), cria um nó vazio,
     registra o filho no pai e marca os dois como modificados (*dirty*).
3. No último caractere, marca `is_end_of_word = true`.
4. Grava a lista de documentos do termo em `post:<id do nó final>`.

**Por que o lote guarda nós em memória?** Nada é gravado antes do commit. Se o
lote tem `carro` e `carta`, a inserção de `carta` precisa enxergar os nós
`c`, `ca`, `car` que `carro` acabou de criar, e eles ainda não existem no
banco. O mapa `nodes` do lote é essa visão "banco + mudanças pendentes".

**Cache entre lotes.** Depois do commit, os nós em memória são iguais aos do
banco, e só o escritor altera a Trie. Então o mapa volta para o `WriterState`
e o próximo lote começa com ele: a raiz e os nós dos termos comuns não são
relidos a cada commit. Um lote descartado (ou com commit que falhou) leva o
cache junto, e o próximo começa do banco. O cache tem um teto
(`Config::writer_cache_nodes`, 1 milhão de nós, ~200 MB): passando dele, saem
primeiro os nós que o último lote não usou e depois os mais fundos.

### 5.4 Commit (`IndexBatch::commit`)

Um único `WriteBatch` com:

| O quê | Operação | Observação |
|---|---|---|
| cada nó *dirty* | `put node:<id>` | nós só lidos não são regravados |
| cada lista nova | `merge post:<id>` | só os IDs novos, ordenados e em deltas |
| cada rótulo | `put doc:<id>` | |
| contadores | `put meta:*` | |

O `db.write(batch)` é atômico. Só depois dele os contadores em memória
(`WriterState`) avançam. Se o lote for descartado sem commit, ou o commit
falhar, nada muda: nem o banco, nem os contadores.

A CLI faz commit a cada 1.000 documentos para limitar a memória: o lote guarda
os termos de todos os documentos até o commit.

### 5.5 O que o RocksDB faz depois

O `WriteBatch` vai para o WAL e para a memtable. Mais tarde, em segundo plano:

- a memtable vira SST (*flush*);
- a compactação chama `merge_postings` para unir os pedaços de cada lista
  (`[1, 2]` + `[5]` + `[9]` → `[1, 2, 5, 9]`) e grava a lista já consolidada.

Enquanto a compactação não roda, uma leitura de `post:<id>` une os pedaços na
hora. Por isso listas muito disputadas podem ficar mais lentas logo depois de
uma carga grande; o plano de performance mede isso.

---

## 6. Fluxo de busca

### 6.1 Termo exato: `casa`

```mermaid
sequenceDiagram
    participant U as Engine::search("casa")
    participant V as View (snapshot)
    participant DB as RocksDB

    U->>V: view() cria o snapshot
    V->>V: tokenize("casa") → ["casa"]
    V->>DB: get node:0 (raiz)
    DB-->>V: filhos: a→1, c→5, e→12, o→13
    V->>DB: get node:5 ('c')
    V->>DB: get node:6 ('a')
    V->>DB: get node:7 ('s')
    V->>DB: get node:8 ('a')
    DB-->>V: fim de palavra ✓
    V->>DB: get post:8
    DB-->>V: [1, 2]
```

Custo: **1 `get` por caractere + 2** (raiz e lista). Se algum caractere não
existir nos filhos, a busca termina ali com zero resultados. Se o caminho
existir mas o nó final não for fim de palavra (`cas`), também dá zero:
prefixo de palavra não é palavra.

### 6.2 Vários termos: `casa carro`

1. Busca a lista de cada termo, como acima.
2. **Lista vazia em qualquer termo:** retorna vazio na hora, sem buscar os
   outros.
3. Ordena as listas da menor para a maior e faz a interseção nessa ordem. A
   interseção de duas listas ordenadas anda com dois ponteiros, em tempo
   linear. Começar pela menor faz o resultado parcial já nascer pequeno.

```text
casa  → [1, 2]
carro → [2]
interseção → [2]
```

Uma palavra com hífen vira vários termos, como na indexação: `e-mail` busca
`e` **e** `mail`.

### 6.3 Prefixo: `ca*`

1. Desce até o nó `ca` (`node:6`).
2. Faz uma busca em largura a partir dele e coleta os nós com ✓: `casa`,
   `carro`. Para no máximo em `max_prefix_terms` (256) termos.
3. Une as listas de todos esses termos (ordena e remove repetidos).
4. O resultado entra na interseção como se fosse a lista de um termo comum.

**Atenção:** prefixos muito curtos (`a*` na Wikipédia) casam milhares de
termos, e os que passam de 256 são ignorados. O resultado fica incompleto, sem
aviso. É um limite conhecido.

### 6.4 Autocompletar: `suggest("ca", 10)`

Mesma busca em largura do prefixo, devolvendo as palavras em vez das listas.
A busca em largura visita primeiro os nós mais rasos, e os filhos estão em
ordem alfabética. Por isso as sugestões saem **das mais curtas para as mais
longas e, no mesmo comprimento, em ordem alfabética**: para `ca`, `casa` (4
letras) vem antes de `carro` (5).

As sugestões vêm **normalizadas** (`coracao`, não `coração`), porque é assim
que os termos estão na Trie.

---

## 7. Concorrência: um escritor, muitos leitores

`Engine` pode ser compartilhado entre threads (`Arc<Engine>`).

```mermaid
flowchart LR
    subgraph Escrita
        B1["engine.batch()"] -->|"trava o Mutex"| W["IndexBatch<br/>(só um por vez)"]
        W -->|"commit atômico"| DB[("RocksDB")]
    end
    subgraph Leitura
        S1["search() thread 1"] -->|snapshot| DB
        S2["search() thread 2"] -->|snapshot| DB
        S3["suggest() thread 3"] -->|snapshot| DB
    end
```

- **Escrita:** `engine.batch()` pega um `Mutex`. Um segundo `batch()` espera
  o primeiro terminar, inclusive na mesma thread: abrir um lote com outro
  aberto trava o programa.
- **Leitura:** buscas não pegam trava nenhuma. Cada uma cria um snapshot e lê
  por ele.
- **Por que nunca se vê um lote pela metade:** o commit é um único
  `WriteBatch`, e a busca lê de um único snapshot. Ou o snapshot é anterior
  ao commit (não vê nada do lote), ou é posterior (vê tudo).
- **Por que um nó nunca aponta para o vazio:** nós só ganham filhos, nunca os
  perdem, e um nó novo é gravado no mesmo `WriteBatch` que o pai que aponta
  para ele.

---

## 8. A CLI interativa

`fire-flower tui <banco>` (código em `src/tui.rs`, interface feita com
[ratatui](https://ratatui.rs)).

```text
 ✿ fire-flower   /tmp/db                              1 resultado  ·  149.25µs
╭ Busca ───────────────────────────────────────────────────────────────────────╮
│› coração bravo                                                               │
╰──────────────────────────────────────────────────────────────────────────────╯
 consulta: coração bra*
╭ Sugestões ─────────────╮╭ Resultados ────────────────────────────────────────╮
│▸ bravo                 ││▸   1 /tmp/brasil.txt                               │
│  brasil                ││                                                    │
│  brasilia              ││                                                    │
╰────────────────────────╯╰────────────────────────────────────────────────────╯
╭ Prévia ──────────────────────────────────────────────────────────────────────╮
│ 2 │ O coração do Brasil bate forte.                                          │
│ 3 │ Brasília é a capital.                                                    │
╰──────────────────────────────────────────────────────────────────────────────╯
 Tab completar   ^N/^P trocar sugestão   ↑↓ resultado   Enter abrir   Esc sair
```

O laço de eventos:

```mermaid
flowchart TB
    D["desenha a tela"] --> E["espera uma tecla"]
    E --> DR["drena as teclas que já chegaram"]
    DR --> C{"a consulta mudou?"}
    C -->|não| D
    C -->|sim| RF["refresh()"]
    RF --> D
```

O que o `refresh()` faz a cada mudança:

1. **Palavra atual:** se a consulta não termina em espaço, a última palavra
   ainda está sendo digitada.
2. **Consulta efetiva:** essa palavra ganha um `*`. Digitar `coração bra`
   busca `coração bra*`, e os resultados aparecem antes de a palavra
   terminar. A consulta efetiva aparece sob a caixa de busca.
3. **Sugestões:** `suggest(palavra atual, 8)`. A selecionada aparece em cinza
   depois do cursor; `Tab` troca a palavra por ela e adiciona um espaço.
4. **Busca:** `search(consulta efetiva)`, cronometrada para o cabeçalho.

Detalhes que mantêm a interface rápida:

- **Drenar teclas:** quem digita rápido ou cola um texto dispara uma busca só,
  não uma por tecla.
- **Rótulos sob demanda:** com um milhão de resultados, só os rótulos das
  linhas visíveis são lidos do banco (e guardados num cache).
- **Prévia:** lê o arquivo do rótulo (até 2 MB) e destaca as palavras que
  casam com a consulta, usando a mesma normalização. Documentos cujo rótulo
  não é um arquivo, como os artigos da Wikipédia que guardarão o título,
  ficam sem prévia.

---

## 9. Mapa do código

| Arquivo | Peças principais |
|---|---|
| `src/lib.rs` | API pública: `Engine`, `Config`, `IndexBatch`, `Error`, `DocId` |
| `src/text.rs` | `tokenize`, `index_terms`, `parse_query`, stopwords, `normalize_term`, `strip_html` |
| `src/codec.rs` | `TrieNode` (`encode`/`decode`), `encode_postings`/`decode_postings`, varint |
| `src/engine.rs` | `Engine::open` (opções do RocksDB), `batch`, `search`, `suggest`, `label`; `View` (leituras sobre snapshot); `IndexBatch` (`add_document`, `insert_term`, `node_mut`, `commit`); `merge_postings`, `intersect` |
| `src/main.rs` | subcomandos `tui`, `index`, `search`, `suggest` |
| `src/tui.rs` | a CLI interativa |
| `tests/engine.rs` | testes de ponta a ponta: persistência, lotes descartados, concorrência… |

API mínima:

```rust
use fire_flower::{Config, Engine};

let engine = Engine::open("/tmp/indice", Config::default())?;

let mut batch = engine.batch();
batch.add_document(1, "O coração do Brasil")?;
batch.commit()?;

engine.search("coração bras*")?;   // Vec<DocId>
engine.suggest("cor", 10)?;        // Vec<String>
```

---

## 10. Custos e limites

| Operação | Custo | Observação |
|---|---|---|
| Buscar termo de *L* letras | *L* + 2 `get`s | com o cache quente, microssegundos |
| Termo inexistente | até *L* + 1 `get`s | para no primeiro caractere que falta |
| AND de *n* termos | *n* buscas + interseções | retorna cedo se algum termo não existe |
| Prefixo | 1 `get` por nó visitado + 1 por lista | limitado a 256 termos |
| Indexar um lote | tokenizar + 1 caminho na Trie por termo distinto do lote | nós no cache do escritor não são relidos |
| Commit | 1 `WriteBatch` | escrita sequencial no WAL |

**Ainda não existe:**

- remover ou atualizar documentos (reindexar o mesmo ID só acrescenta termos);
- ranking por relevância (os resultados saem em ordem de ID);
- busca por frase, OU, stopwords e stemming ("casa" ≠ "casas");
- `fsync` a cada commit: se o *sistema operacional* cair (não só o processo),
  os últimos commits podem se perder. O padrão do RocksDB é não sincronizar a
  cada escrita.

**Possíveis evoluções** (a medir antes, ver
[plano-performance.md](plano-performance.md)):

- um índice ordenado `term:<palavra>` com o iterador do RocksDB faria a busca
  exata em 1 `get` e o prefixo num único scan;
- manter a raiz decodificada em memória, já que toda busca passa por ela;
- uma API com `limit`, para não devolver um milhão de IDs quando só dez serão
  mostrados.

---

## 11. Glossário

| Termo | Significado |
|---|---|
| **Termo** | uma palavra já normalizada (`coracao`) |
| **Token** | um pedaço de texto separado pela tokenização; aqui, sinônimo de termo |
| **Índice invertido** | mapa termo → documentos |
| **Lista de postagem** | a lista de documentos de um termo |
| **Trie** | árvore em que cada aresta é um caractere; palavras com o mesmo começo compartilham caminho |
| **Nó terminal** | nó em que uma palavra termina (✓); a lista dela fica em `post:<id do nó>` |
| **Normalização** | deixar o texto num formato padrão: sem acento, minúsculo |
| **NFKD** | forma Unicode que separa letra e acento e desfaz variantes tipográficas |
| **Varint** | número gravado com só os bytes necessários |
| **Delta** | diferença entre um ID e o anterior na lista ordenada |
| **LSM-tree** | estrutura do RocksDB: escreve anexando, arruma depois |
| **WAL** | log de escrita do RocksDB, usado para recuperação após queda |
| **Memtable** | buffer ordenado em RAM onde as escritas entram primeiro |
| **SST** | arquivo imutável e ordenado no disco |
| **Compactação** | processo de fundo que junta SSTs e consolida merges |
| **Filtro de Bloom** | resumo que responde "com certeza não está" ou "talvez esteja" |
| **Block cache** | cache em RAM dos blocos lidos dos SSTs |
| **WriteBatch** | conjunto de escritas aplicado de forma atômica |
| **Merge operator** | função nossa que o RocksDB usa para unir pedaços de um valor |
| **Snapshot** | visão do banco congelada num instante |
| **Lote (`IndexBatch`)** | conjunto de documentos indexados juntos e gravados num único commit |
