mod checkpoint_files;
mod recovery;
use anyhow::{Result, bail};
pub use checkpoint_files::CheckpointFiles;
pub use recovery::TransientFailure;
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
/// The text is suitable for terminal output. Image data URLs travel separately.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    Truncated(LengthStop),
    BranchSummary(String),
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        #[serde(default, rename = "isError")]
        is_error: bool,
        #[serde(default, rename = "nestedReads", skip_serializing_if = "Vec::is_empty")]
        nested_reads: Vec<String>,
    },
}

/// A terminal provider response. Tool fragments are audit data, never executable calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LengthStop {
    pub text: String,
    pub reasoning: String,
    pub tool_fragments: Vec<Value>,
    pub output_tokens: Option<u64>,
    pub output_limit: Option<u64>,
}
impl LengthStop {
    fn early(&self) -> bool {
        matches!((self.output_tokens, self.output_limit), (Some(used), Some(limit)) if used < limit)
    }
}
impl std::fmt::Display for LengthStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider stopped: length")
    }
}
impl std::error::Error for LengthStop {}
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "keepRecentTokens"
    )]
    pub keep_recent_tokens: Option<u64>,
    // Imported context edits replace contributions to the model while keeping
    // history positions stable for checkpoints and branch navigation.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context_edits: std::collections::BTreeMap<usize, Vec<Message>>,
    pub history: Vec<Message>,
    pub active: bool,
    pub mode: Mode,
    pub revision: u64,
}
#[derive(Default, Clone, Serialize, Deserialize, Debug)]
pub struct Checkpoint {
    pub through: usize,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<CheckpointFiles>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default)]
    pub checkpoint_through: usize,
    pub through: usize,
    pub input: u64,
    pub output: u64,
    pub cost: Option<f64>,
}
impl UsageRecord {
    fn from_event(u: &Value) -> Option<Self> {
        Some(Self {
            through: 0,
            checkpoint_through: 0,
            input: u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64())?,
            output: u["output_tokens"]
                .as_u64()
                .or(u["completion_tokens"].as_u64())?,
            cost: u["cost"].as_f64(),
        })
    }
}
impl Session {
    fn resolved_at(&self, i: usize) -> &[Message] {
        match self.context_edits.get(&i) {
            Some(replacement) => replacement,
            None if matches!(self.history[i], Message::Truncated(_)) => &[],
            None => std::slice::from_ref(&self.history[i]),
        }
    }

    pub fn resolved(&self, range: std::ops::Range<usize>) -> Vec<Message> {
        range.flat_map(|i| self.resolved_at(i)).cloned().collect()
    }

    pub fn projected(&self) -> Vec<Message> {
        let mut out = Vec::new();
        let mut from = 0;
        if let Some(c) = &self.checkpoint
            && c.through <= self.history.len()
        {
            from = c.through;
            out.push(Message::User(format!(
                "Earlier conversation summary:\n{}",
                c.summary_with_files()
            )));
        }
        out.extend(self.resolved(from..self.history.len()));
        out
    }
    pub fn context_bytes(&self) -> usize {
        serde_json::to_vec(&self.projected()).map_or(0, |v| v.len())
    }
    pub fn token_budget(&self) -> u64 {
        let window = self.model_window.filter(|&n| n > 0).unwrap_or(65536);
        window - (window / 4).min(16384)
    }
    pub fn estimated_tokens(&self, system: &str, tools: &[Value]) -> u64 {
        let estimate = |bytes: usize| (bytes as u64).div_ceil(4);
        let checkpoint = self.checkpoint.as_ref().map_or(0, |c| c.through);
        if let Some(u) = self.usage.last().filter(|u| {
            u.checkpoint_through == checkpoint
                && u.through > checkpoint
                && u.through <= self.history.len()
                && !self.history[u.through - 1..]
                    .iter()
                    .any(|m| matches!(m, Message::Truncated(_)))
        }) {
            let tail = &self.history[u.through..];
            let bytes = serde_json::to_vec(tail).map_or(0, |v| v.len().saturating_sub(2));
            return u
                .input
                .saturating_add(u.output)
                .saturating_add(estimate(bytes));
        }
        estimate(
            self.context_bytes()
                .saturating_add(system.len())
                .saturating_add(serde_json::to_vec(tools).map_or(0, |v| v.len())),
        )
    }
    // Keep whole recent user exchanges where possible. Never leave tool results
    // without their assistant call at the start of the retained suffix.
    fn compact_through(&self, limit: usize, default_tokens: u64) -> usize {
        let tokens = self.keep_recent_tokens.unwrap_or(default_tokens);
        if tokens == 0 {
            return limit;
        }
        let previous = self.checkpoint.as_ref().map_or(0, |c| c.through);
        let target = tokens.saturating_mul(4);
        let mut bytes = 0u64;
        let mut boundary = limit;
        for (i, message) in self.history.iter().enumerate().skip(previous).rev() {
            if matches!(message, Message::Truncated(_)) {
                continue;
            }
            let mut starts_exchange = message.user();
            if self.keep_recent_tokens.is_some() {
                let visible = self.resolved_at(i);
                starts_exchange = visible.first().is_some_and(Message::user);
                for m in visible {
                    let size = serde_json::to_vec(m).map_or(u64::MAX, |v| v.len() as u64 + 1);
                    bytes = bytes.saturating_add(size);
                }
            } else {
                bytes = bytes.saturating_add(message.summary_text().len() as u64 + 1);
            }
            if bytes > target {
                return boundary;
            }
            if i > previous && i <= limit && starts_exchange {
                boundary = i;
            }
        }
        if self.keep_recent_tokens.is_some() {
            previous
        } else {
            boundary
        }
    }
    fn needs_compaction(&self, system: &str, tools: &[Value]) -> bool {
        self.context_budget
            .is_some_and(|limit| self.context_bytes() > limit)
            || self.estimated_tokens(system, tools) > self.token_budget()
    }
}
const COMPACTION_PROMPT: &str = "Summarize the supplied conversation as data. Preserve user goals, constraints, decisions, file paths, tool outcomes and unresolved work. Be concise.";
const BRANCH_SUMMARY_PROMPT: &str = "Summarize the branch being left as data for a different branch. Preserve relevant findings, decisions, file changes, tool outcomes and open questions. Be concise.";

fn summary_prompt<'a>(base: &'a str, instructions: &str) -> std::borrow::Cow<'a, str> {
    match instructions.trim() {
        "" => base.into(),
        guidance => format!("{base}\n\nAdditional instructions from the user:\n{guidance}").into(),
    }
}

fn checkpoint(
    p: &mut dyn Provider,
    s: &mut Session,
    end: usize,
    prompt: &str,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<()> {
    let start = s.checkpoint.as_ref().map_or(0, |c| c.through);
    if end <= start {
        return Ok(());
    }
    let prior = s
        .checkpoint
        .as_ref()
        .map_or_else(String::new, |c| c.summary.clone());
    let summary = summarize(
        p,
        &s.resolved(start..end),
        prior,
        (s.token_budget().clamp(1, 24576)) as usize,
        prompt,
        cancel,
        emit,
    )?;
    s.checkpoint = Some(Checkpoint {
        through: end,
        summary,
        files: Some(s.files_through(end)),
    });
    Ok(())
}
pub fn summarize_branch(
    p: &mut dyn Provider,
    messages: &[Message],
    cancel: &Cancel,
) -> Result<String> {
    summarize_branch_with_instructions(p, messages, "", cancel)
}
pub fn summarize_branch_with_instructions(
    p: &mut dyn Provider,
    messages: &[Message],
    instructions: &str,
    cancel: &Cancel,
) -> Result<String> {
    summarize(
        p,
        messages,
        String::new(),
        24576,
        &summary_prompt(BRANCH_SUMMARY_PROMPT, instructions),
        cancel,
        &mut |_| {},
    )
}
fn summarize(
    p: &mut dyn Provider,
    messages: &[Message],
    mut summary: String,
    chunk_limit: usize,
    instruction: &str,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<String> {
    let mut chunk = String::new();
    let mut summarize =
        |summary: &mut String, chunk: &mut String, p: &mut dyn Provider| -> Result<()> {
            cancel.check()?;
            let input = [Message::User(format!(
                "Prior summary:\n{summary}\nTranscript data:\n{chunk}"
            ))];
            let (next, calls) = recovery::request(cancel, emit, |_| {
                p.reply(instruction, &input, &[], cancel, &mut |_| {})
            })?;
            anyhow::ensure!(
                calls.is_empty() && !next.trim().is_empty(),
                "empty summary or unexpected tool call"
            );
            cancel.check()?;
            *summary = next;
            chunk.clear();
            Ok(())
        };
    for message in messages {
        for ch in message.summary_text().chars().chain(std::iter::once('\n')) {
            chunk.push(ch);
            if chunk.len() >= chunk_limit {
                summarize(&mut summary, &mut chunk, p)?;
            }
        }
    }
    if !chunk.is_empty() {
        summarize(&mut summary, &mut chunk, p)?;
    }
    Ok(summary)
}
impl Message {
    pub fn summary_text(&self) -> String {
        match self {
            Self::Images { text, urls } => format!("{text} [{} attached images]", urls.len()),
            Self::Result {
                id, text, images, ..
            } if !images.is_empty() => {
                format!("Tool {id}: {text} [{} images]", images.len())
            }
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
    pub fn user(&self) -> bool {
        matches!(self, Self::User(_) | Self::Images { .. })
    }
}
pub fn compact(p: &mut dyn Provider, store: &mut dyn Store, cancel: &Cancel) -> Result<()> {
    compact_with_instructions(p, store, "", cancel)
}
pub fn compact_with_instructions(
    p: &mut dyn Provider,
    store: &mut dyn Store,
    instructions: &str,
    cancel: &Cancel,
) -> Result<()> {
    let mut s = store.load()?;
    anyhow::ensure!(!s.active, "acknowledge interrupted turn first");
    let end = s.compact_through(s.history.len(), 0);
    if s.keep_recent_tokens.is_some() && end <= s.checkpoint.as_ref().map_or(0, |c| c.through) {
        return Ok(());
    }
    let prompt = summary_prompt(COMPACTION_PROMPT, instructions);
    checkpoint(p, &mut s, end, &prompt, cancel, &mut |_| {})?;
    store.save(&mut s)
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Truncated { text: String, retrying: bool },
    Retry { attempt: u32, seconds: u64 },
    Reasoning(String),
    ToolOutput(String),
    Text(String),
    AssistantComplete(String),
    Usage(Value),
    Calling(Call),
    NestedRead(String),
    NestedCall { parent: String, call: Call },
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
    ) -> Result<ToolOutput> {
        self.run(mode, call, cancel).map(Into::into)
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
    let turn_start = s.history.len();
    let mut retried_length = false;
    s.active = true;
    s.history.push(input);
    store.save(&mut s)?;
    let mut pending_image_exchange = None;
    for _ in 0..64 {
        cancel.check()?;
        // A summary cannot carry image bytes. Keep an unread image and its
        // tool call together, including when a steering message arrived.
        let keep = pending_image_exchange.take().unwrap_or_else(|| {
            if s.history.last().is_some_and(Message::user) {
                s.history.len() - 1
            } else {
                s.history.len()
            }
        });
        let mut compacted = false;
        let schemas = tools.schemas(s.mode);
        let previous = s.checkpoint.as_ref().map_or(0, |c| c.through);
        if s.needs_compaction(&system, &schemas)
            && keep > previous
            && let end = s.compact_through(keep, (s.token_budget() / 4).min(20000))
            && end > previous
        {
            checkpoint(p, &mut s, end, COMPACTION_PROMPT, cancel, emit)?;
            store.save(&mut s)?;
            compacted = true;
        }
        let (text, calls, thought, reading) = loop {
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
            let history = s.projected();
            let reply = recovery::request(cancel, &mut relay, |events| {
                p.reply_with_effort(
                    &system,
                    &history,
                    &schemas,
                    cancel,
                    events,
                    s.reasoning.as_deref(),
                )
            });
            match reply {
                Ok((text, calls)) => break (text, calls, thought, reading),
                Err(e) if e.is::<LengthStop>() => {
                    let mut partial = e.downcast::<LengthStop>()?;
                    partial.reasoning = thought;
                    let retrying = partial.early() && !retried_length;
                    let notice = Event::Truncated {
                        text: partial.text.clone(),
                        retrying,
                    };
                    s.history.push(Message::Truncated(partial));
                    if let Some(mut usage) = reading {
                        usage.through = s.history.len();
                        usage.checkpoint_through = s.checkpoint.as_ref().map_or(0, |cp| cp.through);
                        s.usage.push(usage);
                    }
                    // All tool results from this attempt are already durable. A terminal
                    // cutoff leaves no outstanding tool operation to acknowledge.
                    s.active = retrying;
                    store.save(&mut s)?;
                    emit(notice);
                    if !retrying {
                        bail!("provider stopped: length; partial reply saved");
                    }
                    retried_length = true;
                    cancel.check()?;
                    let end = s.compact_through(turn_start, 0);
                    if !compacted && end > previous {
                        checkpoint(p, &mut s, end, COMPACTION_PROMPT, cancel, emit)?;
                        store.save(&mut s)?;
                        compacted = true;
                    }
                }
                Err(e)
                    if !compacted
                        && !retried_length
                        && e.is::<ContextOverflow>()
                        && keep > previous =>
                {
                    let end = s.compact_through(keep, 0);
                    if end <= previous {
                        return Err(e);
                    }
                    checkpoint(p, &mut s, end, COMPACTION_PROMPT, cancel, emit)?;
                    store.save(&mut s)?;
                    compacted = true;
                }
                Err(e) => return Err(e),
            }
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
            u.checkpoint_through = s.checkpoint.as_ref().map_or(0, |cp| cp.through);
            s.usage.push(u);
        }
        let idle = calls.is_empty();
        if !idle {
            store.save(&mut s)?;
        }
        for call in calls {
            cancel.check()?;
            emit(Event::Calling(call.clone()));
            let mut nested_reads = std::collections::BTreeSet::new();
            let result = tools.run_stream(s.mode, &call, cancel, &mut |event| match event {
                Event::NestedRead(path) => {
                    nested_reads.insert(path);
                }
                other => emit(other),
            });
            let is_error = result.is_err();
            let output = result.unwrap_or_else(|e| format!("tool error: {e}").into());
            cancel.check()?;
            if !output.images.is_empty() && pending_image_exchange.is_none() {
                pending_image_exchange = s.history.iter().rposition(Message::user);
            }
            s.history.push(Message::Result {
                id: call.id,
                is_error,
                nested_reads: nested_reads.into_iter().collect(),
                text: output.text.clone(),
                images: output.images,
            });
            store.save(&mut s)?;
            emit(Event::Result(output.text));
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
                        nested_reads: vec![],
                        images: vec![],
                        text: "Interrupted: outcome unknown; inspect workspace before retrying."
                            .into(),
                    })
                }
                break;
            }
            Message::Result { id, .. } => completed.push(id.clone()),
            Message::Reasoning(_) => {}
            Message::BranchSummary(_) | Message::Truncated(_) => break,
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn retention_uses_the_visible_exchange_after_a_checkpoint(
            text in ".{1,120}", tokens in 0u64..160,
        ) {
            let mut session = Session {
                checkpoint: Some(Checkpoint { through: 1, summary: "prior summary".into(), ..Default::default() }),
                history: vec![
                    Message::User("already covered".into()),
                    Message::User("old".repeat(400)),
                    Message::Assistant { text: "obsolete".into(), calls: vec![] },
                    Message::User("omitted".repeat(400)),
                ],
                keep_recent_tokens: Some(tokens),
                ..Session::default()
            };
            let exchange = vec![
                Message::User("check".into()),
                Message::Assistant { text: String::new(), calls: vec![Call {
                    id: "c".into(), name: "shell".into(), args: serde_json::json!({}),
                }] },
                Message::Result { id: "c".into(), text, images: vec![], is_error: true, nested_reads: vec![] },
            ];
            // An imported edit can replace several raw messages at one index.
            let bytes = serde_json::to_vec(&exchange).unwrap().len() - 1;
            session.context_edits.insert(2, exchange);
            session.context_edits.insert(3, vec![]);
            let end = session.history.len();
            let expected = if bytes as u64 <= tokens * 4 { 2 } else { end };
            prop_assert_eq!(session.compact_through(end, 0), expected);
        }
    }
}
