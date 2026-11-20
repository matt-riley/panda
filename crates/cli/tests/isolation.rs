use panda_coding::Workspace;
use panda_core::Cancel;
use panda_local::sandbox::Native;
use serde_json::json;
#[test]
#[ignore = "requires native macOS Seatbelt"]
fn batch_edit_is_available_only_in_code_mode() -> anyhow::Result<()> {
    use panda_coding::Coding;
    use panda_core::{Call, Mode, Session, Toolset};

    let dir = tempfile::tempdir()?;
    let file = dir.path().join("source");
    std::fs::write(&file, "ab")?;
    let coding = Coding {
        workspace: Native::new(dir.path(), env!("CARGO_BIN_EXE_panda").as_ref())?,
        skills: panda_memory::EmptySkills,
    };
    let batch = Call {
        id: "batch".into(),
        name: "edit".into(),
        args: json!({"path":"source","edits":[{"old":"b","new":"a"},{"old":"a","new":"b"}]}),
    };
    assert!(
        coding
            .schemas(Mode::Plan)
            .iter()
            .all(|s| s["function"]["name"] != "edit")
    );
    let error = coding
        .run(
            &Session {
                mode: Mode::Plan,
                ..Session::default()
            },
            &batch,
            &Cancel::default(),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("unavailable in Plan"));
    assert_eq!(std::fs::read(&file)?, b"ab");

    let tools = coding.schemas(Mode::Code);
    let edit = tools
        .iter()
        .find(|s| s["function"]["name"] == "edit")
        .unwrap();
    let schema = &edit["function"]["parameters"];
    assert_eq!(schema["required"], json!(["path"]));
    assert_eq!(
        schema["properties"]["edits"]["items"]["required"],
        json!(["old", "new"])
    );
    assert_eq!(
        coding
            .run(&Session::default(), &batch, &Cancel::default(), &mut |_| {})?
            .0
            .text,
        "saved\n− 1  ab\n+ 1  ba"
    );
    assert_eq!(std::fs::read(file)?, b"ba");
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt outside an enclosing sandbox"]
fn native_workspace_enforces_policy() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let private = fixture.path().join("private");
    std::fs::write(&private, "sentinel").unwrap();
    std::os::unix::fs::symlink(&private, root.join("link")).unwrap();
    let w = Native::new(&root, env!("CARGO_BIN_EXE_panda").as_ref()).unwrap();
    let run = |name, args| w.invoke(name, &args, &Cancel::default());
    run(
        "write",
        json!({"path":"Makefile","text":"check:\n\t@echo verified\n"}),
    )
    .unwrap();
    assert!(
        run("shell", json!({"command":"make check"}))
            .unwrap()
            .contains("verified")
    );
    assert!(run("shell", json!({"command":"cat link"})).is_err());
    assert!(run("read", json!({"path":"link"})).is_err());
    assert!(run("shell", json!({"command":"sh -c 'echo bad > link'"})).is_err());
    run("shell", json!({"command":"ln Makefile ordinary-link"})).unwrap();
    assert!(root.join("ordinary-link").is_file());
    for dir in [".git", ".panda", ".agents", ".codex", ".pi"] {
        std::fs::create_dir(root.join(dir)).unwrap();
        let path = format!("{dir}/marker");
        assert!(
            run("write", json!({"path":path,"text":"changed"})).is_err(),
            "{path}"
        );
        assert!(
            run("shell", json!({"command":format!("touch {path}")})).is_err(),
            "{path}"
        );
        assert!(!root.join(&path).exists(), "{path}");

        std::fs::write(root.join(&path), "protected fixture").unwrap();
        assert!(
            run("shell", json!({"command":format!("ln {path} alias")})).is_err(),
            "{path}"
        );
        assert!(!root.join("alias").exists(), "{path}");
    }
    for dir in ["one/.agents", "one/two/.pi"] {
        let skill = format!("{dir}/skills/sample/SKILL.md");
        let file = root.join(&skill);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "original").unwrap();
        assert!(run("write", json!({"path":skill,"text":"changed"})).is_err());
        let new_skill = format!("{dir}/skills/new");
        assert!(run("shell", json!({"command":format!("mkdir {new_skill}")})).is_err());
        assert!(!root.join(&new_skill).exists());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        run(
            "write",
            json!({"path": format!("{dir}/skills-notes"), "text":"ordinary"}),
        )
        .unwrap();
    }
    assert!(run("shell", json!({"command":"ln ../private hard-link"})).is_err());
    assert!(!root.join("hard-link").exists());
    assert_eq!(std::fs::read_to_string(private).unwrap(), "sentinel");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(run("shell",json!({"command":format!("/usr/bin/nc -z 127.0.0.1 {}",listener.local_addr().unwrap().port())})).is_err());
}
#[test]
#[ignore = "requires native macOS Seatbelt"]
fn same_session_contract_for_both_hosts() -> anyhow::Result<()> {
    use panda_local::{sandbox::Native, store::Disk};
    use panda_memory::*;
    let mut memory = MemoryStore::default();
    assert_eq!(
        read_fixture(
            Files([("hello.txt".into(), "hello".into())].into()),
            EmptySkills,
            &mut memory
        )?,
        "hello"
    );
    let dir = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    std::fs::write(dir.path().join("hello.txt"), "hello")?;
    let local = Native::new(dir.path(), env!("CARGO_BIN_EXE_panda").as_ref())?;
    let mut db = Disk::open(state.path(), dir.path(), None)?;
    assert_eq!(read_fixture(local, EmptySkills, &mut db)?, "hello");
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt"]
fn retry_after_shell_runs_command_once_and_reopens() -> anyhow::Result<()> {
    use panda_core::*;
    use panda_local::store::Disk;
    struct ProviderFixture(usize);
    impl Provider for ProviderFixture {
        fn reply(
            &mut self,
            _: &str,
            history: &[Message],
            _: &[serde_json::Value],
            _: &Cancel,
            _: &mut Emit,
        ) -> anyhow::Result<(String, Vec<Call>)> {
            self.0 += 1;
            if self.0 == 1 {
                return Ok((
                    String::new(),
                    vec![Call {
                        id: "run-tests".into(),
                        name: "shell".into(),
                        args: json!({"command":"echo ran >> invocations && echo passed"}),
                    }],
                ));
            }
            assert!(
                matches!(history.last(),Some(Message::Result{id,text,is_error:false,..}) if id=="run-tests" && text.contains("passed"))
            );
            if self.0 == 2 {
                return Err(TransientFailure {
                    reason: "503".into(),
                    retry_after: Some(std::time::Duration::ZERO),
                }
                .into());
            }
            Ok(("Tests passed".into(), vec![]))
        }
    }
    let repo = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let coding = panda_coding::Coding {
        workspace: Native::new(repo.path(), env!("CARGO_BIN_EXE_panda").as_ref())?,
        skills: panda_memory::EmptySkills,
    };
    let mut db = Disk::open(state.path(), repo.path(), None)?;
    let id = db.id();
    turn(
        &mut ProviderFixture(0),
        &mut db,
        &coding,
        "Run tests".into(),
        &Cancel::default(),
        &mut |_| {},
    )?;
    drop(db);
    let saved = Disk::open(state.path(), repo.path(), Some(id))?.load()?;
    assert!(!saved.active);
    assert!(
        matches!(&saved.history[..],[Message::User(_),Message::Assistant{calls,..},Message::Result{..},Message::Assistant{text,..}] if calls.len()==1 && text=="Tests passed")
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("invocations"))?,
        "ran\n"
    );
    Ok(())
}
