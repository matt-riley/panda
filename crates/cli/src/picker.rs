//! Focused terminal selector used by :login and :model.
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use std::io;

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
    let mut terminal = crate::renderer::terminal()?;
    let color = std::env::var_os("NO_COLOR").is_none();
    loop {
        let ids: Vec<_> = labels
            .iter()
            .enumerate()
            .filter(|(_, s)| matches(s, &query))
            .map(|(i, _)| i)
            .collect();
        pos = pos.min(ids.len().saturating_sub(1));
        let height = terminal.size()?.height;
        let visible = usize::from(height.saturating_sub(5)).clamp(1, 10);
        terminal.draw(|f| crate::renderer::picker(f, title, &query, labels, &ids, pos, color))?;
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
