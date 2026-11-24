mod rpc;
use crate::attachments;
use anyhow::{Context, Result, bail, ensure};
use panda_core::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    sync::Mutex,
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
    Off,
    #[default]
    Alongside,
    Only,
}

#[derive(Default)]
pub struct Options {
    pub interface: Interface,
    pub scrollback: bool,
    pub pi_rpc: bool,
    pub codemode: CodemodeExposure,
    pub tools: Option<BTreeSet<String>>,
    pub deferred: BTreeSet<String>,
    pub hidden: BTreeSet<String>,
    pub delivery: crate::active::Delivery,
    pub resume: bool,
    pub session_id: Option<i64>,
    pub workspace: Option<crate::workspaces::Choice>,
    pub session_dir: Option<PathBuf>,
    pub no_session: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub scope: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub trust_project: bool,
    pub recent_override: Option<u64>,
    pub compaction: crate::settings::Controls,
    pub prompt: String,
}
impl Options {
    pub fn parse(args: &[String], interactive_terminal: bool) -> Result<Self> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--steering-mode" | "--follow-up-mode" => {
                    let one = match it.next().map(String::as_str) {
                        Some("one" | "one-at-a-time") => true,
                        Some("all" | "all-at-once") => false,
                        _ => anyhow::bail!("expected one or all after {arg}"),
                    };
                    if arg == "--steering-mode" {
                        out.delivery.single_steer = one
                    } else {
                        out.delivery.single_follow = one
                    }
                }
                "--rpc-version" => {
                    out.pi_rpc = match it.next().map(String::as_str) {
                        Some("pi-v1") => true,
                        Some("panda-v1") => false,
                        _ => bail!("expected pi-v1 or panda-v1 after --rpc-version"),
                    }
                }
                "--scrollback" => out.scrollback = true,
                "--trust-project" => out.trust_project = true,
                "--compact-soft-at"
                | "--compact-at"
                | "--compact-cache-at"
                | "--compact-cache-for" => {
                    let value = it
                        .next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .with_context(|| format!("expected unsigned number after {arg}"))?;
                    out.compaction.set(arg, value)?;
                }
                "--keep-recent-tokens" => {
                    out.recent_override =
                        Some(it.next().and_then(|v| v.parse().ok()).context(
                            "expected an unsigned token count after --keep-recent-tokens",
                        )?)
                }
                "--codemode" => {
                    if out.codemode == CodemodeExposure::Off {
                        out.codemode = CodemodeExposure::Alongside;
                    }
                }
                "--defer-tools" | "--hide-tools" => {
                    let list = it
                        .next()
                        .filter(|s| !s.starts_with('-'))
                        .context("expected comma-separated tool names")?;
                    let names = if arg == "--defer-tools" {
                        &mut out.deferred
                    } else {
                        &mut out.hidden
                    };
                    for name in list.split(',').map(str::trim) {
                        ensure!(!name.is_empty(), "empty tool name");
                        names.insert(name.to_owned());
                    }
                }
                "--no-codemode" => out.codemode = CodemodeExposure::Off,
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
                "--resume-any" | "--fork-here" | "--all-sessions" => {
                    use crate::workspaces::Choice;
                    ensure!(
                        out.workspace.is_none(),
                        "choose one cross-workspace operation"
                    );
                    out.workspace = Some(if arg == "--all-sessions" {
                        Choice::Pick
                    } else {
                        let id = it
                            .next()
                            .and_then(|v| v.parse::<i64>().ok())
                            .filter(|i| *i > 0)
                            .with_context(|| format!("expected positive session ID after {arg}"))?;
                        if arg == "--fork-here" {
                            Choice::Fork(id)
                        } else {
                            Choice::Open(id)
                        }
                    });
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
                "--models" => {
                    out.scope = Some(
                        it.next()
                            .filter(|s| !s.is_empty() && !s.starts_with('-'))
                            .context("expected comma-separated model patterns after --models")?
                            .clone(),
                    )
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
        ensure!(
            out.workspace.is_none() || !(out.resume || out.no_session || out.session_id.is_some()),
            "cross-workspace selection conflicts with --continue, --session and --no-session"
        );
        ensure!(
            !out.pi_rpc || out.interface == Interface::Rpc,
            "--rpc-version pi-v1 requires --mode rpc"
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
        ensure!(
            !out.scrollback || (out.interface == Interface::Terminal && interactive_terminal),
            "--scrollback requires an interactive terminal"
        );
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
pub fn prompt_text_at(root: &Path, value: &str) -> Result<String> {
    let path = root.join(value);
    match std::fs::metadata(&path) {
        Ok(info) => {
            ensure!(info.is_file(), "expected a regular prompt file: {value}");
            std::fs::read_to_string(&path).with_context(|| format!("reading prompt file {value}"))
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
    pub prompt: Option<String>,
    pub index: &'a std::sync::RwLock<String>,
}
impl Toolset for PromptedTools<'_> {
    fn definitions(&self, m: Mode) -> Vec<panda_core::ToolDefinition> {
        self.tools.definitions(m)
    }
    fn schemas_for(&self, s: &Session) -> Vec<Value> {
        self.tools.schemas_for(s)
    }
    fn run(
        &self,
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(panda_core::ToolOutput, panda_core::StoreWrites)> {
        if call.name == "view_context" {
            let schemas = self.schemas_for(session);
            if let Some(schema) = schemas
                .iter()
                .find(|s| s["function"]["name"] == "view_context")
            {
                cancel.check()?;
                panda_core::validate_tool_args(
                    &schema["function"]["parameters"],
                    &call.args,
                    "view_context",
                )?;
                return Ok((
                    panda_core::native_compaction::context_info(session, &self.system()?, &schemas)
                        .to_string()
                        .into(),
                    Default::default(),
                ));
            }
        }
        self.tools.run(session, call, cancel, emit)
    }
    fn system(&self) -> Result<String> {
        let mut prompt = match &self.prompt {
            Some(p) => p.clone(),
            None => self.tools.system()?,
        };
        let index = self.index.read().unwrap();
        if !index.is_empty() {
            prompt.push_str("\nAvailable skills:\n");
            prompt.push_str(&index);
        }
        Ok(prompt)
    }
}

type Connect<'a> = dyn Fn(Option<&ModelId>) -> Result<Box<dyn Provider + Send>> + 'a;
pub struct ModelControls<'a> {
    pub list: &'a dyn Fn() -> Result<Vec<ModelId>>,
    pub describe: &'a dyn Fn(&ModelId) -> Result<Value>,
    pub effort: &'a dyn Fn(&ModelId) -> Result<Vec<String>>,
    pub retention: &'a dyn Fn(&ModelId) -> Option<u64>,
}
pub struct Host<'a> {
    pub db: &'a mut dyn sdk::SessionHost,
    pub session_id: Option<i64>,
    pub tools: &'a (dyn Toolset + Sync),
    pub connect: &'a Connect<'a>,
    pub models: ModelControls<'a>,
    pub root: &'a Path,
    pub cancel: &'a Cancel,
}
impl Host<'_> {
    pub fn run(self, options: Options) -> Result<()> {
        if options.interface == Interface::Rpc {
            return rpc::serve(self, options.delivery, options.pi_rpc);
        }
        let input = one_shot_input(self.root, &options.prompt)?;
        let mut p = (self.connect)(None)?;
        if let Some(level) = options.thinking {
            p = Box::new(FixedEffort {
                provider: p,
                level: (level != "default").then_some(level),
            });
        }
        let structured = options.interface == Interface::Json;
        sdk::SessionSdk { host: self.db }.prompt(
            p.as_mut(),
            self.tools,
            input,
            self.cancel,
            &mut |e| {
                if structured && !matches!(e, Event::ResultImages(_)) {
                    println!("{}", json!({"type":"event","event":e}));
                }
            },
            &Mutex::new(sdk::Queue::<String>::default()),
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
}

// Apply the override at the request boundary, including retries and summary
// requests. A continued session keeps its own saved reasoning preference.
struct FixedEffort {
    provider: Box<dyn Provider + Send>,
    level: Option<String>,
}
impl Provider for FixedEffort {
    fn default_model(&self) -> Option<ModelId> {
        self.provider.default_model()
    }
    fn model(&self) -> Option<ModelId> {
        self.provider.model()
    }
    fn prices(&self) -> Option<TokenPrices> {
        self.provider.prices()
    }
    fn compaction_judge(&mut self) -> Option<&mut dyn panda_core::native_compaction::Judge> {
        self.provider.compaction_judge()
    }
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
    fn ordinary_invocations_enable_codemode() {
        for flags in ["", "--print hello", "--mode json hello", "--mode rpc"] {
            let flags = flags
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>();
            assert_eq!(
                Options::parse(&flags, true).unwrap().codemode,
                CodemodeExposure::Alongside
            );
        }
    }

    #[test]
    fn rejects_invalid_one_shot_options() {
        for args in [
            "--print --provider deepseek",
            "--print --provider ../invalid --model foo",
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
        assert!(prompt_text_at(Path::new("."), dir.path().to_str().unwrap()).is_err());
        let file = dir.path().join("invalid.md");
        std::fs::write(&file, [0xff]).unwrap();
        assert!(prompt_text_at(Path::new("."), file.to_str().unwrap()).is_err());
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
            proptest::prop_assert_eq!(prompt_text_at(Path::new("."),&literal).unwrap(), literal);
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
            connect: &|_| Ok(Box::new(Reader)),
            models: ModelControls {
                list: &|| Ok(vec![]),
                describe: &|id| Ok(json!({"id":id.model,"provider":id.provider})),
                effort: &|_| Ok(vec!["off".into()]),
                retention: &|_| None,
            },
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
