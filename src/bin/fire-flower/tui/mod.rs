//! Busca interativa no terminal: resultados por relevância enquanto você
//! digita, autocompletar da palavra atual e prévia com os termos destacados.

use std::io;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use fire_flower::{DocId, Engine, Hit};
use fire_flower::analysis::normalize_term;
use fire_flower::query::{Clause, QueryTerm};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Color;
use ratatui::text::Line;

use self::preview::load_preview;

mod draw;
mod preview;
#[cfg(test)]
mod tests;

const ACCENT: Color = Color::Rgb(255, 122, 48);
const ACCENT_SOFT: Color = Color::Rgb(120, 60, 30);
const MUTED: Color = Color::DarkGray;
const MAX_SUGGESTIONS: usize = 8;
/// A prévia só lê o começo de arquivos grandes.
const MAX_PREVIEW_BYTES: usize = 2 << 20;
/// Linhas longas (minificadas, base64) são cortadas antes do destaque.
const MAX_LINE_CHARS: usize = 400;
/// Quantos resultados ranqueados a lista mostra (o total aparece no cabeçalho).
const MAX_RESULTS: usize = 1000;

pub fn run(engine: &Engine, db_path: &str) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, App::new(engine, db_path));
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, mut app: App) -> io::Result<()> {
    app.refresh();
    while !app.quit {
        terminal.draw(|frame| app.draw(frame))?;
        // Espera o primeiro evento e drena os que já chegaram: quem digita
        // rápido (ou cola um texto) dispara uma busca só, não uma por tecla.
        let mut changed = app.on_event(event::read()?);
        while event::poll(Duration::ZERO)? {
            changed |= app.on_event(event::read()?);
        }
        if changed {
            app.refresh();
        }
    }
    Ok(())
}

struct App<'a> {
    engine: &'a Engine,
    db_path: String,
    query: String,
    /// O que de fato foi buscado (a última palavra vira prefixo enquanto se digita).
    effective: String,
    /// A consulta efetiva já separada em partes (termos e frases).
    clauses: Vec<Clause>,
    /// Todos os termos das partes, para o destaque.
    terms: Vec<QueryTerm>,
    /// Última palavra digitada, normalizada. É o que o autocompletar completa.
    current_word: Option<String>,
    suggestions: Vec<String>,
    suggestion: usize,
    /// Os melhores resultados, da maior nota para a menor.
    results: Vec<Hit>,
    /// Quantos documentos casaram ao todo.
    total: usize,
    selected: usize,
    offset: usize,
    elapsed: Duration,
    error: Option<String>,
    labels: HashMap<DocId, Option<String>>,
    preview: Option<Preview>,
    /// `Some(rolagem)` quando o documento está aberto em tela cheia.
    reader: Option<usize>,
    quit: bool,
}

/// Prévia do documento selecionado, calculada uma vez por (documento, consulta).
struct Preview {
    doc: DocId,
    effective: String,
    content: PreviewContent,
}

enum PreviewContent {
    Unavailable(String),
    File { lines: Vec<Line<'static>>, matches: Vec<usize> },
}

impl<'a> App<'a> {
    fn new(engine: &'a Engine, db_path: &str) -> Self {
        Self {
            engine,
            db_path: db_path.to_string(),
            query: String::new(),
            effective: String::new(),
            clauses: Vec::new(),
            terms: Vec::new(),
            current_word: None,
            suggestions: Vec::new(),
            suggestion: 0,
            results: Vec::new(),
            total: 0,
            selected: 0,
            offset: 0,
            elapsed: Duration::ZERO,
            error: None,
            labels: HashMap::new(),
            preview: None,
            reader: None,
            quit: false,
        }
    }

    // ---------------------------------------------------------------- estado

    /// Recalcula sugestões e resultados depois que a consulta mudou.
    fn refresh(&mut self) {
        let typing = !self.query.is_empty() && !self.query.ends_with(char::is_whitespace);
        let last = if typing { self.query.split_whitespace().last() } else { None };
        self.current_word = last.and_then(|w| normalize_term(w.trim_end_matches('*')));

        // Dentro de aspas abertas a palavra não vira prefixo: frase não tem prefixo.
        let open_quote = self.query.matches('"').count() % 2 == 1;
        self.effective = match last {
            Some(word) if !word.ends_with('*') && !word.ends_with('"') && !open_quote => {
                format!("{}*", self.query.trim_end())
            }
            _ => self.query.trim().to_string(),
        };
        self.clauses = self.engine.parse_query(&self.effective);
        self.terms = self.clauses.iter().flat_map(Clause::terms).collect();

        self.error = None;
        self.suggestions = match &self.current_word {
            Some(word) => self.engine.suggest(word, MAX_SUGGESTIONS).unwrap_or_else(|e| {
                self.error = Some(e.to_string());
                Vec::new()
            }),
            None => Vec::new(),
        };
        // A palavra digitada pode já existir ("op") e vir primeiro por ser a
        // mais curta; começar nela deixaria o Tab sem nada para completar.
        self.suggestion =
            self.suggestions.iter().position(|s| Some(s) != self.current_word.as_ref()).unwrap_or(0);

        let started = Instant::now();
        (self.results, self.total) = match self.engine.search_ranked(&self.effective, MAX_RESULTS) {
            Ok(top) => (top.hits, top.total),
            Err(e) => {
                self.error = Some(e.to_string());
                (Vec::new(), 0)
            }
        };
        self.elapsed = started.elapsed();
        self.selected = 0;
        self.offset = 0;
        self.reader = None;
    }

    fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            _ => false,
        }
    }

    /// Retorna `true` se a consulta mudou.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return false;
        }
        if let Some(scroll) = self.reader.as_mut() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.reader = None,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
                KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
                KeyCode::PageDown | KeyCode::Char(' ') => *scroll += 20,
                KeyCode::Char('n') => self.jump_to_match(true),
                KeyCode::Char('N') => self.jump_to_match(false),
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('w') if ctrl => {
                let trimmed = self.query.trim_end().len();
                self.query.truncate(trimmed);
                let cut = self.query.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.query.truncate(cut);
                return true;
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                return true;
            }
            KeyCode::Char('n') if ctrl => self.cycle_suggestion(1),
            KeyCode::Char('p') if ctrl => self.cycle_suggestion(self.suggestions.len().saturating_sub(1)),
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                return true;
            }
            KeyCode::Backspace => return self.query.pop().is_some(),
            KeyCode::Tab | KeyCode::Right => return self.accept_suggestion(),
            KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down => self.select(self.selected + 1),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(10)),
            KeyCode::PageDown => self.select(self.selected + 10),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Enter => {
                if !self.results.is_empty() {
                    self.ensure_preview();
                    if matches!(self.preview.as_ref().map(|p| &p.content), Some(PreviewContent::File { .. })) {
                        self.reader = Some(0);
                        self.jump_to_match(true);
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn cycle_suggestion(&mut self, step: usize) {
        if !self.suggestions.is_empty() {
            self.suggestion = (self.suggestion + step) % self.suggestions.len();
        }
    }

    /// Troca a palavra atual pela sugestão escolhida e começa a próxima.
    fn accept_suggestion(&mut self) -> bool {
        let Some(word) = self.suggestions.get(self.suggestion).cloned() else { return false };
        if self.current_word.is_none() {
            return false;
        }
        let cut = self.query.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        self.query.truncate(cut);
        self.query.push_str(&word);
        self.query.push(' ');
        true
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.results.len().saturating_sub(1));
    }

    fn jump_to_match(&mut self, forward: bool) {
        let Some(scroll) = self.reader else { return };
        let Some(Preview { content: PreviewContent::File { matches, .. }, .. }) = &self.preview else { return };
        let target = if forward {
            matches.iter().find(|&&m| m > scroll || (scroll == 0 && m == 0))
        } else {
            matches.iter().rev().find(|&&m| m < scroll)
        };
        if let Some(&line) = target {
            self.reader = Some(line);
        }
    }

    fn label(&mut self, doc: DocId) -> Option<String> {
        let engine = self.engine;
        self.labels.entry(doc).or_insert_with(|| engine.label(doc).ok().flatten()).clone()
    }

    fn ensure_preview(&mut self) {
        let Some(doc) = self.results.get(self.selected).map(|h| h.doc) else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|p| p.doc == doc && p.effective == self.effective) {
            return;
        }
        let content = match self.label(doc) {
            Some(label) => load_preview(Path::new(&label), &self.terms, (self.engine.stemmer(), self.engine.inflector())),
            None => PreviewContent::Unavailable("documento sem rótulo".into()),
        };
        self.preview = Some(Preview { doc, effective: self.effective.clone(), content });
    }
}
