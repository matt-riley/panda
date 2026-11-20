use anyhow::{Result, ensure};
use panda_coding::{Coding, Exposure, Workspace};
use panda_core::*;
use panda_local::{codemode::Codemode, store::Disk};
use panda_memory::EmptySkills;
use serde_json::{Value, json};
use std::{
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn call(name: &str, args: Value) -> Call {
    Call {
        name: name.into(),
        id: name.into(),
        args,
    }
}
struct Fixture {
    calls: AtomicUsize,
    arrived: (Mutex<usize>, Condvar),
}
impl Workspace for Fixture {
    fn registered_tools(&self) -> Vec<ToolDefinition> {
        ["fixture","secret","change"].into_iter().map(|name|ToolDefinition{
            schema:json!({"type":"function","function":{"name":name,"description":format!("{} fixture",name),
                "parameters":{"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}}}),
            access:if name=="change"{Access::Mutate}else{Access::Read},
            output:OutputKind::Json,namespace:Some("fixture".into())
        }).collect()
    }
    fn invoke(&self, name: &str, args: &Value, _: &Cancel) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ensure!(name == "fixture", "unexpected dispatch of {name}");
        let value = args["value"].as_i64().unwrap();
        if value != 0 {
            let (lock, wake) = &self.arrived;
            let mut arrived = lock.lock().unwrap();
            *arrived += 1;
            wake.notify_all();
            let (_guard, timeout) = wake
                .wait_timeout_while(arrived, Duration::from_secs(2), |n| *n < 2)
                .unwrap();
            ensure!(!timeout.timed_out(), "reads failed to overlap");
        }
        ensure!(value >= 0, "requested rejection");
        Ok(json!({"value":value}).to_string())
    }
    fn instructions(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![])
    }
}
fn fixture() -> Coding<Fixture, EmptySkills> {
    Coding {
        workspace: Fixture {
            calls: AtomicUsize::default(),
            arrived: (Mutex::new(0), Condvar::new()),
        },
        skills: EmptySkills,
    }
}
struct Search;
impl Provider for Search {
    fn reply(
        &mut self,
        _: &str,
        history: &[Message],
        schemas: &[Value],
        _: &Cancel,
        _: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        let names: Vec<_> = schemas
            .iter()
            .filter_map(|s| s["function"]["name"].as_str())
            .collect();
        ensure!(!names.contains(&"secret") && !names.contains(&"change"));
        if matches!(history.last(),Some(Message::User(text)) if text=="discover") {
            ensure!(names.contains(&"tool_search") && !names.contains(&"fixture"));
            Ok((
                String::new(),
                vec![call("tool_search", json!({"query":"fixture"}))],
            ))
        } else {
            if matches!(history.last(), Some(Message::Result { .. })) {
                ensure!(names.contains(&"fixture"));
            }
            Ok(("done".into(), vec![]))
        }
    }
}
#[test]
fn discoveries_follow_the_selected_branch_across_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let coding = fixture();
    let tools = Exposure::new(&coding, ["fixture".into()].into(), ["secret".into()].into())?;
    let mut db = Disk::open(state.path(), root.path(), None)?;
    let mut session = db.load()?;
    session.mode = Mode::Plan;
    db.save(&mut session)?;
    let id = db.id();
    let cancel = Cancel::default();
    turn(
        &mut Search,
        &mut db,
        &tools,
        "start".into(),
        &cancel,
        &mut |_| {},
    )?;
    let common = db.tree()?.into_iter().find(|n| n.current).unwrap().id;
    turn(
        &mut Search,
        &mut db,
        &tools,
        "discover".into(),
        &cancel,
        &mut |_| {},
    )?;
    drop(db);
    let mut db = Disk::open(state.path(), root.path(), Some(id))?;
    let saved = db.load()?;
    let probe = call("fixture", json!({"value":0}));
    let output = tools.run(&saved, &probe, &cancel, &mut |_| {})?.0;
    assert_eq!(
        serde_json::from_str::<Value>(&output.text)?,
        json!({"value":0})
    );
    assert_eq!(
        run_nested(&tools, &saved, &probe, &cancel)?.text,
        output.text
    );
    assert!(
        run_nested(
            &tools,
            &Session {
                mode: saved.mode,
                ..Default::default()
            },
            &probe,
            &cancel
        )
        .is_err()
    );
    db.navigate(common)?;
    assert!(
        tools
            .run(&db.load()?, &probe, &cancel, &mut |_| {})
            .is_err()
    );
    let sibling = db.load()?;
    assert!(run_nested(&tools, &sibling, &probe, &cancel).is_err());
    for session in [&saved, &sibling] {
        assert!(
            run_nested(
                &tools,
                session,
                &call("secret", json!({"value":0})),
                &cancel
            )
            .is_err()
        );
    }
    Ok(())
}
#[test]
#[ignore = "requires native macOS Seatbelt"]
fn validated_registry_tools_and_async_batches_use_the_same_handlers() -> Result<()> {
    let root = tempfile::tempdir()?;
    let coding = fixture();
    let tools = Exposure::new(&coding, Default::default(), ["secret".into()].into())?;
    let canonical = root.path().canonicalize()?;
    let runner = Codemode::new(&tools, env!("CARGO_BIN_EXE_panda").as_ref(), &canonical);
    let cancel = Cancel::default();
    let session = Session {
        mode: Mode::Plan,
        ..Default::default()
    };
    let run = |name, args| {
        runner
            .run(&session, &call(name, args), &cancel, &mut |_| {})
            .map(|(output, _)| output.text)
    };
    for (name, args) in [
        ("fixture", json!({"value":"bad"})),
        ("change", json!({"value":0})),
        ("secret", json!({"value":0})),
    ] {
        assert!(run(name, args.clone()).is_err());
        assert!(
            run(
                "codemode",
                json!({"code":format!("return tools.{name}({args})")})
            )
            .is_err()
        );
    }
    assert_eq!(coding.workspace.calls.load(Ordering::SeqCst), 0);
    let direct = run("fixture", json!({"value":0}))?;
    assert_eq!(
        direct,
        run(
            "codemode",
            json!({"code":"return tools.fixture({value:0})"})
        )?
    );
    let code = r#"// @options: {"async":true}
const results=await Promise.allSettled([tools.fixture({value:1}),tools.fixture({value:-1})]);
return [results[0].value,results[1].status,String(results[1].reason),describeNamespace('fixture').map(e=>e.name)];"#;
    let output = run("codemode", json!({"code":code}))?;
    let values: Value = serde_json::from_str(&output)?;
    assert_eq!(values[0], json!({"value":1}));
    assert_eq!(values[1], "rejected");
    assert!(values[2].as_str().unwrap().contains("requested rejection"));
    assert_eq!(values[3], json!(["fixture"]));
    run(
        "codemode",
        json!({"code":"asyncTools.fixture({value:0});exit()"}),
    )?;
    assert_eq!(coding.workspace.calls.load(Ordering::SeqCst), 4);
    let err = run("codemode", json!({"code":"await new Promise(()=>{})"})).unwrap_err();
    assert!(err.to_string().contains("no pending tool"));
    Ok(())
}
