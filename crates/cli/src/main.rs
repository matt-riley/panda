mod active;
mod attachments;
mod automation;
mod copy;
mod diagnostics;
mod html;
mod markdown;
mod model_catalog;
use model_catalog::{Catalog, available_models, scope_effort};
mod picker;
mod renderer;
mod routing;
mod screen;
mod settings;
mod terminal_config;
mod ui;
mod vim;
mod workspaces;
use anyhow::{Context, Result, bail, ensure};
use automation::CodemodeExposure;
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

const HELP: &str = ":plan · :code · :model PROVIDER MODEL · :login PROVIDER · :auth [PROVIDER] · :logout [PROVIDER] · :scope PATTERNS · :cycle [next|prev] · :usage · :retry · :resume [ID] · :ack · :tool ID · :tools · :context [BYTES|info] · :transcript [POINTER] · :compact [instructions|preview [guidance]|info] · :new · :session · :prompts · :settings [reload] · :theme [NAME] · :keys · :diagnostics · :bug-report · :changelog [VERSION] · :name NAME · :tree · :fork [TURN] [NAME] · :clone [NAME] · :models PROVIDER [FILTER] · :effort [LEVEL] · :thinking [LEVEL] · :skills [QUERY] · :skill: · :copy · :paste · :detach · :help · :cancel · :q · :quit";
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
        "anthropic" => Ok("ANTHROPIC_API_KEY"),
        "openai-api" => Ok("OPENAI_API_KEY"),
        "google" => Ok("GEMINI_API_KEY"),
        _ => bail!("unknown API-key provider"),
    }
}
fn provider_id(input: &str) -> Result<&str> {
    match input.to_ascii_lowercase().as_str() {
        "1" | "openai" | "codex" | "openai-codex" => Ok("openai-codex"),
        "2" | "deepseek" => Ok("deepseek"),
        "3" | "openrouter" => Ok("openrouter"),
        _ if model_catalog::valid_provider(input) && input != "github-copilot" => Ok(input),
        _ => bail!("unknown provider: choose OpenAI Codex, DeepSeek or OpenRouter"),
    }
}

fn canonical_model<'a>(id: &'a str, model: &'a str) -> (&'a str, &'a str) {
    let name = model.rsplit('/').next().unwrap_or(model);
    if matches!(id, "deepseek" | "openrouter" | "openai-codex")
        && name.to_ascii_lowercase().starts_with("gpt-")
    {
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

const PROVIDERS: [&str; 6] = [
    "openai-codex",
    "deepseek",
    "openrouter",
    "anthropic",
    "openai-api",
    "google",
];
fn configured(state: &Path, id: &str) -> Result<bool> {
    if model_catalog::logged_out(state, id)? {
        return Ok(false);
    }
    if id == "router" {
        return Ok(!Catalog::read(state)?.routes.is_empty());
    }
    if let Some(ep) = Catalog::read(state)?.providers.get(id) {
        return Ok(ep.key(state, id).is_ok());
    }
    if subscription(id) {
        return panda_providers::models::codex_configured(state);
    }
    Ok(
        std::env::var(key_env(id)?).is_ok_and(|k| !k.trim().is_empty())
            || state.join(format!("{id}.key")).try_exists()?,
    )
}
fn model_entries(state: &Path, id: &str, cached: bool) -> Result<Vec<(String, Option<u64>)>> {
    if id == "router" {
        let catalog = Catalog::read(state)?;
        return Ok(catalog
            .routes
            .iter()
            .map(|(name, r)| (name.clone(), r.window(&catalog)))
            .collect());
    }
    if let Some(ep) = Catalog::read(state)?.providers.get(id) {
        return Ok(ep
            .models
            .iter()
            .map(|m| (m.id.clone(), Some(m.context_window)))
            .collect());
    }
    let path = state.join(format!("catalog-{id}.json"));
    if cached
        && let Ok(data) = fs::read(&path)
        && let Ok(rows) = serde_json::from_slice::<Vec<(String, Option<u64>)>>(&data)
        && !rows.is_empty()
    {
        return Ok(rows);
    }
    let key = if matches!(id, "deepseek" | "anthropic" | "openai-api" | "google") {
        Some(
            std::env::var(key_env(id)?)
                .or_else(|_| fs::read_to_string(state.join(format!("{id}.key"))))
                .with_context(|| format!("use :login {id} first"))?,
        )
    } else {
        None
    };
    let catalogue = panda_providers::models::catalogue(id, state, key.as_deref().map(str::trim))?;
    if id == "openrouter" {
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer(&mut file, &catalogue.output_limits)?;
        file.persist(state.join("catalog-openrouter-output.json"))?;
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer(&mut file, &catalogue.prices)?;
        file.persist(state.join("catalog-openrouter-prices.json"))?;
    }
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

fn pick_command(
    state: &Path,
    cmd: &str,
    current: (&str, &str),
    scope: &str,
) -> Result<Option<String>> {
    let catalog = Catalog::read(state)?;
    let ids = catalog.ids();
    if cmd == ":login" {
        let labels = ids
            .iter()
            .map(|id| {
                let label = match *id {
                    "openai-codex" => "OpenAI Codex (ChatGPT subscription)",
                    "deepseek" => "DeepSeek (API key)",
                    "openrouter" => "OpenRouter (API key)",
                    other => other,
                };
                Ok(format!(
                    "{label} · {}",
                    if configured(state, id)? {
                        "configured"
                    } else {
                        "not configured"
                    }
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(
            picker::choose("Search providers", &labels, 0)?.map(|i| format!(":login {}", ids[i]))
        );
    }
    let (models, errors) = available_models(state, scope)?;
    ensure!(
        !models.is_empty(),
        "No available models. Use :login to configure a provider. {}",
        errors.join("; ")
    );
    let labels = models
        .iter()
        .map(|(id, m)| {
            let name = catalog
                .providers
                .get(id)
                .and_then(|ep| ep.model(m).ok())
                .and_then(|m| m.name.as_deref());
            name.map_or_else(
                || format!("{m}  [{id}]"),
                |name| format!("{name}  {m}  [{id}]"),
            )
        })
        .collect::<Vec<_>>();
    let selected = models
        .iter()
        .position(|(id, m)| (id.as_str(), m.as_str()) == current)
        .unwrap_or(0);
    Ok(picker::choose("Search models", &labels, selected)?
        .map(|i| format!(":model {} {}", models[i].0, models[i].1)))
}
fn subscription(p: &str) -> bool {
    p == "openai-codex"
}
fn output_limit(state: &Path, provider: &str, model: &str) -> Result<Option<u64>> {
    let path = state.join("catalog-openrouter-output.json");
    if provider != "openrouter" || !path.try_exists()? {
        return Ok(None);
    }
    let limits: std::collections::BTreeMap<String, u64> = serde_json::from_slice(&fs::read(path)?)?;
    Ok(limits.get(model).copied().filter(|&n| n > 0))
}
fn provider(state: &Path, p: &str, m: &str) -> Result<Box<dyn Provider + Send>> {
    ensure!(
        !model_catalog::logged_out(state, p)?,
        "{p} is logged out; use :login {p}"
    );
    if let Some(ep) = Catalog::read(state)?.providers.get(p) {
        let model = ep.model(m)?;
        if let Some(api) = ep.api.as_deref().filter(|api| *api != "openai-completions") {
            return Ok(Box::new(panda_providers::native::NativeApi {
                family: panda_providers::native::Family::parse(api)?,
                provider: p.into(),
                api: Api {
                    model: m.into(),
                    endpoint: ep.base_url.clone(),
                    key: ep.key(state, p)?,
                },
                max_tokens: model.max_tokens,
                images: model.input.iter().any(|s| s == "image"),
            }));
        }
        return Ok(Box::new(
            Api {
                key: ep.key(state, p)?,
                model: m.into(),
                endpoint: ep.url(),
            }
            .configured(
                Some(model.max_tokens),
                model.input.iter().any(|i| i == "image"),
            ),
        ));
    }
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
    if let Some((family, base)) = panda_providers::native::builtin(p) {
        return Ok(Box::new(panda_providers::native::NativeApi {
            family,
            provider: p.into(),
            api: Api {
                key: key.trim().into(),
                model: m.into(),
                endpoint: base.into(),
            },
            max_tokens: 4096,
            images: true,
        }));
    }
    let endpoint = match p {
        "deepseek" => "https://api.deepseek.com/chat/completions",
        _ => "https://openrouter.ai/api/v1/chat/completions",
    };
    Ok(Box::new(
        Api {
            key: key.trim().into(),
            model: m.into(),
            endpoint: endpoint.into(),
        }
        .with_output_limit(output_limit(state, p, m)?),
    ))
}
fn login(state: &Path, p: &str, cancel: &Cancel, browser: Option<bool>) -> Result<()> {
    if let Some(ep) = Catalog::read(state)?.providers.get(p) {
        let key = ep.key(state, p)?;
        if !key.is_empty() {
            let mut file = tempfile::NamedTempFile::new_in(state)?;
            file.write_all(key.as_bytes())?;
            file.persist(state.join(format!("{p}.key")))?;
        }
        return model_catalog::remove(&state.join(format!("{p}.logged-out")));
    }
    if subscription(p) {
        let account = Subscription {
            provider: p.into(),
            model: String::new(),
            state: state.into(),
        };
        match browser {
            Some(method) => account.login_with_method(cancel, method),
            None => account.login_with_cancel(cancel),
        }?;
        return model_catalog::remove(&state.join(format!("{p}.logged-out")));
    }
    let var = key_env(p)?;
    let key = std::env::var(var)
        .with_context(|| format!("export {var} before starting Panda, then retry :login"))?;
    let mut temp = tempfile::NamedTempFile::new_in(state)?;
    temp.write_all(key.trim().as_bytes())?;
    temp.persist(state.join(format!("{p}.key")))?;
    model_catalog::remove(&state.join(format!("{p}.logged-out")))?;
    println!("Saved {p} credential");
    Ok(())
}
fn safe(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
fn usage(s: &Session) -> String {
    let sdk::UsageTotals {
        input,
        output,
        read,
        write,
        cost,
        unknown,
    } = sdk::UsageTotals::from(s);
    let mut text =
        format!("↑{input} ↓{output} · cache read {read} write {write} · known ${cost:.6}");
    if unknown > 0 {
        text.push_str(&format!(" · {unknown} unpriced"));
    }
    if let (Some(last), Some(window)) = (s.usage.last(), s.model_window) {
        text.push_str(&format!(" · last input {}/{window}", last.input));
    }
    text
}
fn configure_context(
    db: &mut dyn Store,
    state: &Path,
    id: &str,
    model: &str,
    settings: &settings::Settings,
) -> Result<()> {
    let keep_recent_tokens = settings.retention(id, model);
    let path = state.join(format!("catalog-{id}.json"));
    let catalog = Catalog::read(state)?;
    let window = if id == "router" {
        catalog
            .routes
            .get(model)
            .context("unknown virtual model")?
            .window(&catalog)
    } else if let Some(ep) = catalog.providers.get(id) {
        Some(ep.model(model)?.context_window)
    } else if PROVIDERS.contains(&id) && path.try_exists()? {
        let rows: Vec<(String, Option<u64>)> = serde_json::from_slice(&fs::read(path)?)?;
        rows.iter().find(|(m, _)| m == model).and_then(|r| r.1)
    } else {
        None
    };
    let mut s = db.load()?;
    if s.model_window != window
        || s.keep_recent_tokens != keep_recent_tokens
        || s.compaction_thresholds != settings.thresholds()
    {
        s.compaction_thresholds = settings.thresholds();
        s.keep_recent_tokens = keep_recent_tokens;
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
    for (index, m) in s.history.iter().enumerate() {
        match m {
            Message::Bash {
                command, outcome, ..
            } => screen.line(Tone::Tool, &safe(&outcome.context(command))),
            Message::CodemodeStore(_) | Message::Replay(_) => continue,
            Message::Truncated(partial) => {
                screen.line(Tone::Dim, "Partial reply (output length limit)");
                if !partial.reasoning.is_empty() {
                    screen.line(Tone::Thought, &partial.reasoning);
                }
                screen.finish_reply(&partial.text);
            }
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
                if s.model.as_ref().is_some_and(|id| id.provider == "router")
                    && let Some(actual) = s.responders.get(&index)
                {
                    screen.line(
                        Tone::Dim,
                        &format!("Responder · {}/{}", actual.provider, actual.model),
                    );
                }
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
                id,
                text,
                is_error,
                details,
                ..
            } => {
                let text = active::result_text(text, details.as_ref());
                if screen.active {
                    screen.tool_result(calls_by_id.remove(id), &text, *is_error);
                } else {
                    println!("[{}] {}", safe(id), safe(&text));
                }
            }
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
        [flag, id, pointer] if flag == "--recover-transcript" => {
            let disk = Disk::open(
                &dir,
                &root,
                Some(id.parse().context("expected numeric session ID")?),
            )?;
            io::stdout()
                .lock()
                .write_all(disk.recover_transcript(pointer)?.as_bytes())?;
        }
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
            let out = io::BufWriter::new(temp.as_file_mut());
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
            {
                html::export(&mut disk, out)?;
            } else {
                disk.export_pi(out)?;
            }
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
        } else if name == "edit" {
            serde_json::to_writer(
                io::stdout().lock(),
                &panda_local::worker::mutate(&root, name, &arg)?,
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
            "panda — interactive coding harness\n--version prints the release version\n--trust [allow|deny|revoke|status] manages workspace trust. --package install|list|activate|update|remove manages Markdown packages.\n--import-session FILE [--session-dir DIR] imports Pi v3 JSONL. --export-session ID FILE [--session-dir DIR] writes standalone HTML for .html files or Pi v3 JSONL otherwise.\n--list-models [PATTERN] lists available models. --models PATTERNS scopes startup and cycling (Ctrl-P or :cycle).\npanda PROMPT sends the first turn in an interactive terminal and stays open for follow-ups.\n--all-sessions opens the cross-workspace picker. --resume-any ID opens the original workspace. --fork-here ID copies the session into this workspace. --recover-transcript ID POINTER writes private checkpoint JSONL.\n--continue resumes the most recently modified session in this workspace. --print PROMPT writes the final answer. --mode json emits events; --mode rpc accepts JSONL commands. Add --rpc-version pi-v1 to use Pi commands and events.\nPrint and JSON options: [--provider PROVIDER] --model MODEL, --thinking LEVEL, --session-dir DIR, --session ID or --no-session, --tools NAME[,NAME...] or --no-tools. --system-prompt TEXT|PATH replaces the base prompt. Repeat --append-system-prompt TEXT|PATH to add instructions in order. Piped input precedes the prompt.\n--compact-soft-at PCT, --compact-at PCT, --compact-cache-at PCT and --compact-cache-for MINUTES override checkpoint settings for this invocation.\n--codemode enables read-only JavaScript tool composition. For print and JSON, --codemode-only exposes codemode as the sole provider tool and applies --tools to its nested helpers.\n{HELP}"
        );
        return Ok(());
    }
    if let Some(flag @ ("--trust" | "--package")) = args.first().map(String::as_str) {
        let root = std::env::current_dir()?.canonicalize()?;
        let project = panda_local::resources::project_root(&root);
        let state = state_dir()?;
        if flag == "--package" {
            println!(
                "{}",
                panda_local::packages::manage(&state, project, &args[1..])?
            );
        } else {
            ensure!(args.len() <= 2, "usage: --trust [allow|deny|revoke|status]");
            let action = args.get(1).map_or("status", String::as_str);
            if action != "status" {
                panda_local::catalog::decide(&state, project, action)?;
            }
            let status = match panda_local::catalog::trusted(&state, project)? {
                Some(true) => "allowed",
                Some(false) => "denied",
                None => "undecided",
            };
            println!("{} · {status}", project.display());
        }
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--list-models") {
        ensure!(args.len() <= 2, "usage: panda --list-models [PATTERN]");
        let state = state_dir()?;
        let catalog = Catalog::read(&state)?;
        let (models, errors) = available_models(&state, args.get(1).map_or("", String::as_str))?;
        for (id, m) in models {
            if let Some(ep) = catalog.providers.get(&id) {
                let model = ep.model(&m)?;
                println!(
                    "{id}/{m} · {} · context {} · output {} · {}",
                    model.name.as_deref().unwrap_or(&m),
                    model.context_window,
                    model.max_tokens,
                    model.input.join("+")
                );
            } else {
                println!("{id}/{m}");
            }
        }
        for error in errors {
            eprintln!("{error}");
        }
        return Ok(());
    }
    if args.first().is_some_and(|a| {
        matches!(
            a.as_str(),
            "--import-session" | "--export-session" | "--recover-transcript"
        )
    }) {
        return transfer_session(&args);
    }
    let interactive_terminal = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb");
    let mut options = automation::Options::parse(&args, interactive_terminal)?;
    let root = std::env::current_dir()?.canonicalize()?;
    let state = state_dir()?;
    let catalog = Catalog::read(&state)?;
    if let Some(id) = &options.provider {
        ensure!(
            catalog.ids().contains(&id.as_str()),
            "unknown provider {id}"
        );
    }
    if options.interface == automation::Interface::Terminal {
        terminal_config::load(&state)?;
    }
    let (root, selected_disk) = if let Some(choice) = options.workspace {
        let launch_workspace = Native::new(&root, &std::env::current_exe()?)?;
        let dir = match &options.session_dir {
            Some(dir) => launch_workspace.session_dir(dir)?,
            None => state.clone(),
        };
        let Some((chosen, db)) = workspaces::select(
            choice,
            &root,
            &dir,
            interactive_terminal && options.interface == automation::Interface::Terminal,
        )?
        else {
            return Ok(());
        };
        if chosen != root {
            options.trust_project = false;
        }
        (chosen, Some(db))
    } else {
        (root, None)
    };
    let project = panda_local::resources::project_root(&root);
    let saved_trust = panda_local::catalog::trusted(&state, project)?;
    if !options.trust_project
        && saved_trust.is_none()
        && interactive_terminal
        && options.interface == automation::Interface::Terminal
        && project.join(".panda/mcp.json").try_exists()?
    {
        let labels = [
            "Skip this time".into(),
            "Trust this workspace".into(),
            "Decline and remember".into(),
        ];
        match picker::choose(
            &format!(
                "Allow project MCP servers from {}?",
                ui::inline(&project.display().to_string(), 128)
            ),
            &labels,
            0,
        )? {
            Some(1) => {
                panda_local::catalog::decide(&state, project, "allow")?;
                options.trust_project = true;
            }
            Some(2) => panda_local::catalog::decide(&state, project, "deny")?,
            _ => {}
        }
    }
    options.trust_project |= saved_trust == Some(true);
    let packages = panda_local::packages::active(&state, project, options.trust_project)?;
    let mut settings = settings::Settings::read(&state, project)?;
    settings.invocation_controls = options.compaction;
    let cli_recent = options.recent_override;
    let templates = panda_local::prompts::Templates::with_packages(
        &state,
        project,
        options.trust_project,
        packages.prompts,
    )?;
    if options.prompt.starts_with('/')
        && let Some(prompt) = templates.expand(&options.prompt)?
    {
        options.prompt = prompt;
    }
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = std::env::var_os("PANDA_TEST_WORKER").map_or(executable, PathBuf::from);
    let mut native = Native::new(&root, &executable)?;
    native.mcp = panda_local::mcp::Servers::load(&state, project, options.trust_project, &native)?;
    let sessions = match &options.session_dir {
        Some(dir) if !options.no_session => native.session_dir(dir)?,
        _ => state.clone(),
    };
    let home = PathBuf::from(std::env::var_os("HOME").context("missing HOME")?);
    let mut roots = Vec::new();
    for dir in panda_local::resources::project_dirs(&root)
        .into_iter()
        .rev()
    {
        roots.extend(
            [dir.join(".agents/skills"), dir.join(".pi/skills")]
                .into_iter()
                .filter(|path| path.canonicalize().is_ok_and(|real| real == *path)),
        );
    }
    roots.extend([
        home.join(".agents/skills"),
        home.join(".pi/agent/skill-library"),
        home.join(".pi/agent/skills"),
    ]);
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
        skills: Library::new(roots).with_packages(packages.skills),
    };
    let selected_tools = options
        .tools
        .take()
        .map(|names| panda_coding::SelectedTools::new(&coding, names))
        .transpose()?;
    let base: &(dyn Toolset + Sync) = match &selected_tools {
        Some(selected) => selected,
        None => &coding,
    };
    // Native and script dispatch share the same policy. The outer projection
    // additionally controls the synthetic codemode tool itself.
    let exposure =
        panda_coding::Exposure::new(base, options.deferred.clone(), options.hidden.clone())?;
    let codemode =
        panda_local::codemode::Codemode::new(&exposure, &executable, &coding.workspace.root);
    let provider_tools;
    let tools: &(dyn Toolset + Sync) = match options.codemode {
        CodemodeExposure::Off => &exposure,
        CodemodeExposure::Alongside => &codemode,
        CodemodeExposure::Only => {
            provider_tools =
                panda_coding::SelectedTools::new(&codemode, ["codemode".to_owned()].into())?;
            &provider_tools
        }
    };
    let provider_exposure = panda_coding::Exposure::new(
        tools,
        std::mem::take(&mut options.deferred),
        std::mem::take(&mut options.hidden),
    )?;
    let tools: &(dyn Toolset + Sync) = &provider_exposure;
    let index = std::sync::RwLock::new(if settings.skill_index {
        coding.skills.startup_index()?
    } else {
        String::new()
    });
    let replacement = match &options.system_prompt {
        Some(value) => Some(automation::prompt_text_at(&root, value)?),
        None => settings::system_file(&state, project, options.trust_project, "SYSTEM.md")?,
    };
    let appended =
        settings::system_file(&state, project, options.trust_project, "APPEND_SYSTEM.md")?;
    let prompt = if replacement.is_some()
        || appended.is_some()
        || !options.append_system_prompt.is_empty()
        || options.codemode == CodemodeExposure::Only
    {
        let mut prompt = if let Some(value) = &replacement {
            let mut prompt = coding.system_with_base(value)?;
            if options.codemode != CodemodeExposure::Off {
                codemode.append_guidance(&mut prompt);
            }
            prompt
        } else {
            tools.system()?
        };
        if options.codemode == CodemodeExposure::Only {
            prompt.push_str("\nUse codemode for every tool call. Follow tool-selection guidance inside the script using the available helpers.");
        }
        if let Some(text) = appended {
            prompt.push_str("\n\n");
            prompt.push_str(&text);
        }
        for value in &options.append_system_prompt {
            prompt.push_str("\n\n");
            prompt.push_str(&automation::prompt_text_at(&root, value)?);
        }
        Some(prompt)
    } else {
        None
    };
    let prompted = automation::PromptedTools {
        tools,
        prompt,
        index: &index,
    };
    let tools = &prompted as &(dyn Toolset + Sync);
    let selected = state.join("model.json");
    let mut disk = selected_disk.or(options
        .session_id
        .map(|id| Disk::open(&sessions, &root, Some(id)))
        .transpose()?);
    let (mut p, mut model) = initial_model(&selected, &options)?;
    if !catalog.ids().contains(&p.as_str()) {
        let warning = "Saved provider is unavailable; choose a supported provider with :model PROVIDER MODEL.";
        if interactive {
            screen.line(Tone::Removed, warning);
        } else {
            eprintln!("{warning}");
        }
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
    let store: &mut dyn panda_core::sdk::SessionHost = match &mut disk {
        Some(disk) => {
            disk.invocation_keep_recent_tokens = cli_recent;
            disk
        }
        None => {
            let mut session = memory.load()?;
            session.invocation_keep_recent_tokens = cli_recent;
            memory.save(&mut session)?;
            &mut memory
        }
    };
    let mut session = store.load()?;
    if options.model.is_none() {
        if let Some(saved) = &session.model {
            (p, model) = (saved.provider.clone(), saved.model.clone());
        }
        if let Some(scope) = &options.scope {
            let (models, errors) = available_models(&state, scope)?;
            ensure!(
                !models.is_empty(),
                "no available models in scope {scope}. {}",
                errors.join(", ")
            );
            if !models.contains(&(p.clone(), model.clone())) {
                (p, model) = models[0].clone();
            }
            if let Some(effort) = scope_effort(scope, &p, &model)? {
                session.reasoning = Some(effort);
            }
        }
        if catalog.ids().contains(&p.as_str())
            && (session.history.is_empty() || options.scope.is_some())
        {
            let id = ModelId {
                provider: p.clone(),
                model: model.clone(),
            };
            if session.model.as_ref() != Some(&id) || session.reasoning != store.load()?.reasoning {
                session.model = Some(id);
                store.save(&mut session)?;
            }
        }
    } else if options.provider.is_none()
        && let Some(saved) = &session.model
    {
        p = saved.provider.clone();
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
    let persist_default = options.model.is_none();
    let make_provider = |state: &Path, p: &str, m: &str| -> Result<Box<dyn Provider + Send>> {
        let configured = Catalog::read(state)?;
        let id = ModelId {
            provider: p.into(),
            model: m.into(),
        };
        let inner: Box<dyn Provider + Send> = if p == "router" {
            ensure!(
                !model_catalog::logged_out(state, p)?,
                "router is logged out"
            );
            Box::new(routing::Router {
                id,
                state: state.into(),
                route: configured
                    .routes
                    .get(m)
                    .context("unknown virtual model")?
                    .clone(),
                connect: provider,
                persist: persist_default,
                last_price: None,
            })
        } else {
            Box::new(IdentifiedProvider {
                persist_default,
                inner: provider(state, p, m)?,
                id,
                prices: routing::prices(state, &configured, p, m)?,
            })
        };
        match panda_providers::jev::Jev::from_env() {
            Ok(judge) => Ok(Box::new(panda_providers::jev::NativeProvider {
                inner,
                judge,
            })),
            Err(_) => Ok(inner), // An unusable optional judge still permits provider compaction.
        }
    };
    configure_context(store, &state, &p, &model, &settings)?;
    let cancel = Cancel::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.stop())?;
    if !terminal_mode {
        return automation::Host {
            db: store,
            session_id,
            tools,
            connect: &|selection| match selection {
                Some(id) => make_provider(&state, &id.provider, &id.model),
                None => make_provider(&state, &p, &model),
            },
            models: automation::ModelControls {
                list: &|| {
                    Ok(available_models(&state, "")?
                        .0
                        .into_iter()
                        .map(|(provider, model)| ModelId { provider, model })
                        .collect())
                },
                describe: &|id| model_catalog::rpc_model(&state, id),
                effort: &|id| {
                    let mut levels: Vec<_> = effort_levels(&state, &id.provider, &id.model, true)?
                        .into_iter()
                        .map(|level| {
                            if matches!(level.as_str(), "default" | "none") {
                                "off".into()
                            } else {
                                level
                            }
                        })
                        .collect();
                    levels.dedup();
                    Ok(levels)
                },
                retention: &|id| settings.retention(&id.provider, &id.model),
            },
            root: &root,
            cancel: &cancel,
        }
        .run(options);
    }
    let restore_model = |db: &mut Disk| -> Result<(String, String)> {
        let s = db.load()?;
        if let Some(id) = &s.model {
            return Ok((id.provider.clone(), id.model.clone()));
        }
        initial_model(&selected, &automation::Options::default())
    };
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
    let mut scope = options.scope.clone().unwrap_or_default();
    let mut picker = None;
    let mut catalogue: Vec<(String, String)> = Vec::new();
    let mut initial_prompt = (!options.prompt.is_empty()).then_some(options.prompt);

    loop {
        db.invocation_keep_recent_tokens = cli_recent;
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
            let cycled;
            let line = if allow_commands && matches!(line, ":cycle" | ":cycle prev" | ":cycle next")
            {
                let (models, errors) = available_models(&state, &scope)?;
                ensure!(
                    !models.is_empty(),
                    "no available models in scope {scope}. {}",
                    errors.join(", ")
                );
                let pos = models.iter().position(|(id, m)| id == &p && m == &model);
                let n = models.len();
                let next = if line == ":cycle prev" {
                    pos.map_or(n - 1, |i| (i + n - 1) % n)
                } else {
                    pos.map_or(0, |i| (i + 1) % n)
                };
                let (id, m) = &models[next];
                let effort = scope_effort(&scope, id, m)?.unwrap_or_default();
                cycled = format!(":model {id} {m} {effort}");
                &cycled
            } else {
                line
            };
            let chosen;
            let line = if allow_commands && interactive && matches!(line, ":login" | ":model") {
                let Some(cmd) = pick_command(&state, line, (&p, &model), &scope)? else {
                    return Ok(());
                };
                chosen = cmd;
                &chosen
            } else {
                line
            };
            let expanded = if allow_commands
                && line.starts_with(':')
                && !HELP
                    .split(" · ")
                    .any(|s| s.split_whitespace().next() == line.split_whitespace().next())
            {
                templates.expand(line)?
            } else {
                None
            };
            let allow_commands = allow_commands && expanded.is_none();
            let line = expanded.as_deref().unwrap_or(line);
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
                ":diagnostics" | ":bug-report" => {
                    let path =
                        diagnostics::collect(&state, db.id(), &db.load()?, first == ":bug-report")?;
                    screen.line(Tone::Dim, &format!("Saved local report {}", path.display()));
                }
                ":changelog" => {
                    for row in diagnostics::changelog(words.next())?.lines() {
                        screen.line(Tone::Dim, row);
                    }
                }
                ":prompts" => {
                    for name in templates.names() {
                        screen.line(Tone::Dim, &format!("/{name}"));
                    }
                }
                ":settings" => {
                    match words.collect::<Vec<_>>().as_slice() {
                        [] => {}
                        ["reload"] => {
                            let mut next = settings::Settings::read(&state, project)?;
                            next.invocation_controls = options.compaction;
                            configure_context(&mut db, &state, &p, &model, &next)?;
                            *index.write().unwrap() = if next.skill_index {
                                coding.skills.startup_index()?
                            } else {
                                String::new()
                            };
                            settings = next;
                        }
                        _ => bail!("use :settings or :settings reload"),
                    }
                    for row in settings.describe(cli_recent, &p, &model).lines() {
                        screen.line(Tone::Dim, row);
                    }
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
                    if first == ":new" {
                        panda_core::sdk::SessionHost::switch(&mut db, None)?;
                    } else {
                        panda_core::sdk::SessionHost::fork_turn(&mut db, at, rest)?;
                    }
                    screen.clipboard = None;
                    screen.clear();
                    screen.footer.usage.clear();
                    (p, model) = restore_model(&mut db)?;
                    configure_context(&mut db, &state, &p, &model, &settings)?;
                    mode = show(&db, &mut screen)?;
                }
                ":tree" if interactive => {
                    let mut entries = db.tree_entries()?;
                    if entries.is_empty() {
                        screen.line(Tone::Dim, "This session has no prompts or answers yet.");
                        return Ok(());
                    }
                    let mut selection = picker::Selection::for_tree(&entries);
                    let draft = loop {
                        let Some((node, action)) = picker::tree(&entries, &mut selection)? else {
                            return Ok(());
                        };
                        break match action {
                            picker::TreeAction::EditLabel => {
                                let entry = entries.iter_mut().find(|e| e.id == node).unwrap();
                                let current = entry.label.as_deref().unwrap_or_default();
                                if let Some(label) = picker::edit_text(
                                    "Edit label (empty clears, Ctrl-C cancels)",
                                    current,
                                )? && label != current
                                {
                                    db.set_tree_label(node, &label)?;
                                    entry.label = (!label.is_empty()).then_some(label);
                                }
                                continue;
                            }
                            picker::TreeAction::Fork => {
                                Some(sdk::SessionHost::fork_prompt(&mut db, node)?)
                            }
                            picker::TreeAction::Switch => {
                                sdk::SessionHost::navigate_entry(&mut db, node)?
                            }
                            picker::TreeAction::Summarize | picker::TreeAction::GuidedSummary => {
                                let instructions = if action == picker::TreeAction::GuidedSummary {
                                    let Some(text) = picker::edit_text(
                                        "Branch summary instructions (Esc cancels)",
                                        "",
                                    )?
                                    else {
                                        continue;
                                    };
                                    text
                                } else {
                                    String::new()
                                };
                                cancel.reset();
                                let mut attempts = Vec::new();
                                let result = db.navigate_summarized(node, &cancel, |messages| {
                                    eprintln!("Summarizing branch… Ctrl-C to cancel");
                                    summarize_branch_accounted(
                                        make_provider(&state, &p, &model)?.as_mut(),
                                        messages,
                                        &instructions,
                                        &cancel,
                                        &mut attempts,
                                    )
                                });
                                if !attempts.is_empty() {
                                    let mut s = db.load()?;
                                    for a in &mut attempts {
                                        a.through = s.history.len();
                                    }
                                    s.attempts.extend(attempts);
                                    db.save(&mut s)?;
                                }
                                result?
                            }
                        };
                    };
                    screen.clear();
                    (p, model) = restore_model(&mut db)?;
                    configure_context(&mut db, &state, &p, &model, &settings)?;
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
                ":transcript" => {
                    let current = db.load()?.checkpoint.and_then(|c| c.recovery);
                    let pointer = words
                        .next()
                        .or(current.as_deref())
                        .context("this checkpoint has no private transcript")?;
                    for row in db.recover_transcript(pointer)?.lines() {
                        screen.line(Tone::Dim, &safe(row));
                    }
                }
                ":context" | ":compact" => {
                    let action = line[first.len()..].trim();
                    if (first == ":compact" || first == ":context")
                        && (action == "info" || (first == ":context" && action.is_empty()))
                    {
                        let session = db.load()?;
                        let info = native_compaction::context_info(
                            &session,
                            &tools.system()?,
                            &tools.schemas_for(&session),
                        );
                        screen.line(Tone::Dim, &format!("Context info: {info}"));
                    }
                    if first == ":compact"
                        && (action == "preview" || action.starts_with("preview "))
                    {
                        cancel.reset();
                        screen.line(Tone::Dim, "Preparing compaction preview. Esc cancels.");
                        let preview = active::preview(
                            make_provider(&state, &p, &model)?,
                            db.load()?,
                            db.compaction_scope(),
                            action.strip_prefix("preview").unwrap().trim(),
                            &cancel,
                            &screen,
                        )?;
                        screen.line(Tone::Dim, &format!("Compaction preview:\n{preview}"));
                    } else if first == ":compact" && action != "info" {
                        cancel.reset();
                        panda_core::sdk::SessionSdk { host: &mut db }.compact(
                            make_provider(&state, &p, &model)?.as_mut(),
                            &line[first.len()..],
                            &cancel,
                        )?;
                    }
                    let mut s = db.load()?;
                    if let Some(outcome) = &s.native_schedule.last_outcome {
                        screen.line(Tone::Dim, outcome);
                    }
                    if first == ":context"
                        && action != "info"
                        && let Some(arg) = words.next()
                    {
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
                            s.estimated_tokens(&tools.system()?, &tools.schemas_for(&s)),
                            s.token_budget(),
                            s.context_budget.map_or_else(|| "unset".into(), |n| n.to_string()),
                            s.checkpoint.as_ref().map_or(0, |c| c.through)
                        ),
                    );
                    if let Some(pointer) = s.checkpoint.and_then(|c| c.recovery) {
                        screen.line(Tone::Dim, &format!("Private transcript · {pointer}"));
                    }
                }
                ":ack" => acknowledge(&mut db)?,
                ":retry" => {
                    cancel.reset();
                    let mut pending = active::PendingTool::default();
                    sdk::SessionSdk { host: &mut db }.retry(
                        make_provider(&state, &p, &model)?.as_mut(),
                        tools,
                        &cancel,
                        &mut |event| active::display(event, &mut screen, &mut pending),
                        &std::sync::Mutex::new(sdk::Queue::<String> {
                            delivery: options.delivery,
                            ..Default::default()
                        }),
                    )?;
                    screen.footer.usage = usage(&db.load()?);
                }
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
                        picker::resume(&mut db, &state, &root)?
                    } else {
                        for (id, name, _) in db.catalogue(&root)? {
                            screen.line(Tone::Dim, &format!("{id} {name}"));
                        }
                        None
                    };
                    if let Some(id) = selected {
                        sdk::SessionHost::switch(&mut db, Some(id))?;
                        screen.clear();
                        screen.footer.usage.clear();
                        (p, model) = restore_model(&mut db)?;
                        configure_context(&mut db, &state, &p, &model, &settings)?;
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
                    screen.line(Tone::Text, &format!("{title}: 1 OpenAI Codex · 2 DeepSeek · 3 OpenRouter · anthropic · openai-api · google. Enter a number or name (:cancel to close)."));
                }
                ":usage" => {
                    let s = db.load()?;
                    screen.line(Tone::Dim, &usage(&s));
                    for (i, a) in s.attempts.iter().enumerate() {
                        let id = a.model.as_ref().map_or_else(
                            || "unknown model".into(),
                            |id| format!("{}/{}", id.provider, id.model),
                        );
                        let cost = a
                            .cost
                            .map_or_else(|| "unknown".into(), |v| format!("${v:.6}"));
                        screen.line(
                            Tone::Dim,
                            &format!(
                                "{} · {} · {} · {id} · {cost} ({})",
                                i + 1,
                                a.purpose,
                                a.outcome,
                                a.cost_source
                            ),
                        );
                    }
                }
                ":scope" => {
                    let new = line[first.len()..].trim();
                    if !new.is_empty() {
                        let filter = if new == "all" { "" } else { new };
                        let (rows, _) = available_models(&state, filter)?;
                        ensure!(!rows.is_empty(), "no available models in scope {filter}");
                        scope = filter.into();
                    }
                    screen.line(
                        Tone::Dim,
                        &format!(
                            "Model scope {}",
                            if scope.is_empty() { "all" } else { &scope }
                        ),
                    );
                }
                ":auth" | ":logout" => {
                    let ids = if let Some(raw) = words.next() {
                        vec![provider_id(raw)?]
                    } else if first == ":logout" {
                        vec![p.as_str()]
                    } else {
                        catalog.ids()
                    };
                    for id in ids {
                        ensure!(catalog.ids().contains(&id), "unknown provider {id}");
                        if first == ":logout" {
                            ensure!(id != "router", "manage credentials on the backing provider");
                            fs::write(state.join(format!("{id}.logged-out")), b"")?;
                            model_catalog::remove(&state.join(format!("{id}.key")))?;
                            if subscription(id) {
                                panda_providers::models::logout(&state)?;
                            }
                        }
                        screen.line(
                            Tone::Dim,
                            &format!(
                                "{id} · {}",
                                if configured(&state, id)? {
                                    "configured"
                                } else {
                                    "not configured"
                                }
                            ),
                        );
                    }
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
                    configure_context(&mut db, &state, &p, &model, &settings)?;
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
                        ensure!(catalog.ids().contains(&id), "unknown provider");
                        if id == "router" {
                            ensure!(catalog.routes.contains_key(m), "unknown virtual model {m}");
                        }
                        if let Some(ep) = catalog.providers.get(id) {
                            ep.model(m)?;
                        }
                        let mut selected_effort = None;
                        if let Some(effort) = words.next() {
                            ensure!(
                                effort_levels(&state, id, m, true)?
                                    .iter()
                                    .any(|v| v == effort),
                                "unsupported thinking effort {effort}"
                            );
                            selected_effort =
                                Some((effort != "default").then(|| effort.to_owned()));
                        }
                        panda_core::sdk::SessionSdk { host: &mut db }.select_model(
                            ModelId {
                                provider: id.into(),
                                model: m.into(),
                            },
                            selected_effort,
                        )?;
                        let mut temp = tempfile::NamedTempFile::new_in(&state)?;
                        serde_json::to_writer(&mut temp, &(id, m))?;
                        temp.persist(&selected)?;
                        (p, model) = (id.into(), m.into());
                        configure_context(&mut db, &state, &p, &model, &settings)?;
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
                ":help" | ":keys" => {
                    if first == ":help" {
                        screen.line(Tone::Dim, HELP);
                    }
                    screen.line(
                        Tone::Dim,
                        &format!("Picker keys · {}", terminal_config::help()),
                    );
                }
                ":theme" => {
                    let name = terminal_config::theme(&state, words.next())?;
                    screen.line(Tone::Dim, &format!("Theme · {name}"));
                }
                ":skills" => {
                    for diagnostic in coding.skills.diagnostics()? {
                        screen.line(Tone::Dim, &safe(&diagnostic));
                    }
                    let matches = coding.skills.ranked(line[first.len()..].trim())?;
                    if matches.is_empty() {
                        screen.line(Tone::Dim, "No matching skills. Try :skills to list all, or add a skill directory to PANDA_SKILL_ROOTS and restart.");
                    }
                    for (name, path, desc) in matches {
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
                    let (output, _) = coding.run(
                        &s,
                        &Call {
                            id: "user-shell".into(),
                            name: "shell".into(),
                            args: serde_json::json!({"command":command}),
                        },
                        &cancel,
                        &mut |_| {},
                    )?;
                    let output = output.text;
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
                    let mut engine = make_provider(&state, &p, &model)?;
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
                        (input, options.delivery),
                        &cancel,
                        &mut screen,
                        editor.as_mut(),
                    )?;
                    let session = db.load()?;
                    if p == "router"
                        && let Some((_, actual)) = session.responders.last_key_value()
                    {
                        screen.line(
                            Tone::Dim,
                            &format!("Responder · {}/{}", actual.provider, actual.model),
                        );
                    }
                    screen.footer.usage = usage(&session);
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
