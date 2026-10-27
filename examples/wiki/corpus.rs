//! Leitura do corpus JSONL da Wikipédia.

use crate::AnyResult;
use std::io::BufRead;
use fire_flower::Language;
use fire_flower::analysis::index_terms;
use fire_flower::Stopwords;
use std::path::Path;
use std::fs::File;
use std::io::BufReader;
use fire_flower::DocId;
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct Article {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) title: String,
    pub(crate) text: String,
}

pub(crate) struct Doc {
    /// Número da linha no corpus, começando em 1.
    pub(crate) line: u64,
    pub(crate) id: DocId,
    pub(crate) title: String,
    /// O texto sem o título (o motor indexa os dois, como campos).
    pub(crate) text: String,
    /// `title + "\n" + text`: para quem só quer o conjunto de termos.
    pub(crate) body: String,
    pub(crate) bytes: usize,
}

/// Lê o corpus linha a linha, aplicando `skip`/`take` e a troca de IDs.
pub(crate) struct Corpus {
    pub(crate) reader: BufReader<File>,
    pub(crate) line: u64,
    pub(crate) skip: u64,
    pub(crate) end: Option<u64>,
    pub(crate) remap_ids: bool,
    pub(crate) buf: Vec<u8>,
}

impl Corpus {
    pub(crate) fn open(path: &Path, skip: u64, take: Option<u64>, remap_ids: bool) -> AnyResult<Self> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            reader: BufReader::with_capacity(1 << 20, file),
            line: 0,
            skip,
            end: take.map(|t| skip + t),
            remap_ids,
            buf: Vec::new(),
        })
    }
}

impl Iterator for Corpus {
    type Item = AnyResult<Doc>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.end.is_some_and(|end| self.line >= end) {
                return None;
            }
            self.buf.clear();
            match self.reader.read_until(b'\n', &mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
            self.line += 1;
            if self.line <= self.skip || self.buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let article: Article = match serde_json::from_slice(&self.buf) {
                Ok(a) => a,
                Err(e) => return Some(Err(format!("linha {}: {e}", self.line).into())),
            };
            let mut body = String::with_capacity(article.title.len() + article.text.len() + 1);
            body.push_str(&article.title);
            body.push('\n');
            body.push_str(&article.text);
            let id = if self.remap_ids { self.line } else { article.id };
            return Some(Ok(Doc { line: self.line, id, title: article.title, text: article.text, body, bytes: self.buf.len() }));
        }
    }
}

/// Termos únicos de um documento, como o motor os indexa (sem stopwords).
pub(crate) fn unique_terms(body: &str, stopwords: &Stopwords) -> Vec<String> {
    index_terms(body, stopwords)
}

/// "pt,en", "en" ou "nenhum".
pub(crate) fn parse_languages(codes: &str) -> AnyResult<Vec<Language>> {
    if codes.trim().eq_ignore_ascii_case("nenhum") {
        return Ok(Vec::new());
    }
    codes
        .split(',')
        .map(|c| Language::from_code(c).ok_or_else(|| format!("idioma desconhecido: {c:?}").into()))
        .collect()
}

pub(crate) fn stem(path: &Path) -> String {
    path.file_stem().map_or("x".into(), |s| s.to_string_lossy().into_owned())
}
