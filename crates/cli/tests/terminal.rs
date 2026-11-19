use std::{
    io::Write,
    process::{Command, Stdio},
};
#[test]
fn plan_mode_survives_reopening_session() {
    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let run = |input: &str| {
        let mut p = Command::new(env!("CARGO_BIN_EXE_panda"))
            .current_dir(repo.path())
            .env("PANDA_HOME", state.path().join("panda"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        p.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = p.wait_with_output().unwrap();
        assert!(o.status.success());
        String::from_utf8(o.stdout).unwrap()
    };
    assert!(run("/plan\n/quit\n").contains("Plan"));
    assert!(run("/resume 1\n/quit\n").contains("session 1 (Plan)"));
}
#[test]
fn refuses_to_store_credentials_in_a_shared_state_directory() {
    use std::os::unix::fs::PermissionsExt;
    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let p = Command::new(env!("CARGO_BIN_EXE_panda"))
        .current_dir(repo.path())
        .env("PANDA_HOME", state.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!p.status.success());
    assert!(String::from_utf8_lossy(&p.stderr).contains("private"));
}

#[test]
fn retired_provider_requires_explicit_reselection() {
    let repo = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("panda");
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&state)
        .unwrap();
    std::fs::write(
        state.join("model.json"),
        r#"["github-copilot","old-model"]"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_panda"))
        .current_dir(repo.path())
        .env("PANDA_HOME", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/model github-copilot ignored\n/model openai-codex new-model\n/quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        errors.contains("choose a supported provider with :model"),
        "{errors}"
    );
    assert!(errors.contains("unknown provider"));
    assert_eq!(
        std::fs::read_to_string(state.join("model.json")).unwrap(),
        r#"["openai-codex","new-model"]"#
    );
}

#[test]
fn version_exits_without_opening_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("unused");
    let output = Command::new(env!("CARGO_BIN_EXE_panda"))
        .arg("--version")
        .env("PANDA_HOME", &state)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("panda {}\n", include_str!("../../../version.txt").trim())
    );
    assert!(!state.exists());
}

#[test]
fn fullscreen_editor_uses_standard_vim_and_restores_terminal() {
    assert!(
        Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fullscreen.py"))
            .arg(env!("CARGO_BIN_EXE_panda"))
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn rpc_replies_with_correlated_json_and_continue_reopens_session() {
    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    for extra in [vec![], vec!["--continue"]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_panda"))
            .current_dir(repo.path())
            .env("PANDA_HOME", state.path().join("state"))
            .args(extra)
            .args(["--mode", "rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"id\":1,\"type\":\"session\"}\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["id"], 1);
        assert_eq!(record["session_id"], 1);
    }
}

#[test]
fn login_and_model_pickers_accept_provider_names_and_route_gpt() {
    let repo = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut p = Command::new(env!("CARGO_BIN_EXE_panda"))
        .current_dir(repo.path())
        .env("PANDA_HOME", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    p.stdin.take().unwrap().write_all(b"/login\nOpenAI\ncancel\n/model\n/cancel\n/model openrouter openai/gpt-fixture\n/quit\n").unwrap();
    let out = p.wait_with_output().unwrap();
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        transcript.contains("Choose a login provider"),
        "{transcript}"
    );
    assert!(transcript.contains("Codex login method"), "{transcript}");
    assert!(
        transcript.contains("Choose a model provider"),
        "{transcript}"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("model.json")).unwrap(),
        r#"["openai-codex","gpt-fixture"]"#
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires native Chrome for the offline export viewer"]
fn html_export_opens_offline_on_active_branch() {
    assert!(
        Command::new("node")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/html_export.mjs"
            ))
            .arg(env!("CARGO_BIN_EXE_panda"))
            .status()
            .unwrap()
            .success()
    );
}
