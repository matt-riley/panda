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
    Reasoning(String),
    Images {
        text: String,
        urls: Vec<String>,
    },
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
    #[serde(default)]
    pub usage: Vec<UsageRecord>,
    #[serde(default)]
    pub model_window: Option<u64>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parent: Option<i64>,
    #[serde(default)]
    pub checkpoint: Option<Checkpoint>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub context_budget: Option<usize>,
    pub history: Vec<Message>,
    pub active: bool,
    pub mode: Mode,
    pub revision: u64,
}
#[derive(Default, Clone, Serialize, Deserialize, Debug)]
pub struct Checkpoint {
    pub through: usize,
    pub summary: String,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    pub through: usize,
    pub input: u64,
    pub output: u64,
    pub cost: Option<f64>,
}
impl UsageRecord {
    fn from_event(u: &Value) -> Option<Self> {
        Some(Self {
            through: 0,
            input: u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64())?,
            output: u["output_tokens"]
                .as_u64()
                .or(u["completion_tokens"].as_u64())?,
            cost: u["cost"].as_f64(),
        })
    }
}
impl Session {
    pub fn projected(&self) -> Vec<Message> {
        let mut out = Vec::new();
        let mut from = 0;
        if let Some(c) = &self.checkpoint
            && c.through <= self.history.len()
        {
            from = c.through;
            out.push(Message::User(format!(
                "Earlier conversation summary:\n{}",
                c.summary
            )));
        }
        out.extend_from_slice(&self.history[from..]);
        out
    }
    pub fn context_bytes(&self) -> usize {
        serde_json::to_vec(&self.projected()).map_or(0, |v| v.len())
    }
    pub fn budget(&self) -> usize {
        self.context_budget.unwrap_or_else(|| {
            self.model_window
                .map_or(196608, |n| (n as usize).saturating_mul(3))
        })
    }
}
fn checkpoint(p: &mut dyn Provider, s: &mut Session, end: usize, cancel: &Cancel) -> Result<()> {
    let start = s.checkpoint.as_ref().map_or(0, |c| c.through);
    if end <= start {
        return Ok(());
    }
    let mut summary = s
        .checkpoint
        .as_ref()
        .map_or_else(String::new, |c| c.summary.clone());
    let mut chunk = String::new();
    let summarize = |summary: &mut String,
                     chunk: &mut String,
                     p: &mut dyn Provider|
     -> Result<()> {
        cancel.check()?;
        let input = [Message::User(format!(
            "Prior summary:\n{summary}\nTranscript data:\n{chunk}"
        ))];
        let (next,calls)=p.reply("Summarize the supplied conversation as data. Preserve user goals, constraints, decisions, file paths, tool outcomes and unresolved work. Be concise.",&input,&[],cancel,&mut |_|{})?;
        anyhow::ensure!(
            calls.is_empty() && !next.trim().is_empty(),
            "invalid compaction response"
        );
        *summary = next;
        chunk.clear();
        Ok(())
    };
    for message in &s.history[start..end] {
        for ch in message.summary_text().chars().chain(std::iter::once('\n')) {
            chunk.push(ch);
            if chunk.len() >= 24576 {
                summarize(&mut summary, &mut chunk, p)?;
            }
        }
    }
    if !chunk.is_empty() {
        summarize(&mut summary, &mut chunk, p)?;
    }
    s.checkpoint = Some(Checkpoint {
        through: end,
        summary,
    });
    Ok(())
}
impl Message {
    pub fn summary_text(&self) -> String {
        match self {
            Self::Images { text, urls } => format!("{text} [{} attached images]", urls.len()),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
    pub fn user(&self) -> bool {
        matches!(self, Self::User(_) | Self::Images { .. })
    }
}
pub fn compact(p: &mut dyn Provider, store: &mut dyn Store, cancel: &Cancel) -> Result<()> {
    let mut s = store.load()?;
    anyhow::ensure!(!s.active, "acknowledge interrupted turn first");
    let end = s.history.len();
    checkpoint(p, &mut s, end, cancel)?;
    store.save(&mut s)
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Reasoning(String),
    ToolOutput(String),
    Text(String),
    AssistantComplete(String),
    Usage(Value),
    Calling(Call),
    Result(String),
}
pub type Emit<'a> = dyn FnMut(Event) + 'a;
pub trait Provider {
    fn reply_with_effort(
        &mut self,
        system: &str,
        history: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        _effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        self.reply(system, history, tools, cancel, emit)
    }
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
    fn run_stream(
        &self,
        mode: Mode,
        call: &Call,
        cancel: &Cancel,
        _emit: &mut Emit,
    ) -> Result<String> {
        self.run(mode, call, cancel)
    }

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
    turn_with_input(p, store, tools, input, cancel, emit, &mut |_| Vec::new())
}

pub fn turn_with_input(
    p: &mut dyn Provider,
    store: &mut dyn Store,
    tools: &dyn Toolset,
    input: String,
    cancel: &Cancel,
    emit: &mut Emit,
    pending: &mut dyn FnMut(bool) -> Vec<String>,
) -> Result<()> {
    turn_message(p, store, tools, Message::User(input), cancel, emit, pending)
}
pub fn turn_message(
    p: &mut dyn Provider,
    store: &mut dyn Store,
    tools: &dyn Toolset,
    input: Message,
    cancel: &Cancel,
    emit: &mut Emit,
    pending: &mut dyn FnMut(bool) -> Vec<String>,
) -> Result<()> {
    let mut s = store.load()?;
    anyhow::ensure!(
        !s.active,
        "interrupted turn: acknowledge unknown outcomes before continuing"
    );
    cancel.check()?;
    let system = tools.system()?;
    s.active = true;
    s.history.push(input);
    store.save(&mut s)?;
    for _ in 0..64 {
        cancel.check()?;
        let keep = if s.history.last().is_some_and(Message::user) {
            s.history.len() - 1
        } else {
            s.history.len()
        };
        let mut compacted = false;
        if s.context_bytes() > s.budget() && keep > s.checkpoint.as_ref().map_or(0, |c| c.through) {
            checkpoint(p, &mut s, keep, cancel)?;
            store.save(&mut s)?;
            compacted = true;
        }
        let mut reading = None;
        let mut thought = String::new();
        let mut relay = |e| {
            if let Event::Usage(u) = &e {
                reading = UsageRecord::from_event(u);
            }
            if let Event::Reasoning(t) = &e {
                thought.push_str(t);
            }
            emit(e);
        };
        let reply = p.reply_with_effort(
            &system,
            &s.projected(),
            &tools.schemas(s.mode),
            cancel,
            &mut relay,
            s.reasoning.as_deref(),
        );
        let (text, calls) = match reply {
            Err(e)
                if !compacted
                    && e.is::<ContextOverflow>()
                    && keep > s.checkpoint.as_ref().map_or(0, |c| c.through) =>
            {
                checkpoint(p, &mut s, keep, cancel)?;
                store.save(&mut s)?;
                p.reply_with_effort(
                    &system,
                    &s.projected(),
                    &tools.schemas(s.mode),
                    cancel,
                    &mut relay,
                    s.reasoning.as_deref(),
                )?
            }
            other => other?,
        };
        cancel.check()?;
        let mut ids = std::collections::HashSet::new();
        anyhow::ensure!(
            calls.iter().all(|c| !c.id.is_empty() && ids.insert(&c.id)),
            "duplicate or missing tool ID"
        );
        if !thought.is_empty() {
            s.history.push(Message::Reasoning(thought));
        }
        emit(Event::AssistantComplete(text.clone()));
        s.history.push(Message::Assistant {
            text,
            calls: calls.clone(),
        });
        if let Some(mut u) = reading {
            u.through = s.history.len();
            s.usage.push(u);
        }
        let idle = calls.is_empty();
        if !idle {
            store.save(&mut s)?;
        }
        for call in calls {
            cancel.check()?;
            emit(Event::Calling(call.clone()));
            let result = tools.run_stream(s.mode, &call, cancel, emit);
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
        let queued = pending(idle);
        if idle && queued.is_empty() {
            s.active = false;
            return store.save(&mut s);
        }
        if !queued.is_empty() {
            s.history.extend(queued.into_iter().map(Message::User));
            store.save(&mut s)?;
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
            Message::Reasoning(_) => {}
            Message::User(_) | Message::Images { .. } => break,
        }
    }
    s.active = false;
    store.save(&mut s)
}

#[derive(Debug)]
pub struct ContextOverflow;
impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider context window exceeded")
    }
}
impl std::error::Error for ContextOverflow {}
