use anyhow::{Result, ensure};
use panda_coding::Coding;
use panda_core::{Call, Cancel, Emit, Event, Message, Mode, Provider, Store, Toolset, turn};
use panda_local::{codemode::Codemode, sandbox::Native};
use panda_memory::{EmptySkills, MemoryStore};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, Instant},
};

const EXE: &str = env!("CARGO_BIN_EXE_panda");
const RANK: &str = r#"
const counts = new Map();
let offset = 0;
do {
    const page = tools.search({query: "TODO", offset});
    for (const {path} of page.items) counts.set(path, (counts.get(path) || 0) + 1);
    offset = page.next_offset;
} while (offset !== null);
text([...counts].sort((a,b) => b[1]-a[1]).slice(0,5));
"#;

fn call(name: &str, args: Value) -> Call {
    Call {
        id: "fixture".into(),
        name: name.into(),
        args,
    }
}

fn coding(root: &Path) -> Result<Coding<Native, EmptySkills>> {
    Ok(Coding {
        workspace: Native::new(root, EXE.as_ref())?,
        skills: EmptySkills,
    })
}

#[derive(Default)]
struct RankingProvider {
    requests: usize,
    result_bytes: usize,
    counts: BTreeMap<String, usize>,
}
impl Provider for RankingProvider {
    fn reply(
        &mut self,
        _: &str,
        history: &[Message],
        schemas: &[Value],
        _: &Cancel,
        _: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.requests += 1;
        // Count the tool-result content actually presented on every request, including repeated history.
        self.result_bytes += history
            .iter()
            .filter_map(|m| match m {
                Message::Result { text, .. } => Some(text.len()),
                _ => None,
            })
            .sum::<usize>();
        let enabled = schemas.iter().any(|s| s["function"]["name"] == "codemode");
        let offset = match history.last().unwrap() {
            Message::User(_) if enabled => {
                return Ok((String::new(), vec![call("codemode", json!({"code":RANK}))]));
            }
            Message::User(_) => 0,
            Message::Result { text, is_error, .. } => {
                ensure!(!is_error, "{text}");
                if enabled {
                    return Ok((text.clone(), vec![]));
                }
                let page: Value = serde_json::from_str(text)?;
                for item in page["items"].as_array().unwrap() {
                    *self
                        .counts
                        .entry(item["path"].as_str().unwrap().into())
                        .or_default() += 1;
                }
                if let Some(n) = page["next_offset"].as_u64() {
                    n
                } else {
                    let mut rows: Vec<_> = self.counts.iter().collect();
                    rows.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
                    rows.truncate(5);
                    return Ok((serde_json::to_string(&rows)?, vec![]));
                }
            }
            _ => anyhow::bail!("unexpected input"),
        };
        let mut search = call("search", json!({"query":"TODO","offset":offset}));
        search.id = format!("page-{offset}");
        Ok((String::new(), vec![search]))
    }
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn paginated_todo_ranking() -> Result<()> {
    let root = tempfile::tempdir()?;
    for (file, count) in [
        ("a", 48),
        ("b", 40),
        ("c", 32),
        ("d", 24),
        ("e", 16),
        ("f", 8),
    ] {
        fs::write(
            root.path().join(file),
            "TODO implement fixture\n".repeat(count),
        )?;
    }
    let base = coding(root.path())?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    let mut metrics = Vec::new();
    for (codemode, tools) in [
        (false, &base as &dyn Toolset),
        (true, &enabled as &dyn Toolset),
    ] {
        let mut store = MemoryStore::default();
        let mut state = store.load()?;
        state.mode = Mode::Plan;
        store.save(&mut state)?;
        let mut provider = RankingProvider::default();
        let mut offsets = vec![];
        turn(
            &mut provider,
            &mut store,
            tools,
            "Rank files by TODO matches, top five".into(),
            &Cancel::default(),
            &mut |event| {
                if let Event::NestedCall { parent, call } = event {
                    assert_eq!(parent, "fixture");
                    assert_eq!(call.name, "search");
                    offsets.push(call.args["offset"].as_u64().unwrap());
                }
            },
        )?;
        let history = store.load()?.history;
        let Message::Assistant { text, .. } = history.last().unwrap() else {
            panic!("expected answer")
        };
        assert_eq!(
            serde_json::from_str::<Value>(text)?,
            json!([["a", 48], ["b", 40], ["c", 32], ["d", 24], ["e", 16]])
        );
        if codemode {
            assert_eq!(offsets, [0, 50, 100, 150]);
            assert!(history.iter().all(
                |m| !matches!(m,Message::Result{text,..} if text.contains("TODO implement fixture"))
            ));
        }
        println!(
            "codemode={codemode}: {} requests, {} cumulative tool-result bytes",
            provider.requests, provider.result_bytes
        );
        metrics.push((provider.requests, provider.result_bytes));
    }
    assert_eq!((metrics[0].0, metrics[1].0), (5, 2));
    assert!(metrics[1].1 < metrics[0].1);
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn plan_search_patterns_work_directly_and_in_codemode() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("src"))?;
    fs::write(root.path().join("src/lib.rs"), "fn one() {}\nfn two() {}")?;
    fs::write(root.path().join("notes.txt"), "fn three() {}")?;
    let base = coding(root.path())?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    let schemas = enabled.schemas(Mode::Plan);
    for (name, args, expected) in [
        ("repo_map", json!({"glob":"**/*.rs"}), json!(["src/lib.rs"])),
        (
            "search",
            json!({"query":"^fn (one|two)", "regex":true, "glob":"**/*.rs", "offset":1}),
            json!([{"path":"src/lib.rs","line":2,"text":"fn two() {}"}]),
        ),
    ] {
        let schema = schemas
            .iter()
            .find(|s| s["function"]["name"] == name)
            .unwrap();
        let properties = &schema["function"]["parameters"]["properties"];
        for key in args.as_object().unwrap().keys() {
            assert!(properties.get(key).is_some(), "{name} lacks {key}");
        }
        let direct = base.run(Mode::Plan, &call(name, args.clone()), &Cancel::default())?;
        let code = format!("text(tools.{name}({args}))");
        let nested = enabled.run(
            Mode::Plan,
            &call("codemode", json!({ "code": code })),
            &Cancel::default(),
        )?;
        for result in [direct, nested] {
            assert_eq!(
                serde_json::from_str::<Value>(&result)?,
                json!({"items":expected,"next_offset":null})
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn reads_limits_and_cancellation() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("note"), "hello")?;
    let base = coding(root.path())?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    let run = |code: &str| {
        enabled.run(
            Mode::Code,
            &call("codemode", json!({"code":code})),
            &Cancel::default(),
        )
    };
    assert!(
        base.run(
            Mode::Code,
            &call("codemode", json!({"code":"text(1)"})),
            &Cancel::default()
        )
        .is_err()
    );
    assert_eq!(
        run("text(await tools.read({path:tools.repo_map().items[0]}))")?,
        "hello"
    );
    assert_eq!(
        run("text([typeof process,typeof fetch,typeof require,Object.keys(tools).sort()])")?,
        r#"["undefined","undefined","undefined",["read","repo_map","search"]]"#
    );
    assert!(run("tools.write({path:'note', text:'bad'})").is_err());
    assert_eq!(fs::read_to_string(root.path().join("note"))?, "hello");
    assert!(
        run("text('x'.repeat(16385))")
            .unwrap_err()
            .to_string()
            .contains("16 KiB")
    );
    assert!(run("text(new Uint8Array(128*1024*1024).length)").is_err());
    let script = "while(true){}";
    let start = Instant::now();
    assert!(run(script).unwrap_err().to_string().contains("time limit"));
    assert!(start.elapsed() < Duration::from_secs(15));

    let cancel = Cancel::default();
    let mut calls = 0;
    let (started, first_call) = std::sync::mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        let signal = cancel.clone();
        scope.spawn(move || {
            first_call.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            signal.stop();
        });
        let script = "tools.read({path:'note'}); const end = Date.now()+5000; while(Date.now()<end){}; tools.repo_map()";
        let start = Instant::now();
        let result = enabled.run_stream(
            Mode::Plan,
            &call("codemode", json!({"code":script})),
            &cancel,
            &mut |e| {
                if matches!(e, Event::NestedCall { .. }) {
                    calls += 1;
                    started.send(()).unwrap();
                }
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(start.elapsed() < Duration::from_secs(3));
        Ok(())
    })?;
    assert_eq!(calls, 1);
    Ok(())
}
