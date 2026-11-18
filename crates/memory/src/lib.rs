use anyhow::{Result, bail, ensure};
use panda_coding::{Skills, Workspace, field};
use panda_core::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[derive(Default)]
pub struct MemoryStore(Session);
impl Store for MemoryStore {
    fn load(&self) -> Result<Session> {
        Ok(self.0.clone())
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        ensure!(self.0.revision == s.revision, "stale revision");
        s.revision += 1;
        self.0 = s.clone();
        Ok(())
    }
}
pub struct Files(pub BTreeMap<String, String>);
impl Workspace for Files {
    fn invoke(&self, name: &str, args: &Value, _: &Cancel) -> Result<String> {
        ensure!(name == "read", "unknown operation");
        self.0
            .get(field(args, "path")?)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing file"))
    }
    fn instructions(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![])
    }
}
pub struct EmptySkills;
impl Skills for EmptySkills {
    fn search(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn load(&self, _: &str) -> Result<String> {
        bail!("no skills configured")
    }
}
pub struct Reader;
impl Provider for Reader {
    fn reply(
        &mut self,
        _: &str,
        h: &[Message],
        _: &[Value],
        _: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        match h.last() {
            Some(Message::User(_)) => Ok((
                String::new(),
                vec![Call {
                    id: "read-1".into(),
                    name: "read".into(),
                    args: json!({"path":"hello.txt"}),
                }],
            )),
            Some(Message::Result { text, .. }) => {
                emit(Event::Text(text.clone()));
                Ok((text.clone(), vec![]))
            }
            _ => bail!("unexpected transcript"),
        }
    }
}
pub fn read_fixture<W: Workspace, S: Skills>(
    workspace: W,
    skills: S,
    store: &mut dyn Store,
) -> Result<String> {
    let coding = panda_coding::Coding { workspace, skills };
    turn(
        &mut Reader,
        store,
        &coding,
        "read the greeting".into(),
        &Cancel::default(),
        &mut |_| {},
    )?;
    match store.load()?.history.pop() {
        Some(Message::Assistant { text, calls }) if calls.is_empty() => Ok(text),
        _ => bail!("missing final answer"),
    }
}
