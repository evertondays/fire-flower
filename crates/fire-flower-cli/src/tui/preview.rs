//! Prévia do documento: leitura do arquivo e destaque dos termos da consulta.

use std::path::Path;

use fire_flower::Stemmer;
use fire_flower::analysis::normalize_term;
use fire_flower::query::QueryTerm;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_normalization::char::is_combining_mark;

use super::{MAX_LINE_CHARS, MAX_PREVIEW_BYTES, PreviewContent};
use crate::style::{ACCENT, MUTED};



/// Se a palavra do texto casa com algum termo da consulta. Com stemming, o
/// termo exato é comparado com o radical da palavra; o prefixo, com a palavra
/// ou o radical (e com o radical do prefixo, ver [`QueryTerm::stem`]).
///
/// Com flexões na consulta (`inflector`), a palavra também casa um termo
/// exato do mesmo radical ("planetas" destaca "planeta").
fn matches_word(word: &str, terms: &[QueryTerm], stemmer: &Stemmer, inflector: &Stemmer) -> bool {
    let Some(folded) = normalize_term(word) else { return false };
    let stem = stemmer.is_on().then(|| stemmer.stem(&folded));
    let term = stem.as_deref().unwrap_or(&folded);
    let root = inflector.is_on().then(|| inflector.stem(&folded));
    terms.iter().any(|t| {
        if t.prefix {
            folded.starts_with(&t.text) || term.starts_with(&t.text) || t.stem.as_deref() == Some(term)
        } else {
            term == t.text || root.as_ref().is_some_and(|r| *r == inflector.stem(&t.text))
        }
    })
}

/// Lê o arquivo do rótulo e destaca as palavras que casam com a consulta.
pub(super) fn load_preview(path: &Path, terms: &[QueryTerm], stemmers: (&Stemmer, &Stemmer)) -> PreviewContent {
    if !path.is_file() {
        return PreviewContent::Unavailable("sem prévia: o rótulo não é um arquivo legível".into());
    }
    let bytes = match std::fs::read(path) {
        Ok(mut b) => {
            b.truncate(MAX_PREVIEW_BYTES);
            b
        }
        Err(e) => return PreviewContent::Unavailable(format!("sem prévia: {e}")),
    };
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    let number_width = total.to_string().len();

    let mut lines = Vec::with_capacity(total);
    let mut matches = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let raw: String = raw.chars().take(MAX_LINE_CHARS).map(|c| if c == '\t' { ' ' } else { c }).collect();
        let (spans, hit) = highlight(&raw, terms, stemmers);
        if hit {
            matches.push(i);
        }
        let mut line = vec![Span::styled(format!(" {:>number_width$} │ ", i + 1), Style::new().fg(MUTED))];
        line.extend(spans);
        lines.push(Line::from(line));
    }
    PreviewContent::File { lines, matches }
}

/// Divide a linha em palavras (mesma fronteira do tokenizador) e destaca as que casam.
pub(super) fn highlight(line: &str, terms: &[QueryTerm], (stemmer, inflector): (&Stemmer, &Stemmer)) -> (Vec<Span<'static>>, bool) {
    let is_word = |c: char| c.is_alphanumeric() || is_combining_mark(c);
    let hit_style = Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut hit = false;
    let mut plain_start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if !is_word(c) {
            continue;
        }
        let mut end = start + c.len_utf8();
        while let Some(&(i, c)) = chars.peek() {
            if !is_word(c) {
                break;
            }
            end = i + c.len_utf8();
            chars.next();
        }
        let word = &line[start..end];
        if matches_word(word, terms, stemmer, inflector) {
            if plain_start < start {
                spans.push(Span::raw(line[plain_start..start].to_string()));
            }
            spans.push(Span::styled(word.to_string(), hit_style));
            plain_start = end;
            hit = true;
        }
    }
    if plain_start < line.len() {
        spans.push(Span::raw(line[plain_start..].to_string()));
    }
    (spans, hit)
}
