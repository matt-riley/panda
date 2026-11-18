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
            system: &str,
            history: &[Message],
            _: &[serde_json::Value],
            cancel: &Cancel,
            emit: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            if system.starts_with("Summarize") {
                if std::env::var_os("PANDA_SLOW_SUMMARY").is_some() {
                    loop {
                        cancel.check()?;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                let payload = format!("{history:?}");
                ensure!(
                    payload.contains("Quartz") && !payload.contains("Granite"),
                    "wrong summary source"
                );
                return Ok(("Quartz finding".into(), vec![]));
            }
            if let [
                Message::User(_),
                Message::Assistant { text, .. },
                Message::BranchSummary(summary),
                Message::User(question),
            ] = history
                && text == "Granite"
                && summary == "Quartz finding"
                && question == "What survived?"
            {
                emit(Event::Text("Context verified".into()));
                return Ok(("Context verified".into(), vec![]));
            }
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
