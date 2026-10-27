//! Serialização compacta dos valores gravados no RocksDB (formato v5).
//!
//! Inteiros usam varint (LEB128): números pequenos ocupam 1–2 bytes em vez de
//! 8. Listas são ordenadas por documento e gravam a diferença (delta) entre IDs
//! consecutivos, então documentos próximos custam ~1 byte cada.
//!
//! | Chave | Valor |
//! |---|---|
//! | `node:<id>` | flags, `n`, `n × (caractere, filho)` |
//! | `post:<nó>` | `n`, saltos, `n × (Δdoc, tf << 1 \| tem título, [tf do título])` |
//! | `pos:<nó>` | `n`, saltos, `n × (Δdoc, k, k × Δposição)` (a primeira posição é absoluta) |
//!
//! **Saltos (v5):** listas com mais de `2 × SKIP_BLOCK` entradas guardam,
//! antes das entradas, uma tabela de blocos: `s`, `s × (Δúltimo doc, bytes,
//! entradas − 1)`. Listas menores gravam `s = 0`. As entradas são exatamente as
//! do v3 (os deltas continuam de um bloco para o outro), então decodificar a
//! lista inteira só pula a tabela; os cursores ([`PostingsCursor`],
//! [`PositionsCursor`]) a usam para ir direto ao bloco de um documento sem
//! decodificar os anteriores.
//!
//! O lote grava blocos de `SKIP_BLOCK` entradas, mas a tabela aceita blocos
//! de qualquer tamanho: assim o merge só concatena as tabelas dos pedaços (um
//! pedaço pequeno vira um bloco), sem percorrer a lista unida. No v4 os blocos
//! tinham tamanho fixo e o merge recalculava a tabela a cada leitura de uma
//! lista com pedaços pendentes (termo frequente antes de compactar: +43%).
//! | `norm:<bloco>` | `n`, `n × (Δdeslocamento, tamanho)` dos documentos do bloco |

pub(crate) mod list;
pub(crate) mod merge;
pub(crate) mod node_set;
pub(crate) mod norms;
pub(crate) mod positions;
pub(crate) mod postings;
pub(crate) mod trie_node;
pub(crate) mod varint;
#[cfg(test)]
mod tests;

pub use self::list::{SKIP_BLOCK, list_len};
pub use self::merge::{merge_positions, merge_postings};
pub use self::node_set::{decode_node_set, encode_node_set, merge_node_sets};
pub use self::norms::{DocLens, NORM_BLOCK_DOCS, NormBlock};
pub use self::positions::{PositionsCursor, decode_positions, encode_positions_into};
pub use self::postings::{PostingsCursor, decode_docs, decode_postings, encode_postings, encode_postings_into};
pub use self::trie_node::TrieNode;
pub use self::varint::{get_varint, put_varint};
