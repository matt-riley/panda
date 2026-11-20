use crate::attachments;
use anyhow::{Context, Result, bail, ensure};
use panda_core::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, BufRead, IsTerminal, Read},
    path::{Path, PathBuf},
    sync::{Mutex, mpsc},
    time::Duration,
};

#[derive(Default, PartialEq)]
pub enum Interface {
    #[default]
    Terminal,
    Print,
    Json,
    Rpc,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum CodemodeExposure {
    #[default]
    Off,
    Alongside,
    Only,
}

#[derive(Default)]
pub struct Options {
    pub interface: Interface,
    pub codemode: CodemodeExposure,
    pub tools: Option<BTreeSet<String>>,
    pub resume: bool,
    pub session_id: Option<i64>,
    pub session_dir: Option<PathBuf>,
    pub no_session: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub prompt: String,
}
impl Options {
    pub fn parse(args: &[String], interactive_terminal: bool) -> Result<Self> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--codemode" => {
                    if out.codemode == CodemodeExposure::Off {
                        out.codemode = CodemodeExposure::Alongside;
                    }
                }
                "--codemode-only" => out.codemode = CodemodeExposure::Only,
                "--tools" | "--no-tools" => {
                    ensure!(out.tools.is_none(), "specify --tools or --no-tools once");
                    let mut names = BTreeSet::new();
                    if arg == "--tools" {
                        let list = it
                            .next()
                            .filter(|s| !s.starts_with('-'))
                            .context("expected comma-separated tool names after --tools")?;
                        for name in list.split(',').map(str::trim) {
                            ensure!(!name.is_empty(), "empty name in --tools");
                            names.insert(name.to_owned());
                        }
                    }
                    out.tools = Some(names);
                }
                "--no-session" => out.no_session = true,
                "--session-dir" => {
                    ensure!(out.session_dir.is_none(), "specify --session-dir once");
                    let dir = it
                        .next()
                        .filter(|s| !s.is_empty() && !s.starts_with('-'))
                        .context("expected a directory after --session-dir")?;
                    out.session_dir = Some(PathBuf::from(dir));
                }
                "--session" => {
                    ensure!(out.session_id.is_none(), "specify --session once");
                    let id = it
                        .next()
                        .and_then(|value| value.parse::<i64>().ok())
                        .filter(|&id| id > 0)
                        .context("expected a positive numeric session ID after --session")?;
                    out.session_id = Some(id);
                }
                "--system-prompt" | "--append-system-prompt" => {
                    let value = it
                        .next()
                        .filter(|s| !s.starts_with("--") && !matches!(s.as_str(), "-p" | "-c"))
                        .with_context(|| format!("expected text or a file path after {arg}"))?;
                    if arg == "--system-prompt" {
                        ensure!(out.system_prompt.is_none(), "specify --system-prompt once");
                        out.system_prompt = Some(value.clone());
                    } else {
                        out.append_system_prompt.push(value.clone());
                    }
                }
                "--provider" | "--model" | "--thinking" => {
                    let value = it
                        .next()
                        .filter(|s| !s.trim().is_empty() && !s.starts_with('-'))
                        .with_context(|| format!("expected a value for {arg}"))?
                        .clone();
                    match arg.as_str() {
                        "--provider" => out.provider = Some(crate::provider_id(&value)?.to_owned()),
                        "--model" => out.model = Some(value),
                        _ => out.thinking = Some(value),
                    }
                }
                "--continue" | "-c" => out.resume = true,
                "--print" | "-p" => out.interface = Interface::Print,
                "--mode" => {
                    out.interface = match it.next().map(String::as_str) {
                        Some("json") => Interface::Json,
                        Some("rpc") => Interface::Rpc,
                        _ => bail!("expected --mode json or rpc"),
                    }
                }
                "--" => {
                    out.prompt = it.cloned().collect::<Vec<_>>().join(" ");
                    break;
                }
                x if x.starts_with('-') => bail!("unknown argument {x}"),
                _ => {
                    out.prompt = std::iter::once(arg)
                        .chain(it)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ");
                    break;
                }
            }
        }
        ensure!(
            out.interface != Interface::Rpc || out.prompt.is_empty(),
            "RPC reads requests on stdin"
        );
        let one_shot = out.codemode == CodemodeExposure::Only
            || out.no_session
            || out.session_id.is_some()
            || out.session_dir.is_some()
            || out.model.is_some()
            || out.thinking.is_some()
            || out.provider.is_some()
            || out.tools.is_some()
            || out.system_prompt.is_some()
            || !out.append_system_prompt.is_empty();
        if out.interface == Interface::Terminal
            && !out.prompt.is_empty()
            && (!interactive_terminal || one_shot)
        {
            out.interface = Interface::Print;
        }
        if one_shot {
            ensure!(
                matches!(out.interface, Interface::Print | Interface::Json),
                "one-shot options apply to print and JSON runs"
            );
        }
        ensure!(
            out.session_id.is_none() || !(out.resume || out.no_session),
            "--session conflicts with --continue or --no-session"
        );
        ensure!(
            !out.no_session || !out.resume,
            "--no-session conflicts with --continue"
        );
        ensure!(
            out.provider.is_none() || out.model.is_some(),
            "--provider requires --model"
        );
        if let Some(names) = &mut out.tools {
            let only = out.codemode == CodemodeExposure::Only && !names.is_empty();
            let listed = names.remove("codemode");
            out.codemode = if only {
                CodemodeExposure::Only
            } else if listed {
                CodemodeExposure::Alongside
            } else {
                CodemodeExposure::Off
            };
        }
        Ok(out)
    }
}

/// An explicit CLI argument names a regular UTF-8 file or supplies the text itself.
pub fn prompt_text(value: &str) -> Result<String> {
    match std::fs::metadata(value) {
        Ok(info) => {
            ensure!(info.is_file(), "expected a regular prompt file: {value}");
            std::fs::read_to_string(value).with_context(|| format!("reading prompt file {value}"))
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) || e.raw_os_error() == Some(libc::ENAMETOOLONG) =>
        {
            Ok(value.to_owned())
        }
        Err(e) => Err(e).with_context(|| format!("checking prompt file {value}")),
    }
}

/// Keep the turn's token budgeting, retries and tool rounds on the same prompt.
pub struct PromptedTools<'a> {
    pub tools: &'a (dyn Toolset + Sync),
    pub prompt: String,
}
impl Toolset for PromptedTools<'_> {
    fn run_in_session(
        &self,
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(panda_core::ToolOutput, panda_core::StoreWrites)> {
        self.tools.run_in_session(session, call, cancel, emit)
    }
    fn system(&self) -> Result<String> {
        Ok(self.prompt.clone())
    }
    fn schemas(&self, mode: Mode) -> Vec<Value> {
        self.tools.schemas(mode)
    }
    fn run(&self, mode: Mode, call: &Call, cancel: &Cancel) -> Result<String> {
        self.tools.run(mode, call, cancel)
    }
    fn run_stream(
        &self,
        mode: Mode,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<panda_core::ToolOutput> {
        self.tools.run_stream(mode, call, cancel, emit)
    }
}

pub struct Host<'a> {
    pub db: &'a mut (dyn Store + Send),
    pub session_id: Option<i64>,
    pub tools: &'a (dyn Toolset + Sync),
    pub connect: &'a dyn Fn() -> Result<Box<dyn Provider + Send>>,
    pub root: &'a Path,
    pub cancel: &'a Cancel,
}
impl Host<'_> {
    pub fn run(&mut self, options: Options) -> Result<()> {
        if options.interface == Interface::Rpc {
            return self.rpc();
        }
        let input = one_shot_input(self.root, &options.prompt)?;
        let mut p = (self.connect)()?;
        if let Some(level) = options.thinking {
            p = Box::new(FixedEffort {
                provider: p,
                level: (level != "default").then_some(level),
            });
        }
        let structured = options.interface == Interface::Json;
        turn_message(
            p.as_mut(),
            self.db,
            self.tools,
            input,
            self.cancel,
            &mut |e| {
                if structured {
                    println!("{}", json!({"type":"event","event":e}));
                }
            },
            &mut |_| vec![],
        )?;
        let s = self.db.load()?;
        let Some(Message::Assistant { text, .. }) = s.history.last() else {
            bail!("missing final response");
        };
        if structured {
            println!(
                "{}",
                json!({"type":"done","session_id":self.session_id,"text":text})
            );
        } else {
            println!("{text}");
        }
        Ok(())
    }
    fn rpc(&mut self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        while let Ok(line) = rx.recv() {
            let Some(r) = decode(&line) else { continue };
            let id = &r["id"];
            let result = (|| -> Result<Value> {
                Ok(match r["type"].as_str() {
                    Some("session") => {
                        json!({"id":id,"type":"session","session_id":self.session_id,"session":self.db.load()?})
                    }
                    Some("ack") => {
                        acknowledge(self.db)?;
                        json!({"id":id,"type":"ok"})
                    }
                    Some("prompt") => {
                        let text = r["text"].as_str().context("expected text")?;
                        let input = attachments::prepare(self.root, text, None)?;
                        let mut p = (self.connect)()?;
                        let queue = Mutex::new((Vec::new(), Vec::new()));
                        let queue_ref = &queue;
                        let cancel = self.cancel;
                        cancel.reset();
                        let db = &mut *self.db;
                        let tools = self.tools;
                        let run = std::thread::scope(|scope| -> Result<()> {
                            let worker = scope.spawn(move || {
                                turn_message(
                                    p.as_mut(),
                                    db,
                                    tools,
                                    input,
                                    cancel,
                                    &mut |e| {
                                        println!("{}", json!({"id":id,"type":"event","event":e}))
                                    },
                                    &mut |idle| {
                                        let mut q = queue_ref.lock().unwrap();
                                        let mut items = std::mem::take(&mut q.0);
                                        if idle && items.is_empty() {
                                            items.append(&mut q.1);
                                        }
                                        items
                                    },
                                )
                            });
                            while !worker.is_finished() {
                                match rx.recv_timeout(Duration::from_millis(20)) {
                                    Ok(line) => {
                                        if let Some(cmd) = decode(&line) {
                                            let cmd_id = &cmd["id"];
                                            let response = match cmd["type"].as_str() {
                                                Some("cancel") => {
                                                    cancel.stop();
                                                    json!({"id":cmd_id,"type":"ok"})
                                                }
                                                Some(kind @ ("steer" | "follow_up"))
                                                    if cmd["text"].is_string() =>
                                                {
                                                    let mut q = queue.lock().unwrap();
                                                    let dest = if kind == "steer" {
                                                        &mut q.0
                                                    } else {
                                                        &mut q.1
                                                    };
                                                    dest.push(cmd["text"].as_str().unwrap().into());
                                                    json!({"id":cmd_id,"type":"queued"})
                                                }
                                                _ => {
                                                    json!({"id":cmd_id,"type":"error","error":"turn active"})
                                                }
                                            };
                                            println!("{response}");
                                        }
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                }
                            }
                            worker
                                .join()
                                .map_err(|_| anyhow::anyhow!("turn worker failed"))?
                        });
                        let (steer, follow) = queue.into_inner().unwrap();
                        let undelivered = [steer, follow].concat();
                        match run {
                            Ok(()) => {
                                json!({"id":id,"type":"done","session_id":self.session_id,"undelivered":undelivered})
                            }
                            Err(e) => {
                                json!({"id":id,"type":"error","error":e.to_string(),"undelivered":undelivered})
                            }
                        }
                    }
                    _ => bail!("expected session, prompt or ack"),
                })
            })();
            println!(
                "{}",
                result.unwrap_or_else(|e| json!({"id":id,"type":"error","error":e.to_string()}))
            );
        }
        Ok(())
    }
}

// Apply the override at the request boundary, including retries and summary
// requests. A continued session keeps its own saved reasoning preference.
struct FixedEffort {
    provider: Box<dyn Provider + Send>,
    level: Option<String>,
}
impl Provider for FixedEffort {
    fn reply(
        &mut self,
        system: &str,
        history: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.provider
            .reply_with_effort(system, history, tools, cancel, emit, self.level.as_deref())
    }
}

fn one_shot_input(root: &Path, prompt: &str) -> Result<Message> {
    let mut piped = String::new();
    if prompt.is_empty() || !io::stdin().is_terminal() {
        io::stdin().read_to_string(&mut piped)?;
    }
    if prompt.is_empty() {
        ensure!(!piped.trim().is_empty(), "missing prompt");
        return attachments::prepare(root, &piped, None);
    }
    ensure!(
        !prompt.trim().is_empty() || !piped.trim().is_empty(),
        "missing prompt"
    );
    if !piped.is_empty() {
        if !piped.ends_with('\n') {
            piped.push('\n');
        }
        piped.push('\n');
    }
    attachments::prepare_with_prefix(root, piped, prompt, None)
}
fn decode(line: &str) -> Option<Value> {
    match serde_json::from_str(line) {
        Ok(v) => Some(v),
        Err(e) => {
            println!("{}", json!({"type":"error","error":e.to_string()}));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panda_local::store::Disk;
    use panda_memory::{EmptySkills, Files, Reader};
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[test]
    fn rejects_invalid_one_shot_options() {
        for args in [
            "--print --provider deepseek",
            "--print --provider unknown --model foo",
            "--print --model",
            "--print --thinking --no-session",
            "--no-session --continue --mode json",
            "--session 1",
            "--mode rpc --session 1",
            "--print --session",
            "--print --session invalid",
            "--print --session 0",
            "--print --session -1",
            "--print --session 9223372036854775808",
            "--print --session 1 --session 2",
            "--print --session 1 -c",
            "--print --session 1 --no-session",
            "--session-dir sessions",
            "--mode rpc --session-dir sessions",
            "--print --session-dir",
            "--print --session-dir --continue",
            "--print --session-dir one --session-dir two",
            "--mode rpc --model foo",
            "--no-session",
            "--codemode-only",
            "--mode rpc --codemode-only",
            "--tools read",
            "--mode rpc --no-tools",
            "--print --tools",
            "--mode json --tools read,",
            "--print --tools read --no-tools",
            "--print --no-tools --tools read",
            "--print --system-prompt",
            "--print --append-system-prompt --no-tools",
            "--print --system-prompt first --system-prompt second",
            "--system-prompt interactive",
            "--mode rpc --append-system-prompt extra",
        ] {
            let args = args
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(Options::parse(&args, true).is_err(), "{args:?}");
        }
    }

    #[test]
    fn prompt_files_must_be_regular_and_utf8() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prompt_text(dir.path().to_str().unwrap()).is_err());
        let file = dir.path().join("invalid.md");
        std::fs::write(&file, [0xff]).unwrap();
        assert!(prompt_text(file.to_str().unwrap()).is_err());
    }

    proptest::proptest! {
        #[test]
        fn codemode_only_accepts_nested_selections(
            include_codemode in proptest::bool::ANY, read in proptest::bool::ANY,
            search in proptest::bool::ANY, selection_first in proptest::bool::ANY,
        ) {
            let helpers: BTreeSet<_> = [("read", read), ("search", search)]
                .into_iter().filter(|(_, enabled)| *enabled).map(|(name, _)| name.to_owned()).collect();
            let mut names = helpers.clone();
            if include_codemode { names.insert("codemode".into()); }
            let mut args = if names.is_empty() {
                vec!["--no-tools".into()]
            } else {
                vec!["--tools".into(), names.iter().cloned().collect::<Vec<_>>().join(",")]
            };
            if selection_first { args.push("--codemode-only".into()); }
            else { args.insert(0, "--codemode-only".into()); }
            args.push("prompt".into());
            let options = Options::parse(&args, true).unwrap();
            proptest::prop_assert!(matches!(options.interface, Interface::Print));
            let expected = if names.is_empty() { CodemodeExposure::Off } else { CodemodeExposure::Only };
            proptest::prop_assert_eq!(options.codemode, expected);
            proptest::prop_assert_eq!(options.tools, Some(helpers));
        }

        #[test]
        fn inline_prompts_preserve_text(text in r"[^\x00]{0,650}") {
            // A single path component longer than NAME_MAX is still a valid inline prompt.
            let literal = format!("{}{}", "x".repeat(256), text);
            proptest::prop_assert_eq!(prompt_text(&literal).unwrap(), literal);
        }
    }

    #[test]
    fn oneshot_fixture() -> Result<()> {
        let Ok(args) = std::env::var("PANDA_ONESHOT_ARGS") else {
            return Ok(());
        };
        crate::run(serde_json::from_str(&args)?, |state, provider, model| {
            Ok(Box::new(
                panda_providers::Api {
                    endpoint: format!("{}/{provider}", std::env::var("PANDA_ONESHOT_SERVER")?),
                    model: model.to_owned(),
                    key: "fixture".into(),
                }
                .with_output_limit(crate::output_limit(state, provider, model)?),
            ))
        })
    }

    #[test]
    #[ignore = "requires the native macOS host and a loopback HTTP fixture"]
    fn reproducible_one_shot_requests() {
        assert!(
            Command::new("python3")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oneshot.py"))
                .arg(std::env::current_exe().unwrap())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn fixture() {
        let Ok(mode) = std::env::var("PANDA_AUTOMATION_FIXTURE") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let coding = panda_coding::Coding {
            workspace: Files([("hello.txt".into(), "fixture answer".into())].into()),
            skills: EmptySkills,
        };
        let mut db = Disk::open(root, root, None).unwrap();
        Host {
            session_id: Some(db.id()),
            db: &mut db,
            tools: &coding,
            connect: &|| Ok(Box::new(Reader)),
            root,
            cancel: &Cancel::default(),
        }
        .run(
            Options::parse(
                &match mode.as_str() {
                    "rpc" => vec!["--mode".into(), "rpc".into()],
                    "json" => vec!["--mode".into(), "json".into(), "read".into()],
                    _ => vec!["--print".into(), "read".into()],
                },
                false,
            )
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn scripted_tool_round_trip_in_each_automation_interface() {
        for mode in ["print", "json", "rpc"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "automation::tests::fixture", "--nocapture"])
                .env("PANDA_AUTOMATION_FIXTURE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            if mode == "rpc" {
                writeln!(
                    child.stdin.take().unwrap(),
                    "{}",
                    json!({"type":"prompt","id":7,"text":"read"})
                )
                .unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            let out = String::from_utf8(output.stdout).unwrap();
            if mode == "print" {
                assert!(out.lines().any(|l| l == "fixture answer"));
            } else {
                let events: Vec<Value> = out
                    .lines()
                    .filter(|l| l.starts_with('{'))
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
                assert!(events.iter().any(|v| v["event"]["type"] == "calling"));
                assert!(
                    events
                        .iter()
                        .any(|v| v["event"]["data"] == "fixture answer")
                );
                assert_eq!(events.last().unwrap()["type"], "done");
                if mode == "rpc" {
                    assert!(events.iter().all(|v| v["id"] == 7));
                }
            }
        }
    }
}
