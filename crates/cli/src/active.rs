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

pub fn display(event: Event, screen: &mut Screen, pending: &mut Option<Call>) {
    match event {
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
        Event::Calling(call) if screen.active => {
            screen.line(Tone::Tool, &ui::header(&call));
            *pending = Some(call);
        }
        Event::Result(text) if screen.active => screen.tool_result(
            pending.take().as_ref(),
            &text,
            text.starts_with("tool error: "),
        ),
        Event::Calling(c) => println!(
            "\n[{} {}] {}",
            safe(&c.name),
            safe(&c.id),
            safe(&c.args.to_string())
        ),
        Event::Result(s) => println!("{}", safe(&s)),
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
    draft: &mut String,
) -> Result<()> {
    let mut pending = None;
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
                    terminal.draw(|frame| screen.draw_working(frame, draft, started.elapsed()))?;
                    last_frame = std::time::Instant::now();
                    dirty = false;
                }
                if !event::poll(Duration::from_millis(25))? {
                    continue;
                }
                match event::read()? {
                    Input::Resize(..) => {}
                    Input::Paste(s) => draft.push_str(&safe(&s)),
                    Input::Key(key) if key.kind != KeyEventKind::Release => {
                        match (key.code, key.modifiers) {
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
                                target.push(std::mem::take(draft));
                            }
                            (KeyCode::Up, KeyModifiers::ALT) => {
                                let mut q = queue.lock().unwrap();
                                let mut messages = q.take(false);
                                messages.append(&mut q.follow);
                                messages.push(std::mem::take(draft));
                                *draft = messages.join("\n").trim_end().into();
                            }
                            (KeyCode::Backspace, _) => {
                                draft.pop();
                            }
                            (KeyCode::Char(c), mods)
                                if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                draft.push(c)
                            }
                            _ => continue,
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
        held.push(std::mem::take(draft));
        *draft = held.join("\n").trim_end().into();
    }
    std::io::stdout().flush()?;
    result
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
        run(
            &mut Script,
            &mut store,
            &coding,
            Message::User("begin".into()),
            &Cancel::default(),
            &mut screen,
            &mut String::new(),
        )
        .unwrap();
        assert!(
            matches!(store.load().unwrap().history.as_slice(), [Message::User(_),Message::Assistant{..},Message::Result{..},Message::User(c),Message::Assistant{..},Message::User(n),Message::Assistant{..}] if c=="correction" && n=="next")
        );
    }
}
