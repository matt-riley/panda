use crate::{
    safe,
    screen::{Screen, Tone},
    ui,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use panda_core::*;
use std::{
    io::Write,
    sync::{Mutex, mpsc},
    time::Duration,
};

#[derive(Default)]
struct Queue {
    steer: Vec<String>,
    follow: Vec<String>,
}
impl Queue {
    fn take(&mut self, idle: bool) -> Vec<String> {
        let mut messages = std::mem::take(&mut self.steer);
        if idle && messages.is_empty() {
            messages.append(&mut self.follow);
        }
        messages
    }
}
struct Raw;
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), event::DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Default)]
pub struct PendingTool {
    call: Option<Call>,
    details: Option<ToolDetails>,
}
pub fn display(event: Event, screen: &mut Screen, pending: &mut PendingTool) {
    match event {
        Event::Truncated { text, retrying } => {
            if screen.active {
                screen.finish_reply(&text);
            }
            screen.line(
                Tone::Dim,
                if retrying {
                    "Partial reply saved. Retrying once."
                } else {
                    "Partial reply saved (output length limit)."
                },
            );
        }
        Event::Retry { attempt, seconds } => {
            if screen.active {
                screen.retry = Some((
                    attempt,
                    std::time::Instant::now() + Duration::from_secs(seconds),
                ));
            } else {
                eprintln!("Retry {attempt}/3 in {seconds}s");
            }
        }
        Event::Reasoning(s) => {
            if screen.active {
                screen.delta(Tone::Thought, &s);
            }
        }
        Event::ToolOutput(s) => {
            if screen.active {
                screen.live_chunk(&s);
            }
        }
        Event::Text(s) => screen.stream(&s),
        Event::ResultDetails { details, .. } => pending.details = Some(details),
        Event::NestedRead(_) => {}
        Event::NestedCall { call, .. } => {
            let line = format!("  {}", ui::header(&call));
            if screen.active {
                screen.line(Tone::Tool, &line);
            } else {
                println!("{line}");
            }
        }
        Event::AssistantComplete(s) => {
            if screen.active {
                screen.finish_reply(&s);
            }
        }
        Event::Calling(call) if screen.active => {
            screen.line(Tone::Tool, &ui::header(&call));
            pending.call = Some(call);
        }
        Event::Result(text) => {
            let error = text.starts_with("tool error: ");
            let display = result_text(&text, pending.details.take().as_ref());
            if screen.active {
                screen.tool_result(pending.call.take().as_ref(), &display, error);
            } else {
                println!("{}", safe(&display));
            }
        }
        Event::Calling(c) => println!(
            "\n[{} {}] {}",
            safe(&c.name),
            safe(&c.id),
            safe(&c.args.to_string())
        ),
        Event::Usage(u) => {
            let input = u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64());
            let output = u["output_tokens"]
                .as_u64()
                .or(u["completion_tokens"].as_u64());
            if let (Some(i), Some(o)) = (input, output) {
                screen.footer.usage = format!("↑{i} ↓{o}");
            }
        }
    }
}

pub fn run(
    p: &mut (dyn Provider + Send),
    db: &mut (dyn Store + Send),
    tools: &(dyn Toolset + Sync),
    input: Message,
    cancel: &Cancel,
    screen: &mut Screen,
    editor: Option<&mut reedline::Reedline>,
) -> Result<()> {
    screen.retry = None;
    screen.end_reply();
    let mut pending = PendingTool::default();
    if !screen.active {
        turn_message(
            p,
            db,
            tools,
            input,
            cancel,
            &mut |e| display(e, screen, &mut pending),
            &mut |_| vec![],
        )?;
        println!();
        return Ok(());
    }
    let mut prompt = crate::vim::Steering {
        vim: screen.footer.vim.clone(),
        editor: editor.expect("interactive screen requires its editor"),
    };
    terminal::enable_raw_mode()?;
    let _raw = Raw;
    crossterm::execute!(std::io::stdout(), event::EnableBracketedPaste)?;
    let mut terminal = crate::renderer::terminal()?;
    let queue = Mutex::new(Queue::default());
    let (tx, rx) = mpsc::channel();
    let result = std::thread::scope(|scope| {
        let queue = &queue;
        let worker = scope.spawn(move || {
            turn_message(
                p,
                db,
                tools,
                input,
                cancel,
                &mut |e| {
                    let _ = tx.send(e);
                },
                &mut |idle| queue.lock().unwrap().take(idle),
            )
        });
        let started = std::time::Instant::now();
        let mut last_frame = started;
        let mut dirty = true;
        let mut handle = || -> Result<()> {
            loop {
                for e in rx.try_iter() {
                    display(e, screen, &mut pending);
                    dirty = true;
                }
                if worker.is_finished() {
                    break;
                }
                if dirty || last_frame.elapsed() >= Duration::from_millis(125) {
                    terminal.draw(|frame| {
                        screen.draw_working(
                            frame,
                            prompt.editor.current_buffer_contents(),
                            prompt.editor.current_insertion_point(),
                            prompt.editor.current_selection(),
                            started.elapsed(),
                        )
                    })?;
                    last_frame = std::time::Instant::now();
                    dirty = false;
                }
                if !event::poll(Duration::from_millis(25))? {
                    continue;
                }
                match event::read()? {
                    Input::Resize(..) => {}
                    Input::Paste(s) => {
                        prompt.input(Input::Paste(safe(&s)));
                    }
                    Input::Key(key) if key.kind != KeyEventKind::Release => {
                        let draft = prompt.editor.current_buffer_contents();
                        match (key.code, key.modifiers) {
                            _ if prompt.vim.commanding() => {
                                if let Some(command) = prompt.input(Input::Key(key)) {
                                    if matches!(command.as_str(), ":q" | ":quit") {
                                        cancel.stop();
                                    }
                                    screen.commands.push_back(command);
                                }
                            }
                            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                                screen.commands.push_back(":copy".into())
                            }
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => cancel.stop(),
                            (KeyCode::Enter, mods) if !draft.trim().is_empty() => {
                                let follow = mods.contains(KeyModifiers::ALT);
                                let mut q = queue.lock().unwrap();
                                screen.line(
                                    Tone::User,
                                    &format!(
                                        "Queued {} · {}",
                                        if follow { "follow-up" } else { "steering" },
                                        draft
                                    ),
                                );
                                let target = if follow { &mut q.follow } else { &mut q.steer };
                                target.push(draft.to_owned());
                                prompt.replace(String::new());
                            }
                            (KeyCode::Up, KeyModifiers::ALT) => {
                                let mut q = queue.lock().unwrap();
                                let mut messages = q.take(false);
                                messages.append(&mut q.follow);
                                messages.push(draft.to_owned());
                                prompt.replace(messages.join("\n").trim_end().into());
                            }
                            _ => {
                                prompt.input(Input::Key(key));
                            }
                        }
                    }
                    _ => continue,
                }
                dirty = true;
            }
            Ok(())
        };
        let ui_result = handle();
        if ui_result.is_err() {
            cancel.stop();
        }
        let done = worker
            .join()
            .map_err(|_| anyhow::anyhow!("turn worker failed"))?;
        for e in rx {
            display(e, screen, &mut pending);
        }
        ui_result.and(done)
    });
    let mut q = queue.into_inner().unwrap();
    let mut held = q.take(false);
    held.append(&mut q.follow);
    if !held.is_empty() {
        held.push(prompt.editor.current_buffer_contents().to_owned());
        prompt.replace(held.join("\n").trim_end().into());
    }
    std::io::stdout().flush()?;
    result
}

pub(super) fn result_text<'a>(
    text: &'a str,
    details: Option<&ToolDetails>,
) -> std::borrow::Cow<'a, str> {
    let Some(ToolDetails::Codemode {
        elapsed_ms,
        success,
        images,
        items,
    }) = details
    else {
        return text.into();
    };
    let status = if *success { "completed" } else { "failed" };
    let mut lines = vec![format!("Codemode {status} in {elapsed_ms} ms")];
    let count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { .. }))
        .count();
    let mut number = 0;
    if items.is_empty() {
        lines.push(text.to_owned());
        lines.extend(
            images
                .iter()
                .map(|i| format!("Image · {}", ui::inline(&i.label, 180))),
        );
    } else {
        for item in items {
            match item {
                OutputItem::Text { start, end } | OutputItem::Console { start, end } => {
                    let Some(chunk) = text.get(*start..*end) else {
                        return text.into();
                    };
                    if matches!(item, OutputItem::Text { .. }) && count > 1 {
                        number += 1;
                        lines.push(format!("==> text {number}/{count} <=="));
                    }
                    lines.push(chunk.to_owned());
                }
                OutputItem::Image { index } => {
                    if let Some(image) = images.get(*index) {
                        lines.push(format!("Image · {}", ui::inline(&image.label, 180)));
                    }
                }
            }
        }
    }
    lines.join("\n").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use panda_coding::Coding;
    use panda_local::sandbox::Native;
    use panda_memory::{EmptySkills, MemoryStore};
    use serde_json::Value;

    #[test]
    #[ignore = "requires native macOS Seatbelt"]
    fn interactive_worker_receives_real_shell_steering() {
        let status = std::process::Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/steering.py"))
            .arg(std::env::current_exe().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
    }

    // The parent test gives this child a controlling terminal and types into it.
    #[test]
    fn fixture() {
        if std::env::var_os("PANDA_STEERING_FIXTURE").is_none() {
            return;
        }
        struct Script;
        impl Provider for Script {
            fn reply(
                &mut self,
                _: &str,
                h: &[Message],
                _: &[Value],
                _: &Cancel,
                emit: &mut Emit,
            ) -> Result<(String, Vec<Call>)> {
                match h.last().unwrap() {
                    Message::User(s) if s == "begin" => Ok((
                        String::new(),
                        vec![Call {
                            id: "shell".into(),
                            name: "shell".into(),
                            args: serde_json::json!({"command":"printf 'RUNNING%s' 'FIXTURE'; /bin/sleep 1; printf 'done'"}),
                        }],
                    )),
                    Message::User(s) if s == "correction" || s == "next" => {
                        let text = format!("received {s}");
                        emit(Event::Text(text.clone()));
                        Ok((text, vec![]))
                    }
                    other => anyhow::bail!("unexpected next request: {other:?}"),
                }
            }
        }
        let native = Native::new(
            &std::env::current_dir().unwrap(),
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        let coding = Coding {
            workspace: native,
            skills: EmptySkills,
        };
        let mut screen = Screen::open(true).unwrap();
        let mut store = MemoryStore::default();
        let mut editor = screen.editor(std::path::Path::new("."));
        run(
            &mut Script,
            &mut store,
            &coding,
            Message::User("begin".into()),
            &Cancel::default(),
            &mut screen,
            Some(&mut editor),
        )
        .unwrap();
        assert_eq!(screen.commands.pop_front().as_deref(), Some(":model"));
        assert!(
            matches!(store.load().unwrap().history.as_slice(), [Message::User(_),Message::Assistant{..},Message::Result{..},Message::User(c),Message::Assistant{..},Message::User(n),Message::Assistant{..}] if c=="correction" && n=="next")
        );
    }
}
