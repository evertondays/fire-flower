//! Cores e peças de tela comuns à busca interativa e ao painel do monitor.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType};

pub(crate) const ACCENT: Color = Color::Rgb(255, 122, 48);
pub(crate) const ACCENT_SOFT: Color = Color::Rgb(120, 60, 30);
pub(crate) const MUTED: Color = Color::DarkGray;

/// Moldura arredondada com título.
pub(crate) fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(title, Style::new().fg(Color::Gray).bold()))
}

/// Corta pela esquerda: em caminhos, o fim (nome do arquivo) é o que importa.
pub(crate) fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 2 {
        return s.to_string();
    }
    let skip = count - (max - 1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

/// 1234567 -> "1.234.567"
pub(crate) fn format_count(n: usize) -> String {
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
