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

struct Hit {
    index: usize,
    preview: String,
}

pub fn choose(title: &str, labels: &[String], selected: usize) -> io::Result<Option<usize>> {
    pick(title, labels, selected, false, |query| {
        labels
            .iter()
            .enumerate()
            .filter(|(_, text)| matches(text, query))
            .map(|(index, _)| Hit {
                index,
                preview: String::new(),
            })
            .collect()
    })
}

fn pick(
    title: &str,
    labels: &[String],
    selected: usize,
    preview: bool,
    search: impl Fn(&str) -> Vec<Hit>,
) -> io::Result<Option<usize>> {
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    let _restore = Restore;
    let mut state = Selection {
        query: String::new(),
        pos: selected,
    };
    let mut hits = search("");
    let mut terminal = crate::renderer::terminal()?;
    let color = std::env::var_os("NO_COLOR").is_none();
    loop {
        let ids: Vec<_> = hits.iter().map(|h| h.index).collect();
        state.pos = state.pos.min(ids.len().saturating_sub(1));
        let height = terminal.size()?.height;
        let visible = usize::from(height.saturating_sub(if preview { 11 } else { 5 })).clamp(1, 10);
        let results = crate::renderer::PickerResults {
            ids: &ids,
            pos: state.pos,
            preview: preview.then(|| hits.get(state.pos).map_or("", |hit| hit.preview.as_str())),
        };
        terminal
            .draw(|f| crate::renderer::picker(f, title, &state.query, labels, results, color))?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        match state.key(k, &ids, visible) {
            Action::Close(id) => return Ok(id),
            Action::Filter => hits = search(&state.query),
            Action::Stay => {}
        }
    }
}

#[derive(Default)]
struct Selection {
    query: String,
    pos: usize,
}

#[derive(Debug, PartialEq)]
enum Action {
    Close(Option<usize>),
    Filter,
    Stay,
}

impl Selection {
    fn key(&mut self, k: event::KeyEvent, ids: &[usize], visible: usize) -> Action {
        if k.kind == KeyEventKind::Release {
            return Action::Stay;
        }
        match k.code {
            KeyCode::Esc => return Action::Close(None),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close(None);
            }
            KeyCode::Enter if !ids.is_empty() => return Action::Close(Some(ids[self.pos])),
            KeyCode::Up if !ids.is_empty() => self.pos = (self.pos + ids.len() - 1) % ids.len(),
            KeyCode::Down if !ids.is_empty() => self.pos = (self.pos + 1) % ids.len(),
            KeyCode::PageDown => {
                self.pos = self
                    .pos
                    .saturating_add(visible)
                    .min(ids.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.pos = self.pos.saturating_sub(visible),
            KeyCode::Backspace => {
                use unicode_segmentation::UnicodeSegmentation;
                if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(start);
                }
                self.pos = 0;
                return Action::Filter;
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.pos = 0;
                return Action::Filter;
            }
            _ => {}
        }
        Action::Stay
    }
}

use panda_local::store::SessionPreview;

pub fn resume(sessions: &[SessionPreview]) -> io::Result<Option<i64>> {
    let labels: Vec<_> = sessions
        .iter()
        .map(|s| {
            let title = if s.name.is_empty() {
                s.messages
                    .first()
                    .map_or("Empty session", |(_, text)| text.as_str())
            } else {
                &s.name
            };
            format!("#{}  {}", s.id, crate::ui::inline(title, 80))
        })
        .collect();
    let selected = pick("Resume session", &labels, 0, true, |query| {
        let query = query.trim().to_lowercase();
        sessions
            .iter()
            .enumerate()
            .filter_map(|(index, s)| {
                session_preview(s, &query).map(|preview| Hit { index, preview })
            })
            .collect()
    })?;
    Ok(selected.map(|index| sessions[index].id))
}

fn session_preview(session: &SessionPreview, query: &str) -> Option<String> {
    let messages = &session.messages;
    let mut start = messages.len().saturating_sub(2);
    let mut offset = 0;
    if !query.is_empty() {
        if let Some((index, pos)) = messages
            .iter()
            .enumerate()
            .find_map(|(i, (_, text))| text.to_lowercase().find(query).map(|pos| (i, pos)))
        {
            start = index;
            // A lowercase character can occupy a different number of UTF-8 bytes
            // than the original. Locate the match in the original before slicing.
            let original = &messages[index].1;
            let mut folded = 0;
            let head = original
                .char_indices()
                .find(|(_, c)| {
                    folded += c.to_lowercase().map(char::len_utf8).sum::<usize>();
                    folded > pos
                })
                .map_or(0, |(byte, _)| byte);
            let preceding = &original[..head];
            offset = preceding
                .char_indices()
                .rev()
                .take_while(|(_, c)| *c != '\n')
                .take(18)
                .last()
                .map_or(head, |(byte, _)| byte);
        } else if !session.name.to_lowercase().contains(query) && session.id.to_string() != query {
            return None;
        }
    }
    if messages.is_empty() {
        return Some("No conversation yet".into());
    }
    Some(
        messages[start..]
            .iter()
            .take(2)
            .enumerate()
            .map(|(i, (role, text))| {
                let from = if i == 0 { offset } else { 0 };
                format!(
                    "{role}: {}{}",
                    if from > 0 { "…" } else { "" },
                    crate::ui::inline(&text[from..], 220)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn session(id: i64, name: &str, reply: &str) -> SessionPreview {
        SessionPreview {
            id,
            name: name.into(),
            messages: vec![("Panda", reply.into())],
        }
    }

    #[test]
    fn excerpt_reaches_a_late_match_after_unicode_and_removes_terminal_controls() {
        let text = format!(
            "{}İ🐼\n\x1b[2J Found the Needle here",
            "unrelated ".repeat(800)
        );
        let entry = session(3, "duplicate", &text);
        let preview = session_preview(&entry, "needle").unwrap();
        assert!(preview.contains("Found the Needle here"));
        assert!(!preview.contains('\x1b'));
        assert!(preview.len() < 100);
        assert!(session_preview(&entry, "duplicate").is_some());
    }

    proptest! {
        #[test]
        fn resume_picker_key_sequences(actions in prop::collection::vec(0u8..16, 1..80)) {
            // Duplicate names and different transcript matches exercise selection
            // by stable identity after the filtered results have been reordered.
            let entries = [session(18,"Alpha","🐼é"),session(12,"Alpha","beta"),session(7,"Beta","alpha")];
            let mut picker = Selection::default();
            let mut query = String::new();
            let mut position = 0usize;
            for action in actions {
                let needle = query.to_lowercase();
                let ids: Vec<_> = entries.iter().enumerate().filter_map(|(i,s)| session_preview(s,&needle).map(|_|i)).collect();
                let expected: Vec<_> = ["alpha 🐼é","alpha beta","beta alpha"].iter().enumerate().filter_map(|(i,s)|s.contains(&needle).then_some(i)).collect();
                prop_assert_eq!(&ids,&expected);
                let code = match action {
                    0..=5 => { let c=['A','b','é','🐼','z','l'][action as usize]; query.push(c); position=0; KeyCode::Char(c) }
                    6|7 => {query.pop(); position=0; KeyCode::Backspace}
                    8 => {if !ids.is_empty(){position=position.checked_sub(1).unwrap_or(ids.len()-1)} KeyCode::Up}
                    9 => {if !ids.is_empty(){position=(position+1)%ids.len()} KeyCode::Down}
                    10 => {position=position.saturating_sub(2); KeyCode::PageUp}
                    11 => {position=(position+2).min(ids.len().saturating_sub(1)); KeyCode::PageDown}
                    12 => KeyCode::Esc,
                    _ => KeyCode::Enter
                };
                let result = picker.key(event::KeyEvent::new(code,KeyModifiers::NONE),&ids,2);
                match code {
                    KeyCode::Esc => prop_assert_eq!(result,Action::Close(None)),
                    KeyCode::Enter if !expected.is_empty() => prop_assert_eq!(result,Action::Close(Some(expected[position]))),
                    KeyCode::Enter => prop_assert_eq!(result,Action::Stay),
                    _ => {}
                }
                prop_assert_eq!(&picker.query, &query);
                prop_assert_eq!(picker.pos,position);
                if matches!(code, KeyCode::Enter | KeyCode::Esc) && (code==KeyCode::Esc || !expected.is_empty()){
                    picker=Selection::default(); query.clear(); position=0;
                }
            }
        }
    }
}
