//! Formato do banco e metadados gravados na criação (posições, idiomas,
//! stemming), e o merge operator das listas.

use std::path::Path;

use rocksdb::{DB, MergeOperands, Options, WriteBatch};

use crate::{Error, Result, codec};
use crate::codec::ListKind;
use crate::analysis::stopwords;
use crate::analysis::stem::Stemming;
use crate::analysis::stopwords::{Language, Stopwords};
use crate::engine::keys::{
    META_EDITS, META_FORMAT, META_LANGUAGES, META_MAYBE_EMPTY, META_NEXT_NODE, META_OVERLAYS, META_POSITIONS,
    META_STEMMING, POS, POST, POV, PSV, STEM,
};

/// Versão do formato gravado. Bancos de outra versão precisam ser recarregados.
pub const FORMAT_VERSION: u64 = 6;

/// O que o banco grava, decidido na criação ([`resolve_format`]).
pub(super) struct Features {
    /// Posições de cada termo (`pos:`), para frases.
    pub(super) positions: bool,
    /// Termos de cada documento (`fwd:`), para editar e remover.
    pub(super) edits: bool,
}

/// Confere a versão do formato e o que o banco grava (posições, `fwd:`).
/// Num banco novo, grava as três coisas.
pub(super) fn resolve_format(db: &DB, wanted_positions: Option<bool>, wanted_edits: Option<bool>) -> Result<Features> {
    match read_counter(db, META_FORMAT)? {
        Some(FORMAT_VERSION) => Ok(Features {
            positions: stored_flag(db, META_POSITIONS, wanted_positions, "posições")?,
            edits: stored_flag(db, META_EDITS, wanted_edits, "edição de documentos (fwd:)")?,
        }),
        Some(v) => Err(Error::Config(format!(
            "banco no formato {v}, mas este binário lê o formato {FORMAT_VERSION}; recarregue o banco"
        ))),
        None if read_counter(db, META_NEXT_NODE)?.is_some() => Err(old_format()),
        None => {
            let features =
                Features { positions: wanted_positions.unwrap_or(true), edits: wanted_edits.unwrap_or(true) };
            let mut wb = WriteBatch::default();
            wb.put(META_FORMAT, varint(FORMAT_VERSION));
            wb.put(META_POSITIONS, varint(u64::from(features.positions)));
            wb.put(META_EDITS, varint(u64::from(features.edits)));
            db.write(wb)?;
            Ok(features)
        }
    }
}

/// Uma opção ligada ou desligada na criação: a config pode repetir o que está
/// gravado, mas não pedir o contrário.
fn stored_flag(db: &DB, key: &[u8], wanted: Option<bool>, what: &str) -> Result<bool> {
    let name = String::from_utf8_lossy(key);
    let stored = read_counter(db, key)?.ok_or_else(|| Error::Corrupt(format!("{name} ausente")))? != 0;
    match wanted {
        Some(wanted) if wanted != stored => Err(Error::Config(format!(
            "o banco foi criado {} {what}, mas a config pede {}",
            if stored { "com" } else { "sem" },
            if wanted { "com" } else { "sem" }
        ))),
        _ => Ok(stored),
    }
}

fn old_format() -> Error {
    Error::Config("formato antigo (v1, sem tf nem posições); recarregue o banco".into())
}

/// O RocksDB grava as opções da última abertura em `OPTIONS-<n>`, inclusive o
/// nome do merge operator, que muda a cada formato.
pub(super) fn previous_merge_operator(path: &Path) -> Option<String> {
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
pub(super) fn stored_format_error(path: &Path) -> Option<Error> {
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
pub(super) fn resolve_languages(db: &DB, wanted: Option<&[Language]>) -> Result<Vec<Language>> {
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
pub(super) fn resolve_stemming(db: &DB, key: &[u8], wanted: Option<Stemming>) -> Result<Stemming> {
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

fn parse_languages(codes: &str) -> Result<Vec<Language>> {
    codes
        .split(',')
        .filter(|c| !c.trim().is_empty())
        .map(|c| Language::from_code(c).ok_or_else(|| Error::Corrupt(format!("idioma desconhecido no banco: {c:?}"))))
        .collect()
}

pub(super) fn read_counter(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    db.get_pinned(key)?.map(|v| codec::get_varint(&v)).transpose()
}

pub(super) fn varint(v: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    codec::put_varint(&mut buf, v);
    buf
}

/// Merge operator completo de `post:` e `pos:`: une as listas por documento,
/// com a gravação mais nova vencendo, e descarta as lápides (ver
/// `codec::merge_lists`). O RocksDB o chama com o valor base, ou sem base
/// quando sabe que não há valor mais antigo (leituras e compactação no último
/// nível). Devolver `None` (dado corrompido) faz a leitura falhar em vez de mentir.
pub(super) fn full_merge(key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    merge(key, existing, operands, true)
}

/// Merge operator parcial: só operandos consecutivos, e pode haver valor mais
/// antigo em outro nível. Une como o completo, mas guarda as lápides.
pub(super) fn partial_merge(key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    merge(key, existing, operands, false)
}

fn merge(key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands, full: bool) -> Option<Vec<u8>> {
    let started = crate::engine::merge_trace::trace_start();
    let inputs = existing.into_iter().chain(operands.iter());
    let result = if key.starts_with(POST) {
        codec::merge_lists(inputs, ListKind::Postings, full)
    } else if key.starts_with(POS) {
        codec::merge_lists(inputs, ListKind::Positions, full)
    } else if key.starts_with(POV) {
        // O remendo guarda as lápides nos dois merges: a base é outra chave.
        codec::merge_overlays(inputs, ListKind::Postings)
    } else if key.starts_with(PSV) {
        codec::merge_overlays(inputs, ListKind::Positions)
    } else if key.starts_with(STEM) || key == META_MAYBE_EMPTY || key == META_OVERLAYS {
        codec::merge_node_sets(inputs)
    } else {
        None
    };
    if let Some(started) = started {
        crate::engine::merge_trace::trace_record(full, existing, operands, result.as_deref(), started);
    }
    result
}
