# fire-flower

Motor de busca full-text embutido: os termos ficam numa **Trie persistida no
RocksDB**, um registro por nó. Sem servidor externo — é uma biblioteca Rust
(mais um binário de linha de comando para testar).

Suporta:

- busca exata, normalizada (`Ação` = `acao` = `ACAO`);
- stopwords por idioma (português e inglês por enquanto: `de`, `o`, `the`…)
  ficam fora do índice e são ignoradas na consulta (`casa de papel` busca
  `casa` E `papel`). Os idiomas são escolhidos na criação do banco
  (`--idiomas pt,en`, o padrão) e ficam gravados nele;
- várias palavras combinadas com **E** (`roupa rei`);
- busca por prefixo (`program*`);
- **plural e flexões** opcionais: `--stemming pt-plural` ("planetas" acha
  o artigo "Planeta", quase sem custo), `--flexoes pt-plural` (o mesmo, com a
  palavra exata valendo mais que a flexão, mas buscas ~2× mais lentas) e
  `--stemming pt` (Snowball). São escolhidos na criação do banco e ficam
  gravados nele;
- busca por **frase** (`"casa de papel"`): os termos em sequência, com as
  stopwords contando como buraco;
- **ranking BM25F**: os resultados saem por relevância (`search_top`), com o
  título como campo de peso próprio (`add_titled`; `k1 = 1,2`, `b = 0,75`,
  peso do título 50); `search` continua devolvendo todos os IDs em ordem;
- autocompletar (`suggest`), das palavras mais curtas para as mais longas;
- indexação em lotes atômicos, com buscas rodando em paralelo.

## Uso

### Linha de comando

```bash
cargo build --release -j 4
```

Indexar arquivos e pastas (recursivo; ignora arquivos ocultos e binários;
`.html`/`.htm` têm as tags removidas):

```bash
./target/release/fire-flower index /tmp/indice ~/documentos
```

Busca interativa, com resultados enquanto você digita:

```bash
./target/release/fire-flower tui /tmp/indice
```

| Tecla | Ação |
|---|---|
| digitar | busca na hora; a última palavra vale como prefixo até você dar espaço (dentro de aspas, não: frase não tem prefixo) |
| `Tab` ou `→` | completa a palavra com a sugestão (em cinza depois do cursor) |
| `Ctrl+N` / `Ctrl+P` | troca a sugestão |
| `↑` `↓` `PgUp` `PgDn` | escolhe o resultado |
| `Enter` | abre o documento inteiro (`n`/`N` pulam entre ocorrências) |
| `Ctrl+W` / `Ctrl+U` | apaga a última palavra / a consulta toda |
| `Esc` | volta / sai |

Os resultados vêm por relevância (BM25), com a nota ao lado do título, e a
lista mostra os 1.000 melhores; o cabeçalho mostra o total. A linha
"consulta:" mostra cada parte da consulta: uma frase aparece entre aspas, com
`_` no lugar das stopwords (`"copa _ mundo"`).

A prévia lê o arquivo apontado pelo rótulo do documento. Documentos cujo
rótulo não é um arquivo (ex.: título de artigo) aparecem sem prévia.

### Wikipédia em português

Carga completa (~1,1 milhão de artigos), com painel de progresso no terminal:

```bash
python3 scripts/carregar_wikipedia.py
```

Por padrão lê `~/datasets/ptwiki/ptwiki.jsonl` e grava em
`~/bench-db/ptwiki-full`, apagando o banco anterior (pede confirmação). O
painel mostra artigos/s, MB/s, tempo restante, memória e tamanho do banco.
`Ctrl+C` interrompe e mostra o comando `--pular N` para continuar de onde
parou. No fim, o banco é compactado (~10 s; deixa as buscas 30–50% mais
rápidas) e um resumo vai para `~/bench-db/ptwiki-full.carga.json`. A carga
inteira leva ~2 minutos, com lotes de 10 mil artigos e pico de ~1,4 GB de
memória. Veja `--help` para `--corpus`, `--banco`, `--lote` e
`--sem-compactar`.

Por baixo, o script chama:

```bash
./target/release/fire-flower index-jsonl ~/bench-db/ptwiki-full ~/datasets/ptwiki/ptwiki.jsonl
```

Cada linha do JSONL é `{"id", "title", "text"}`: indexa o texto e o título
(como campo próprio, que pesa mais no ranking) sob o `id`, e guarda o título
como rótulo. No `index` de arquivos, o título é o nome do arquivo sem a
extensão.

Por padrão o banco grava as **posições** de cada termo, que a busca por frase
usa. Com `--sem-posicoes` (em `index`, `index-jsonl` e
`scripts/carregar_wikipedia.py`) o banco fica menor e a carga mais rápida, mas
frases dão erro. A escolha fica gravada no banco, como os idiomas.

Para achar outras formas da palavra há duas opções (nas mesmas opções de
criação; ver [docs/plano-stemming.md](docs/plano-stemming.md)):

- `--stemming pt-plural` (a recomendada): plural e singular viram o mesmo
  termo no índice ("planetas" acha "Planeta", "eleicoes" acha "eleição").
  Na Wikipédia inteira, a carga e o tamanho ficam iguais, termos e E custam
  +2–10% e frases +17–35%; a ordenação perde pouco (o artigo "Torre" pode
  ficar atrás de "Torres").
- `--flexoes pt-plural`: o índice guarda as palavras como estão e um mapa
  radical → palavras; a busca soma à palavra exata as outras do mesmo
  radical, com peso menor (`Config::inflection_weight`, 0,5). A ordenação fica
  como sem stemming e frases, prefixos e autocompletar ficam exatos, mas
  termos e E ficam ~2× mais lentos.
- `--stemming pt` (Snowball): junta também verbos e derivados ("correndo" e
  "corrida" viram `corr`), mas piora bem a ordenação.

Nos dois, o radical sai da palavra sem acentos ("informacoes" acha
"informações"). Também ficam gravados no banco: mudar exige recarregar.

### Busca sem interface

Busca e autocompletar sem interface, para scripts:

```bash
./target/release/fire-flower search /tmp/indice "coração bras*"
```

A saída é `nota  id  rótulo`, da nota maior para a menor. `--por-id` lista
todos os documentos em ordem de ID, sem ranking. Frases vão entre aspas
simples no shell:

```bash
./target/release/fire-flower search /tmp/indice '"revolução francesa" 1789'
```

```bash
./target/release/fire-flower suggest /tmp/indice prog 10
```

### Biblioteca

```rust
use fire_flower::{Config, Engine};

let engine = Engine::open("/tmp/indice", Config::default())?;

let mut batch = engine.batch();
batch.add_document(1, "O coração do Brasil")?;
batch.add_document(2, "Coração de estudante")?;
batch.commit()?; // um único WriteBatch atômico

assert_eq!(engine.search("CORACAO")?, [1, 2]);
assert_eq!(engine.search("coração bras*")?, [1]);
assert_eq!(engine.search("\"coração do brasil\"")?, [1]);
assert_eq!(engine.suggest("es", 5)?, ["estudante"]);

// Os 10 mais relevantes (BM25): Vec<Hit { doc, score }>.
let top = engine.search_top("coração", 10)?;
// Com o total de documentos que casaram.
let ranked = engine.search_ranked("coração", 10)?; // TopHits { hits, total }
```

`Engine` é `Sync`: compartilhe com `Arc<Engine>`. Só um lote de indexação
existe por vez (os outros esperam); buscas nunca esperam e nunca veem um lote
pela metade.

## Arquitetura

Explicação completa, com o que é o RocksDB, um banco real byte a byte e os
fluxos de indexação e busca: [docs/como-funciona.md](docs/como-funciona.md).

| Camada | Arquivo | Responsabilidade |
|---|---|---|
| Interface | `src/main.rs` + `src/tui.rs` | subcomandos (clap) e busca interativa (ratatui) |
| Texto | `src/text.rs` | remove HTML, acentos (NFKD), pontuação; minúsculas; tokens > 64 caracteres são descartados |
| Índice (Trie) | `src/engine.rs` | travessia e criação de nós, busca, prefixo, autocompletar |
| Armazenamento | `src/engine.rs` + `src/codec.rs` | chaves no RocksDB e serialização binária |

### Chaves no RocksDB

Formato v5 (`meta:format = 5`). Um banco de formato anterior não abre: o
`open` diz em que formato ele está e pede para recarregar.

| Chave | Valor |
|---|---|
| `node:<id>` | nó da Trie: flag de fim de palavra + filhos (`caractere → id`). `node:0` é a raiz |
| `post:<id>` | lista de postagem do termo que termina no nó `<id>`: `(Δdoc, tf << 1 \| tem título, [tf no título])` por documento |
| `pos:<id>` | posições do termo em cada documento: `(Δdoc, k, k × Δposição)` (só com posições); o título vem primeiro e o texto começa 100 posições depois |
| `norm:<bloco>` | tamanho (tokens indexados) de cada documento com `id ÷ 1024 = bloco`, e o do título |
| `doc:<id>` | rótulo opcional do documento (o CLI grava o caminho do arquivo) |
| `meta:next_node_id`, `meta:next_doc_id` | contadores de ID |
| `meta:doc_count`, `meta:total_len`, `meta:total_title_len` | total de documentos, soma dos tamanhos e dos títulos (BM25) |
| `stem:<radical>` | nós das palavras com esse radical (só com `--flexoes`) |
| `meta:format`, `meta:positions`, `meta:languages`, `meta:stemming`, `meta:inflections` | formato, se grava posições, idiomas das stopwords, stemming e flexões |

Os valores usam varint; as listas gravam a diferença entre IDs consecutivos,
então documentos com IDs próximos custam ~1 byte cada. `post:` e `pos:` ficam
em chaves separadas para que a busca comum não leia posições. Listas com mais
de 256 documentos têm, antes das entradas, uma tabela de saltos (último
documento, tamanho e quantidade de entradas de cada bloco, 128 na gravação):
o E e as frases vão direto ao bloco de um documento, sem decodificar a lista
inteira, e o merge só concatena as tabelas dos pedaços.

### Configuração do RocksDB

- filtro de Bloom, 10 bits por chave (`Config::bloom_bits_per_key`);
- block cache LRU de 256 MiB (`Config::block_cache_bytes`), com índices e
  filtros dentro do cache e os do nível 0 fixados nele;
- filtros e índices **particionados** em blocos de 4 KB, com o nível de topo
  fixado no cache. Sem isso, na Wikipédia inteira, um SST gerado pela
  compactação tinha um filtro de ~6 MB num bloco só, maior que uma parte do
  cache (o LRU se divide em 64 partes de 4 MB): cada leitura relia o filtro
  inteiro, e a carga caía de ~5.000 para ~200 artigos/s;
- compressão LZ4.

## Performance

O plano de medição está em [docs/plano-performance.md](docs/plano-performance.md)
e os resultados ficam num repositório separado,
[fire-flower-bench](https://github.com/evertondays/fire-flower-bench), que o `perf.py` espera clonado ao
lado deste (`../fire-flower-bench`, ou `FF_BENCH_DIR`); os mais recentes estão em
[RESULTADOS.md](https://github.com/evertondays/fire-flower-bench/blob/main/RESULTADOS.md).

```bash
python3 scripts/perf.py tudo --escala 10k
```

## Diferenças em relação ao documento de design original

1. **Listas de postagem com merge operator.** O documento faz ler → alterar →
   regravar a lista inteira a cada documento. Para palavras comuns ("de", "o")
   isso fica cada vez mais caro. Aqui cada lote grava só os IDs novos com
   `merge`, e o RocksDB une os pedaços na leitura e na compactação.
2. **Sem `PostingsListID` separado.** A lista de um termo fica em `post:<id do
   nó terminal>`; não é preciso um segundo contador nem guardar o ID no nó.
3. **Filhos guardam IDs numéricos** (um vetor ordenado de `(char, u64)`), não
   strings `"node:124"`. A ordem deixa o autocompletar em ordem alfabética.
4. **Lote invertido, com cache de nós.** O lote junta termo → documentos e só
   percorre a Trie no commit, uma vez por termo distinto (e não uma vez por
   documento). Os nós lidos ou criados ficam num cache do escritor que passa de
   um lote para o outro (`Config::writer_cache_nodes`, 3 milhões de nós por
   padrão; ficam os usados mais recentemente), então os nós dos termos comuns
   não são relidos a cada commit.
5. **Serialização binária própria** em vez de Gob/JSON (menor e sem
   dependências).

## Limitações conhecidas

- **Sem remoção ou atualização de documentos.** Reindexar o mesmo ID troca o
  tf, as posições e o tamanho dele (e não conta o documento duas vezes), mas
  termos que saíram do texto continuam apontando para ele.
- **Peso do título ajustado para a Wikipédia.** O peso 50 saiu de consultas
  com resposta conhecida na Wikipédia, onde o título descreve o artigo; em
  outro tipo de corpus, ajuste `Config::title_weight`. Páginas de
  desambiguação podem passar à frente do artigo principal.
- **Frases:** uma stopword dentro da frase só conta a distância (`"casa de
  papel"` também casa "casa da papel"); frase só de stopwords fica vazia; não
  há prefixo dentro de frase (`"revolução franc*"`).
- **Posições custam espaço.** Na fatia de 10k da Wikipédia, o banco
  compactado tem ~51 MB com posições e ~21 MB sem; use `--sem-posicoes` se
  não precisar de frases.
- **Uma leitura por caractere.** Buscar uma palavra de 10 letras faz 7 `Get`s
  (a raiz e os quatro níveis seguintes vêm do cache de nós das buscas,
  `Config::reader_cache_depth`). Com o block cache quente isso fica em
  microssegundos. Os prefixos curtos são a consulta mais cara (~3 ms na
  Wikipédia inteira): visitam centenas de nós e unem até 256 listas.
- **Prefixos muito curtos** (`a*`) expandem no máximo
  `Config::max_prefix_terms` termos (256); os demais são ignorados. No
  ranking, o prefixo conta como um termo só: o idf vem de todos os documentos
  que têm alguma completação, e o tf é a soma das completações no documento.
- **Plural desligado por padrão** ("casa" e "casas" são termos
  diferentes; use `--stemming pt-plural` ou `--flexoes pt-plural`). O `pt-plural` só junta plural e
  singular; o Snowball (`pt`) junta também verbos e derivados, mas às vezes
  palavras sem relação (`coreia` e `cor` viram `cor`).
- **Stopwords não são buscáveis.** "The Who" vira só `who`… que também é
  stopword em inglês, então a consulta fica vazia. Como prefixo continuam
  valendo (`de*` acha "dezembro"). As listas estão em `src/stopwords/*.txt`;
  para usar outro idioma, crie o banco com `--idiomas en` (ou `nenhum`).

## Compilação

O crate `rocksdb` compila o RocksDB do código-fonte (C++). Usamos a versão
0.23 porque as versões mais novas exigem um compilador C++20, e o g++ 9 do
Ubuntu 20.04 não tem a biblioteca padrão C++20. A primeira compilação leva
alguns minutos.

```bash
cargo test
```
