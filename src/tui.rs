//! Busca interativa no terminal: resultados enquanto você digita,
//! autocompletar da palavra atual e prévia com os termos destacados.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use fire_flower::text::{normalize_term, tokenize};
use fire_flower::{DocId, Engine};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_normalization::char::is_combining_mark;

const ACCENT: Color = Color::Rgb(255, 122, 48);
const ACCENT_SOFT: Color = Color::Rgb(120, 60, 30);
const MUTED: Color = Color::DarkGray;
const MAX_SUGGESTIONS: usize = 8;
/// A prévia só lê o começo de arquivos grandes.
const MAX_PREVIEW_BYTES: usize = 2 << 20;
/// Linhas longas (minificadas, base64) são cortadas antes do destaque.
const MAX_LINE_CHARS: usize = 400;

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

/// Um termo da consulta efetiva, já normalizado.
struct QueryTerm {
    text: String,
    prefix: bool,
}

struct App<'a> {
    engine: &'a Engine,
    db_path: String,
    query: String,
    /// O que de fato foi buscado (a última palavra vira prefixo enquanto se digita).
    effective: String,
    terms: Vec<QueryTerm>,
    /// Última palavra digitada, normalizada. É o que o autocompletar completa.
    current_word: Option<String>,
    suggestions: Vec<String>,
    suggestion: usize,
    results: Vec<DocId>,
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
            terms: Vec::new(),
            current_word: None,
            suggestions: Vec::new(),
            suggestion: 0,
            results: Vec::new(),
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

        self.effective = match last {
            Some(word) if !word.ends_with('*') => format!("{}*", self.query.trim_end()),
            _ => self.query.trim().to_string(),
        };
        self.terms = parse_terms(&self.effective);

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
        self.results = match self.engine.search(&self.effective) {
            Ok(docs) => docs,
            Err(e) => {
                self.error = Some(e.to_string());
                Vec::new()
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
        let Some(&doc) = self.results.get(self.selected) else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|p| p.doc == doc && p.effective == self.effective) {
            return;
        }
        let content = match self.label(doc) {
            Some(label) => load_preview(Path::new(&label), &self.terms),
            None => PreviewContent::Unavailable("documento sem rótulo".into()),
        };
        self.preview = Some(Preview { doc, effective: self.effective.clone(), content });
    }

    // ----------------------------------------------------------------- desenho

    fn draw(&mut self, frame: &mut Frame) {
        let [header, search, status, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.draw_header(frame, header);
        self.draw_search(frame, search);
        self.draw_status(frame, status);
        self.ensure_preview();
        if self.reader.is_some() {
            self.draw_reader(frame, body);
        } else {
            let [top, bottom] =
                Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
            let [suggestions, results] =
                Layout::horizontal([Constraint::Length(26), Constraint::Min(20)]).areas(top);
            self.draw_suggestions(frame, suggestions);
            self.draw_results(frame, results);
            self.draw_preview(frame, bottom);
        }
        self.draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let left = Line::from(vec![
            Span::styled(" ✿ fire-flower ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::styled(format!("  {}", self.db_path), Style::new().fg(MUTED)),
        ]);
        let right = if self.effective.is_empty() {
            Line::default()
        } else {
            Line::from(vec![
                Span::styled(format_count(self.results.len()), Style::new().fg(ACCENT).bold()),
                Span::raw(if self.results.len() == 1 { " resultado" } else { " resultados" }),
                Span::styled(format!("  ·  {:.2?} ", self.elapsed), Style::new().fg(MUTED)),
            ])
        };
        frame.render_widget(Paragraph::new(left), area);
        frame.render_widget(Paragraph::new(right).right_aligned(), area);
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(" Busca ", Style::new().fg(ACCENT).bold()));
        let mut spans = vec![Span::styled("› ", Style::new().fg(ACCENT).bold())];
        if self.query.is_empty() {
            spans.push(Span::styled(
                "digite para buscar…  (palavras com E, termo* para prefixo)",
                Style::new().fg(MUTED).italic(),
            ));
        } else {
            spans.push(Span::raw(self.query.clone()));
            if let Some(ghost) = self.ghost() {
                spans.push(Span::styled(ghost, Style::new().fg(MUTED)));
            }
        }
        let inner = block.inner(area);
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
        let x = inner.x + 2 + self.query.chars().count() as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }

    /// Resto da sugestão escolhida, mostrado em cinza depois do cursor.
    fn ghost(&self) -> Option<String> {
        let word = self.current_word.as_ref()?;
        let suggestion = self.suggestions.get(self.suggestion)?;
        let rest = suggestion.strip_prefix(word.as_str())?;
        (!rest.is_empty()).then(|| rest.to_string())
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.error {
            Some(err) => Line::from(Span::styled(format!(" ⚠ {err}"), Style::new().fg(Color::Red))),
            None if self.effective.is_empty() => Line::default(),
            None => Line::from(vec![
                Span::styled(" consulta: ", Style::new().fg(MUTED)),
                Span::styled(self.effective.clone(), Style::new().fg(Color::Gray)),
            ]),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_suggestions(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Sugestões ");
        if self.suggestions.is_empty() {
            let hint = if self.current_word.is_some() { "nenhuma palavra" } else { "—" };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {hint}"), Style::new().fg(MUTED))).block(block),
                area,
            );
            return;
        }
        let typed = self.current_word.as_deref().unwrap_or("");
        let items: Vec<ListItem> = self
            .suggestions
            .iter()
            .map(|s| {
                let rest = s.strip_prefix(typed).unwrap_or(s);
                ListItem::new(Line::from(vec![
                    Span::styled(typed.to_string(), Style::new().fg(ACCENT)),
                    Span::raw(rest.to_string()),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::new().bg(ACCENT_SOFT).add_modifier(Modifier::BOLD));
        let mut state = ListState::default().with_selected(Some(self.suggestion));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_results(&mut self, frame: &mut Frame, area: Rect) {
        let block = panel(" Resultados ");
        if self.results.is_empty() {
            let msg = if self.effective.is_empty() { " comece a digitar" } else { " nada encontrado" };
            frame.render_widget(
                Paragraph::new(Span::styled(msg, Style::new().fg(MUTED))).block(block),
                area,
            );
            return;
        }

        // Só os rótulos visíveis são lidos do banco: com 1 milhão de
        // resultados, montar a lista inteira travaria a interface.
        let height = block.inner(area).height.max(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        let end = (self.offset + height).min(self.results.len());
        let id_width = self.results[end - 1].to_string().len().max(3);
        let label_width = (area.width as usize).saturating_sub(id_width + 6);

        let visible: Vec<DocId> = self.results[self.offset..end].to_vec();
        let items: Vec<ListItem> = visible
            .into_iter()
            .map(|doc| {
                let label = self.label(doc).unwrap_or_else(|| "(sem rótulo)".into());
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{doc:>id_width$} "), Style::new().fg(MUTED)),
                    Span::raw(truncate_left(&label, label_width)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::new().bg(ACCENT_SOFT).add_modifier(Modifier::BOLD));
        let mut state = ListState::default().with_selected(Some(self.selected - self.offset));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_preview(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Prévia ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        match self.preview.as_ref().map(|p| &p.content) {
            None => {}
            Some(PreviewContent::Unavailable(why)) => {
                frame.render_widget(Paragraph::new(Span::styled(format!(" {why}"), Style::new().fg(MUTED))), inner);
            }
            Some(PreviewContent::File { lines, matches }) => {
                let shown: Vec<Line> = if matches.is_empty() {
                    lines.iter().take(inner.height as usize).cloned().collect()
                } else {
                    matches.iter().take(inner.height as usize).map(|&i| lines[i].clone()).collect()
                };
                frame.render_widget(Paragraph::new(shown), inner);
            }
        }
    }

    fn draw_reader(&mut self, frame: &mut Frame, area: Rect) {
        let doc = self.results.get(self.selected).copied();
        let label = doc.and_then(|d| self.label(d)).unwrap_or_default();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(format!(" {} ", truncate_left(&label, area.width as usize - 6)), Style::new().bold()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(Preview { content: PreviewContent::File { lines, matches }, .. }) = &self.preview else { return };

        let max_scroll = lines.len().saturating_sub(inner.height as usize);
        let scroll = self.reader.unwrap_or(0).min(max_scroll);
        self.reader = Some(scroll);
        let position = Line::from(Span::styled(
            format!(" linha {}/{} · {} com ocorrências ", scroll + 1, lines.len(), matches.len()),
            Style::new().fg(MUTED),
        ));
        frame.render_widget(Paragraph::new(position).right_aligned(), Rect { height: 1, ..area });
        let shown: Vec<Line> = lines.iter().skip(scroll).take(inner.height as usize).cloned().collect();
        frame.render_widget(Paragraph::new(shown), inner);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.reader.is_some() {
            &[("↑↓/PgUp/PgDn", "rolar"), ("n/N", "próxima/anterior ocorrência"), ("Esc", "voltar")]
        } else {
            &[
                ("Tab", "completar"),
                ("^N/^P", "trocar sugestão"),
                ("↑↓", "resultado"),
                ("Enter", "abrir"),
                ("^W", "apagar"),
                ("Esc", "sair"),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        for (key, action) in keys {
            spans.push(Span::styled(*key, Style::new().fg(ACCENT).bold()));
            spans.push(Span::styled(format!(" {action}   "), Style::new().fg(MUTED)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(title, Style::new().fg(Color::Gray).bold()))
}

/// Separa a consulta em termos normalizados, com a mesma regra da busca.
fn parse_terms(query: &str) -> Vec<QueryTerm> {
    let mut terms = Vec::new();
    for piece in query.split_whitespace() {
        let (piece, prefix) = match piece.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (piece, false),
        };
        let tokens = tokenize(piece);
        let last = tokens.len().saturating_sub(1);
        for (i, text) in tokens.into_iter().enumerate() {
            terms.push(QueryTerm { text, prefix: prefix && i == last });
        }
    }
    terms
}

fn matches_term(word: &str, terms: &[QueryTerm]) -> bool {
    terms.iter().any(|t| if t.prefix { word.starts_with(&t.text) } else { word == t.text })
}

/// Lê o arquivo do rótulo e destaca as palavras que casam com a consulta.
fn load_preview(path: &Path, terms: &[QueryTerm]) -> PreviewContent {
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
        let (spans, hit) = highlight(&raw, terms);
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
fn highlight(line: &str, terms: &[QueryTerm]) -> (Vec<Span<'static>>, bool) {
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
        if normalize_term(word).is_some_and(|w| matches_term(&w, terms)) {
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

/// Corta pela esquerda: em caminhos, o fim (nome do arquivo) é o que importa.
fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 2 {
        return s.to_string();
    }
    let skip = count - (max - 1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

/// 1234567 -> "1.234.567"
fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_marks_normalized_matches() {
        let terms = parse_terms("coracao bras*");
        let (spans, hit) = highlight("O Coração do Brasil, não do Chile.", &terms);
        assert!(hit);
        let marked: Vec<&str> =
            spans.iter().filter(|s| s.style.bg == Some(ACCENT)).map(|s| s.content.as_ref()).collect();
        assert_eq!(marked, ["Coração", "Brasil"]);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "O Coração do Brasil, não do Chile.");
    }

    /// Desenha a tela num terminal virtual. Rode com `--nocapture` para ver.
    #[test]
    fn renders_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("brasil.txt");
        std::fs::write(&doc, "Introdução\nO coração do Brasil bate forte.\nBrasília é a capital.\n").unwrap();
        let engine = Engine::open(dir.path().join("db"), fire_flower::Config::default()).unwrap();
        let mut batch = engine.batch();
        batch.add_labeled(&doc.display().to_string(), &std::fs::read_to_string(&doc).unwrap()).unwrap();
        batch.add_labeled("outro.txt", "brasileiro bravo").unwrap();
        batch.commit().unwrap();

        let mut app = App::new(&engine, "/tmp/db");
        for c in "coração bra".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.refresh();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .chunks(90)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();
        println!("{screen}");

        assert_eq!(app.effective, "coração bra*");
        assert_eq!(app.results.len(), 1, "só o primeiro tem coração");
        assert_eq!(app.suggestions, ["bravo", "brasil", "brasilia", "brasileiro"]);
        assert!(screen.contains("✿ fire-flower"));
        assert!(screen.contains("coração bra"));

        // Tab completa a palavra atual com a sugestão escolhida.
        app.on_key(KeyEvent::from(KeyCode::Char('s')));
        app.refresh();
        assert_eq!(app.ghost().as_deref(), Some("il"));
        assert!(app.on_key(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(app.query, "coração brasil ");

        // "brasil" existe e é a sugestão mais curta, mas o Tab deve ir além dela.
        app.query = "brasil".into();
        app.refresh();
        assert_eq!(app.suggestions[0], "brasil");
        assert_eq!(app.ghost().as_deref(), Some("ia"));
    }

    #[test]
    fn helpers() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1234567), "1.234.567");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_left("abc", 4), "abc");
    }
}
