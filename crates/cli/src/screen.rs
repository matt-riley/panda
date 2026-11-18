use crate::{safe, ui};
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use panda_core::Call;
use reedline::{
    KeyCode, KeyModifiers, Reedline, ReedlineEvent, Signal, StatusLine, Vi,
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
    pub location: String,
    pub session: String,
    pub model: String,
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
        let right = ui::inline(&self.model, 150);
        let left = format!("{} {}", self.session, self.usage)
            .trim_end()
            .to_owned();
        let gap = width.saturating_sub(left.width() + right.width()).max(1);
        let stats = if left.width() + right.width() < width {
            format!("{left}{}{right}", " ".repeat(gap))
        } else {
            format!("{left} · {right}")
        };
        let line = |s: &str| wrap(s, width).into_iter().next().unwrap_or_default();
        Tone::Dim.paint(
            &format!(
                "{}\n{}\n{}",
                "─".repeat(width),
                line(&self.location),
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
    wrapped: Mutex<(usize, Vec<Row>)>,
    pub footer: Footer,
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
            offset: 0,
            input_rows: Arc::new(AtomicUsize::new(1)),
            footer: Footer::default(),
        })
    }
    pub fn line(&mut self, tone: Tone, text: &str) {
        if !self.active {
            println!("{text}");
            return;
        }
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
        self.delta(Tone::Text, text);
    }
    pub fn delta(&mut self, tone: Tone, text: &str) {
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
        self.rows.clear();
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
                if (self.expanded || self.selected_tool == Some(i))
                    && let Some((end, full)) = self.previews.get(&i)
                {
                    skip = *end;
                    for line in full.lines() {
                        cache
                            .1
                            .extend(wrap(line, width).into_iter().map(|s| (*tone, s)));
                    }
                } else {
                    cache
                        .1
                        .extend(wrap(s, width).into_iter().map(|s| (*tone, s)));
                }
            }
        }
    }
    pub fn body(&self) -> String {
        let (w, h) = Self::dimensions();
        self.reflow(w);
        let cache = self.wrapped.lock().unwrap();
        let live: Vec<_> = self.live.lines().rev().take(3).collect();
        let count = h.saturating_sub(5 + self.input_rows.load(Ordering::Relaxed) + live.len());
        let end = cache
            .1
            .len()
            .saturating_sub(self.offset.min(cache.1.len().saturating_sub(count)));
        let start = end.saturating_sub(count);
        let mut out = String::new();
        for (tone, s) in &cache.1[start..end] {
            out.push_str(&tone.paint(s, self.color));
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
    pub fn working(&self, draft: &str) -> io::Result<()> {
        if self.active {
            self.wrapped.lock().unwrap().0 = 0;
            let mut out = io::stdout().lock();
            execute!(out, MoveTo(0, 0), Clear(ClearType::FromCursorDown))?;
            write!(
                out,
                "{}{}\r\n{}",
                self.body().replace('\n', "\r\n"),
                Tone::Tool.paint(
                    &if draft.is_empty() {
                        "Working…  Enter steers · Alt-Enter follows · Ctrl-C cancels".into()
                    } else {
                        format!(
                            "❯ {}",
                            ui::inline(draft, Self::dimensions().0.saturating_sub(2))
                        )
                    },
                    self.color
                ),
                self.footer
                    .render(Self::dimensions().0, self.color)
                    .replace('\n', "\r\n")
            )?;
            out.flush()?;
        }
        Ok(())
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
                if action == "paste-image" {
                    match crate::attachments::clipboard() {
                        Ok(image) => {
                            self.clipboard = Some(image);
                            self.line(Tone::Dim, "Clipboard image attached · /detach to remove");
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
                                    "Clipboard image attached · /detach to remove",
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
            .map(|s| (9 + s.width()).div_ceil(width))
            .sum();
        self.0.store(rows, Ordering::Relaxed);
        let mut text = reedline::StyledText::default();
        text.push((Default::default(), line.to_owned()));
        text
    }
}
impl Screen {
    pub fn editor(&self, root: &Path) -> Reedline {
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
        Reedline::create()
            .with_completer(Box::new(crate::attachments::Completion(root.into())))
            .with_menu(reedline::ReedlineMenu::EngineCompleter(Box::new(
                reedline::ColumnarMenu::default().with_name("completion"),
            )))
            .with_highlighter(Box::new(InputLayout(self.input_rows.clone())))
            .with_edit_mode(Box::new(Vi::new(insert, normal, visual)))
            .use_bracketed_paste(true)
            .with_ansi_colors(std::env::var_os("NO_COLOR").is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn oversized_grapheme_preserves_both_characters() {
        assert_eq!(wrap("🪀𑩗", 2), ["🪀", "𑩗"]);
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

    #[test]
    fn footer_uses_two_status_rows_below_the_rule() {
        let footer = Footer {
            location: "~/code (main)".into(),
            session: "Plan".into(),
            usage: "↑6 ↓12".into(),
            model: "deepseek/flash".into(),
        };
        let plain = footer.render(60, false);
        let rows: Vec<_> = plain.lines().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "─".repeat(60));
        assert_eq!(rows[1], "~/code (main)");
        assert_eq!(rows[2].width(), 60);
        assert!(rows[2].ends_with("deepseek/flash"));
        assert!(!plain.contains('\x1b'));
    }
}
