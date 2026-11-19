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
fn selected_tools_restrict_direct_and_nested_dispatch() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("note"), "original")?;
    let base = coding(root.path())?;
    let read_only = panda_coding::SelectedTools::new(&base, ["read".to_owned()].into())?;
    let enabled = Codemode::new(&read_only, EXE.as_ref());
    let run = |name, args| enabled.run(Mode::Code, &call(name, args), &Cancel::default());
    let schema = enabled.schemas(Mode::Code);
    assert_eq!(
        schema
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["read", "codemode"]
    );
    assert!(
        !schema[1]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("tools.search")
    );
    assert_eq!(run("read", json!({"path":"note"}))?, "original");
    assert!(run("write", json!({"path":"note","text":"modified"})).is_err());
    assert_eq!(fs::read_to_string(root.path().join("note"))?, "original");
    assert_eq!(
        run(
            "codemode",
            json!({"code":"text([Object.keys(tools), tools.read({path:'note'})])"})
        )?,
        r#"[["read"],"original"]"#
    );
    assert!(
        run(
            "codemode",
            json!({"code":"tools.search({query:'original'})"})
        )
        .is_err()
    );

    let writes = panda_coding::SelectedTools::new(&base, ["write".to_owned()].into())?;
    assert!(writes.schemas(Mode::Plan).is_empty());
    assert!(
        writes
            .run(
                Mode::Plan,
                &call("write", json!({"path":"note","text":"modified"})),
                &Cancel::default()
            )
            .is_err()
    );
    assert_eq!(fs::read_to_string(root.path().join("note"))?, "original");
    Ok(())
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
            json!({"query":"^FN (ONE|TWO)", "regex":true, "ignoreCase":true, "context":0, "glob":"**/*.rs", "offset":1}),
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
fn gitignore_pages_match_in_plan_and_codemode() -> Result<()> {
    let root = tempfile::tempdir()?;
    let dir = root.path();
    fs::create_dir(dir.join("src"))?;
    fs::create_dir(dir.join("build"))?;
    fs::write(
        dir.join(".gitignore"),
        "/build/\n*.skip.rs\n!root.skip.rs\n",
    )?;
    fs::write(dir.join("src/.gitignore"), "/a-local.rs\n!z-keep.skip.rs\n")?;
    fs::write(dir.join("build/.gitignore"), "!unreachable.rs\n")?;
    for ignored in [
        "a.skip.rs",
        "build/unreachable.rs",
        "src/a-local.rs",
        "src/b.skip.rs",
    ] {
        fs::write(dir.join(ignored), "match")?;
    }

    let mut paths = vec![".hidden.rs".to_owned(), "root.skip.rs".to_owned()];
    paths.extend((0..48).map(|i| format!("src/{i:02}.rs")));
    paths.push("src/z-keep.skip.rs".to_owned());
    for path in &paths {
        fs::write(dir.join(path), "match")?;
    }

    let base = coding(dir)?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    for (name, mut args) in [
        ("search", json!({"query":"match"})),
        ("repo_map", json!({"glob":"**/*.rs"})),
    ] {
        let expected: Vec<_> = paths
            .iter()
            .map(|path| match name {
                "search" => json!({"path":path,"line":1,"text":"match"}),
                _ => json!(path),
            })
            .collect();
        let code = format!(
            "const first = tools.{name}({args}); text([first, tools.{name}({{...{args}, offset:first.next_offset}})]);"
        );
        let nested = enabled.run(
            Mode::Plan,
            &call("codemode", json!({"code":code})),
            &Cancel::default(),
        )?;
        let direct_page = |args: &Value| -> Result<Value> {
            Ok(serde_json::from_str(&base.run(
                Mode::Plan,
                &call(name, args.clone()),
                &Cancel::default(),
            )?)?)
        };
        let first = direct_page(&args)?;
        args["offset"] = first["next_offset"].clone();
        let pages = json!([first, direct_page(&args)?]);
        assert_eq!(
            pages,
            json!([
                {"items":expected[..50],"next_offset":50},
                {"items":expected[50..],"next_offset":null}
            ]),
            "{name}"
        );
        assert_eq!(serde_json::from_str::<Value>(&nested)?, pages, "{name}");
    }
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn search_context_pages_count_only_hits_in_plan_and_codemode() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join(".gitignore"), "discard.txt\n")?;
    fs::write(root.path().join("discard.txt"), "TODO")?;
    // Consecutive hits straddle the page boundary. The first and last hit
    // also touch file boundaries, where context must stop.
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    for i in 0..51 {
        hits.push(lines.len());
        lines.push(if i % 2 == 0 { "TODO" } else { "Todo" });
        if i < 49 {
            lines.push("nearby");
        }
    }
    fs::write(root.path().join("notes.txt"), lines.join("\n"))?;
    let expected: Vec<_> = hits
        .into_iter()
        .map(|i| {
            json!({
                "path":"notes.txt","line":i+1,"text":lines[i],
                "before":lines[i.saturating_sub(2)..i],
                "after":lines[i+1..(i+3).min(lines.len())]
            })
        })
        .collect();
    let base = coding(root.path())?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    let mut args = json!({"query":"todo","ignoreCase":true,"context":2});
    let code = format!(
        "const args = {args}; const first = tools.search(args); text([first, tools.search({{...args, offset:first.next_offset}})]);"
    );
    let expected_pages = [
        json!({"items":expected[..50],"next_offset":50}),
        json!({"items":expected[50..],"next_offset":null}),
    ];
    for expected_page in &expected_pages {
        let direct = base.run(
            Mode::Plan,
            &call("search", args.clone()),
            &Cancel::default(),
        )?;
        let direct: Value = serde_json::from_str(&direct)?;
        assert_eq!(&direct, expected_page);
        args["offset"] = direct["next_offset"].clone();
    }
    let nested = enabled.run(
        Mode::Plan,
        &call("codemode", json!({"code":code})),
        &Cancel::default(),
    )?;
    assert_eq!(
        serde_json::from_str::<Value>(&nested)?,
        json!(expected_pages)
    );
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn ls_pages_match_in_plan_and_codemode() -> Result<()> {
    let root = tempfile::tempdir()?;
    let dir = root.path();
    fs::write(dir.join(".hidden"), "")?;
    fs::create_dir(dir.join("empty"))?;
    fs::create_dir(dir.join("sub"))?;
    fs::write(dir.join("sub/child"), "")?;
    let mut entries = vec![
        json!({"name":".hidden","type":"file"}),
        json!({"name":"empty","type":"directory"}),
    ];
    for i in 0..48 {
        let name = format!("file-{i:02}");
        fs::write(dir.join(&name), "")?;
        entries.push(json!({"name":name,"type":"file"}));
    }
    entries.push(json!({"name":"sub","type":"directory"}));

    let base = coding(dir)?;
    let enabled = Codemode::new(&base, EXE.as_ref());
    let direct = |args| base.run(Mode::Plan, &call("ls", args), &Cancel::default());
    let nested = |code: &str| {
        enabled.run(
            Mode::Plan,
            &call("codemode", json!({"code":code})),
            &Cancel::default(),
        )
    };
    let first: Value = serde_json::from_str(&direct(json!({}))?)?;
    let second: Value = serde_json::from_str(&direct(json!({"offset":first["next_offset"]}))?)?;
    let pages = json!([first, second]);
    assert_eq!(
        pages,
        json!([
            {"items":entries[..50],"next_offset":50},
            {"items":entries[50..],"next_offset":null}
        ])
    );
    let js = "const first = tools.ls(); text([first, tools.ls({offset:first.next_offset})]);";
    assert_eq!(serde_json::from_str::<Value>(&nested(js)?)?, pages);

    for (path, items) in [
        ("empty", json!([])),
        ("sub", json!([{"name":"child","type":"file"}])),
    ] {
        let args = json!({"path":path});
        let js = format!("text(tools.ls({args}))");
        for result in [direct(args)?, nested(&js)?] {
            assert_eq!(
                serde_json::from_str::<Value>(&result)?,
                json!({"items":items,"next_offset":null})
            );
        }
    }
    for result in [
        direct(json!({"path":"missing"})),
        nested("tools.ls({path:'missing'})"),
    ] {
        assert!(result.unwrap_err().to_string().contains("missing"));
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
        r#"["undefined","undefined","undefined",["ls","read","repo_map","search"]]"#
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
