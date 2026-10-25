# fire-flower

Motor de busca full-text embutido: os termos ficam numa **Trie persistida no
RocksDB**, um registro por nó. Sem servidor externo — é uma biblioteca Rust
(mais um binário de linha de comando para testar).

Suporta:

- busca exata, normalizada (`Ação` = `acao` = `ACAO`);
- várias palavras combinadas com **E** (`roupa rei`);
- busca por prefixo (`program*`);
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
| digitar | busca na hora; a última palavra vale como prefixo até você dar espaço |
| `Tab` ou `→` | completa a palavra com a sugestão (em cinza depois do cursor) |
| `Ctrl+N` / `Ctrl+P` | troca a sugestão |
| `↑` `↓` `PgUp` `PgDn` | escolhe o resultado |
| `Enter` | abre o documento inteiro (`n`/`N` pulam entre ocorrências) |
| `Ctrl+W` / `Ctrl+U` | apaga a última palavra / a consulta toda |
| `Esc` | volta / sai |

A prévia lê o arquivo apontado pelo rótulo do documento. Documentos cujo
rótulo não é um arquivo (ex.: título de artigo) aparecem sem prévia.

Busca e autocompletar sem interface, para scripts:

```bash
./target/release/fire-flower search /tmp/indice "coração bras*"
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
assert_eq!(engine.suggest("es", 5)?, ["estudante"]);
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

| Chave | Valor |
|---|---|
| `node:<id>` | nó da Trie: flag de fim de palavra + filhos (`caractere → id`). `node:0` é a raiz |
| `post:<id>` | lista de postagem do termo que termina no nó `<id>`: IDs de documentos ordenados |
| `doc:<id>` | rótulo opcional do documento (o CLI grava o caminho do arquivo) |
| `meta:next_node_id`, `meta:next_doc_id` | contadores de ID |

Os valores usam varint; as listas de postagem gravam a diferença entre IDs
consecutivos, então documentos com IDs próximos custam ~1 byte cada.

### Configuração do RocksDB

- filtro de Bloom, 10 bits por chave (`Config::bloom_bits_per_key`);
- block cache LRU de 256 MiB (`Config::block_cache_bytes`), com índices e
  filtros dentro do cache e os do nível 0 fixados nele;
- compressão LZ4.

## Diferenças em relação ao documento de design original

1. **Listas de postagem com merge operator.** O documento faz ler → alterar →
   regravar a lista inteira a cada documento. Para palavras comuns ("de", "o")
   isso fica cada vez mais caro. Aqui cada lote grava só os IDs novos com
   `merge`, e o RocksDB une os pedaços na leitura e na compactação.
2. **Sem `PostingsListID` separado.** A lista de um termo fica em `post:<id do
   nó terminal>`; não é preciso um segundo contador nem guardar o ID no nó.
3. **Filhos guardam IDs numéricos** (`BTreeMap<char, u64>`), não strings
   `"node:124"`. O `BTreeMap` deixa o autocompletar em ordem alfabética.
4. **Nós criados no meio de um lote.** Como nada vai para o banco antes do
   commit, o lote mantém em memória os nós que leu ou criou; senão a segunda
   palavra com o mesmo prefixo não enxergaria os nós criados pela primeira.
5. **Serialização binária própria** em vez de Gob/JSON (menor e sem
   dependências).

## Limitações conhecidas

- **Sem remoção ou atualização de documentos.** Reindexar o mesmo ID só
  adiciona termos; termos que saíram do texto continuam apontando para ele.
- **Sem ranking.** Os resultados vêm em ordem de ID, não de relevância (não há
  TF-IDF/BM25 nem frequência dos termos).
- **Uma leitura por caractere.** Buscar uma palavra de 10 letras faz 11 `Get`s.
  Com o block cache quente isso fica em microssegundos, mas um índice de termos
  ordenado (`term:<palavra>` com iterador de prefixo do RocksDB) faria o mesmo
  com 1 leitura, e o autocompletar com um único scan.
- **Prefixos muito curtos** (`a*`) expandem no máximo
  `Config::max_prefix_terms` termos (256); os demais são ignorados.
- **Sem stopwords nem stemming** ("casa" e "casas" são termos diferentes).

## Compilação

O crate `rocksdb` compila o RocksDB do código-fonte (C++). Usamos a versão
0.23 porque as versões mais novas exigem um compilador C++20, e o g++ 9 do
Ubuntu 20.04 não tem a biblioteca padrão C++20. A primeira compilação leva
alguns minutos.

```bash
cargo test
```
