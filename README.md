# fire-flower

Motor de busca full-text embutido: os termos ficam numa **Trie persistida no
RocksDB**, um registro por nó. Sem servidor externo — é uma biblioteca Rust
(mais um binário de linha de comando para testar).

Suporta:

- busca exata, normalizada (`Ação` = `acao` = `ACAO`);
- stopwords por idioma (português e inglês por enquanto: `de`, `o`, `the`…)
  ficam fora do índice e são ignoradas na consulta (`casa de papel` busca
  `casa` E `papel`). Os idiomas são escolhidos na criação do banco
  (`--languages pt,en`, o padrão) e ficam gravados nele;
- várias palavras combinadas com **E** (`roupa rei`);
- busca por prefixo (`program*`);
- **plural e flexões** opcionais: `--stemming pt-plural` ("planetas" acha
  o artigo "Planeta", quase sem custo), `--inflections pt-plural` (o mesmo, com a
  palavra exata valendo mais que a flexão, mas buscas ~2× mais lentas) e
  `--stemming pt` (Snowball). São escolhidos na criação do banco e ficam
  gravados nele;
- busca por **frase** (`"casa de papel"`): os termos em sequência, com as
  stopwords contando como buraco;
- **ranking BM25F**: os resultados saem por relevância (`search_top`), com o
  título como campo de peso próprio (`add_titled`; `k1 = 1,2`, `b = 0,75`,
  peso do título 50); `search` continua devolvendo todos os IDs em ordem;
- autocompletar (`suggest`), das palavras mais curtas para as mais longas;
- indexação em lotes atômicos, com buscas rodando em paralelo;
- **edição e remoção** de documentos: indexar um ID que já existe o
  substitui, e `delete_document` o remove, no mesmo commit atômico. Editar
  só acrescenta (a versão antiga sai na compactação), então a busca continua
  rápida mesmo com edições pendentes.

## Uso

### Docker (sem instalar nada)

A imagem tem só a linha de comando, para amd64 e arm64 (Mac M1/M2 inclusive).
A pasta atual vira `/data` dentro do container: bancos e documentos são
caminhos relativos a ela.

```bash
docker pull ghcr.io/evertondays/fire-flower
```

```bash
# indexa a pasta documentos/ num banco chamado meu-banco (criado na pasta atual)
docker run --rm -v "$PWD:/data" ghcr.io/evertondays/fire-flower index meu-banco documentos/

# busca
docker run --rm -v "$PWD:/data" ghcr.io/evertondays/fire-flower search meu-banco "casa de papel"

# busca interativa (precisa do -it)
docker run --rm -it -v "$PWD:/data" ghcr.io/evertondays/fire-flower tui meu-banco

# reindexa a pasta: arquivos alterados são substituídos e os que sumiram, removidos
docker run --rm -v "$PWD:/data" ghcr.io/evertondays/fire-flower index --sync meu-banco documentos/
```

Com um alias, fica igual ao binário:

```bash
alias fire-flower='docker run --rm -it -v "$PWD:/data" ghcr.io/evertondays/fire-flower'

fire-flower index meu-banco documentos/
fire-flower tui meu-banco
```

- **Tags:** `latest` (última versão), `0.1.0` etc. (versões fixas) e `main`
  (o que está na branch principal).
- **Linux com UID diferente de 1000:** o container roda como o usuário 1000;
  para os bancos ficarem com o seu dono, acrescente
  `--user "$(id -u):$(id -g)"`.
- **Windows (PowerShell):** troque `"$PWD:/data"` por `"${PWD}:/data"`.
- **Em scripts, sem terminal:** tire o `-it` (o Docker recusa `-t` sem
  terminal).
- **Compilar a imagem localmente:** `docker build -t fire-flower .` (a primeira
  vez leva uns minutos, por causa do RocksDB; depois só o fire-flower
  recompila).

### Linha de comando

```bash
cargo build --release -j 4
```

Indexar arquivos e pastas (recursivo; ignora arquivos ocultos e binários;
`.html`/`.htm` têm as tags removidas):

```bash
./target/release/fire-flower index /tmp/indice ~/documentos
```

Rodar `index` de novo cria documentos novos para os mesmos arquivos. Para
manter o banco igual à pasta, use `--sync`: um arquivo já indexado (achado
pelo caminho) é substituído com o mesmo ID, e um que sumiu da pasta é
removido.

```bash
./target/release/fire-flower index --sync /tmp/indice ~/documentos
```

Remover documentos pelo ID:

```bash
./target/release/fire-flower delete /tmp/indice 42 43
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
| `Ctrl+R` | reabre o banco para ver o que foi gravado desde a abertura |
| `Esc` | volta / sai |

A TUI, `search` e `suggest` abrem o banco **só para leitura**: funcionam ao
lado de um `index` ou de uma aplicação que está gravando no mesmo banco, sem
travá-lo, com block cache de 32 MB. A visão é a do momento da abertura; a TUI
reabre sozinha depois de 10 s, na próxima tecla. `delete` e `compact` não
criam um banco vazio num caminho errado.

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
`Ctrl+C` interrompe e mostra o comando `--skip N` para continuar de onde
parou. No fim, o banco é compactado (~10 s; deixa as buscas 30–50% mais
rápidas) e um resumo vai para `~/bench-db/ptwiki-full.carga.json`. A carga
inteira leva ~2 minutos, com lotes de 10 mil artigos e pico de ~1,4 GB de
memória. Veja `--help` para `--corpus`, `--db`, `--batch` e
`--no-compact`.

Por baixo, o script chama:

```bash
./target/release/fire-flower index-jsonl ~/bench-db/ptwiki-full ~/datasets/ptwiki/ptwiki.jsonl
```

Cada linha do JSONL é `{"id", "title", "text"}`: indexa o texto e o título
(como campo próprio, que pesa mais no ranking) sob o `id`, e guarda o título
como rótulo. Um `id` que já existe é substituído, e uma linha
`{"op": "delete", "id": 42}` remove o documento. No `index` de arquivos, o
título é o nome do arquivo sem a extensão.

Por padrão o banco grava as **posições** de cada termo, que a busca por frase
usa. Com `--no-positions` (em `index`, `index-jsonl` e
`scripts/carregar_wikipedia.py`) o banco fica menor e a carga mais rápida, mas
frases dão erro. A escolha fica gravada no banco, como os idiomas.

Para achar outras formas da palavra há duas opções (nas mesmas opções de
criação; ver [docs/plano-stemming.md](docs/plano-stemming.md)):

- `--stemming pt-plural` (a recomendada): plural e singular viram o mesmo
  termo no índice ("planetas" acha "Planeta", "eleicoes" acha "eleição").
  Na Wikipédia inteira, a carga e o tamanho ficam iguais, termos e E custam
  +2–10% e frases +17–35%; a ordenação perde pouco (o artigo "Torre" pode
  ficar atrás de "Torres").
- `--inflections pt-plural`: o índice guarda as palavras como estão e um mapa
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

A saída é `nota  id  rótulo`, da nota maior para a menor. `--by-id` lista
todos os documentos em ordem de ID, sem ranking. Frases vão entre aspas
simples no shell:

```bash
./target/release/fire-flower search /tmp/indice '"revolução francesa" 1789'
```

```bash
./target/release/fire-flower suggest /tmp/indice prog 10
```

### JavaScript (Node, Bun e Deno)

O binding [`bindings/node`](bindings/node/) (napi-rs) roda o motor no mesmo
processo: ~0,1 µs por chamada vazia e a busca top 10 com a latência do Rust.

```javascript
const { Engine } = require("@fire-flower/node");
const engine = await Engine.open("./indice");
await engine.index([{ id: 1, title: "Planeta Terra", text: "…" }]); // commit fora do event loop
const top = engine.searchTopSync("planeta ter*", { k: 10 });          // top.ids, top.scores, top.total
```

Detalhes, compilação e medidas: [bindings/node/README.md](bindings/node/README.md)
e [docs/plano-bindings.md](docs/plano-bindings.md).

### Monitoramento

O processo que grava o banco pode publicar métricas de custo desprezível
(`Config::metrics`, ou `--metrics` no `index`, `index-jsonl` e `compact`):
uma thread grava a cada segundo um snapshot na pasta do banco
(`fire-flower.metrics.json`). O `monitor` lê esse arquivo, o `/proc` do
processo e a pasta, sem abrir o banco nem falar com o motor:

```bash
./target/release/fire-flower index-jsonl --metrics /tmp/wiki ptwiki.jsonl
```

```bash
./target/release/fire-flower monitor /tmp/wiki
```

O painel mostra consultas por segundo, p50 e p99 por tipo de consulta,
documentos indexados, substituídos e removidos por segundo, versões mortas
esperando a compactação, RocksDB, memória e disco, com alertas (escrita
parada, compactação atrasada, disco cheio, muitas versões mortas). Para o
Prometheus, `--serve` expõe `/metrics` em OpenMetrics; `--json` imprime uma
leitura e sai:

```bash
./target/release/fire-flower monitor /tmp/wiki --serve 127.0.0.1:9187
```

Dashboard do Grafana e regras de alerta em
[`contrib/monitoring/`](contrib/monitoring/); a lista das métricas, o que
ficou de fora e por quê estão em
[docs/plano-monitoramento.md](docs/plano-monitoramento.md).

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

// Editar e remover: o mesmo ID substitui o documento inteiro.
let mut batch = engine.batch();
batch.add_document(1, "O coração de Minas")?;
batch.delete_document(2)?;
batch.commit()?;
assert!(engine.search("brasil")?.is_empty());
assert_eq!(engine.search("coração")?, [1]);
```

`Engine` é `Sync`: compartilhe com `Arc<Engine>`. Só um lote de indexação
existe por vez (os outros esperam); buscas nunca esperam e nunca veem um lote
pela metade, nem uma edição pela metade. No mesmo lote, a última operação de
cada documento vence.

IDs de documento vão até 2^40 − 1 (~1,1 trilhão). Substituir um documento dá
a ele um ID interno novo, depois dos documentos sem edição nas listas, e a
versão antiga vira **morta**: a
busca a pula, e a compactação a tira das listas. Até lá, a versão morta ainda
ocupa espaço, conta no df (o idf muda um pouco) e uma palavra que ficou sem
documento ainda aparece no autocompletar. Numa aplicação que edita aos
poucos, ligue a manutenção em segundo plano: ela compacta quando as edições
acumuladas passam de 1% dos documentos (no mínimo 1.000).

```rust
use std::sync::Arc;
use fire_flower::{Maintenance, MaintenanceOptions};

let engine = Arc::new(engine);
let maintenance = Maintenance::spawn(Arc::clone(&engine), MaintenanceOptions::default());
// ... edições e buscas ...
maintenance.stop()?; // ou só descarte
```

Sem ela, chame `Engine::compact` (ou `fire-flower compact`) depois de editar
muito. A compactação não bloqueia buscas nem lotes, e só tira uma versão morta
quando nenhuma busca em andamento ainda a vê viva.

Para quem não pode bloquear (um event loop, um binding), `try_batch` dá
`Error::Busy` em vez de esperar o lote aberto; `owned_batch` devolve um lote
dono de um `Arc<Engine>` (`OwnedBatch`, `Send + 'static`), que pode ser
guardado num objeto e fazer commit em outra thread. `labels(&ids)` lê os
rótulos de vários documentos num só `MultiGet`. Todo erro tem um código
estável (`Error::code()`: `FF_BUSY`, `FF_OPEN`, `FF_READ_ONLY`…).

```rust
use std::sync::Arc;

let engine = Arc::new(engine);
let mut batch = engine.try_owned_batch()?; // Error::Busy se houver outro lote
batch.add_document(3, "em outra thread")?;
std::thread::spawn(move || batch.commit()).join().unwrap()?;
```

Outro processo pode ler o mesmo banco enquanto este grava, sem travá-lo:

```rust
// Vê o banco como estava na abertura; abra de novo para ver o que veio depois.
let reader = Engine::open_read_only("/tmp/indice", Config::reader())?;
```

Para acompanhar quem grava (vários workers do PHP-FPM, um cluster do Node, um
serviço de busca ao lado do indexador), o `Reader` reabre sozinho quando o
banco muda. Ele percebe a mudança pelo WAL e pelo MANIFEST do RocksDB, uma
listagem da pasta no máximo a cada intervalo, sem custo para quem grava.

```rust
use std::time::Duration;
use fire_flower::Reader;

let reader = Reader::open("/tmp/indice", Config::reader(), Duration::from_secs(1))?;
reader.refresh()?;                        // reabre se o banco mudou (no máximo 1×/s)
let hits = reader.engine().search_top("coração", 10)?;
```

Métricas (ver [Monitoramento](#monitoramento)): com `Config::metrics`, o
motor conta consultas, commits, edições, remoções e compactações (somas
atômicas num contador da própria thread: ~7 ns por consulta) e grava o
snapshot que o `fire-flower monitor` lê. `Engine::metrics_snapshot` devolve
os mesmos números no próprio processo. Compilar sem a feature `metrics`
(`--no-default-features`) tira os ganchos por completo.

## Arquitetura

Explicação completa, com o que é o RocksDB, um banco real byte a byte e os
fluxos de indexação e busca: [docs/como-funciona.md](docs/como-funciona.md).

| Camada | Onde | Responsabilidade |
|---|---|---|
| Interface | `crates/fire-flower-cli/` | subcomandos (clap), `index-jsonl`, busca interativa (`tui/`, ratatui) e `monitor/` |
| Texto | `src/analysis/` | remove HTML, acentos (NFKD), pontuação; minúsculas; tokens > 64 caracteres são descartados; stopwords e stemming |
| Consulta | `src/query/` | separa a consulta em termos, prefixos (`termo*`) e frases (`"..."`) |
| Índice (Trie) | `src/engine/` | indexação (`writer/`), busca, prefixo, frases, ranking (`ranking/`), autocompletar |
| Armazenamento | `src/engine/` + `src/codec/` | chaves no RocksDB e serialização binária (um arquivo por formato) |

O mapa arquivo por arquivo está em [docs/como-funciona.md](docs/como-funciona.md#9-mapa-do-código).
Como o código é escrito, testado e medido (idioma, organização, visibilidade,
imports, erros, performance, git): [docs/diretrizes-de-codigo.md](docs/diretrizes-de-codigo.md).

### Chaves no RocksDB

Formato v7 (`meta:format = 7`). Um banco de formato anterior não abre: o
`open` diz em que formato ele está e pede para recarregar.

Em `post:`, `pos:` e `norm:` o documento é o **ID interno** da versão: o
próprio ID na primeira versão, e `2^63 + (versão << 40) + ID` numa
substituição ou readição (a busca devolve o ID do usuário).

| Chave | Valor |
|---|---|
| `node:<id>` | nó da Trie: flag de fim de palavra + filhos (`caractere → id`). `node:0` é a raiz |
| `post:<id>` | lista de postagem do termo que termina no nó `<id>`: `(Δdoc, tf << 1 \| tem título, [tf no título])` por documento |
| `pos:<id>` | posições do termo em cada documento: `(Δdoc, k, k × Δposição)` (só com posições); o título vem primeiro e o texto começa 100 posições depois |
| `norm:<bloco>` | tamanho (tokens indexados) de cada documento com `id ÷ 1024 = bloco`, e o do título |
| `doc:<id>` | rótulo opcional do documento (o CLI grava o caminho do arquivo) |
| `ver:<id>` | ID interno da última versão do documento, só para os que já foram substituídos ou removidos |
| `meta:next_node_id`, `meta:next_doc_id` | contadores de ID |
| `meta:doc_count`, `meta:total_len`, `meta:total_title_len` | total de documentos, soma dos tamanhos e dos títulos (BM25) |
| `stem:<radical>` | nós das palavras com esse radical (só com `--inflections`) |
| `meta:dead` | as versões mortas que ainda estão nas listas (até a compactação) |
| `meta:epoch` | número do último commit (as buscas sabem quais mortes já veem) |
| `meta:format`, `meta:positions`, `meta:languages`, `meta:stemming`, `meta:inflections` | formato, se grava posições, idiomas das stopwords, stemming e flexões |

Os valores usam varint; as listas gravam a diferença entre IDs consecutivos,
então documentos com IDs próximos custam ~1 byte cada. `post:` e `pos:` ficam
em chaves separadas para que a busca comum não leia posições. Listas com mais
de 256 documentos têm, antes das entradas, uma tabela de saltos (último
documento, tamanho e quantidade de entradas de cada bloco, 128 na gravação):
o E e as frases vão direto ao bloco de um documento, sem decodificar a lista
inteira, e o merge só concatena as tabelas dos pedaços.

Editar não regrava listas: a versão nova entra no fim delas (o merge só
concatena, como na carga), e a antiga sai de `norm:` e entra em `meta:dead`.
A busca pula os mortos ao decodificar a lista que conduz o E (um teste por
documento, só enquanto há mortos). A compactação os tira das listas pelo
merge operator e por um filtro de compactação, e tira da Trie as palavras que
ficaram sem documento.

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
python3 scripts/perf.py all --scale 10k
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

- **Até a compactação, as edições ficam aproximadas em três pontos.** As
  versões mortas ocupam espaço nas listas, contam no df (o idf de um termo
  editado fica um pouco menor, então as notas mudam um pouco), e uma palavra
  cujos documentos foram todos substituídos ou removidos continua no
  autocompletar (a busca por ela já não acha nada). A compactação acerta os
  três; use a `Maintenance` (biblioteca) ou `fire-flower compact`.
- **Peso do título ajustado para a Wikipédia.** O peso 50 saiu de consultas
  com resposta conhecida na Wikipédia, onde o título descreve o artigo; em
  outro tipo de corpus, ajuste `Config::title_weight`. Páginas de
  desambiguação podem passar à frente do artigo principal.
- **Frases:** uma stopword dentro da frase só conta a distância (`"casa de
  papel"` também casa "casa da papel"); frase só de stopwords fica vazia; não
  há prefixo dentro de frase (`"revolução franc*"`).
- **Posições custam espaço.** Na fatia de 10k da Wikipédia, o banco
  compactado tem ~51 MB com posições e ~21 MB sem; use `--no-positions` se
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
  diferentes; use `--stemming pt-plural` ou `--inflections pt-plural`). O `pt-plural` só junta plural e
  singular; o Snowball (`pt`) junta também verbos e derivados, mas às vezes
  palavras sem relação (`coreia` e `cor` viram `cor`).
- **Stopwords não são buscáveis.** "The Who" vira só `who`… que também é
  stopword em inglês, então a consulta fica vazia. Como prefixo continuam
  valendo (`de*` acha "dezembro"). As listas estão em `src/analysis/stopwords/*.txt`;
  para usar outro idioma, crie o banco com `--languages en` (ou `none`).

## Compilação

O crate `rocksdb` compila o RocksDB do código-fonte (C++). Usamos a versão
0.23 porque as versões mais novas exigem um compilador C++20, e o g++ 9 do
Ubuntu 20.04 não tem a biblioteca padrão C++20. A primeira compilação leva
alguns minutos.

```bash
cargo test
```

É um workspace: o core (a biblioteca) fica na raiz e a CLI em
`crates/fire-flower-cli` (só ela depende de clap e ratatui). Na raiz,
`cargo build --release` compila os dois, e o binário sai em
`target/release/fire-flower`.
