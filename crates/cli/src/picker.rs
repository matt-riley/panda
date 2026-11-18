//! Focused terminal selector used by /login and /model.
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write};

fn matches(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query.to_lowercase().split_whitespace().all(|word| {
        let mut chars = text.chars();
        word.chars().all(|c| chars.by_ref().any(|v| v == c))
    })
}

pub fn choose(title: &str, labels: &[String], selected: usize) -> io::Result<Option<usize>> {
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    let _restore = Restore;
    let mut query = String::new();
    let mut pos = selected;
    let mut out = io::stdout().lock();
    loop {
        let ids: Vec<_> = labels
            .iter()
            .enumerate()
            .filter(|(_, s)| matches(s, &query))
            .map(|(i, _)| i)
            .collect();
        pos = pos.min(ids.len().saturating_sub(1));
        let (width, height) = terminal::size()?;
        let visible = usize::from(height.saturating_sub(4)).clamp(1, 10);
        let start = pos
            .saturating_sub(visible / 2)
            .min(ids.len().saturating_sub(visible));
        execute!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        let line = |text: &str| crate::ui::inline(text, usize::from(width.saturating_sub(1)));
        write!(
            out,
            "{}\r\n{}\r\n",
            line(&format!("{title}: {query}")),
            line("↑ ↓ move · Enter select · Esc cancel")
        )?;
        for (n, i) in ids.iter().enumerate().skip(start).take(visible) {
            write!(
                out,
                "{}\r\n",
                line(&format!(
                    "{} {}",
                    if n == pos { "→" } else { " " },
                    labels[*i]
                ))
            )?;
        }
        if ids.is_empty() {
            write!(out, "No matches\r\n")?;
        }
        out.flush()?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind == KeyEventKind::Release {
            continue;
        }
        match k.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Enter if !ids.is_empty() => return Ok(Some(ids[pos])),
            KeyCode::Up if !ids.is_empty() => pos = (pos + ids.len() - 1) % ids.len(),
            KeyCode::Down if !ids.is_empty() => pos = (pos + 1) % ids.len(),
            KeyCode::PageDown => pos = pos.saturating_add(visible),
            KeyCode::PageUp => pos = pos.saturating_sub(visible),
            KeyCode::Backspace => {
                query.pop();
                pos = 0;
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                query.push(c);
                pos = 0;
            }
            _ => {}
        }
    }
}
