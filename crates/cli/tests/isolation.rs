use panda_coding::Workspace;
use panda_core::Cancel;
use panda_local::sandbox::Native;
use serde_json::json;
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
