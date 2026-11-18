use crossterm::event::{Event, KeyCode, KeyModifiers};
use reedline::{
    EditCommand, EditMode, EventStatus, PromptEditMode, PromptViMode, Reedline, ReedlineEvent,
    ReedlineRawEvent, Vi,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    vi: Vi,
    command: Option<reedline::LineBuffer>,
}

/// The painter and key dispatcher see the same modal state, including a
/// separate Vim command line so cancelling a command preserves the draft.
#[derive(Clone, Default)]
pub struct Vim(Arc<Mutex<State>>);

impl Vim {
    pub fn new(
        insert: reedline::Keybindings,
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
        Self(Arc::new(Mutex::new(State {
            vi: Vi::new(insert, normal, visual),
            command: None,
        })))
    }

    pub fn label(&self) -> String {
        let state = self.0.lock().unwrap();
        if let Some(line) = &state.command {
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
        if let Some(line) = &mut state.command {
            match Event::from(event) {
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
                Event::Paste(s) => line.insert_str(&crate::ui::inline(&s, usize::MAX)),
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

    proptest! {
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
