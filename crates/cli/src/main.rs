use anyhow::{Context, Result, bail, ensure};
use panda_coding::{Coding, Workspace};
use panda_core::*;
use panda_local::{
    sandbox::{Native, state_dir},
    skills::Library,
    store::Disk,
};
use panda_providers::{Api, subscription::Subscription};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

const HELP: &str =
    "/plan · /code · /model PROVIDER MODEL · /login PROVIDER · /resume [ID] · /ack · /quit";
fn key_env(provider: &str) -> Result<&'static str> {
    match provider {
        "deepseek" => Ok("DEEPSEEK_API_KEY"),
        "openrouter" => Ok("OPENROUTER_API_KEY"),
        _ => bail!("unknown API-key provider"),
    }
}
fn subscription(p: &str) -> bool {
    p == "openai-codex"
}
fn provider(state: &Path, p: &str, m: &str) -> Result<Box<dyn Provider>> {
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
        .with_context(|| format!("set {var} or use /login {p}"))?;
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
fn login(state: &Path, p: &str, cancel: &Cancel) -> Result<()> {
    if subscription(p) {
        return Subscription {
            provider: p.into(),
            model: String::new(),
            state: state.into(),
        }
        .login_with_cancel(cancel);
    }
    let var = key_env(p)?;
    let key = std::env::var(var)
        .with_context(|| format!("export {var} before starting Panda, then retry /login"))?;
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
fn show(db: &Disk) -> Result<()> {
    let s = db.load()?;
    println!("session {} ({:?})", db.id(), s.mode);
    for m in &s.history {
        match m {
            Message::User(t) => println!("you> {}", safe(t)),
            Message::Assistant { text, calls } => {
                println!("{}", safe(text));
                for c in calls {
                    println!("[{} {}]", safe(&c.name), safe(&c.id))
                }
            }
            Message::Result { id, text, .. } => println!("[{}] {}", safe(id), safe(text)),
        }
    }
    if s.active {
        println!(
            "Interrupted turn. Outstanding tool outcomes are unknown. Inspect the workspace and use /ack to continue."
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("panda {}", include_str!("../../../version.txt").trim());
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("--file-worker") {
        let arg = serde_json::from_reader(io::stdin())?;
        let name = args.get(1).context("missing tool name")?;
        print!(
            "{}",
            panda_local::worker::run(&std::env::current_dir()?, name, &arg)?
        );
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "panda — interactive coding harness\n--version prints the release version\n{HELP}"
        );
        return Ok(());
    }
    ensure!(args.is_empty(), "unknown arguments (see --help)");
    let root = std::env::current_dir()?.canonicalize()?;
    let state = state_dir()?;
    let native = Native::new(&root, &std::env::current_exe()?)?;
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
    for (p, _) in native.instructions()? {
        println!("Instructions: {}", safe(&p))
    }
    let coding = Coding {
        workspace: native,
        skills: Library::new(roots),
    };
    let mut db = Disk::open(&state, &root, None)?;
    show(&db)?;
    let selected = state.join("model.json");
    let (mut p, mut model): (String, String) = if selected.try_exists()? {
        serde_json::from_slice(&fs::read(&selected)?)?
    } else {
        ("deepseek".into(), "deepseek-flash".into())
    };
    if !subscription(&p) && key_env(&p).is_err() {
        eprintln!(
            "Saved provider is unavailable; choose a supported provider with /model PROVIDER MODEL."
        );
    }
    let cancel = Cancel::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.stop())?;
    println!("{HELP}");
    loop {
        if io::stdin().is_terminal() {
            print!("panda> ");
            io::stdout().flush()?
        }
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line == "/quit" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let action = (|| -> Result<()> {
            let mut words = line.split_whitespace();
            let first = words.next().unwrap();
            match first {
                "/plan" | "/code" => {
                    let mut s = db.load()?;
                    s.mode = if first == "/plan" {
                        Mode::Plan
                    } else {
                        Mode::Code
                    };
                    db.save(&mut s)?;
                    println!("{:?} mode", s.mode)
                }
                "/ack" => acknowledge(&mut db)?,
                "/resume" => {
                    if let Some(n) = words.next() {
                        let id = n.parse()?;
                        if id != db.id() {
                            db = Disk::open(&state, &root, Some(id))?
                        }
                        show(&db)?
                    } else {
                        println!("Sessions: {:?}", db.list(&root)?)
                    }
                }
                "/login" => {
                    cancel.reset();
                    login(&state, words.next().unwrap_or(&p), &cancel)?
                }
                "/model" => {
                    if let Some(id) = words.next() {
                        let m = words.next().context("usage: /model PROVIDER MODEL")?;
                        ensure!(subscription(id) || key_env(id).is_ok(), "unknown provider");
                        let mut temp = tempfile::NamedTempFile::new_in(&state)?;
                        serde_json::to_writer(&mut temp, &(id, m))?;
                        temp.persist(&selected)?;
                        p = id.into();
                        model = m.into();
                    }
                    println!("{} {}", safe(&p), safe(&model))
                }
                "/help" => println!("{HELP}"),
                unknown if unknown.starts_with('/') => bail!("unknown command: {HELP}"),
                _ => {
                    let mut engine = provider(&state, &p, &model)?;
                    cancel.reset();
                    turn(
                        engine.as_mut(),
                        &mut db,
                        &coding,
                        line.into(),
                        &cancel,
                        &mut |e| match e {
                            Event::Text(s) => {
                                print!("{}", safe(&s));
                                let _ = io::stdout().flush();
                            }
                            Event::Calling(c) => println!(
                                "\n[{} {}] {}",
                                safe(&c.name),
                                safe(&c.id),
                                safe(&c.args.to_string())
                            ),
                            Event::Result(s) => println!("{}", safe(&s)),
                            Event::Usage(_) => {}
                        },
                    )?;
                    println!();
                }
            }
            Ok(())
        })();
        if let Err(e) = action {
            eprintln!("Panda: {}", safe(&format!("{e:#}")));
            if db.load()?.active {
                eprintln!("Turn interrupted. Inspect the workspace, then /ack.")
            }
        }
    }
    Ok(())
}
