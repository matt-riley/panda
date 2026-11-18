use super::*;
#[test]
fn fixture() {
    if std::env::var_os("PANDA_TREE_FIXTURE").is_none() {
        return;
    }
    struct Offline;
    impl Provider for Offline {
        fn reply(
            &mut self,
            _: &str,
            history: &[Message],
            _: &[serde_json::Value],
            _: &Cancel,
            emit: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            let [Message::User(text)] = history else {
                bail!("unexpected branch context");
            };
            let answer = match text.as_str() {
                "Choose stone" => "Granite",
                "Choose stone instead" => "Quartz",
                _ => bail!("unexpected edited prompt: {text}"),
            };
            emit(Event::Text(answer.into()));
            Ok((answer.into(), vec![]))
        }
    }
    run(vec!["--continue".into()], |_, _, _| Ok(Box::new(Offline))).unwrap();
}
#[test]
#[ignore = "requires a native macOS terminal"]
fn edit_quit_reopen_and_switch_answers() {
    assert!(
        std::process::Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tree.py"))
            .arg(std::env::current_exe().unwrap())
            .status()
            .unwrap()
            .success()
    );
}
