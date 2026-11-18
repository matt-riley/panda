//! Native frame rendering for active turns and keyboard-driven selectors.
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::io::{self, Stdout};

pub fn terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    Ok(terminal)
}

pub fn ink(color: Color, enabled: bool) -> Style {
    if enabled {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

pub const ACCENT: Color = Color::Rgb(112, 201, 209);
pub const MUTED: Color = Color::Rgb(145, 158, 175);

pub fn picker(
    frame: &mut Frame,
    title: &str,
    query: &str,
    labels: &[String],
    ids: &[usize],
    pos: usize,
    color: bool,
) {
    let area = frame.area();
    let height = area.height.min(16).min((labels.len().min(10) as u16) + 6);
    let width = area.width.min(78);
    let card = Rect::new(
        (area.width - width) / 2,
        (area.height - height) / 2,
        width,
        height,
    );
    let accent = ink(ACCENT, color);
    let border = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .title(Line::from(format!(" {title} ")).style(accent.add_modifier(Modifier::BOLD)));
    let inside = border.inner(card);
    frame.render_widget(border, card);
    let [search, rule, results, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inside);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ❯ ", accent),
            Span::raw(query),
        ])),
        search,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width.into())).style(ink(MUTED, color)),
        rule,
    );
    let visible = usize::from(results.height);
    let start = pos
        .saturating_sub(visible / 2)
        .min(ids.len().saturating_sub(visible));
    let mut lines: Vec<_> = ids
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(n, &i)| {
            let selected = n == pos;
            let label = crate::ui::inline(&labels[i], 512);
            let line = Line::from(format!(" {} {label}", if selected { "›" } else { " " }));
            if selected {
                let mut style = accent.add_modifier(Modifier::BOLD);
                if color {
                    style = style.bg(Color::Rgb(28, 52, 62));
                }
                line.style(style)
            } else {
                line
            }
        })
        .collect();
    if ids.is_empty() {
        lines.push(Line::from(" No matches").style(ink(MUTED, color)));
    }
    frame.render_widget(Paragraph::new(lines), results);
    frame.render_widget(
        Paragraph::new(" ↑↓ move   Enter select   Esc close").style(ink(MUTED, color)),
        hint,
    );
}
