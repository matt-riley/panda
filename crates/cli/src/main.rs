mod active;
mod attachments;
mod automation;
mod copy;
mod markdown;
mod picker;
mod renderer;
mod screen;
mod ui;
mod vim;
use anyhow::{Context, Result, bail, ensure};
use panda_coding::{Coding, Workspace};
use panda_core::*;
use panda_local::{
    sandbox::{Native, state_dir},
    skills::Library,
    store::Disk,
};
use panda_providers::{Api, subscription::Subscription};
use screen::{Screen, Tone};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

const HELP: &str = ":plan · :code · :model PROVIDER MODEL · :login PROVIDER · :resume [ID] · :ack · :tool ID · :tools · :context [BYTES] · :compact · :new · :session · :name NAME · :tree · :fork [TURN] [NAME] · :clone [NAME] · :models PROVIDER [FILTER] · :effort [LEVEL] · :thinking [LEVEL] · :skills [QUERY] · :skill: · :copy · :paste · :detach · :help · :cancel · :q · :quit";
fn command_matches(prefix: &str) -> impl Iterator<Item = &'static str> + '_ {
    HELP.split(" · ")
        .filter_map(|usage| usage.split_whitespace().next())
        .filter(move |name| name[1..].starts_with(prefix))
}

fn editor_command(setting: &str, path: &Path) -> Result<std::process::Command> {
    let words = shlex::split(setting).context("invalid quoting in editor setting")?;
    let (program, args) = words.split_first().context("empty editor command")?;
    let mut command = std::process::Command::new(program);
    command.args(args).arg(path);
    Ok(command)
}

fn key_env(provider: &str) -> Result<&'static str> {
    match provider {
        "deepseek" => Ok("DEEPSEEK_API_KEY"),
        "openrouter" => Ok("OPENROUTER_API_KEY"),
        _ => bail!("unknown API-key provider"),
    }
}
fn provider_id(input: &str) -> Result<&'static str> {
    match input.to_ascii_lowercase().as_str() {
        "1" | "openai" | "codex" | "openai-codex" => Ok("openai-codex"),
        "2" | "deepseek" => Ok("deepseek"),
        "3" | "openrouter" => Ok("openrouter"),
        _ => bail!("unknown provider: choose OpenAI Codex, DeepSeek or OpenRouter"),
    }
}

fn canonical_model<'a>(id: &'a str, model: &'a str) -> (&'a str, &'a str) {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name.to_ascii_lowercase().starts_with("gpt-") {
        ("openai-codex", name)
    } else {
        (id, model)
    }
}

fn initial_model(settings: &Path, options: &automation::Options) -> Result<(String, String)> {
    if let (Some(provider), Some(model)) = (&options.provider, &options.model) {
        return Ok((provider.clone(), model.clone()));
    }
    let (saved_provider, saved_model): (String, String) = if settings.try_exists()? {
        serde_json::from_slice(&fs::read(settings)?)?
    } else {
        ("deepseek".into(), "deepseek-flash".into())
    };
    if let Some(model) = &options.model {
        return Ok((provider_id(&saved_provider)?.into(), model.clone()));
    }
    if let Ok(id) = provider_id(&saved_provider) {
        let (id, model) = canonical_model(id, &saved_model);
        return Ok((id.into(), model.into()));
    }
    Ok((saved_provider, saved_model))
}

#[derive(Clone, Copy)]
enum Picker {
    Login,
    Models,
    Select,
}

const PROVIDERS: [&str; 3] = ["openai-codex", "deepseek", "openrouter"];
fn configured(state: &Path, id: &str) -> Result<bool> {
    if subscription(id) {
        return panda_providers::models::codex_configured(state);
    }
    Ok(
        std::env::var(key_env(id)?).is_ok_and(|k| !k.trim().is_empty())
            || state.join(format!("{id}.key")).try_exists()?,
    )
}
fn model_entries(state: &Path, id: &str, cached: bool) -> Result<Vec<(String, Option<u64>)>> {
    let path = state.join(format!("catalog-{id}.json"));
    if cached
        && let Ok(data) = fs::read(&path)
        && let Ok(rows) = serde_json::from_slice::<Vec<(String, Option<u64>)>>(&data)
        && !rows.is_empty()
    {
        return Ok(rows);
    }
    let key = if id == "deepseek" {
        Some(
            std::env::var(key_env(id)?)
                .or_else(|_| fs::read_to_string(state.join("deepseek.key")))
                .context("use :login deepseek first")?,
        )
    } else {
        None
    };
    let catalogue = panda_providers::models::catalogue(id, state, key.as_deref().map(str::trim))?;
    if subscription(id) {
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer(&mut file, &catalogue.efforts)?;
        file.persist(state.join("codex-efforts.json"))?;
    }
    let rows = catalogue.models;
    if !rows.is_empty() {
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer(&mut file, &rows)?;
        file.persist(path)?;
    }
    Ok(rows)
}
fn effort_levels(
    state: &Path,
    provider: &str,
    model: &str,
    update_cache: bool,
) -> Result<Vec<String>> {
    let mut options = vec![String::from("default")];
    if subscription(provider) {
        let path = state.join("codex-efforts.json");
        let cached = || -> Option<Vec<String>> {
            let rows: std::collections::BTreeMap<String, Vec<String>> =
                serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            rows.get(model).cloned()
        };
        let levels = if let Some(levels) = cached() {
            levels
        } else if update_cache {
            model_entries(state, provider, false)?;
            cached().unwrap_or_default()
        } else {
            panda_providers::models::catalogue(provider, state, None)?
                .efforts
                .remove(model)
                .unwrap_or_default()
        };
        options.extend(levels);
    } else {
        options.extend(["none", "low", "medium", "high", "xhigh", "max"].map(String::from));
    }
    Ok(options)
}

fn pick_command(state: &Path, cmd: &str, current: (&str, &str)) -> Result<Option<String>> {
    if cmd == ":login" {
        let mut labels = Vec::new();
        for (id, label) in PROVIDERS.iter().zip([
            "OpenAI Codex (ChatGPT subscription)",
            "DeepSeek (API key)",
            "OpenRouter (API key)",
        ]) {
            labels.push(format!(
                "{label} · {}",
                if configured(state, id)? {
                    "configured"
                } else {
                    "not configured"
                }
            ));
        }
        return Ok(picker::choose("Search providers", &labels, 0)?
            .map(|i| format!(":login {}", PROVIDERS[i])));
    }
    let mut models = Vec::new();
    let mut errors = Vec::new();
    for id in PROVIDERS {
        if configured(state, id)? {
            match model_entries(state, id, true) {
                Ok(rows) => models.extend(
                    rows.into_iter()
                        .filter(|(m, _)| canonical_model(id, m).0 == id)
                        .map(|(m, _)| (id, m)),
                ),
                Err(e) => errors.push(format!("{id}: {e:#}")),
            }
        }
    }
    ensure!(
        !models.is_empty(),
        "No available models. Use :login to configure a provider. {}",
        errors.join("; ")
    );
    let labels = models
        .iter()
        .map(|(id, m)| format!("{m}  [{id}]"))
        .collect::<Vec<_>>();
    let selected = models
        .iter()
        .position(|(id, m)| (*id, m.as_str()) == current)
        .unwrap_or(0);
    Ok(picker::choose("Search models", &labels, selected)?
        .map(|i| format!(":model {} {}", models[i].0, models[i].1)))
}
fn subscription(p: &str) -> bool {
    p == "openai-codex"
}
fn provider(state: &Path, p: &str, m: &str) -> Result<Box<dyn Provider + Send>> {
    if subscription(p) {
        return Ok(Box::new(Subscription {
            provider: p.into(),
            model: m.into(),
            state: state.into(),
        }));
    }
    let var = key_env(p)?;
    let key = std::env::var(var)
        .or_else(|_| fs::read_to_string(state.join(format!("{p}.key"))))
        .with_context(|| format!("set {var} or use :login {p}"))?;
    let endpoint = match p {
        "deepseek" => "https://api.deepseek.com/chat/completions",
        _ => "https://openrouter.ai/api/v1/chat/completions",
    };
    Ok(Box::new(Api {
        key: key.trim().into(),
        model: m.into(),
        endpoint: endpoint.into(),
    }))
}
fn login(state: &Path, p: &str, cancel: &Cancel, browser: Option<bool>) -> Result<()> {
    if subscription(p) {
        let account = Subscription {
            provider: p.into(),
            model: String::new(),
            state: state.into(),
        };
        return match browser {
            Some(method) => account.login_with_method(cancel, method),
            None => account.login_with_cancel(cancel),
        };
    }
    let var = key_env(p)?;
    let key = std::env::var(var)
        .with_context(|| format!("export {var} before starting Panda, then retry :login"))?;
    let mut temp = tempfile::NamedTempFile::new_in(state)?;
    temp.write_all(key.trim().as_bytes())?;
    temp.persist(state.join(format!("{p}.key")))?;
    println!("Saved {p} credential");
    Ok(())
}
fn safe(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
fn usage(s: &Session) -> String {
    let input: u64 = s.usage.iter().map(|u| u.input).sum();
    let output: u64 = s.usage.iter().map(|u| u.output).sum();
    let mut text = format!("↑{input} ↓{output}");
    if let Some(last) = s.usage.last()
        && let Some(window) = s.model_window
    {
        text.push_str(&format!(" · last input {}/{window}", last.input));
    }
    if s.usage.iter().any(|u| u.cost.is_some()) {
        text.push_str(&format!(
            " · reported ${:.4}",
            s.usage.iter().filter_map(|u| u.cost).sum::<f64>()
        ));
    }
    text
}
fn model_window(db: &mut dyn Store, state: &Path, id: &str, model: &str) -> Result<()> {
    let path = state.join(format!("catalog-{id}.json"));
    let window = if matches!(id, "deepseek" | "openai-codex" | "openrouter") && path.try_exists()? {
        let rows: Vec<(String, Option<u64>)> = serde_json::from_slice(&fs::read(path)?)?;
        rows.iter().find(|(m, _)| m == model).and_then(|r| r.1)
    } else {
        None
    };
    let mut s = db.load()?;
    if s.model_window != window {
        s.model_window = window;
        db.save(&mut s)?;
    }
    Ok(())
}
fn show(db: &Disk, screen: &mut Screen) -> Result<Mode> {
    let s = db.load()?;
    screen.footer.effort = s.reasoning.clone();
    screen.line(Tone::Dim, &format!("session {} ({:?})", db.id(), s.mode));
    let mut calls_by_id = std::collections::HashMap::new();
    for m in &s.history {
        match m {
            Message::BranchSummary(t) => {
                screen.line(Tone::Dim, &format!("Branch summary · {}", safe(t)))
            }
            Message::Reasoning(t) => screen.line(Tone::Thought, t),
            Message::Images { text, urls } => screen.line(
                Tone::User,
                &format!("you> {} [{} images]", safe(text), urls.len()),
            ),
            Message::User(t) => screen.line(Tone::User, &format!("you> {}", safe(t))),
            Message::Assistant { text, calls } => {
                screen.finish_reply(text);
                for c in calls {
                    if screen.active {
                        calls_by_id.insert(&c.id, c);
                        screen.line(Tone::Tool, &ui::header(c))
                    } else {
                        println!("[{} {}]", safe(&c.name), safe(&c.id))
                    }
                }
            }
            Message::Result {
                id, text, is_error, ..
            } if screen.active => screen.tool_result(calls_by_id.remove(id), text, *is_error),
            Message::Result { id, text, .. } => println!("[{}] {}", safe(id), safe(text)),
        }
    }
    if s.active {
        screen.line(Tone::Removed,
            "Interrupted turn. Outstanding tool outcomes are unknown. Inspect the workspace and use :ack to continue."
        );
    }
    screen.footer.usage = usage(&s);
    Ok(s.mode)
}

fn transfer_session(args: &[String]) -> Result<()> {
    let root = std::env::current_dir()?.canonicalize()?;
    let native = Native::new(&root, &std::env::current_exe()?)?;
    let (args, dir) = match args {
        [args @ .., flag, dir] if flag == "--session-dir" => {
            (args, native.session_dir(Path::new(dir))?)
        }
        _ => (args, state_dir()?),
    };
    match args {
        [flag, file] if flag == "--import-session" => {
            let input = std::io::BufReader::new(std::fs::File::open(file)?);
            let disk = Disk::import_pi(&dir, &root, input)?;
            println!("{}", serde_json::json!({"session_id":disk.id()}));
        }
        [flag, id, file] if flag == "--export-session" => {
            let mut disk = Disk::open(
                &dir,
                &root,
                Some(id.parse().context("expected numeric session ID")?),
            )?;
            let path = Path::new(file);
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            disk.export_pi(std::io::BufWriter::new(temp.as_file_mut()))?;
            temp.persist_noclobber(path)
                .with_context(|| format!("creating export {file}"))?;
        }
        _ => anyhow::bail!(
            "expected --import-session FILE or --export-session ID FILE, optionally followed by --session-dir DIR"
        ),
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    run(args, provider)
}

fn run(
    args: Vec<String>,
    provider: fn(&Path, &str, &str) -> Result<Box<dyn Provider + Send>>,
) -> Result<()> {
    if args == ["--version"] {
        println!("panda {}", include_str!("../../../version.txt").trim());
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("--file-worker") {
        let arg = serde_json::from_reader(io::stdin())?;
        let name = args.get(1).context("missing tool name")?;
        let root = std::env::current_dir()?;
        if name == "read" {
            serde_json::to_writer(
                io::stdout().lock(),
                &panda_local::worker::read_output(&root, &arg)?,
            )?;
        } else {
            print!("{}", panda_local::worker::run(&root, name, &arg)?);
        }
        return Ok(());
    }
    if args == ["--codemode-worker"] {
        return panda_local::codemode::worker();
    }
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "panda — interactive coding harness\n--version prints the release version\n--import-session FILE [--session-dir DIR] imports Pi v3 JSONL. --export-session ID FILE [--session-dir DIR] writes Pi v3 JSONL.\npanda PROMPT sends the first turn in an interactive terminal and stays open for follow-ups.\n--continue resumes the most recently modified session in this workspace. --print PROMPT writes the final answer. --mode json emits events; --mode rpc accepts JSONL commands.\nPrint and JSON options: [--provider PROVIDER] --model MODEL, --thinking LEVEL, --session-dir DIR, --session ID or --no-session, --tools NAME[,NAME...] or --no-tools. --system-prompt TEXT|PATH replaces the base prompt. Repeat --append-system-prompt TEXT|PATH to add instructions in order. Piped input precedes the prompt.\n--codemode enables read-only JavaScript tool composition.\n{HELP}"
        );
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "--import-session" | "--export-session"))
    {
        return transfer_session(&args);
    }
    let interactive_terminal = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb");
    let mut options = automation::Options::parse(&args, interactive_terminal)?;
    let root = std::env::current_dir()?.canonicalize()?;
    let state = state_dir()?;
    let native = Native::new(&root, &std::env::current_exe()?)?;
    let sessions = match &options.session_dir {
        Some(dir) if !options.no_session => native.session_dir(dir)?,
        _ => state.clone(),
    };
    let home = PathBuf::from(std::env::var_os("HOME").context("missing HOME")?);
    let mut roots = vec![
        root.join(".agents/skills"),
        root.join(".pi/skills"),
        home.join(".agents/skills"),
        home.join(".pi/agent/skill-library"),
        home.join(".pi/agent/skills"),
    ];
    if let Some(extra) = std::env::var_os("PANDA_SKILL_ROOTS") {
        roots.extend(std::env::split_paths(&extra))
    }
    let terminal_mode = options.interface == automation::Interface::Terminal;
    let interactive = terminal_mode && interactive_terminal;
    let mut screen = Screen::open(interactive)?;
    if interactive {
        screen.footer.location = screen::Footer::location(&root, &home);
    }
    let scratch = if interactive {
        Some(tempfile::NamedTempFile::new_in(&state)?)
    } else {
        None
    };
    let mut editor = scratch
        .as_ref()
        .map(|tmp| -> Result<_> {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".into());
            let command = editor_command(&editor, tmp.path())?;
            Ok(screen
                .editor(&root)
                .with_buffer_editor(command, tmp.path().into()))
        })
        .transpose()?;
    if terminal_mode {
        for (p, _) in native.instructions()? {
            screen.line(Tone::Dim, &format!("Instructions: {}", safe(&p)))
        }
    }
    let coding = Coding {
        workspace: native,
        skills: Library::new(roots),
    };
    let executable = std::env::current_exe()?;
    let selected_tools = options
        .tools
        .take()
        .map(|names| panda_coding::SelectedTools::new(&coding, names))
        .transpose()?;
    let base: &(dyn Toolset + Sync) = match &selected_tools {
        Some(selected) => selected,
        None => &coding,
    };
    let codemode = panda_local::codemode::Codemode::new(base, &executable);
    let tools: &(dyn Toolset + Sync) = if options.codemode { &codemode } else { base };
    let prompted;
    let tools = if options.system_prompt.is_some() || !options.append_system_prompt.is_empty() {
        let mut prompt = if let Some(value) = &options.system_prompt {
            let mut prompt = coding.system_with_base(&automation::prompt_text(value)?)?;
            if options.codemode {
                codemode.append_guidance(&mut prompt);
            }
            prompt
        } else {
            tools.system()?
        };
        for value in &options.append_system_prompt {
            prompt.push_str("\n\n");
            prompt.push_str(&automation::prompt_text(value)?);
        }
        prompted = automation::PromptedTools { tools, prompt };
        &prompted as &(dyn Toolset + Sync)
    } else {
        tools
    };
    let selected = state.join("model.json");
    let mut disk = options
        .session_id
        .map(|id| Disk::open(&sessions, &root, Some(id)))
        .transpose()?;
    let (mut p, mut model) = initial_model(&selected, &options)?;
    if !subscription(&p) && key_env(&p).is_err() {
        let warning = "Saved provider is unavailable; choose a supported provider with :model PROVIDER MODEL.";
        if interactive {
            screen.line(Tone::Removed, warning);
        } else {
            eprintln!("{warning}");
        }
    }
    if let Some(level) = options.thinking.as_deref()
        && level != "default"
    {
        let levels = effort_levels(&state, &p, &model, false)?;
        ensure!(
            levels.iter().any(|s| s == level),
            "available efforts for {model}: {}",
            levels.join(", ")
        );
    }
    if disk.is_none() && !options.no_session {
        let initial = if options.resume {
            Disk::latest(&sessions, &root)?
        } else {
            None
        };
        disk = Some(Disk::open(&sessions, &root, initial)?);
    }
    let session_id = disk.as_ref().map(Disk::id);
    let mut memory = panda_memory::MemoryStore::default();
    let store: &mut (dyn Store + Send) = match &mut disk {
        Some(disk) => disk,
        None => &mut memory,
    };
    model_window(store, &state, &p, &model)?;
    let cancel = Cancel::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.stop())?;
    if !terminal_mode {
        return automation::Host {
            db: store,
            session_id,
            tools,
            connect: &|| provider(&state, &p, &model),
            root: &root,
            cancel: &cancel,
        }
        .run(options);
    }
    let mut db = disk.context("terminal mode requires a saved session")?;
    let mut mode = show(&db, &mut screen)?;
    screen.line(
        Tone::Dim,
        if interactive {
            "Panda   :help for commands"
        } else {
            HELP
        },
    );
    let mut picker = None;
    let mut catalogue: Vec<(String, String)> = Vec::new();
    let mut initial_prompt = (!options.prompt.is_empty()).then_some(options.prompt);

    loop {
        if interactive {
            screen.footer.session = format!("{mode:?} · session {}", db.id());
            screen.footer.model = format!("{p}/{model}");
        }
        let (line, allow_commands) = if let Some(prompt) = initial_prompt.take() {
            (prompt, false)
        } else if let Some(command) = screen.commands.pop_front() {
            (command, true)
        } else if interactive {
            match screen.read(&mut editor)? {
                reedline::Signal::Success(line) => (line, screen.recalled_images.is_none()),
                reedline::Signal::HostCommand(command) => (command, true),
                reedline::Signal::CtrlD => break,
                reedline::Signal::CtrlC(_) => {
                    screen.recalled_images = None;
                    continue;
                }
                _ => continue,
            }
        } else {
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                break;
            }
            (line, true)
        };
        let line = if allow_commands {
            line.trim()
        } else {
            line.as_str()
        };
        let alias;
        let line = if allow_commands && let Some(name) = line.strip_prefix('/') {
            alias = format!(":{name}");
            alias.as_str()
        } else {
            line
        };
        if allow_commands && matches!(line, ":q" | ":quit") {
            break;
        }
        if line.trim().is_empty() || (allow_commands && line == ":") {
            if !allow_commands {
                screen.recalled_images = None;
            }
            continue;
        }
        if interactive {
            screen.line(Tone::User, &format!("❯ {line}"));
        }
        if allow_commands && line == ":cancel" {
            picker = None;
            continue;
        }
        let action = (|| -> Result<()> {
            let command;
            let line = match (picker.take(), line.starts_with(':')) {
                (Some(choice), false) => {
                    command = match choice {
                        Picker::Login => format!(":login {}", provider_id(line.trim())?),
                        Picker::Models => format!(":models {}", provider_id(line.trim())?),
                        Picker::Select => format!(
                            ":model {}",
                            line.trim()
                                .parse::<usize>()
                                .context("enter a model number")?
                        ),
                    };
                    command.as_str()
                }
                _ => line,
            };
            let chosen;
            let line = if allow_commands && interactive && matches!(line, ":login" | ":model") {
                let Some(cmd) = pick_command(&state, line, (&p, &model))? else {
                    return Ok(());
                };
                chosen = cmd;
                &chosen
            } else {
                line
            };
            let mut words = line.split_whitespace();
            let first = if allow_commands {
                words.next().unwrap()
            } else {
                ""
            };
            match first {
                ":plan" | ":code" => {
                    let mut s = db.load()?;
                    s.mode = if first == ":plan" {
                        Mode::Plan
                    } else {
                        Mode::Code
                    };
                    db.save(&mut s)?;
                    mode = s.mode;
                    screen.line(Tone::Dim, &format!("{mode:?} mode"))
                }
                ":session" => {
                    let s = db.load()?;
                    screen.line(
                        Tone::Dim,
                        &format!(
                            "Session {} {} · {} messages · {}",
                            db.id(),
                            s.name,
                            s.history.len(),
                            usage(&s)
                        ),
                    );
                }
                ":name" => {
                    let mut s = db.load()?;
                    s.name = line[first.len()..].trim().into();
                    db.save(&mut s)?;
                    screen.line(Tone::Dim, &format!("session {} {}", db.id(), s.name));
                }
                ":new" | ":fork" | ":clone" => {
                    let mut rest = line[first.len()..].trim();
                    let mut at = None;
                    if first == ":fork"
                        && let Some(word) = rest.split_whitespace().next()
                        && let Ok(n) = word.parse()
                    {
                        at = Some(n);
                        rest = rest[word.len()..].trim();
                    }
                    db = if first == ":new" {
                        Disk::open(&state, &root, None)?
                    } else {
                        db.fork(&state, &root, at, rest)?
                    };
                    screen.clipboard = None;
                    screen.clear();
                    screen.footer.usage.clear();
                    model_window(&mut db, &state, &p, &model)?;
                    mode = show(&db, &mut screen)?;
                }
                ":tree" if interactive => {
                    let entries = db.tree()?;
                    if entries.is_empty() {
                        screen.line(Tone::Dim, "This session has no prompts or answers yet.");
                        return Ok(());
                    }
                    let Some((node, action)) = picker::tree(&entries)? else {
                        return Ok(());
                    };
                    let draft = match action {
                        picker::TreeAction::Fork => {
                            let (child, prompt) = db.fork_prompt(&state, &root, node)?;
                            db = child;
                            Some(prompt)
                        }
                        picker::TreeAction::Switch => db.navigate(node)?,
                        picker::TreeAction::Summarize => {
                            cancel.reset();
                            db.navigate_summarized(node, &cancel, |messages| {
                                eprintln!("Summarizing branch… Ctrl-C to cancel");
                                summarize_branch(
                                    provider(&state, &p, &model)?.as_mut(),
                                    messages,
                                    &cancel,
                                )
                            })?
                        }
                    };
                    screen.clear();
                    mode = show(&db, &mut screen)?;
                    let text = match draft {
                        Some(Message::User(text)) => {
                            screen.recalled_images = Some(Vec::new());
                            text
                        }
                        Some(Message::Images { text, urls }) => {
                            screen.recalled_images = Some(urls);
                            text
                        }
                        _ => String::new(),
                    };
                    vim::Steering {
                        vim: screen.footer.vim.clone(),
                        editor: editor.as_mut().unwrap(),
                    }
                    .replace(safe(&text));
                }
                ":tree" => {
                    for (id, name, parent) in db.catalogue(&root)? {
                        screen.line(
                            Tone::Dim,
                            &format!(
                                "{id} {name} {}",
                                parent.map(|id| format!("← {id}")).unwrap_or_default()
                            ),
                        );
                    }
                    let mut n = 0;
                    for msg in db.load()?.history {
                        if let Message::Assistant { text, calls } = msg
                            && calls.is_empty()
                        {
                            n += 1;
                            screen.line(Tone::Dim, &format!("turn {n}  {}", ui::inline(&text, 80)));
                        }
                    }
                }
                ":context" | ":compact" => {
                    if first == ":compact" {
                        cancel.reset();
                        compact(provider(&state, &p, &model)?.as_mut(), &mut db, &cancel)?;
                    }
                    let mut s = db.load()?;
                    if let Some(arg) = words.next() {
                        let bytes: usize = arg.parse()?;
                        ensure!(bytes >= 4096, "minimum context budget is 4096 bytes");
                        s.context_budget = Some(bytes);
                        db.save(&mut s)?;
                    }
                    screen.line(
                        Tone::Dim,
                        &format!(
                            "Context {} bytes · estimated {} / {} input tokens · byte limit {} · {} messages covered by summary",
                            s.context_bytes(),
                            s.estimated_tokens(&tools.system()?, &tools.schemas(s.mode)),
                            s.token_budget(),
                            s.context_budget.map_or_else(|| "unset".into(), |n| n.to_string()),
                            s.checkpoint.map_or(0, |c| c.through)
                        ),
                    );
                }
                ":ack" => acknowledge(&mut db)?,
                ":resume" => {
                    let selected = if let Some(n) = words.next() {
                        Some(if let Ok(id) = n.parse() {
                            id
                        } else {
                            let name = line[first.len()..].trim();
                            let mut matching = db
                                .catalogue(&root)?
                                .into_iter()
                                .filter(|(_, label, _)| label == name);
                            let id = matching.next().context("no session with that name")?.0;
                            ensure!(
                                matching.next().is_none(),
                                "ambiguous session name; use its numeric ID"
                            );
                            id
                        })
                    } else if interactive {
                        picker::resume(&db.previews(&root)?)?
                    } else {
                        for (id, name, _) in db.catalogue(&root)? {
                            screen.line(Tone::Dim, &format!("{id} {name}"));
                        }
                        None
                    };
                    if let Some(id) = selected {
                        if id != db.id() {
                            db = Disk::open(&state, &root, Some(id))?
                        }
                        screen.clear();
                        screen.footer.usage.clear();
                        model_window(&mut db, &state, &p, &model)?;
                        mode = show(&db, &mut screen)?
                    }
                }
                ":login" | ":model" if line == first => {
                    let login = first == ":login";
                    picker = Some(if login { Picker::Login } else { Picker::Models });
                    let title = if login {
                        "Choose a login provider"
                    } else {
                        "Choose a model provider"
                    };
                    screen.line(Tone::Text, &format!("{title}: 1 OpenAI Codex · 2 DeepSeek · 3 OpenRouter. Enter a number or name (:cancel to close)."));
                }
                ":login" => {
                    cancel.reset();
                    if interactive {
                        crossterm::execute!(
                            io::stdout(),
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                            crossterm::cursor::MoveTo(0, 0)
                        )?;
                    }
                    let id = provider_id(words.next().unwrap_or(&p))?;
                    let browser = if interactive && subscription(id) {
                        let options = ["Open browser".to_owned(), "Use device code".to_owned()];
                        let Some(i) = picker::choose("Sign in to OpenAI Codex", &options, 0)?
                        else {
                            return Ok(());
                        };
                        crossterm::execute!(
                            io::stdout(),
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                            crossterm::cursor::MoveTo(0, 0)
                        )?;
                        Some(i == 0)
                    } else {
                        None
                    };
                    login(&state, id, &cancel, browser)?;
                    screen.line(
                        Tone::Dim,
                        &format!("Signed in to {id}. Use :model to select a model."),
                    );
                }
                ":models" | ":model"
                    if first == ":models"
                        || (words.clone().count() == 1
                            && words.clone().next().is_some_and(|w| {
                                w.parse::<usize>().is_err() && provider_id(w).is_ok()
                            })) =>
                {
                    let id = provider_id(words.next().unwrap_or(&p))?;
                    let filter = words.next().unwrap_or("").to_lowercase();
                    let entries = model_entries(&state, id, false)?;
                    model_window(&mut db, &state, &p, &model)?;
                    catalogue = entries
                        .into_iter()
                        .map(|(m, _)| m)
                        .filter(|m| {
                            canonical_model(id, m).0 == id && m.to_lowercase().contains(&filter)
                        })
                        .map(|m| (id.to_owned(), m))
                        .collect();
                    for (i, (_, m)) in catalogue.iter().enumerate() {
                        screen.line(Tone::Dim, &format!("{}  {m}", i + 1));
                    }
                    if catalogue.is_empty() {
                        screen.line(Tone::Dim, "No matching models.");
                    } else {
                        picker = Some(Picker::Select);
                        screen.line(Tone::Dim, "Enter a model number, or use :model NUMBER. :cancel closes the picker.");
                    }
                }
                ":thinking" | ":effort" => {
                    let mut s = db.load()?;
                    let levels = if interactive
                        || words.clone().next().is_some_and(|level| level != "default")
                    {
                        effort_levels(&state, &p, &model, true)?
                    } else {
                        vec!["default".into()]
                    };
                    let choice = if let Some(level) = words.next() {
                        Some(level)
                    } else if interactive {
                        let current = s.reasoning.as_deref().unwrap_or("default");
                        let pos = levels.iter().position(|s| s == current).unwrap_or(0);
                        let Some(i) = picker::choose("Reasoning effort", &levels, pos)? else {
                            return Ok(());
                        };
                        Some(levels[i].as_str())
                    } else {
                        None
                    };
                    if let Some(level) = choice {
                        ensure!(
                            levels.iter().any(|s| s == level),
                            "available efforts for {model}: {}",
                            levels.join(", ")
                        );
                        s.reasoning = (level != "default").then(|| level.to_owned());
                        db.save(&mut s)?;
                        screen.footer.effort = s.reasoning.clone();
                    }
                    screen.line(
                        Tone::Dim,
                        &format!(
                            "Thinking effort {}",
                            s.reasoning.as_deref().unwrap_or("default")
                        ),
                    );
                }
                ":model" => {
                    if let Some(id) = words.next() {
                        let (id, m) = if let Ok(n) = id.parse::<usize>() {
                            let (pi, mi) = catalogue
                                .get(n.checked_sub(1).context("index starts at 1")?)
                                .context("use :models first")?;
                            (pi.as_str(), mi.as_str())
                        } else {
                            if let Some(m) = words.next() {
                                (provider_id(id)?, m)
                            } else {
                                canonical_model(&p, id)
                            }
                        };
                        let (id, m) = canonical_model(id, m);
                        ensure!(subscription(id) || key_env(id).is_ok(), "unknown provider");
                        let mut temp = tempfile::NamedTempFile::new_in(&state)?;
                        serde_json::to_writer(&mut temp, &(id, m))?;
                        temp.persist(&selected)?;
                        (p, model) = (id.into(), m.into());
                        model_window(&mut db, &state, &p, &model)?;
                    }
                    screen.line(Tone::Dim, &format!("{} {}", safe(&p), safe(&model)))
                }
                ":tool" => screen.select_tool(words.next().context("usage: :tool ID")?)?,
                ":tools" => {
                    let mut found = false;
                    for message in db.load()?.history {
                        match message {
                            Message::Assistant { calls, .. } => {
                                for call in calls {
                                    screen.line(
                                        Tone::Tool,
                                        &format!("{} [{}]", ui::header(&call), safe(&call.id)),
                                    );
                                }
                            }
                            Message::Result { id, text, .. } => {
                                found = true;
                                screen.line(
                                    Tone::Text,
                                    &format!("[{}] {}", safe(&id), ui::result(&text, true)),
                                );
                            }
                            _ => {}
                        }
                    }
                    if !found {
                        screen.line(Tone::Dim, "No tool results in this session.");
                    }
                }
                ":copy" => screen.line(Tone::Dim, copy::copy_last(&db.load()?.history)?),
                ":paste" => {
                    screen.clipboard = Some(attachments::clipboard()?);
                    screen.line(
                        Tone::Dim,
                        "Clipboard image attached to the next prompt. :detach removes it.",
                    );
                }
                ":detach" => {
                    if let Some(images) = &mut screen.recalled_images {
                        images.clear();
                    }
                    screen.clipboard = None;
                }
                ":help" => screen.line(Tone::Dim, HELP),
                ":skills" => {
                    for (name, path, desc) in coding.skills.ranked(line[first.len()..].trim())? {
                        screen.line(Tone::Dim, &format!("{name}  {desc}  {}", path.display()));
                    }
                }
                cmd if cmd.starts_with('!') => {
                    let mut s = db.load()?;
                    ensure!(!s.active, "acknowledge interrupted turn first");
                    let private = line.starts_with("!!");
                    let command = &line[if private { 2 } else { 1 }..];
                    ensure!(!command.trim().is_empty(), "expected shell command");
                    cancel.reset();
                    let output = coding.run(
                        s.mode,
                        &Call {
                            id: "user-shell".into(),
                            name: "shell".into(),
                            args: serde_json::json!({"command":command}),
                        },
                        &cancel,
                    )?;
                    screen.line(Tone::Tool, &safe(&output));
                    if !private {
                        s.history.push(Message::User(format!(
                            "I ran {command:?}. Output:\n{output}"
                        )));
                        db.save(&mut s)?;
                    }
                }
                unknown if unknown.starts_with(':') && !unknown.starts_with(":skill:") => {
                    bail!("unknown command: {HELP}")
                }
                _ => {
                    let mut engine = provider(&state, &p, &model)?;
                    cancel.reset();
                    let mut input = if let Some(mut urls) = screen.recalled_images.take() {
                        // The saved message already includes its resolved files and skill text.
                        urls.extend(screen.clipboard.iter().cloned());
                        if urls.is_empty() {
                            Message::User(line.into())
                        } else {
                            Message::Images {
                                text: line.into(),
                                urls,
                            }
                        }
                    } else {
                        attachments::prepare(&root, line, screen.clipboard.as_deref())?
                    };
                    if let Some(name) = first.strip_prefix(":skill:") {
                        let (path, body) = coding.skills.named(name)?;
                        screen.line(Tone::Dim, &format!("Skill loaded: {}", path.display()));
                        match &mut input {
                            Message::User(t) | Message::Images { text: t, .. } => {
                                t.push_str(&format!("\nSelected skill {}:\n{body}", path.display()))
                            }
                            _ => unreachable!(),
                        }
                    }
                    screen.clipboard = None;
                    active::run(
                        engine.as_mut(),
                        &mut db,
                        tools,
                        input,
                        &cancel,
                        &mut screen,
                        editor.as_mut(),
                    )?;
                    screen.footer.usage = usage(&db.load()?);
                }
            }
            Ok(())
        })();
        if let Err(e) = action {
            let error = format!("Panda: {}", safe(&format!("{e:#}")));
            if interactive {
                screen.line(Tone::Removed, &error);
            } else {
                eprintln!("{error}");
            }
            if db.load()?.active {
                if interactive {
                    screen.line(
                        Tone::Removed,
                        "Turn interrupted. Inspect the workspace, then :ack.",
                    );
                } else {
                    eprintln!("Turn interrupted. Inspect the workspace, then :ack.");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod editor_tests {
    #[test]
    fn external_editor_honours_quoted_arguments() {
        let command = super::editor_command("'my editor' --wait", "draft".as_ref()).unwrap();
        assert_eq!(command.get_program(), "my editor");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--wait", "draft"]);
    }
}

#[cfg(test)]
mod tree_tests;
