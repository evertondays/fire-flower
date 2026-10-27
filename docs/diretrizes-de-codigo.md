# Diretrizes e padrões de código

Como o código do fire-flower é escrito, organizado, testado e medido. Vale
para mudanças novas; o que ainda não segue a regra é corrigido quando o
arquivo for mexido. A estrutura de pastas detalhada está na seção 9 do
[como-funciona.md](como-funciona.md#9-mapa-do-código).

## 1. Idioma

| Em inglês | Em português |
|---|---|
| identificadores (tipos, funções, variáveis, módulos) | comentários e doc comments (`//`, `///`, `//!`) |
| opções e subcomandos da CLI (`--languages`, `wiki relevance`, `perf.py all`) | texto de ajuda da CLI |
| chaves de JSON (`docs_per_s`, `peak_rss`) e valores gravados (`"none"`) | mensagens de erro e o que aparece na tela (TUI, barra de progresso, relatórios) |
| nomes de arquivos de dados, conjuntos de consulta e rótulos (`rel-title`, `phrase-2`) | documentação em `docs/` e o README |
| nomes de branch (`docker-release`, `coding-guidelines`) | mensagens de commit e descrições de PR |

Um valor que é ao mesmo tempo dado e texto de tela (como a fase do
`index-jsonl`) fica em inglês no dado e é traduzido na hora de mostrar.

Palavras usadas como **dado** nos testes e benchmarks (`"coração"`,
`"populacao"`, `"sempre"`) não são identificadores: ficam como estão.

## 2. Organização

- **Uma pasta por parte da busca**, um arquivo por responsabilidade:
  `analysis/` (texto → termos), `query/` (consulta → partes), `codec/`
  (formatos gravados), `engine/` (índice e armazenamento, com `ranking/` e
  `writer/`), `bin/fire-flower/` (CLI).
- **Arquivos com até ~600 linhas.** Passou disso ou juntou duas
  responsabilidades: vira pasta, com um `mod.rs` que guarda o tipo principal
  e declara os submódulos.
- **Testes unitários no `tests.rs` da pasta** (`#[cfg(test)] mod tests;` no
  `mod.rs`), não no fim de cada arquivo. Testes de ponta a ponta em
  `tests/engine/`, um arquivo por tema; funções de apoio no `main.rs` de lá.
- **Harness e scripts seguem o mesmo padrão:** `examples/wiki/` tem um arquivo
  por subcomando; `scripts/` tem um script por ferramenta.
- **Cada arquivo começa com um `//!`** de uma ou duas linhas dizendo o que ele
  faz.

## 3. Módulos e visibilidade

- **Visibilidade mínima:** privado → `pub(super)` → `pub(crate)` → `pub`.
  `pub(crate)` só quando mais de um módulo usa o item; campos de struct
  também. Módulos filhos já enxergam os itens privados do pai: não é preciso
  abrir nada para eles.
- **API pública pequena**, toda reexportada pelo `lib.rs` (`Engine`,
  `Config`, `IndexBatch`, `Error`, `DocId`…). Os módulos `analysis` e
  `query` são públicos porque a CLI e o harness usam o tokenizador e o
  parser.
- **`internals` é só para benchmarks e harness** (`#[doc(hidden)]`). Dentro
  da crate, importe pelo caminho canônico (`crate::codec::…`), nunca por
  `crate::internals`.

## 4. Imports

Três grupos separados por linha em branco, cada um em ordem alfabética, com
os itens de um mesmo módulo juntos entre chaves:

```rust
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use rocksdb::{DB, Options};
use rustc_hash::FxHashMap;

use crate::{DocId, Error, Result};
use crate::codec::{NormBlock, TrieNode};
use crate::engine::view::View;
```

- Sem `use x::*`, exceto `use super::*` nos `tests.rs`.
- Linha de import com mais de ~120 colunas quebra no estilo do rustfmt (uma
  chave por linha de abertura e fechamento).

## 5. Formatação

- O projeto **não usa `cargo fmt`**: o código não está no formato dele, e
  rodar no projeto inteiro geraria uma diferença enorme. Siga o estilo do
  arquivo que está editando.
- Linhas de até ~120 colunas (99% do código já está assim).
- `if` curto numa linha só quando cabe:
  `if exact.len() < n { self.search_with(&clauses, true) } else { Ok(exact) }`.

## 6. Nomes

- Convenções do Rust: `snake_case` para funções e variáveis, `CamelCase` para
  tipos, `SCREAMING_SNAKE_CASE` para constantes.
- **Opções da CLI saem do nome do campo** (`#[arg(long)]` em
  `no_positions` vira `--no-positions`). Não fixe `long = "..."` nem
  `value_name = "..."`, a não ser que o nome derivado fique ruim.
- Chaves de JSON em `snake_case` (`lines_committed`); com campos em inglês,
  não é preciso `#[serde(rename)]`.
- Conjuntos de consulta, rótulos de resultado e arquivos de dados em
  `kebab-case` (`and-rare-freq`, `ptwiki-full-compacted`).
- Branches em `kebab-case`, em inglês, saindo da `main` atualizada.

## 7. Erros

- A biblioteca devolve `crate::Result<T>` com o `Error` de `src/error.rs`
  (`Db`, `Corrupt`, `Config`…). Nada de `unwrap` no caminho de produção da
  biblioteca: o que depende de dado gravado ou de entrada do usuário vira
  `Err`. Invariante garantida pelo próprio código usa `expect` com a
  invariante escrita (`expect("há pelo menos uma lista")`). Nos testes e no
  harness, `unwrap` pode.
- Dado gravado que não decodifica é `Error::Corrupt`; config que não combina
  com o banco é `Error::Config`. A mensagem diz o que estava errado e o que
  fazer ("recarregue o banco"), em português.
- Na CLI e no harness, `AnyResult` (`Result<_, Box<dyn Error>>`) e `?`; o
  `main` da CLI mostra o erro como `erro: …` e sai com código 1.

## 8. Comentários e documentação

- Comentário explica **por quê**, não o quê: a regra, a decisão ou o número
  que justifica o código ("o RocksDB leva minutos para compilar: dependências
  numa camada própria"). O que o código já diz não precisa ser repetido.
- Doc comment (`///`) em todo item `pub` e nos `pub(crate)` que não são
  óbvios; funções com algoritmo (top-k, frases, merge) descrevem os passos.
- Mudou um comportamento, uma opção ou um formato: atualize o README e o
  `docs/como-funciona.md` no mesmo PR. Os `docs/plano-*.md` são registro
  histórico de cada plano: não precisam acompanhar o código depois que o
  plano termina.

## 9. Formatos gravados

- Os valores no RocksDB estão em `src/codec/` e as chaves em
  `src/engine/keys.rs`. Mudança no formato de algum valor sobe o
  `FORMAT_VERSION` (`src/engine/meta.rs`), e o `open` de um banco antigo
  falha com uma mensagem pedindo para recarregar, sem alterar o banco.
- Opções decididas na criação (idiomas, posições, stemming, flexões) ficam
  gravadas em `meta:*`; abrir com outra opção é `Error::Config`.
- Quebrar compatibilidade (bancos ou resultados antigos) é decisão explícita
  e vai escrita no commit e no PR.

## 10. Testes e corretude

Antes de abrir um PR:

```bash
cargo test --release --all-targets
cargo test --release --doc
cargo clippy --release --all-targets   # zero warnings
```

- Bug corrigido ganha um teste que falhava antes.
- Mudança no índice, na busca ou no ranking passa pelo **oráculo**, que
  compara o motor com um índice ingênuo em memória e precisa dar **zero
  divergências** (`python3 scripts/perf.py correctness --scale 10k`; o
  `perf.py all` roda também os de 100k e da Wikipédia reaberta).
- Mudança no ranking também passa pela relevância
  (`python3 scripts/perf.py relevance`): os conjuntos de guarda (`rel-title`,
  `rel-hub`) não podem piorar.

## 11. Performance

**Nenhuma mudança pode perder desempenho sem que isso seja uma decisão
explícita.** Refatoração deve perder zero.

- **Mudou o motor:** rode o A/B completo, `main` e a branch uma logo depois da
  outra, na mesma máquina, cada uma com a sua pasta de bancos:

  ```bash
  git worktree add ../fire-flower-main main
  (cd ../fire-flower-main && FF_PERF_DB=~/bench-db/perf-main python3 scripts/perf.py all --scale full)
  FF_PERF_DB=~/bench-db/perf-branch python3 scripts/perf.py all --scale full
  ```

  Cada rodada leva ~21 min. Nada de compilar ou rodar outra coisa pesada
  enquanto mede.
- **Não edite arquivos versionados durante uma rodada:** o rótulo do commit
  ganha `-modificado` e o resultado deixa de valer para o commit.
- **Ruído:** latências de um conjunto variam ±5% entre binários diferentes só
  pelo alinhamento do código, mesmo com a biblioteca idêntica. Uma diferença
  só conta se aparecer em medições independentes (rodada completa, `pr`,
  repetição alternando os binários). Na dúvida, faça bisect pelos commits.
- Funções pequenas do caminho quente (BM25, limites do top-k, cursores de
  listas e de tamanhos) levam `#[inline]`; ao mover uma delas de arquivo, o
  atributo vai junto.
- Resultados e análises ficam no
  [fire-flower-bench](https://github.com/evertondays/fire-flower-bench)
  (`results/`, `RESULTADOS.md` e um `ANALISE-*.md` por investigação).

## 12. Git e PRs

- Uma branch por assunto, em inglês, saindo da `main` atualizada; tudo entra
  por PR.
- Um commit por passo lógico (renomear, dividir um módulo, corrigir imports),
  com a suíte passando em cada um. O título resume a mudança em português; o
  corpo explica o porquê e o que quebra, se quebrar algo.
- A descrição do PR diz o que muda, como foi testado, o resultado do A/B
  (quando mexe no motor) e o que fazer depois do merge.
- Commits e PRs feitos com o Claude Code levam o `Co-Authored-By` e a linha
  de atribuição que ele gera.

## 13. Docker e CI

- O workflow `docker` compila a imagem para amd64 e arm64, roda um teste
  rápido (index, search, suggest, index-jsonl) e publica em
  `ghcr.io/evertondays/fire-flower`: a tag `main` a cada push na `main`, e
  `X.Y.Z`, `X.Y` e `latest` a cada tag `vX.Y.Z`. Num PR, só compila e testa.
- Mudou a CLI (comando novo, opção que a imagem usa): ajuste o teste do
  workflow e a seção Docker do README.
- A versão do Rust do `Dockerfile` (`RUST_VERSION`) acompanha a usada no
  desenvolvimento.
