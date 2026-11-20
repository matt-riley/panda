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
        let themed = match color {
            ACCENT => crate::terminal_config::color("accent"),
            MUTED => crate::terminal_config::color("muted"),
            _ => None,
        };
        Style::default().fg(themed.unwrap_or(color))
    } else {
        Style::default()
    }
}

pub const ACCENT: Color = Color::Rgb(112, 201, 209);
pub const MUTED: Color = Color::Rgb(145, 158, 175);

pub struct PickerResults<'a> {
    pub hint: Option<&'a str>,
    pub ids: &'a [usize],
    pub pos: usize,
    pub preview: Option<&'a str>,
}
pub fn picker(
    frame: &mut Frame,
    title: &str,
    query: &str,
    labels: &[String],
    results: PickerResults,
    color: bool,
) {
    let PickerResults {
        ids,
        pos,
        preview,
        hint: key_hint,
    } = results;
    let area = frame.area();
    let preview_rows = if preview.is_some() { 6 } else { 0 };
    let height = area
        .height
        .min(labels.len().clamp(1, 10) as u16 + 6 + preview_rows);
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
    let [results, detail] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(preview_rows.min(results.height / 2 + u16::from(results.height > 2))),
    ])
    .areas(results);
    if let Some(preview) = preview {
        frame.render_widget(
            Paragraph::new(preview)
                .block(Block::default().borders(Borders::TOP).title(" Preview "))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            detail,
        );
    }
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
                    style = style.bg(crate::terminal_config::color("selection")
                        .unwrap_or(Color::Rgb(28, 52, 62)));
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
        Paragraph::new(
            key_hint
                .map(str::to_owned)
                .unwrap_or_else(crate::terminal_config::help),
        )
        .style(ink(MUTED, color)),
        hint,
    );
}
