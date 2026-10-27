//! Formato do banco e metadados gravados na criação (posições, idiomas,
//! stemming), e o merge operator das listas.

use crate::Result;
use crate::engine::keys::STEM;
use crate::engine::keys::POS;
use crate::engine::keys::POST;
use rocksdb::MergeOperands;
use crate::codec;
use crate::engine::keys::META_STEMMING;
use crate::analysis::stem::Stemming;
use crate::analysis::stopwords;
use crate::engine::keys::META_LANGUAGES;
use crate::analysis::stopwords::Stopwords;
use crate::analysis::stopwords::Language;
use rocksdb::Options;
use std::path::Path;
use rocksdb::WriteBatch;
use crate::engine::keys::META_NEXT_NODE;
use crate::Error;
use crate::engine::keys::META_POSITIONS;
use crate::engine::keys::META_FORMAT;
use rocksdb::DB;

/// Versão do formato gravado. Bancos de outra versão precisam ser recarregados.
pub const FORMAT_VERSION: u64 = 5;

/// Confere a versão do formato e se o banco tem posições. Num banco novo,
/// grava as duas coisas.
pub(crate) fn resolve_format(db: &DB, wanted_positions: Option<bool>) -> Result<bool> {
    match read_counter(db, META_FORMAT)? {
        Some(FORMAT_VERSION) => {
            let stored = read_counter(db, META_POSITIONS)?
                .ok_or_else(|| Error::Corrupt("meta:positions ausente".into()))?
                != 0;
            match wanted_positions {
                Some(wanted) if wanted != stored => Err(Error::Config(format!(
                    "o banco foi criado {} posições, mas a config pede {}",
                    if stored { "com" } else { "sem" },
                    if wanted { "com" } else { "sem" }
                ))),
                _ => Ok(stored),
            }
        }
        Some(v) => Err(Error::Config(format!(
            "banco no formato {v}, mas este binário lê o formato {FORMAT_VERSION}; recarregue o banco"
        ))),
        None if read_counter(db, META_NEXT_NODE)?.is_some() => Err(old_format()),
        None => {
            let positions = wanted_positions.unwrap_or(true);
            let mut wb = WriteBatch::default();
            wb.put(META_FORMAT, varint(FORMAT_VERSION));
            wb.put(META_POSITIONS, varint(u64::from(positions)));
            db.write(wb)?;
            Ok(positions)
        }
    }
}

pub(crate) fn old_format() -> Error {
    Error::Config("formato antigo (v1, sem tf nem posições); recarregue o banco".into())
}

/// O RocksDB grava as opções da última abertura em `OPTIONS-<n>`, inclusive o
/// nome do merge operator, que muda a cada formato.
pub(crate) fn previous_merge_operator(path: &Path) -> Option<String> {
    let entries = std::fs::read_dir(path).ok()?;
    let (_, latest) = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u64 = name.strip_prefix("OPTIONS-")?.parse().ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)?;
    let options = std::fs::read_to_string(latest).ok()?;
    let name = options.lines().find_map(|l| l.trim().strip_prefix("merge_operator="))?;
    // Sem merge operator o RocksDB grava "nullptr" (ou nada).
    (name != "nullptr" && !name.is_empty()).then(|| name.to_string())
}

/// Lê o formato gravado sem abrir para escrita (sem merge operator, sem
/// recuperar o WAL) e monta o erro: v1 (sem `meta:format`) ou outra versão.
pub(crate) fn stored_format_error(path: &Path) -> Option<Error> {
    let db = DB::open_for_read_only(&Options::default(), path, false).ok()?;
    match read_counter(&db, META_FORMAT).ok()? {
        None if read_counter(&db, META_NEXT_NODE).ok()?.is_some() => Some(old_format()),
        Some(v) if v != FORMAT_VERSION => Some(Error::Config(format!(
            "banco no formato {v}, mas este binário lê o formato {FORMAT_VERSION}; recarregue o banco"
        ))),
        _ => None,
    }
}

/// Idiomas das stopwords do banco. Num banco sem a chave, grava os da config
/// (ou o padrão).
pub(crate) fn resolve_languages(db: &DB, wanted: Option<&[Language]>) -> Result<Vec<Language>> {
    let normalize = |langs: &[Language]| Stopwords::new(langs).languages().to_vec();
    let stored = match db.get_pinned(META_LANGUAGES)? {
        Some(v) => Some(parse_languages(&String::from_utf8_lossy(&v))?),
        None => None,
    };
    let languages = match (stored, wanted) {
        (Some(stored), Some(wanted)) if normalize(wanted) != stored => {
            return Err(Error::Config(format!(
                "o banco foi criado com stopwords de [{}], mas a config pede [{}]",
                stopwords::describe(&stored),
                stopwords::describe(&normalize(wanted))
            )));
        }
        (Some(stored), _) => stored,
        (None, wanted) => normalize(wanted.unwrap_or(Language::DEFAULT)),
    };
    if db.get_pinned(META_LANGUAGES)?.is_none() {
        let codes: Vec<&str> = languages.iter().map(|l| l.code()).collect();
        db.put(META_LANGUAGES, codes.join(","))?;
    }
    Ok(languages)
}

/// Stemming do banco (`key` = `meta:stemming` ou `meta:inflections`). Num
/// banco sem a chave, grava o da config (ou `Off`); um banco que já tem dados
/// e não tem a chave é de antes dela: `Off`.
pub(crate) fn resolve_stemming(db: &DB, key: &[u8], wanted: Option<Stemming>) -> Result<Stemming> {
    let what = if key == META_STEMMING { "stemming" } else { "flexões na consulta" };
    let stored = match db.get_pinned(key)? {
        Some(v) => {
            let code = String::from_utf8_lossy(&v);
            Some(Stemming::from_code(&code).ok_or_else(|| Error::Corrupt(format!("{what} desconhecido no banco: {code:?}")))?)
        }
        None if read_counter(db, META_NEXT_NODE)?.is_some() => Some(Stemming::Off),
        None => None,
    };
    let stemming = match (stored, wanted) {
        (Some(stored), Some(wanted)) if stored != wanted => {
            return Err(Error::Config(format!(
                "o banco foi criado com {what} {stored}, mas a config pede {wanted}; recarregue o banco"
            )));
        }
        (Some(stored), _) => stored,
        (None, wanted) => wanted.unwrap_or_default(),
    };
    if db.get_pinned(key)?.is_none() {
        db.put(key, stemming.code())?;
    }
    Ok(stemming)
}

pub(crate) fn parse_languages(codes: &str) -> Result<Vec<Language>> {
    codes
        .split(',')
        .filter(|c| !c.trim().is_empty())
        .map(|c| Language::from_code(c).ok_or_else(|| Error::Corrupt(format!("idioma desconhecido no banco: {c:?}"))))
        .collect()
}

pub(crate) fn read_counter(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    db.get_pinned(key)?.map(|v| codec::get_varint(&v)).transpose()
}

pub(crate) fn varint(v: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    codec::put_varint(&mut buf, v);
    buf
}

/// Merge operator de `post:` e `pos:`: une as listas por documento, com a
/// gravação mais nova vencendo (ver `codec::merge_postings`). É associativo,
/// então o RocksDB pode combinar pedaços consecutivos em qualquer agrupamento.
/// Devolver `None` (dado corrompido) faz a leitura falhar em vez de mentir.
pub(crate) fn merge_operator(key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    let inputs = existing.into_iter().chain(operands.iter());
    if key.starts_with(POST) {
        codec::merge_postings(inputs)
    } else if key.starts_with(POS) {
        codec::merge_positions(inputs)
    } else if key.starts_with(STEM) {
        codec::merge_node_sets(inputs)
    } else {
        None
    }
}
