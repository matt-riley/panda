use crossterm::event::{Event, KeyCode, KeyModifiers};
use reedline::{
    EditCommand, EditMode, EventStatus, PromptEditMode, PromptViMode, Reedline, ReedlineEvent,
    ReedlineRawEvent, Vi,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    vi: Vi,
    command: Option<CommandLine>,
}

#[derive(Default)]
struct CommandLine {
    line: reedline::LineBuffer,
    // Keep the initial matches while Tab cycles through inserted names.
    cycle: Option<(Vec<&'static str>, usize)>,
}

impl CommandLine {
    fn matches(&self) -> Vec<&'static str> {
        let text = self.line.get_buffer();
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        if self.line.insertion_point() != end {
            return Vec::new();
        }
        crate::command_matches(&text[..end]).collect()
    }

    fn complete(&mut self, backwards: bool) {
        let (matches, index) = match self.cycle.take() {
            Some((matches, index)) => {
                let count = matches.len();
                let next = if backwards {
                    (index + count - 1) % count
                } else {
                    (index + 1) % count
                };
                (matches, next)
            }
            None => {
                let matches = self.matches();
                if matches.is_empty() {
                    return;
                }
                let index = if backwards { matches.len() - 1 } else { 0 };
                (matches, index)
            }
        };
        let name = &matches[index][1..];
        let end = self.line.insertion_point();
        self.line.replace_range(..end, name);
        self.line.set_insertion_point(name.len());
        self.cycle = Some((matches, index));
    }

    fn hints(&self) -> String {
        if let Some((matches, index)) = &self.cycle {
            // Put the selected name first so it stays visible on narrow terminals.
            return matches[*index..]
                .iter()
                .chain(&matches[..*index])
                .copied()
                .collect::<Vec<_>>()
                .join("  ");
        }
        self.matches().join("  ")
    }
}

/// The painter and key dispatcher see the same modal state, including a
/// separate Vim command line so cancelling a command preserves the draft.
#[derive(Clone, Default)]
pub struct Vim(Arc<Mutex<State>>);

impl Vim {
    pub fn new(
        mut insert: reedline::Keybindings,
        mut normal: reedline::Keybindings,
        visual: reedline::Keybindings,
    ) -> Self {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            normal.add_binding(
                modifiers,
                KeyCode::Char(':'),
                ReedlineEvent::ExecuteHostCommand("command-line".into()),
            );
        }
        for bindings in [&mut insert, &mut normal] {
            bindings.add_binding(
                KeyModifiers::CONTROL,
                KeyCode::Char('p'),
                ReedlineEvent::ExecuteHostCommand(":cycle".into()),
            );
        }
        Self(Arc::new(Mutex::new(State {
            vi: Vi::new(insert, normal, visual),
            command: None,
        })))
    }

    pub fn label(&self) -> String {
        let state = self.0.lock().unwrap();
        if let Some(command) = &state.command {
            let line = &command.line;
            let (left, right) = line.get_buffer().split_at(line.insertion_point());
            return format!(":{left}▏{right}");
        }
        match state.vi.edit_mode() {
            PromptEditMode::Vi(PromptViMode::Normal) => "normal",
            PromptEditMode::Vi(PromptViMode::Visual) => "visual",
            _ => "insert",
        }
        .into()
    }

    pub fn hints(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .command
            .as_ref()
            .map(CommandLine::hints)
            .filter(|s| !s.is_empty())
    }

    pub fn commanding(&self) -> bool {
        self.0.lock().unwrap().command.is_some()
    }
}

impl EditMode for Vim {
    fn edit_mode(&self) -> PromptEditMode {
        self.0.lock().unwrap().vi.edit_mode()
    }
    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        self.0.lock().unwrap().vi.handle_mode_specific_event(event)
    }
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let mut state = self.0.lock().unwrap();
        if let Some(command) = &mut state.command {
            let raw = Event::from(event);
            if let Event::Key(key) = &raw {
                if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                    command.complete(
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    );
                    return ReedlineEvent::Repaint;
                }
                command.cycle = None;
            }
            let line = &mut command.line;
            match raw {
                Event::Key(key) => match key.code {
                    KeyCode::Enter => {
                        let command = format!(":{}", line.get_buffer().trim());
                        state.command = None;
                        return ReedlineEvent::ExecuteHostCommand(command);
                    }
                    KeyCode::Esc => state.command = None,
                    KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                        state.command = None
                    }
                    KeyCode::Backspace if line.is_empty() => state.command = None,
                    KeyCode::Backspace => line.delete_left_grapheme(),
                    KeyCode::Delete => line.delete_right_grapheme(),
                    KeyCode::Left => line.move_left(),
                    KeyCode::Right => line.move_right(),
                    KeyCode::Home => line.move_to_start(),
                    KeyCode::End => line.move_to_end(),
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        line.insert_char(c)
                    }
                    _ => {}
                },
                Event::Paste(s) => {
                    command.cycle = None;
                    line.insert_str(&crate::ui::inline(&s, usize::MAX));
                }
                _ => {}
            }
            return ReedlineEvent::Repaint;
        }
        let event = state.vi.parse_event(event);
        if matches!(&event, ReedlineEvent::ExecuteHostCommand(s) if s == "command-line") {
            state.command = Some(Default::default());
            ReedlineEvent::Repaint
        } else {
            event
        }
    }
}

/// Drive Reedline's own Vim grammar and edit buffer from Ratatui's event loop.
pub struct Steering<'a> {
    pub vim: Vim,
    pub editor: &'a mut Reedline,
}
impl Steering<'_> {
    pub fn replace(&mut self, text: String) {
        self.vim
            .handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
                PromptViMode::Insert,
            )));
        self.editor
            .run_edit_commands(&[EditCommand::Clear, EditCommand::InsertString(text)]);
    }
    pub fn input(&mut self, event: Event) -> Option<String> {
        let before = self.vim.edit_mode();
        let parsed = self.vim.parse_event(event.try_into().ok()?);
        let command = self.apply(parsed);
        if before != self.vim.edit_mode()
            && matches!(
                self.vim.edit_mode(),
                PromptEditMode::Vi(PromptViMode::Visual)
            )
        {
            self.editor
                .run_edit_commands(&[EditCommand::MoveToPosition {
                    position: self.editor.current_insertion_point(),
                    select: true,
                }]);
        }
        command
    }
    fn apply(&mut self, event: ReedlineEvent) -> Option<String> {
        use ReedlineEvent::*;
        let select = false;
        match event {
            Edit(commands) => self.editor.run_edit_commands(&commands),
            Multiple(events) => {
                for event in events {
                    if let Some(cmd) = self.apply(event) {
                        return Some(cmd);
                    }
                }
            }
            UntilFound(events) => {
                if let Some(event) = events
                    .into_iter()
                    .find(|e| matches!(e, Edit(_) | Up | Down | Left | Right | ToStart | ToEnd))
                {
                    return self.apply(event);
                }
            }
            Esc if self.editor.current_selection().is_some() => {
                self.editor
                    .run_edit_commands(&[EditCommand::MoveToPosition {
                        position: self.editor.current_insertion_point(),
                        select: false,
                    }])
            }
            Repaint => self.editor.run_edit_commands(&[]),
            Left => self
                .editor
                .run_edit_commands(&[EditCommand::MoveLeft { select }]),
            Right => self
                .editor
                .run_edit_commands(&[EditCommand::MoveRight { select }]),
            Up => self
                .editor
                .run_edit_commands(&[EditCommand::MoveLineUp { select }]),
            Down => self
                .editor
                .run_edit_commands(&[EditCommand::MoveLineDown { select }]),
            ToStart => self
                .editor
                .run_edit_commands(&[EditCommand::MoveToStart { select }]),
            ToEnd => self
                .editor
                .run_edit_commands(&[EditCommand::MoveToEnd { select }]),
            ExecuteHostCommand(cmd) => return Some(cmd),
            _ => {}
        }
        Option::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use proptest::prelude::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn vim() -> Vim {
        Vim::new(
            reedline::default_vi_insert_keybindings(),
            reedline::default_vi_normal_keybindings(),
            reedline::default_vi_visual_keybindings(),
        )
    }

    #[test]
    fn visual_delete_includes_the_grapheme_under_the_cursor() {
        let vim = vim();
        let mut editor = Reedline::create().with_edit_mode(Box::new(vim.clone()));
        let mut prompt = Steering {
            editor: &mut editor,
            vim,
        };
        prompt.replace("a🐼cd".into());
        prompt.input(key(KeyCode::Esc));
        for c in "0lvd".chars() {
            prompt.input(key(KeyCode::Char(c)));
        }
        assert_eq!(prompt.editor.current_buffer_contents(), "acd");
    }

    #[test]
    fn editing_a_completion_refilters_and_arguments_are_left_alone() {
        let vim = vim();
        let mut editor = Reedline::create().with_edit_mode(Box::new(vim.clone()));
        let mut prompt = Steering {
            editor: &mut editor,
            vim,
        };
        prompt.input(key(KeyCode::Esc));
        for c in ":mo".chars() {
            prompt.input(key(KeyCode::Char(c)));
        }
        prompt.input(key(KeyCode::Tab));
        prompt.input(key(KeyCode::Char('s')));
        prompt.input(key(KeyCode::Tab));
        assert_eq!(prompt.vim.hints().as_deref(), Some(":models"));
        prompt.input(Event::Paste(" openrouter".into()));
        prompt.input(key(KeyCode::BackTab));
        assert!(prompt.vim.hints().is_none());
        assert_eq!(
            prompt.input(key(KeyCode::Enter)).as_deref(),
            Some(":models openrouter")
        );
    }

    proptest! {
        #[test]
        fn command_completion_key_sequences(
            prefix in prop::sample::select(vec!["mo", "q", "res", "unknown", "🐼"]),
            cycles in prop::collection::vec(any::<bool>(), 1..40),
            argument in "[a-z🐼é]{0,20}",
            cancel in any::<bool>()
        ) {
            let vim = vim();
            let mut editor = Reedline::create().with_edit_mode(Box::new(vim.clone()));
            let mut prompt = Steering { editor: &mut editor, vim: vim.clone() };
            prompt.replace("keep 🐼 draft".into());
            prompt.input(key(KeyCode::Esc));
            prompt.input(key(KeyCode::Char('0')));
            prompt.input(key(KeyCode::Char(':')));
            // Complete the first word with an existing argument to its right.
            prompt.input(Event::Paste(format!("{prefix} {argument}")));
            prompt.input(key(KeyCode::Home));
            for _ in prefix.chars() { prompt.input(key(KeyCode::Right)); }
            let names: &[&str] = match prefix {
                "mo" => &["model","models"], "q" => &["q","quit"], "res" => &["resume"], _ => &[],
            };
            let mut selected = None;
            for back in cycles {
                let code = if back {KeyCode::BackTab} else {KeyCode::Tab};
                prop_assert_eq!(prompt.input(key(code)), None);
                if !names.is_empty() {
                    let n = names.len();
                    selected = Some(match selected {
                        None if back => n - 1, None => 0,
                        Some(i) if back => (i+n-1)%n, Some(i) => (i+1)%n,
                    });
                }
                let expected = selected.map_or(prefix, |i: usize| names[i]);
                prop_assert_eq!(vim.label(), format!(":{expected}▏ {argument}"));
            }
            let name = selected.map_or(prefix, |i| names[i]);
            let expected = format!(":{name} {argument}").trim_end().to_owned();
            let keycode = if cancel { KeyCode::Esc } else { KeyCode::Enter };
            prop_assert_eq!(prompt.input(key(keycode)), (!cancel).then_some(expected));
            prop_assert_eq!(prompt.editor.current_buffer_contents(), "keep 🐼 draft");
            prop_assert_eq!(prompt.editor.current_insertion_point(), 0);
            prop_assert_eq!(vim.label(), "normal");
        }

        #[test]
        fn steering_key_sequences_follow_a_grapheme_model(actions in prop::collection::vec(0u8..10, 1..80)) {
            let vim = vim();
            let mut editor = Reedline::create().with_edit_mode(Box::new(vim.clone()));
            let mut prompt = Steering {editor: &mut editor, vim};
            let mut text = vec!["🐼", "a", "é"];
            let mut cursor = 2usize;
            prompt.replace(text.concat());
            prompt.input(key(KeyCode::Esc));
            for action in actions {
                let last = text.len().saturating_sub(1);
                let keys = match action {
                    0 => { cursor = 0; "0" }
                    1 => { cursor = last; "$" }
                    2 => { cursor = cursor.saturating_sub(2); "2h" }
                    3 => { cursor = (cursor+2).min(last); "2l" }
                    4 => { if !text.is_empty() {text.remove(cursor); cursor = cursor.min(text.len().saturating_sub(1));} "x" }
                    5 => {text.insert(cursor,"🐼"); "i🐼\x1b"}
                    6 => {text.insert(cursor,"é"); "ié\x1b"}
                    7 => ":quit\x1b",
                    8 => ":q\r",
                    _ => { cursor = cursor.saturating_sub(1); "h" }
                };
                for c in keys.chars() {
                    let code = match c { '\x1b' => KeyCode::Esc, '\r' => KeyCode::Enter, _ => KeyCode::Char(c) };
                    let command = prompt.input(key(code));
                    prop_assert_eq!(command.as_deref(), (c == '\r').then_some(":q"));
                }
                prop_assert_eq!(prompt.editor.current_buffer_contents(), text.concat());
                prop_assert_eq!(prompt.editor.current_insertion_point(), text[..cursor].concat().len());
                prop_assert_eq!(prompt.vim.label(), "normal");
            }
        }
    }
}
