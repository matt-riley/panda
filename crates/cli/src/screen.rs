use crate::{safe, ui};
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use panda_core::Call;
use reedline::{
    KeyCode, KeyModifiers, Reedline, ReedlineEvent, Signal, StatusLine,
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
};
use std::{
    io::{self, Write},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Thought,
    Text,
    Dim,
    User,
    Tool,
    Added,
    Removed,
}
impl Tone {
    fn paint(self, text: &str, color: bool) -> String {
        let code = match self {
            Self::Text => "0",
            Self::Dim => "90",
            Self::Thought => "3;90",
            Self::User => "1;36",
            Self::Tool => "33",
            Self::Added => "32;48;5;22",
            Self::Removed => "91;48;5;52",
        };
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.into()
        }
    }
}

type Row = (Tone, String);
#[derive(Clone)]
enum DisplayRow {
    Plain(Tone, String),
    Markdown(ratatui::text::Line<'static>),
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0;
    let mut append = |g: &str| {
        let cells = g.width();
        if used > 0 && used + cells > width {
            rows.push(String::new());
            used = 0;
        }
        if cells <= width {
            rows.last_mut().unwrap().push_str(g);
            used += cells;
        }
    };
    for g in text.graphemes(true) {
        if g.width() <= width {
            append(g);
        } else {
            for c in g.chars() {
                append(c.encode_utf8(&mut [0; 4]));
            }
        }
    }
    rows
}

#[derive(Clone, Default)]
pub struct Footer {
    pub vim: crate::vim::Vim,
    pub location: String,
    pub session: String,
    pub model: String,
    pub effort: Option<String>,
    pub usage: String,
}
impl Footer {
    pub fn location(root: &Path, home: &Path) -> String {
        let mut label = root
            .strip_prefix(home)
            .map(|p| Path::new("~").join(p))
            .unwrap_or_else(|_| root.into())
            .display()
            .to_string();
        if let Ok(out) = Command::new("git")
            .args(["symbolic-ref", "--short", "-q", "HEAD"])
            .current_dir(root)
            .output()
            && out.status.success()
        {
            label.push_str(&format!(
                " ({})",
                String::from_utf8_lossy(&out.stdout).trim()
            ));
        }
        ui::inline(&label, 256)
    }
    fn render(&self, width: usize, color: bool) -> String {
        let mut right = ui::inline(&self.model, 150);
        if let Some(e) = &self.effort {
            right.push_str(&format!(" · {e}"));
        }
        let left = format!("{}  {} {}", self.vim.label(), self.session, self.usage)
            .trim_end()
            .to_owned();
        let gap = width.saturating_sub(left.width() + right.width()).max(1);
        let stats = if left.width() + right.width() < width {
            format!("{left}{}{right}", " ".repeat(gap))
        } else {
            let available = width.saturating_sub(right.width() + 1);
            if available < self.vim.label().width() {
                left
            } else {
                let short = wrap(&left, available)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                format!(
                    "{short}{}{right}",
                    " ".repeat(width.saturating_sub(short.width() + right.width()))
                )
            }
        };
        let line = |s: &str| wrap(s, width).into_iter().next().unwrap_or_default();
        Tone::Dim.paint(
            &format!(
                "{}\n{}\n{}",
                "─".repeat(width),
                line(self.vim.hints().as_deref().unwrap_or(&self.location)),
                line(&stats)
            ),
            color,
        )
    }
}
impl StatusLine for Footer {
    fn status(&mut self, _: &str, _: usize, columns: u16, color: bool) -> String {
        self.render(usize::from(columns.saturating_sub(1)).max(1), color)
    }
}

pub struct Screen {
    pub active: bool,
    live: String,
    selected_tool: Option<usize>,
    tool_rows: std::collections::HashMap<String, usize>,
    pub clipboard: Option<String>,
    thoughts: bool,
    expanded: bool,
    previews: std::collections::HashMap<usize, (usize, String)>,
    color: bool,
    rows: Vec<Row>,
    // Cache wrapped transcript rows across editor repaints and page navigation.
    wrapped: Mutex<(usize, Vec<DisplayRow>)>,
    replies: std::collections::HashMap<usize, crate::markdown::Reply>,
    current_reply: Option<usize>,
    pub footer: Footer,
    pub commands: std::collections::VecDeque<String>,
    offset: usize,
    input_rows: Arc<AtomicUsize>,
}
impl Screen {
    pub fn open(active: bool) -> io::Result<Self> {
        if active {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                Clear(ClearType::All),
                MoveTo(0, 0)
            )?;
        }
        Ok(Self {
            active,
            live: String::new(),
            selected_tool: None,
            tool_rows: Default::default(),
            clipboard: None,
            thoughts: false,
            expanded: false,
            previews: Default::default(),
            color: std::env::var_os("NO_COLOR").is_none(),
            rows: Vec::new(),
            wrapped: Mutex::default(),
            replies: Default::default(),
            current_reply: None,
            offset: 0,
            input_rows: Arc::new(AtomicUsize::new(1)),
            footer: Footer::default(),
            commands: Default::default(),
        })
    }
    pub fn line(&mut self, tone: Tone, text: &str) {
        if !self.active {
            println!("{text}");
            return;
        }
        self.wrapped.get_mut().unwrap().0 = 0;
        self.rows.extend(
            safe(text)
                .replace('\t', "    ")
                .split('\n')
                .map(|line| (tone, line.to_owned())),
        );
        self.offset = 0;
    }
    pub fn stream(&mut self, text: &str) {
        if !self.active {
            print!("{}", safe(text));
            let _ = io::stdout().flush();
            return;
        }
        if !text.is_empty() {
            self.reply().push(text);
        }
    }
    fn reply(&mut self) -> &mut crate::markdown::Reply {
        self.wrapped.get_mut().unwrap().0 = 0;
        let index = *self.current_reply.get_or_insert_with(|| {
            let i = self.rows.len();
            self.rows.push((Tone::Text, String::new()));
            i
        });
        self.replies.entry(index).or_default()
    }
    pub fn end_reply(&mut self) {
        self.current_reply = None;
    }
    pub fn finish_reply(&mut self, text: &str) {
        if !self.active {
            println!("{}", safe(text));
            return;
        }
        if !text.is_empty() || self.current_reply.is_some() {
            self.reply().finish(text);
        }
        self.end_reply();
    }
    pub fn delta(&mut self, tone: Tone, text: &str) {
        self.wrapped.get_mut().unwrap().0 = 0;
        let clean = safe(text).replace('\t', "    ");
        let mut parts = clean.split('\n');
        let first = parts.next().unwrap_or_default();
        if let Some((prev, last)) = self.rows.last_mut()
            && *prev == tone
        {
            last.push_str(first);
        } else {
            self.rows.push((tone, first.to_owned()));
        }
        self.rows.extend(parts.map(|line| (tone, line.to_owned())));
    }
    pub fn clear(&mut self) {
        self.wrapped.get_mut().unwrap().0 = 0;
        self.rows.clear();
        self.replies.clear();
        self.current_reply = None;
        self.previews.clear();
        self.tool_rows.clear();
        self.live.clear();
        self.selected_tool = None;
        self.clipboard = None;
        self.offset = 0;
    }
    pub fn tool_result(&mut self, call: Option<&Call>, text: &str, error: bool) {
        self.live.clear();
        if call.is_some_and(|c| c.name == "edit")
            && !error
            && let Some(hunk) = text.strip_prefix("saved\n")
        {
            for line in hunk.lines() {
                let tone = match line.chars().next() {
                    Some('−') => Tone::Removed,
                    Some('+') => Tone::Added,
                    _ => Tone::Dim,
                };
                self.line(tone, line);
            }
            return;
        }
        if let Some(c) = call.filter(|c| c.name == "edit" && !error && text == "saved") {
            for (key, prefix, tone) in [("old", "−", Tone::Removed), ("new", "+", Tone::Added)] {
                if let Some(snippet) = c.args[key].as_str() {
                    for line in snippet.split('\n').take(8) {
                        self.line(tone, &format!("{prefix} {}", ui::inline(line, 512)));
                    }
                    if snippet.split('\n').nth(8).is_some() {
                        self.line(tone, "  …");
                    }
                }
            }
        }
        let start = self.rows.len();
        if let Some(c) = call {
            self.tool_rows.insert(c.id.clone(), start);
        }
        self.line(
            if error { Tone::Removed } else { Tone::Dim },
            &ui::result(text, false),
        );
        self.previews.insert(start, (self.rows.len(), safe(text)));
    }
    fn dimensions() -> (usize, usize) {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        (
            usize::from(cols.saturating_sub(1)).max(1),
            usize::from(rows),
        )
    }
    fn reflow(&self, width: usize) {
        let mut cache = self.wrapped.lock().unwrap();
        if cache.0 != width {
            cache.0 = width;
            cache.1.clear();
            let mut skip = 0;
            for (i, (tone, s)) in self.rows.iter().enumerate() {
                if i < skip || (*tone == Tone::Thought && !self.thoughts) {
                    continue;
                }
                if let Some(reply) = self.replies.get(&i) {
                    cache
                        .1
                        .extend(reply.rows(width).into_iter().map(DisplayRow::Markdown));
                    continue;
                }
                if (self.expanded || self.selected_tool == Some(i))
                    && let Some((end, full)) = self.previews.get(&i)
                {
                    skip = *end;
                    for line in full.lines() {
                        cache.1.extend(
                            wrap(line, width)
                                .into_iter()
                                .map(|s| DisplayRow::Plain(*tone, s)),
                        );
                    }
                } else {
                    cache.1.extend(
                        wrap(s, width)
                            .into_iter()
                            .map(|s| DisplayRow::Plain(*tone, s)),
                    );
                }
            }
        }
    }
    pub fn body(&self) -> String {
        let (w, h) = Self::dimensions();
        let input_rows = self.input_rows.load(Ordering::Relaxed);
        self.reflow(w);
        let cache = self.wrapped.lock().unwrap();
        let live: Vec<_> = self.live.lines().rev().take(3).collect();
        let count = h.saturating_sub(5 + input_rows + live.len());
        let end = cache
            .1
            .len()
            .saturating_sub(self.offset.min(cache.1.len().saturating_sub(count)));
        let start = end.saturating_sub(count);
        let mut out = String::new();
        for row in &cache.1[start..end] {
            out.push_str(&match row {
                DisplayRow::Plain(tone, s) => tone.paint(s, self.color),
                DisplayRow::Markdown(line) => crate::markdown::ansi(line, self.color),
            });
            out.push('\n');
        }
        out.push_str(&"\n".repeat(count.saturating_sub(end - start)));
        for line in live.into_iter().rev() {
            out.push_str(&Tone::Dim.paint(&ui::inline(line, w / 2), self.color));
            out.push('\n');
        }
        out.push_str(&Tone::Dim.paint(&"─".repeat(w), self.color));
        out.push('\n');
        out
    }
    pub fn select_tool(&mut self, id: &str) -> anyhow::Result<()> {
        let index = *self
            .tool_rows
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no expandable result with ID {id}"))?;
        self.selected_tool = if self.selected_tool == Some(index) {
            None
        } else {
            Some(index)
        };
        self.wrapped.lock().unwrap().0 = 0;
        Ok(())
    }
    pub fn live_chunk(&mut self, chunk: &str) {
        self.live.push_str(&safe(chunk));
        if self.live.len() > 4096 {
            let start = self.live.ceil_char_boundary(self.live.len() - 4096);
            self.live.drain(..start);
        }
    }
    pub fn draw_working(
        &self,
        frame: &mut ratatui::Frame,
        draft: &str,
        cursor: usize,
        selection: Option<(usize, usize)>,
        elapsed: std::time::Duration,
    ) {
        use crate::renderer::{ACCENT, MUTED, ink};
        use ratatui::{
            layout::{Constraint, Layout},
            style::{Color, Modifier},
            text::Line,
            widgets::{Block, Borders, Paragraph},
        };
        let area = frame.area();
        if area.is_empty() {
            return;
        }
        let width = usize::from(area.width.saturating_sub(1)).max(1);
        let prompt_width = width.saturating_sub(2).max(1);
        let mut draft_lines = vec![Line::from("❯ ")];
        let mut width_used = 0;
        let mut caret = (0, 0);
        for (offset, grapheme) in draft.grapheme_indices(true) {
            let newline = grapheme == "\n" || grapheme == "\r\n";
            if !newline && width_used > 0 && width_used + grapheme.width() > prompt_width {
                draft_lines.push(Line::from("│ "));
                width_used = 0;
            }
            if offset <= cursor {
                caret = (draft_lines.len() - 1, width_used);
            }
            if newline {
                draft_lines.push(Line::from("│ "));
                width_used = 0;
            } else {
                let style = if selection.is_some_and(|(start, end)| (start..end).contains(&offset))
                {
                    ratatui::style::Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Default::default()
                };
                draft_lines
                    .last_mut()
                    .unwrap()
                    .push_span(ratatui::text::Span::styled(grapheme, style));
                width_used += grapheme.width();
            }
        }
        if cursor == draft.len() {
            caret = (draft_lines.len() - 1, width_used);
        }
        let prompt_height =
            (draft_lines.len().clamp(1, 4) as u16).min(area.height.saturating_sub(5).max(1));
        let [history, activity, prompt, footer] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(prompt_height + 2),
            Constraint::Length(2),
        ])
        .areas(area);
        self.reflow(width);
        let cached = self.wrapped.lock().unwrap();
        let live: Vec<_> = self
            .live
            .lines()
            .rev()
            .take(3.min(history.height.into()))
            .collect();
        let room = usize::from(history.height).saturating_sub(live.len());
        let mut lines: Vec<_> = cached
            .1
            .iter()
            .skip(cached.1.len().saturating_sub(room))
            .map(|row| {
                let (tone, text) = match row {
                    DisplayRow::Plain(t, s) => (t, s),
                    DisplayRow::Markdown(line) => {
                        return if self.color {
                            line.clone()
                        } else {
                            Line::raw(line.to_string())
                        };
                    }
                };
                let color = match tone {
                    Tone::Thought | Tone::Dim => MUTED,
                    Tone::Text => Color::Reset,
                    Tone::User => ACCENT,
                    Tone::Tool => Color::Yellow,
                    Tone::Added => Color::Green,
                    Tone::Removed => Color::Red,
                };
                Line::from(text.as_str()).style(ink(color, self.color))
            })
            .collect();
        lines.extend(
            live.into_iter()
                .rev()
                .map(|s| Line::from(s).style(ink(MUTED, self.color))),
        );
        frame.render_widget(Paragraph::new(lines), history);
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            [(elapsed.as_millis() / 125 % 10) as usize];
        let hints = if width >= 65 {
            "   Enter steer · Alt-Enter queue · Ctrl-C stop"
        } else {
            ""
        };
        let status = format!(
            "{spinner} Working…  {}:{:02}{hints}",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        );
        frame.render_widget(
            Paragraph::new(status).style(ink(ACCENT, self.color).add_modifier(Modifier::BOLD)),
            activity,
        );
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(ink(MUTED, self.color));
        let inner = block.inner(prompt);
        frame.render_widget(block, prompt);
        let (row, column) = caret;
        let skip = (row + 1).saturating_sub(inner.height.into());
        let prompt_lines = draft_lines
            .into_iter()
            .skip(skip)
            .take(inner.height.into())
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(prompt_lines), inner);
        let stats = self.footer.render(width, false);
        let footer_lines: Vec<_> = stats.lines().skip(1).map(Line::from).collect();
        frame.render_widget(
            Paragraph::new(footer_lines).style(ink(MUTED, self.color)),
            footer,
        );
        if inner.height > 0 && inner.width > 0 {
            frame.set_cursor_position((
                inner.x + (2 + column).min(usize::from(inner.width - 1)) as u16,
                inner.y + ((row.saturating_sub(skip)) as u16).min(inner.height - 1),
            ));
        }
    }
    pub fn read(&mut self, editor: &mut Option<Reedline>) -> io::Result<Signal> {
        let mut engine = editor
            .take()
            .unwrap()
            .with_status_line(Box::new(self.footer.clone()));
        self.wrapped.lock().unwrap().0 = 0;
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        let signal = loop {
            let signal = engine.read_line(&ui::Status(self))?;
            if let Signal::HostCommand(action) = &signal {
                if action.starts_with(':') {
                    break Signal::Success(action.clone());
                }
                if action == "paste-image" {
                    match crate::attachments::clipboard() {
                        Ok(image) => {
                            self.clipboard = Some(image);
                            self.line(Tone::Dim, "Clipboard image attached · :detach to remove");
                        }
                        Err(e) => self.line(Tone::Removed, &e.to_string()),
                    }
                    self.wrapped.lock().unwrap().0 = 0;
                    continue;
                }
                if action == "expand" || action == "thoughts" {
                    if action == "paste-image" {
                        match crate::attachments::clipboard() {
                            Ok(image) => {
                                self.clipboard = Some(image);
                                self.line(
                                    Tone::Dim,
                                    "Clipboard image attached · :detach to remove",
                                );
                            }
                            Err(e) => self.line(Tone::Removed, &e.to_string()),
                        }
                        self.wrapped.lock().unwrap().0 = 0;
                        continue;
                    }
                    if action == "expand" {
                        self.expanded = !self.expanded;
                    } else {
                        self.thoughts = !self.thoughts;
                    }
                    self.wrapped.lock().unwrap().0 = 0;
                    continue;
                }
                let (w, h) = Self::dimensions();
                self.reflow(w);
                let page = h
                    .saturating_sub(5 + self.input_rows.load(Ordering::Relaxed))
                    .max(1);
                self.offset = if action == "page-up" {
                    self.offset
                        .saturating_add(page)
                        .min(self.wrapped.lock().unwrap().1.len().saturating_sub(page))
                } else {
                    self.offset.saturating_sub(page)
                };
                continue;
            }
            break signal;
        };
        *editor = Some(engine);
        Ok(signal)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }
}
// Reedline measures the editable buffer before rendering its prompt. Reserve those
// physical rows so multiline paste grows upwards and the footer stays in view.
struct InputLayout(Arc<AtomicUsize>);
impl reedline::Highlighter for InputLayout {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        let width = Screen::dimensions().0 + 1;
        let rows = line
            .split('\n')
            .map(|s| (2 + s.width()).div_ceil(width))
            .sum();
        self.0.store(rows, Ordering::Relaxed);
        let mut text = reedline::StyledText::default();
        text.push((Default::default(), line.to_owned()));
        text
    }
}
impl Screen {
    pub fn editor(&mut self, root: &Path) -> Reedline {
        let mut insert = default_vi_insert_keybindings();
        use reedline::MenuBuilder;
        insert.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion".into()),
                ReedlineEvent::MenuNext,
            ]),
        );
        insert.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('g'),
            ReedlineEvent::OpenEditor,
        );
        insert.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('v'),
            ReedlineEvent::ExecuteHostCommand("paste-image".into()),
        );
        let mut normal = default_vi_normal_keybindings();
        let mut visual = default_vi_visual_keybindings();
        for bindings in [&mut insert, &mut normal, &mut visual] {
            for (key, name) in [('o', "expand"), ('t', "thoughts")] {
                bindings.add_binding(
                    KeyModifiers::CONTROL,
                    KeyCode::Char(key),
                    ReedlineEvent::ExecuteHostCommand(name.into()),
                );
            }
            for (key, name) in [
                (KeyCode::PageUp, "page-up"),
                (KeyCode::PageDown, "page-down"),
            ] {
                bindings.add_binding(
                    KeyModifiers::NONE,
                    key,
                    ReedlineEvent::ExecuteHostCommand(name.into()),
                );
            }
        }
        self.footer.vim = crate::vim::Vim::new(insert, normal, visual);
        Reedline::create()
            .with_completer(Box::new(crate::attachments::Completion(root.into())))
            .with_menu(reedline::ReedlineMenu::EngineCompleter(Box::new(
                reedline::ColumnarMenu::default().with_name("completion"),
            )))
            .with_highlighter(Box::new(InputLayout(self.input_rows.clone())))
            .with_edit_mode(Box::new(self.footer.vim.clone()))
            .use_bracketed_paste(true)
            .with_ansi_colors(std::env::var_os("NO_COLOR").is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn streamed_reply_matches_reopening_the_saved_session() {
        use crate::{active::display, show};
        use panda_core::{Call, Cancel, Emit, Event, Message, Provider, Store, turn};
        struct Script(&'static str);
        impl Provider for Script {
            fn reply(
                &mut self,
                _: &str,
                _: &[Message],
                _: &[serde_json::Value],
                _: &Cancel,
                emit: &mut Emit,
            ) -> anyhow::Result<(String, Vec<Call>)> {
                for c in self.0.chars() {
                    emit(Event::Text(c.to_string()));
                }
                Ok((self.0.into(), vec![]))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut db = panda_local::store::Disk::open(dir.path(), dir.path(), None).unwrap();
        let mut live = Screen::open(false).unwrap();
        live.active = true;
        show(&db, &mut live).unwrap();
        let text = "## Answer\n\n- **bold** and `inline`\n\n```rs\nfn main() {}\n```";
        live.line(Tone::User, "you> review");
        let coding = panda_coding::Coding {
            workspace: panda_memory::Files(Default::default()),
            skills: panda_memory::EmptySkills,
        };
        let mut completed = 0;
        turn(
            &mut Script(text),
            &mut db,
            &coding,
            "review".into(),
            &Cancel::default(),
            &mut |event| {
                if matches!(event, Event::AssistantComplete(_)) {
                    let before = live.body();
                    display(event, &mut live, &mut None);
                    assert_eq!(live.body(), before);
                    completed += 1;
                } else {
                    display(event, &mut live, &mut None);
                    live.reflow(37);
                }
            },
        )
        .unwrap();
        assert_eq!(completed, 1);
        assert!(
            matches!(db.load().unwrap().history.last(),Some(Message::Assistant{text:raw,..}) if raw==text)
        );
        let id = db.id();
        drop(db);
        let db = panda_local::store::Disk::open(dir.path(), dir.path(), Some(id)).unwrap();
        let mut resumed = Screen::open(false).unwrap();
        resumed.active = true;
        show(&db, &mut resumed).unwrap();
        assert_eq!(live.body(), resumed.body());
        for w in [11, 37, 80] {
            let frame = |s: &Screen| {
                let mut term =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 30)).unwrap();
                term.draw(|f| s.draw_working(f, "", 0, None, std::time::Duration::ZERO))
                    .unwrap();
                term.backend().buffer().clone()
            };
            assert_eq!(frame(&live), frame(&resumed));
        }
        live.active = false;
        resumed.active = false;
    }

    #[test]
    fn oversized_grapheme_preserves_both_characters() {
        assert_eq!(wrap("🪀𑩗", 2), ["🪀", "𑩗"]);
    }

    #[test]
    fn work_indicator_is_above_prompt_even_with_a_draft() {
        let screen = Screen::open(false).unwrap();
        for draft in ["", "correction"] {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 18)).unwrap();
            term.draw(|f| {
                screen.draw_working(f, draft, draft.len(), None, std::time::Duration::ZERO)
            })
            .unwrap();
            let buf = term.backend().buffer();
            let lines = buf
                .content
                .chunks(40)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect::<Vec<_>>();
            let status = lines.iter().position(|l| l.contains("Working")).unwrap();
            assert!(lines[status + 1].contains("──"));
            assert!(lines[status + 2].contains(&format!("❯ {draft}")));
            assert!(lines[status + 3].contains("──"));
            assert!(lines.len() <= 18);
        }
    }
    proptest! {
        #[test]
        fn layout_fits_terminal_cells(text in any::<String>(), width in 2usize..160) {
            let clean=ui::inline(&text,256);
            let rows=wrap(&clean,width);
            prop_assert!(rows.iter().all(|s|s.width()<=width));
            prop_assert_eq!(rows.concat(),clean);
        }
    }

    proptest! {
        #[test]
        fn busy_and_picker_frames_fit_small_terminals(w in 1u16..95, h in 1u16..30, draft in ".{0,64}") {
            let screen=Screen::open(false).unwrap();
            let mut term=ratatui::Terminal::new(ratatui::backend::TestBackend::new(w,h)).unwrap();
            term.draw(|f|screen.draw_working(f,&draft,draft.len(),None,std::time::Duration::from_secs(123))).unwrap();
            let labels=vec![draft.clone()];
            term.draw(|f| crate::renderer::picker(f,"Reasoning effort",&draft,&labels,&[0],0,false)).unwrap();
            prop_assert_eq!(term.backend().buffer().content.len(),usize::from(w)*usize::from(h));
        }
    }

    #[test]
    fn footer_uses_two_status_rows_below_the_rule() {
        let footer = Footer {
            vim: Default::default(),
            effort: None,
            location: "~:code (main)".into(),
            session: "Plan".into(),
            usage: "↑6 ↓12".into(),
            model: "deepseek/flash".into(),
        };
        let plain = footer.render(60, false);
        let rows: Vec<_> = plain.lines().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "─".repeat(60));
        assert_eq!(rows[1], "~:code (main)");
        assert_eq!(rows[2].width(), 60);
        assert!(rows[2].starts_with("insert  Plan"));
        assert!(rows[2].ends_with("deepseek/flash"));
        assert!(!plain.contains('\x1b'));
    }
}
