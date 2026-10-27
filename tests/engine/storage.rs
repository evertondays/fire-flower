//! Armazenamento: opções gravadas na criação, formatos antigos e filtros de Bloom.

use fire_flower::{Config, Engine, Error};

use crate::{open, p};

#[test]
pub(crate) fn languages_are_stored_on_creation_and_checked_on_open() {
    use fire_flower::{Error, Language};
    let dir = tempfile::tempdir().unwrap();
    let english = Config { languages: Some(vec![Language::English]), ..Config::default() };
    {
        let engine = Engine::open(dir.path(), english.clone()).unwrap();
        assert_eq!(engine.languages(), [Language::English]);
        engine.index_document(1, "the house de papel").unwrap();
    }
    // Sem pedir nada, vale o que está gravado: "de" é termo, "the" não.
    let engine = open(&dir);
    assert_eq!(engine.languages(), [Language::English]);
    assert_eq!(engine.search("de").unwrap(), [1]);
    assert!(engine.postings("the").unwrap().is_empty());
    assert_eq!(engine.search("the house").unwrap(), [1]);
    drop(engine);
    // Pedir o mesmo funciona; pedir outro idioma é erro, não uma busca errada.
    drop(Engine::open(dir.path(), english).unwrap());
    let pt = Config { languages: Some(vec![Language::Portuguese]), ..Config::default() };
    assert!(matches!(Engine::open(dir.path(), pt), Err(Error::Config(_))));
}

#[test]
pub(crate) fn database_without_stopwords() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Config { languages: Some(vec![]), ..Config::default() }).unwrap();
    assert!(engine.languages().is_empty());
    engine.index_document(1, "o que é").unwrap();
    assert_eq!(engine.search("o que").unwrap(), [1]);
}

#[test]
pub(crate) fn database_without_positions() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path(), Config { positions: Some(false), ..Config::default() }).unwrap();
        assert!(!engine.has_positions());
        engine.index_document(1, "casa de papel").unwrap();
        engine.index_document(2, "papel de casa").unwrap();
        let mut pos_keys = 0;
        engine.for_each_raw(|k, _| pos_keys += k.starts_with(b"pos:") as usize).unwrap();
        assert_eq!(pos_keys, 0);
    }
    // Sem pedir nada vale o que está gravado; pedir posições é erro.
    let engine = open(&dir);
    assert!(!engine.has_positions());
    assert_eq!(engine.search("casa papel").unwrap(), [1, 2]);
    assert_eq!(engine.search_top("casa", 10).unwrap().len(), 2);
    assert_eq!(engine.term_postings("casa").unwrap(), [p(1, 1), p(2, 1)]);
    assert!(matches!(engine.search("\"casa de papel\""), Err(Error::Config(_))));
    assert!(matches!(engine.search_top("\"casa de papel\"", 5), Err(Error::Config(_))));
    // Uma "frase" de um termo só é termo comum e funciona.
    assert_eq!(engine.search("\"o papel\"").unwrap(), [1, 2]);
    drop(engine);
    let with = Config { positions: Some(true), ..Config::default() };
    assert!(matches!(Engine::open(dir.path(), with), Err(Error::Config(_))));
}

#[test]
pub(crate) fn old_format_is_rejected_with_a_clear_error() {
    // Banco v1: contadores sem `meta:format`.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.put(b"post:4", [1u8, 7]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v1 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato antigo")), "{err}");

    // v1 com merges pendentes no WAL: a recuperação roda o merge operator.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_merge_operator_associative("fire_flower_postings_union", |_, existing, ops| {
            let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
            for op in ops {
                out.extend_from_slice(op);
            }
            Some(out)
        });
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.merge(b"post:4", [1u8, 7]).unwrap();
        db.merge(b"post:4", [1u8, 9]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v1 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato antigo")), "{err}");
    // E o banco antigo não foi tocado: o WAL com os merges continua lá e o
    // merge operator antigo ainda lê a lista como era.
    let mut opts = rocksdb::Options::default();
    opts.set_merge_operator_associative("fire_flower_postings_union", |_, existing, ops| {
        let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
        for op in ops {
            out.extend_from_slice(op);
        }
        Some(out)
    });
    let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
    assert_eq!(db.get(b"post:4").unwrap().unwrap(), [1u8, 7, 1, 9]);
}

/// Contador do RocksDB pelo nome, a partir do texto das estatísticas.
pub(crate) fn ticker(engine: &Engine, name: &str) -> u64 {
    let stats = engine.rocksdb_stats().expect("estatísticas ligadas");
    stats
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(" COUNT : ")?.trim().parse().ok())
        .unwrap_or_else(|| panic!("contador {name} ausente"))
}

/// Bytes de filtro de Bloom que o RocksDB carregou no cache, por `Get`, em
/// buscas sobre um SST com ~1 milhão de chaves e um cache de 1 MB.
pub(crate) fn filter_bytes_per_get(partition_filters: bool) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { block_cache_bytes: 1 << 20, statistics: true, partition_filters, ..Config::default() };
    // Palavras aleatórias de 9 letras: quase sem prefixo comum, ~6 nós cada.
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let words: Vec<String> = (0..150_000)
        .map(|_| {
            (0..9)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    (b'a' + (rng % 26) as u8) as char
                })
                .collect()
        })
        .collect();
    {
        let engine = Engine::open(dir.path(), config.clone()).unwrap();
        let mut batch = engine.batch();
        for (i, chunk) in words.chunks(20).enumerate() {
            batch.add_document(i as u64, &chunk.join(" ")).unwrap();
        }
        batch.commit().unwrap();
        // Um SST só, com ~1 milhão de chaves: filtro de ~1 MB se for inteiro.
        engine.compact();
    }
    let engine = Engine::open(dir.path(), config).unwrap();
    let before = (ticker(&engine, "rocksdb.block.cache.filter.bytes.insert"), engine_reads());
    for w in words.iter().step_by(71).take(2000) {
        assert!(!engine.search(w).unwrap().is_empty(), "{w}");
    }
    let filter_bytes = ticker(&engine, "rocksdb.block.cache.filter.bytes.insert") - before.0;
    filter_bytes / (engine_reads() - before.1).max(1)
}

#[test]
pub(crate) fn bloom_filters_larger_than_a_cache_shard_are_not_reread_on_every_get() {
    // O cenário reproduz o problema com o filtro inteiro (~1 MB relido por Get)...
    let whole = filter_bytes_per_get(false);
    assert!(whole > 256 << 10, "o cenário não reproduziu o problema: {whole} bytes de filtro por Get");
    // ...e com partições de 4 KB o filtro fica no cache.
    let partitioned = filter_bytes_per_get(true);
    assert!(partitioned < 4096, "{partitioned} bytes de filtro carregados por Get (antes: {whole})");
}

pub(crate) fn engine_reads() -> u64 {
    fire_flower::internals::reads_on_this_thread()
}

/// Banco v2 (formato anterior ao título) com merges pendentes no WAL: tem que
/// ser recusado com a mensagem de recarregar, sem rodar o merge operator novo
/// e sem alterar o banco.
#[test]
pub(crate) fn v2_database_is_rejected_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let concat = |_: &[u8], existing: Option<&[u8]>, ops: &rocksdb::MergeOperands| {
        let mut out = existing.map(<[u8]>::to_vec).unwrap_or_default();
        for op in ops {
            out.extend_from_slice(op);
        }
        Some(out)
    };
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_merge_operator_associative("fire_flower_v2", concat);
        let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
        db.put(b"meta:format", [2u8]).unwrap();
        db.put(b"meta:next_node_id", [5u8]).unwrap();
        db.merge(b"post:4", [1u8, 7, 1]).unwrap();
        db.merge(b"post:4", [1u8, 9, 1]).unwrap();
    }
    let err = Engine::open(dir.path(), Config::default()).err().expect("v2 tem que falhar");
    assert!(matches!(&err, Error::Config(m) if m.contains("formato 2") && m.contains("recarregue")), "{err}");
    // O merge operator antigo ainda lê a lista como era: nada foi tocado.
    let mut opts = rocksdb::Options::default();
    opts.set_merge_operator_associative("fire_flower_v2", concat);
    let db = rocksdb::DB::open(&opts, dir.path()).unwrap();
    assert_eq!(db.get(b"post:4").unwrap().unwrap(), [1u8, 7, 1, 1, 9, 1]);
}
