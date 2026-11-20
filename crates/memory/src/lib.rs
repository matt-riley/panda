use anyhow::{Result, bail, ensure};
use panda_coding::{Skills, Workspace, field};
use panda_core::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
mod sessions;
pub use sessions::MemoryStore;
pub struct Files(pub BTreeMap<String, String>);
impl Workspace for Files {
    fn invoke(&self, name: &str, args: &Value, _: &Cancel) -> Result<String> {
        if name == "instructions" {
            let file = std::path::Path::new(field(args, "path")?);
            let dirs: Vec<_> = file
                .parent()
                .into_iter()
                .flat_map(std::path::Path::ancestors)
                .collect();
            return Ok(dirs
                .into_iter()
                .rev()
                .filter_map(|d| {
                    let p = d.join("AGENTS.md");
                    self.0
                        .get(p.to_str()?)
                        .map(|text| format!("Instructions from {}:\n{text}", p.display()))
                })
                .collect::<Vec<_>>()
                .join("\n"));
        }
        ensure!(name == "read", "unknown operation");
        let text = self
            .0
            .get(field(args, "path")?)
            .ok_or_else(|| anyhow::anyhow!("missing file"))?;
        panda_coding::read_range(text.as_bytes(), args)
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
