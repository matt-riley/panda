use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default, Clone)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release)
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release)
    }
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Acquire) {
            bail!("cancelled")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub args: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    User(String),
    Assistant {
        text: String,
        calls: Vec<Call>,
    },
    Result {
        id: String,
        text: String,
        #[serde(default, rename = "isError")]
        is_error: bool,
    },
}
#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum Mode {
    Plan,
    #[default]
    Code,
}
#[derive(Default, Clone, Serialize, Deserialize, Debug)]
pub struct Session {
    pub history: Vec<Message>,
    pub active: bool,
    pub mode: Mode,
    pub revision: u64,
}
pub enum Event {
    Text(String),
    Usage(Value),
    Calling(Call),
    Result(String),
}
pub type Emit<'a> = dyn FnMut(Event) + 'a;
pub trait Provider {
    fn reply(
        &mut self,
        system: &str,
        history: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)>;
}
pub trait Store {
    fn load(&self) -> Result<Session>;
    fn save(&mut self, s: &mut Session) -> Result<()>;
}
pub trait Toolset {
    fn system(&self) -> Result<String>;
    fn schemas(&self, mode: Mode) -> Vec<Value>;
    fn run(&self, mode: Mode, call: &Call, cancel: &Cancel) -> Result<String>;
}

pub fn turn(
    p: &mut dyn Provider,
    store: &mut dyn Store,
    tools: &dyn Toolset,
    input: String,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<()> {
    let mut s = store.load()?;
    anyhow::ensure!(
        !s.active,
        "interrupted turn: acknowledge unknown outcomes before continuing"
    );
    cancel.check()?;
    let system = tools.system()?;
    s.active = true;
    s.history.push(Message::User(input));
    store.save(&mut s)?;
    for _ in 0..64 {
        cancel.check()?;
        let (text, calls) = p.reply(&system, &s.history, &tools.schemas(s.mode), cancel, emit)?;
        cancel.check()?;
        let mut ids = std::collections::HashSet::new();
        anyhow::ensure!(
            calls.iter().all(|c| !c.id.is_empty() && ids.insert(&c.id)),
            "duplicate or missing tool ID"
        );
        s.history.push(Message::Assistant {
            text,
            calls: calls.clone(),
        });
        if calls.is_empty() {
            s.active = false;
            return store.save(&mut s);
        }
        store.save(&mut s)?;
        for call in calls {
            cancel.check()?;
            emit(Event::Calling(call.clone()));
            let result = tools.run(s.mode, &call, cancel);
            let is_error = result.is_err();
            let output = result.unwrap_or_else(|e| format!("tool error: {e}"));
            cancel.check()?;
            s.history.push(Message::Result {
                id: call.id,
                is_error,
                text: output.clone(),
            });
            store.save(&mut s)?;
            emit(Event::Result(output));
        }
    }
    bail!("turn limit reached")
}

// Close an interrupted exchange without dispatching its outstanding calls.
pub fn acknowledge(store: &mut dyn Store) -> Result<()> {
    let mut s = store.load()?;
    if !s.active {
        return Ok(());
    }
    let mut completed = Vec::new();
    for msg in s.history.iter().rev() {
        match msg {
            Message::Assistant { calls, .. } => {
                let pending: Vec<_> = calls
                    .iter()
                    .filter(|c| !completed.contains(&c.id))
                    .map(|c| c.id.clone())
                    .collect();
                for id in pending {
                    s.history.push(Message::Result {
                        id,
                        is_error: true,
                        text: "Interrupted: outcome unknown; inspect workspace before retrying."
                            .into(),
                    })
                }
                break;
            }
            Message::Result { id, .. } => completed.push(id.clone()),
            Message::User(_) => break,
        }
    }
    s.active = false;
    store.save(&mut s)
}
