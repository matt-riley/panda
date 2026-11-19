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

fn label_hits(labels: &[String], query: &str) -> Vec<Hit> {
    labels
        .iter()
        .enumerate()
        .filter(|(_, text)| matches(text, query))
        .map(|(index, _)| Hit {
            index,
            preview: String::new(),
        })
        .collect()
}
pub fn choose(title: &str, labels: &[String], selected: usize) -> io::Result<Option<usize>> {
    Ok(pick(
        title,
        labels,
        &mut Selection::at(selected),
        PickerMode::List,
        |query| label_hits(labels, query),
    )?
    .map(|p| p.index))
}

#[derive(Clone, Copy)]
enum PickerMode<'a> {
    List,
    Preview,
    Tree(&'a [panda_local::store::TreeEntry]),
}
#[derive(Debug, PartialEq)]
pub enum TreeAction {
    Switch,
    Summarize,
    GuidedSummary,
    Fork,
    EditLabel,
}
struct Picked {
    index: usize,
    action: TreeAction,
}

fn pick(
    title: &str,
    labels: &[String],
    state: &mut Selection,
    mode: PickerMode<'_>,
    search: impl Fn(&str) -> Vec<Hit>,
) -> io::Result<Option<Picked>> {
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    let _restore = Restore;
    let preview = matches!(mode, PickerMode::Preview);
    let visible_hits = |query: &str, labeled_only: bool| {
        let mut hits = search(query);
        if labeled_only && let PickerMode::Tree(entries) = mode {
            hits.retain(|h| entries[h.index].label.is_some());
        }
        hits
    };
    let mut hits = visible_hits(&state.query, state.labeled_only);
    let mut terminal = crate::renderer::terminal()?;
    let color = std::env::var_os("NO_COLOR").is_none();
    loop {
        let ids: Vec<_> = hits.iter().map(|h| h.index).collect();
        state.pos = state.pos.min(ids.len().saturating_sub(1));
        let height = terminal.size()?.height;
        let visible = usize::from(height.saturating_sub(if preview { 11 } else { 5 })).clamp(1, 10);
        let can_fork = matches!(mode, PickerMode::Tree(entries)
            if ids.get(state.pos).is_some_and(|&i| entries[i].is_prompt));
        let hint = matches!(mode, PickerMode::Tree(_)).then(|| {
            let fork = if can_fork { "   Ctrl-F fork" } else { "" };
            let summary = if state.filtering {
                "Ctrl-S summarize"
            } else {
                "s summarize   / filter"
            };
            format!(" Enter switch{fork}   {summary}   Esc close")
        });
        let results = crate::renderer::PickerResults {
            hint: hint.as_deref(),
            ids: &ids,
            pos: state.pos,
            preview: preview.then(|| hits.get(state.pos).map_or("", |hit| hit.preview.as_str())),
        };
        let title = if matches!(mode, PickerMode::Tree(_)) {
            if state.labeled_only {
                "Labeled entries · Shift-L edit · Ctrl-L all · Shift-S guided summary"
            } else {
                "Session tree · Shift-L edit · Ctrl-L labeled · Shift-S guided summary"
            }
        } else {
            title
        };
        terminal
            .draw(|f| crate::renderer::picker(f, title, &state.query, labels, results, color))?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        let action = if matches!(mode, PickerMode::Tree(_)) {
            state.tree_key(k, &ids, visible, can_fork)
        } else {
            state.key(k, &ids, visible)
        };
        match action {
            Action::Tree(index, action) => {
                return Ok(Some(Picked { index, action }));
            }
            Action::Close(id) => {
                return Ok(id.map(|index| Picked {
                    index,
                    action: TreeAction::Switch,
                }));
            }
            Action::ToggleLabels => {
                let selected = ids.get(state.pos);
                state.labeled_only = !state.labeled_only;
                hits = visible_hits(&state.query, state.labeled_only);
                state.pos = hits
                    .iter()
                    .position(|h| Some(&h.index) == selected)
                    .unwrap_or(0);
            }
            Action::Filter => hits = visible_hits(&state.query, state.labeled_only),
            Action::Stay => {}
        }
    }
}

#[derive(Default)]
pub struct Selection {
    labeled_only: bool,
    filtering: bool,
    query: String,
    pos: usize,
}

#[derive(Debug, PartialEq)]
enum Action {
    ToggleLabels,
    Tree(usize, TreeAction),
    Close(Option<usize>),
    Filter,
    Stay,
}

impl Selection {
    pub fn at(pos: usize) -> Self {
        Self {
            pos,
            ..Self::default()
        }
    }

    fn tree_key(
        &mut self,
        k: event::KeyEvent,
        ids: &[usize],
        visible: usize,
        can_fork: bool,
    ) -> Action {
        if k.kind != KeyEventKind::Release {
            match k.code {
                KeyCode::Char('l') if k.modifiers == KeyModifiers::CONTROL => {
                    return Action::ToggleLabels;
                }
                KeyCode::Char('L') if k.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    return ids
                        .get(self.pos)
                        .map_or(Action::Stay, |&i| Action::Tree(i, TreeAction::EditLabel));
                }
                KeyCode::Char('S') if k.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    return ids.get(self.pos).map_or(Action::Stay, |&i| {
                        Action::Tree(i, TreeAction::GuidedSummary)
                    });
                }
                KeyCode::Char('f') if k.modifiers == KeyModifiers::CONTROL => {
                    return if can_fork {
                        Action::Tree(ids[self.pos], TreeAction::Fork)
                    } else {
                        Action::Stay
                    };
                }
                KeyCode::Char('s')
                    if (!self.filtering && k.modifiers.is_empty())
                        || k.modifiers == KeyModifiers::CONTROL =>
                {
                    return ids
                        .get(self.pos)
                        .copied()
                        .map_or(Action::Stay, |i| Action::Tree(i, TreeAction::Summarize));
                }
                KeyCode::Char('/') if !self.filtering => {
                    self.filtering = true;
                    return Action::Stay;
                }
                KeyCode::Char(_) | KeyCode::Backspace => self.filtering = true,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    self.filtering = false
                }
                _ => {}
            }
        }
        self.key(k, ids, visible)
    }

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

fn tree_labels(entries: &[panda_local::store::TreeEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}{}{}{}{} {}",
                "  ".repeat(
                    entry
                        .branch_depth
                        .saturating_sub(usize::from(entry.starts_branch))
                        .min(8)
                ),
                if entry.starts_branch { "↳ " } else { "" },
                if entry.current { "● " } else { "" },
                entry
                    .label
                    .as_deref()
                    .map(|s| format!("[{}] ", crate::ui::inline(s, 80)))
                    .unwrap_or_default(),
                if entry.is_prompt { "You:" } else { "Panda:" },
                crate::ui::inline(&entry.text, 240)
            )
        })
        .collect()
}

pub fn tree(
    entries: &[panda_local::store::TreeEntry],
    state: &mut Selection,
) -> io::Result<Option<(i64, TreeAction)>> {
    let labels = tree_labels(entries);
    Ok(pick(
        "Session tree",
        &labels,
        state,
        PickerMode::Tree(entries),
        |query| label_hits(&labels, query),
    )?
    .map(|p| (entries[p.index].id, p.action)))
}

pub fn edit_text(title: &str, initial: &str) -> io::Result<Option<String>> {
    use reedline::{
        DefaultPrompt, DefaultPromptSegment, EditCommand, Emacs, Reedline, ReedlineEvent, Signal,
        default_emacs_keybindings,
    };
    let prompt = DefaultPrompt {
        left_prompt: DefaultPromptSegment::Basic(title.into()),
        right_prompt: DefaultPromptSegment::Empty,
    };
    let mut keys = default_emacs_keybindings();
    keys.add_binding(KeyModifiers::NONE, KeyCode::Esc, ReedlineEvent::CtrlC);
    let mut editor = Reedline::create().with_edit_mode(Box::new(Emacs::new(keys)));
    editor.run_edit_commands(&[EditCommand::InsertString(crate::ui::inline(
        initial,
        usize::MAX,
    ))]);
    Ok(match editor.read_line(&prompt)? {
        Signal::Success(text) => Some(text.trim().to_owned()),
        _ => None,
    })
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
    let selected = pick(
        "Resume session",
        &labels,
        &mut Selection::default(),
        PickerMode::Preview,
        |query| {
            let query = query.trim().to_lowercase();
            sessions
                .iter()
                .enumerate()
                .filter_map(|(index, s)| {
                    session_preview(s, &query).map(|preview| Hit { index, preview })
                })
                .collect()
        },
    )?;
    Ok(selected.map(|index| sessions[index.index].id))
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
        fn tree_action_key_sequences(keys in prop::collection::vec(0u8..13,1..96)) {
            let mut state=Selection::default();
            let mut search=false;
            let mut selected=0;
            let ids=[41,89];
            for n in keys {
                let code=match n {0..=2=>KeyCode::Char('s'),3=>KeyCode::Char('/'),4=>KeyCode::Down,5=>KeyCode::Up,6=>KeyCode::Char('a'),7=>KeyCode::Enter,8=>KeyCode::Esc,10=>KeyCode::Char('f'),11|12=>KeyCode::Char('S'),_=>KeyCode::Backspace};
                let modifiers = match n {
                    2|10 => KeyModifiers::CONTROL,
                    12 => KeyModifiers::SHIFT,
                    _ => KeyModifiers::NONE,
                };
                let mut k=event::KeyEvent::new(code,modifiers);
                if n==1 {k.kind=KeyEventKind::Release;}
                let can_fork = selected == 0;
                let expected=match n {
                    1=>Action::Stay,
                    2=>Action::Tree(ids[selected], TreeAction::Summarize),
                    0 if !search=>Action::Tree(ids[selected], TreeAction::Summarize),
                    10 if can_fork=>Action::Tree(ids[selected], TreeAction::Fork),
                    10=>Action::Stay,
                    11|12=>Action::Tree(ids[selected], TreeAction::GuidedSummary),
                    3 if !search=>{search=true;Action::Stay}
                    4|5=>{selected=1-selected;search=false;Action::Stay}
                    7=>Action::Close(Some(ids[selected])),
                    8=>Action::Close(None),
                    _=>{selected=0;search=true;Action::Filter}
                };
                prop_assert_eq!(state.tree_key(k,&ids,2,can_fork),expected);
                prop_assert_eq!(search,state.filtering);
                prop_assert_eq!(selected,state.pos);
            }
        }

        #[test]
        fn tree_picker_key_sequences(keys in prop::collection::vec(0u8..12,1..96)) {
            use panda_local::store::TreeEntry;
            let entries = [
                TreeEntry{label:None,id:41,branch_depth:0,starts_branch:false,is_prompt:true,text:"root".into(),current:false},
                TreeEntry{label:None,id:53,branch_depth:1,starts_branch:true,is_prompt:false,text:"same 🐼".into(),current:false},
                TreeEntry{label:None,id:89,branch_depth:1,starts_branch:true,is_prompt:false,text:"same 🐼".into(),current:true},
            ];
            let labels=tree_labels(&entries);
            let mut state=Selection::at(2);
            let mut typed=Vec::new();
            let mut row=2usize;
            for key in keys {
                let ids:Vec<_>=labels.iter().enumerate().filter(|(_,label)|matches(label,&state.query)).map(|(i,_)|i).collect();
                let expected:Vec<i64>=match typed.as_slice() {
                    []=>vec![41,53,89], ['🐼']=>vec![53,89], ['●']=>vec![89], ['●','🐼']=>vec![89], _=>vec![]
                };
                prop_assert_eq!(ids.iter().map(|&i|entries[i].id).collect::<Vec<_>>(),expected.clone());
                let code=match key {
                    0..=2=>{let c=['🐼','●','z'][key as usize];typed.push(c); row=0;KeyCode::Char(c)}
                    3|4=>{typed.pop();row=0;KeyCode::Backspace}
                    5=>{if !ids.is_empty(){row=row.checked_sub(1).unwrap_or(ids.len()-1);}KeyCode::Up}
                    6=>{if !ids.is_empty(){row=(row+1)%ids.len();}KeyCode::Down}
                    7=>{row=0;KeyCode::PageUp}
                    8=>{row=ids.len().saturating_sub(1);KeyCode::PageDown}
                    9=>KeyCode::Esc, _=>KeyCode::Enter
                };
                let outcome=state.key(event::KeyEvent::new(code,KeyModifiers::NONE),&ids,3);
                prop_assert_eq!(state.pos,row);
                match outcome {
                    Action::Close(choice)=>{
                        prop_assert_eq!(choice.map(|i|entries[i].id),if code==KeyCode::Enter {Some(expected[row])}else{None});
                        state=Selection::at(2);row=2;typed.clear();
                    }
                    _=>prop_assert!(code!=KeyCode::Esc && (code!=KeyCode::Enter || expected.is_empty()))
                }
            }
        }

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
