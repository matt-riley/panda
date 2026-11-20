//! Session operations shared by terminal, RPC and embedded Rust hosts.
use super::*;
use anyhow::{Context, ensure};
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
pub struct Delivery {
    pub single_steer: bool,
    pub single_follow: bool,
}
pub struct Queue<T = String> {
    pub steer: Vec<T>,
    pub follow: Vec<T>,
    pub delivery: Delivery,
}
impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self {
            steer: Vec::new(),
            follow: Vec::new(),
            delivery: Delivery::default(),
        }
    }
}
impl<T> Queue<T> {
    pub fn take(&mut self, idle: bool) -> Vec<T> {
        let (queue, single) = if !self.steer.is_empty() {
            (&mut self.steer, self.delivery.single_steer)
        } else if idle {
            (&mut self.follow, self.delivery.single_follow)
        } else {
            return Vec::new();
        };
        let count = if single {
            1.min(queue.len())
        } else {
            queue.len()
        };
        queue.drain(..count).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub parent: Option<i64>,
    pub message: Option<Message>,
    pub label: Option<String>,
    pub active: bool,
}
/// Session IDs belong to one workspace. Stores must preserve the active handle on errors.
pub trait SessionHost: Store + Send {
    fn session_id(&self) -> i64;
    fn entries(&mut self) -> Result<Vec<Entry>>;
    fn switch(&mut self, id: Option<i64>) -> Result<()>;
    fn create_session(&mut self, seed: Session) -> Result<()>;
    fn switch_reference(&mut self, reference: &str) -> Result<()> {
        let id = reference
            .strip_prefix("panda-session:")
            .unwrap_or(reference)
            .parse()
            .context("expected a Panda session ID")?;
        self.switch(Some(id))
    }
    fn fork_turn(&mut self, turn: Option<usize>, name: &str) -> Result<()>;
    fn fork_prompt(&mut self, entry: i64) -> Result<Message>;
    fn navigate_entry(&mut self, entry: i64) -> Result<Option<Message>>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}
impl ShellOutcome {
    pub fn context(&self, command: &str) -> String {
        format!("Ran `{command}`\n```\n{}\n```", self.output)
    }
}
#[derive(Debug)]
pub struct ShellFailure {
    pub outcome: ShellOutcome,
    pub diagnostic: String,
}
impl std::fmt::Display for ShellFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic)
    }
}
impl std::error::Error for ShellFailure {}
#[derive(Default)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub read: u64,
    pub write: u64,
    pub cost: f64,
    pub unknown: usize,
}
impl From<&Session> for UsageTotals {
    fn from(s: &Session) -> Self {
        let accounted: std::collections::BTreeSet<_> = s
            .attempts
            .iter()
            .filter(|a| a.purpose == "turn" && a.usage.is_some())
            .map(|a| a.through)
            .collect();
        let legacy = s.usage.iter().filter(|u| !accounted.contains(&u.through));
        let readings = legacy
            .clone()
            .chain(s.attempts.iter().filter_map(|a| a.usage.as_ref()));
        let mut input = 0;
        let mut output = 0;
        let mut read = 0;
        let mut write = 0;
        let cost: f64 = legacy
            .clone()
            .filter_map(|u| u.cost)
            .chain(s.attempts.iter().filter_map(|a| a.cost))
            .sum();
        for u in readings {
            input += u.input;
            output += u.output;
            read += u.cache_read.unwrap_or(0);
            write += u.cache_write.unwrap_or(0);
        }
        let unknown = s
            .attempts
            .iter()
            .filter(|a| a.cost_source == "unknown")
            .count()
            + legacy.filter(|u| u.cost.is_none()).count();
        Self {
            input,
            output,
            read,
            write,
            cost,
            unknown,
        }
    }
}
pub struct SessionSdk<'a> {
    pub host: &'a mut dyn SessionHost,
}
impl SessionSdk<'_> {
    pub fn state(&self) -> Result<Session> {
        self.host.load()
    }
    pub fn messages(&self) -> Result<Vec<Message>> {
        Ok(self.state()?.history)
    }
    pub fn new_session(&mut self, parent: Option<&str>) -> Result<()> {
        let current = self.host.session_id();
        let state = self.state()?;
        let mut seed = Session {
            model: state.model,
            model_window: state.model_window,
            keep_recent_tokens: state.keep_recent_tokens,
            reasoning: state.reasoning,
            disable_auto_compaction: state.disable_auto_compaction,
            disable_auto_retry: state.disable_auto_retry,
            ..Session::default()
        };
        if let Some(parent) = parent {
            self.host.switch_reference(parent)?;
            seed.parent = Some(self.host.session_id());
        }
        if let Err(error) = self.host.create_session(seed) {
            self.host.switch(Some(current))?;
            return Err(error);
        }
        Ok(())
    }
    pub fn select_model(&mut self, model: ModelId, effort: Option<Option<String>>) -> Result<()> {
        self.select_model_with_limits(model, effort, None)
    }
    pub fn select_model_with_limits(
        &mut self,
        model: ModelId,
        effort: Option<Option<String>>,
        limits: Option<(Option<u64>, Option<u64>)>,
    ) -> Result<()> {
        ensure!(
            !model.provider.is_empty() && !model.model.is_empty(),
            "missing provider or model"
        );
        let mut session = self.state()?;
        ensure!(!session.active, "acknowledge interrupted turn first");
        if let Some((window, keep)) = limits {
            session.model_window = window;
            session.keep_recent_tokens = keep;
        } else if session.model.as_ref() != Some(&model) {
            session.model_window = None;
        }
        session.model = Some(model);
        if let Some(effort) = effort {
            session.reasoning = effort;
        }
        self.host.save(&mut session)
    }
    pub fn compact(&mut self, p: &mut dyn Provider, instructions: &str, k: &Cancel) -> Result<()> {
        compact_with_instructions(p, self.host, instructions, k)
    }
    pub fn prompt<T: Into<Message>>(
        &mut self,
        p: &mut dyn Provider,
        tools: &dyn Toolset,
        input: Message,
        k: &Cancel,
        emit: &mut Emit,
        queue: &Mutex<Queue<T>>,
    ) -> Result<()> {
        drive_turn(p, self.host, tools, Some(input), k, emit, &mut |idle| {
            queue
                .lock()
                .unwrap()
                .take(idle)
                .into_iter()
                .map(Into::into)
                .collect()
        })
    }
    pub fn retry<T: Into<Message>>(
        &mut self,
        p: &mut dyn Provider,
        tools: &dyn Toolset,
        k: &Cancel,
        emit: &mut Emit,
        queue: &Mutex<Queue<T>>,
    ) -> Result<()> {
        drive_turn(p, self.host, tools, None, k, emit, &mut |idle| {
            queue
                .lock()
                .unwrap()
                .take(idle)
                .into_iter()
                .map(Into::into)
                .collect()
        })
    }
    pub fn shell(
        &mut self,
        tools: &dyn Toolset,
        command: &str,
        exclude: bool,
        k: &Cancel,
        emit: &mut Emit,
    ) -> Result<ShellOutcome> {
        let mut session = self.state()?;
        ensure!(!session.active, "acknowledge interrupted turn first");
        let call = Call {
            id: "user-bash".into(),
            name: "shell".into(),
            args: serde_json::json!({"command":command}),
        };
        let outcome = match tools.run(&session, &call, k, emit) {
            Ok((out, writes)) => {
                ensure!(writes.is_empty(), "unexpected shell store writes");
                match out.details {
                    Some(ToolDetails::Shell { outcome }) => outcome,
                    _ => ShellOutcome {
                        output: out.text,
                        exit_code: Some(0),
                        cancelled: false,
                        truncated: false,
                        full_output_path: None,
                    },
                }
            }
            Err(error) => error.downcast::<ShellFailure>()?.outcome,
        };
        session.history.push(Message::Bash {
            command: command.to_owned(),
            outcome: outcome.clone(),
            exclude,
        });
        self.host.save(&mut session)?;
        Ok(outcome)
    }
}
impl Session {
    /// Retain a branch prefix with its matching usage, model and compaction metadata.
    pub fn truncate_branch(&mut self, end: usize) -> Result<()> {
        ensure!(end <= self.history.len(), "unknown history boundary");
        if end < self.history.len() {
            self.preferences.retain(|&depth, _| depth <= end);
            (self.model, self.reasoning) = self
                .preferences
                .last_key_value()
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
        }
        self.responders.retain(|&i, _| i < end);
        self.context_edits.retain(|&i, _| i < end);
        self.attempts.retain(|a| a.through <= end);
        self.history.truncate(end);
        self.usage.retain(|u| u.through <= end);
        if self.checkpoint.as_ref().is_some_and(|c| c.through > end) {
            self.checkpoint = None;
        }
        Ok(())
    }
    pub fn after_turn(&self, n: usize) -> Result<usize> {
        if n == 0 {
            return Ok(0);
        }
        Ok(self
            .history
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m,Message::Assistant{calls,..} if calls.is_empty()))
            .nth(n - 1)
            .context("unknown completed turn")?
            .0
            + 1)
    }
}
