use anyhow::{Result, bail};
use panda_core::{sdk::*, *};
use panda_local::store::Disk;
use panda_memory::MemoryStore;
use serde_json::{Value, json};
use std::sync::Mutex;

struct Tools;
impl Toolset for Tools {
    fn system(&self) -> Result<String> {
        Ok(String::new())
    }
    fn definitions(&self, _: Mode) -> Vec<ToolDefinition> {
        vec![]
    }
    fn run(
        &self,
        _: &Session,
        _: &Call,
        _: &Cancel,
        _: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        bail!("unexpected tool")
    }
}
struct Script {
    fail_once: bool,
    cancel_once: bool,
}
impl Provider for Script {
    fn reply(
        &mut self,
        system: &str,
        h: &[Message],
        _: &[Value],
        k: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        if std::mem::take(&mut self.fail_once) {
            bail!("fixture provider failed")
        }
        if std::mem::take(&mut self.cancel_once) {
            k.stop();
            k.check()?;
        }
        let text = if system.starts_with("Summarize") {
            String::from("summary")
        } else {
            format!(
                "answer {}",
                h.iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::User(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .unwrap_or("empty")
            )
        };
        emit(Event::Text(text.clone()));
        Ok((text, vec![]))
    }
}
fn sequence(host: &mut dyn SessionHost) -> Result<Value> {
    let cancel = Cancel::default();
    let tools = Tools;
    let mut events = vec![];
    let queue = Mutex::new(Queue::<String> {
        steer: vec!["steered".into()],
        follow: vec!["follow".into()],
        delivery: Delivery {
            single_steer: true,
            single_follow: true,
        },
    });
    let mut p = Script {
        fail_once: true,
        cancel_once: false,
    };
    let mut sdk = SessionSdk { host };
    sdk.select_model(
        ModelId {
            provider: "fixture".into(),
            model: "model".into(),
        },
        Some(Some("medium".into())),
    )?;
    let mut emit = |e| events.push(serde_json::to_value(e).unwrap());
    let err = sdk
        .prompt(
            &mut p,
            &tools,
            Message::User("begin".into()),
            &cancel,
            &mut emit,
            &queue,
        )
        .unwrap_err();
    assert!(err.to_string().contains("fixture provider failed"));
    sdk.retry(&mut p, &tools, &cancel, &mut emit, &queue)?;
    let original = sdk.host.session_id();
    let messages = sdk.messages()?;
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m,Message::User(t) if t=="begin"))
            .count(),
        1
    );
    sdk.host.entries()?;
    sdk.compact(&mut p, "concise", &cancel)?;
    let entries = sdk.host.entries()?;
    let active = entries.iter().find(|e| e.active).unwrap();
    assert!(
        active.message.is_none(),
        "compaction creates an active metadata entry"
    );
    let branch_shape: Vec<_> = entries.iter().map(|e| (&e.message, e.active)).collect();
    let fork_label = entries
        .iter()
        .find(|e| matches!(&e.message,Some(Message::User(t)) if t=="follow"))
        .unwrap()
        .id;
    let draft = sdk.host.fork_prompt(fork_label)?;
    assert_eq!(draft, Message::User("follow".into()));
    let branch = sdk.host.session_id();
    p.cancel_once = true;
    assert!(
        sdk.prompt(
            &mut p,
            &tools,
            Message::User("cancel".into()),
            &cancel,
            &mut emit,
            &queue
        )
        .is_err()
    );
    acknowledge(sdk.host)?;
    cancel.reset();
    let forked = sdk.messages()?;
    sdk.host.switch(Some(original))?;
    assert_eq!(sdk.messages()?, messages);
    sdk.host.switch(Some(branch))?;
    assert_eq!(sdk.messages()?, forked);
    let state = sdk.state()?;
    assert!(!state.active);
    Ok(
        json!({"messages":messages,"branch":forked,"parent":state.parent,"model":state.model,"tree":branch_shape,"events":events}),
    )
}
#[test]
fn sqlite_and_embedded_hosts_share_session_semantics() -> Result<()> {
    let state = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut disk = Disk::open(state.path(), workspace.path(), None)?;
    let actual = sequence(&mut disk)?;
    let id = disk.session_id();
    drop(disk);
    let disk = Disk::open(state.path(), workspace.path(), Some(id))?;
    assert_eq!(json!(disk.load()?.history), actual["branch"]);
    assert_eq!(actual, sequence(&mut MemoryStore::default())?);
    Ok(())
}
