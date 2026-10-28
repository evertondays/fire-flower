//! Desenho da tela: cabeçalho, busca, sugestões, resultados, prévia e rodapé.

use fire_flower::Hit;
use fire_flower::query::Clause;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};

use super::{ACCENT, ACCENT_SOFT, App, MUTED, Preview, PreviewContent};


impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
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
                Span::styled(format_count(self.total), Style::new().fg(ACCENT).bold()),
                Span::raw(if self.total == 1 { " resultado" } else { " resultados" }),
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
                "digite para buscar…  (palavras com E, termo* para prefixo, \"frase exata\")",
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
    pub(super) fn ghost(&self) -> Option<String> {
        let word = self.current_word.as_ref()?;
        let suggestion = self.suggestions.get(self.suggestion)?;
        let rest = suggestion.strip_prefix(word.as_str())?;
        (!rest.is_empty()).then(|| rest.to_string())
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.error {
            Some(err) => Line::from(Span::styled(format!(" ⚠ {err}"), Style::new().fg(Color::Red))),
            None if self.effective.is_empty() => Line::default(),
            None => {
                let mut spans = vec![Span::styled(" consulta: ", Style::new().fg(MUTED))];
                for (i, clause) in self.clauses.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(" E ", Style::new().fg(MUTED)));
                    }
                    spans.push(match clause {
                        Clause::Term(t) => Span::styled(
                            format!("{}{}", t.text, if t.prefix { "*" } else { "" }),
                            Style::new().fg(Color::Gray),
                        ),
                        // A frase é uma unidade: entre aspas, com `_` nas stopwords.
                        Clause::Phrase(_) => Span::styled(describe_phrase(clause), Style::new().fg(ACCENT)),
                    });
                }
                if self.clauses.is_empty() {
                    spans.push(Span::styled("(só stopwords)", Style::new().fg(MUTED).italic()));
                }
                Line::from(spans)
            }
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
        let visible: Vec<Hit> = self.results[self.offset..end].to_vec();
        let id_width = visible.iter().map(|h| h.doc.to_string().len()).max().unwrap_or(0).max(3);
        let label_width = (area.width as usize).saturating_sub(id_width + 14);
        let items: Vec<ListItem> = visible
            .into_iter()
            .map(|hit| {
                let label = self.label(hit.doc).unwrap_or_else(|| "(sem rótulo)".into());
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>id_width$} ", hit.doc), Style::new().fg(MUTED)),
                    Span::styled(format!("{:>6.2} ", hit.score), Style::new().fg(ACCENT)),
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
        let doc = self.results.get(self.selected).map(|h| h.doc);
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
                ("^R", "recarregar"),
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


/// `"copa _ mundo"`: a frase com um `_` por stopword pulada.
pub(super) fn describe_phrase(clause: &Clause) -> String {
    let Clause::Phrase(parts) = clause else { return String::new() };
    let mut words = Vec::new();
    let mut next = 0;
    for part in parts {
        while next < part.offset {
            words.push("_".to_string());
            next += 1;
        }
        words.push(part.text.clone());
        next = part.offset + 1;
    }
    format!("\"{}\"", words.join(" "))
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(title, Style::new().fg(Color::Gray).bold()))
}


/// Corta pela esquerda: em caminhos, o fim (nome do arquivo) é o que importa.
pub(super) fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 2 {
        return s.to_string();
    }
    let skip = count - (max - 1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

/// 1234567 -> "1.234.567"
pub(super) fn format_count(n: usize) -> String {
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
