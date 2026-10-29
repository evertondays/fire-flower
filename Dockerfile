# syntax=docker/dockerfile:1.7
#
# Imagem do fire-flower: só o binário da CLI numa Debian enxuta.
#
#   docker build -t fire-flower .
#   docker run --rm -it -v "$PWD:/data" fire-flower index meu-banco docs/
#
# A pasta montada em /data é o diretório de trabalho: bancos e documentos são
# caminhos relativos a ela.

ARG RUST_VERSION=1.90
ARG DEBIAN_VERSION=bookworm

# ---------------------------------------------------------------- compilação
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS build

# O RocksDB é compilado do fonte (C++) e o bindgen precisa da libclang.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libclang-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Dependências primeiro, com fontes vazios: a camada (e o RocksDB, que leva
# minutos para compilar) só é refeita quando o Cargo.toml ou o Cargo.lock mudam.
COPY Cargo.toml Cargo.lock ./
COPY crates/fire-flower-cli/Cargo.toml crates/fire-flower-cli/
RUN mkdir -p src benches crates/fire-flower-cli/src \
 && echo 'fn main() {}' > crates/fire-flower-cli/src/main.rs \
 && touch src/lib.rs benches/micro.rs \
 && cargo build --release --locked --bin fire-flower \
 && rm -rf src benches crates/fire-flower-cli/src

COPY src ./src
COPY benches ./benches
COPY crates ./crates
# O COPY preserva as datas dos arquivos, que podem ser mais antigas que as dos
# fontes vazios: sem o touch, o cargo acharia que nada mudou.
RUN touch src/lib.rs crates/fire-flower-cli/src/main.rs \
 && cargo build --release --locked --bin fire-flower \
 && cp target/release/fire-flower /usr/local/bin/fire-flower

# ---------------------------------------------------------------- execução
FROM debian:${DEBIAN_VERSION}-slim

LABEL org.opencontainers.image.title="fire-flower" \
      org.opencontainers.image.description="Motor de busca full-text embutido: Trie persistida no RocksDB" \
      org.opencontainers.image.source="https://github.com/evertondays/fire-flower"

# Usuário comum com o UID mais comum (1000): no Linux, os bancos criados na
# pasta montada ficam com o dono certo. Outro UID: `--user "$(id -u):$(id -g)"`.
RUN groupadd --gid 1000 fire-flower \
 && useradd --uid 1000 --gid 1000 --no-create-home --shell /usr/sbin/nologin fire-flower \
 && mkdir /data && chown 1000:1000 /data

COPY --from=build /usr/local/bin/fire-flower /usr/local/bin/fire-flower

USER 1000:1000
WORKDIR /data
ENV TERM=xterm-256color

ENTRYPOINT ["fire-flower"]
CMD ["--help"]
