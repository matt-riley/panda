use crate::Cancel;
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub enum Kind {
    Moment,
    Goal,
    Score,
    Span,
    Faithfulness,
    Coverage,
}
pub struct Question {
    pub kind: Kind,
    pub state: Value,
    pub questions: Map<String, Value>,
    pub scoring_tool: bool,
}
impl Question {
    pub fn new(kind: Kind, state: Value) -> Self {
        Self {
            kind,
            state,
            questions: Map::new(),
            scoring_tool: false,
        }
    }
    pub fn probability(&mut self, key: &str, instruction: &str) {
        self.questions.insert(
            key.into(),
            json!({"type":"noul","instructions":instruction}),
        );
    }
    pub fn choose(&mut self, key: &str, instruction: &str, criteria: Value) {
        self.questions.insert(
            key.into(),
            json!({"type":"choice","instructions":instruction,"criteria":criteria}),
        );
    }
}
#[derive(Default, Clone, Debug)]
pub struct JudgeStats {
    pub judged: usize,
    pub failed: usize,
    pub reused: usize,
    pub usage: Vec<Value>,
}
pub trait Judge {
    fn available(&self) -> bool {
        true
    }
    fn ask(&mut self, q: Question, timeout: Duration, cancel: &Cancel) -> Result<Value>;
    fn stats(&self) -> JudgeStats {
        JudgeStats::default()
    }
    fn mask(&self, text: &str) -> String {
        text.to_owned()
    }
    fn bind(&mut self, _scope: Option<String>) {}
}
pub fn probability(answers: &Value, key: &str) -> Option<f64> {
    let a = &answers[key];
    (a["type"] == "noul")
        .then(|| a["noul"].as_f64())
        .flatten()
        .filter(|n| (0.0..=1.0).contains(n))
}
pub fn choice<'a>(answers: &'a Value, key: &str) -> Option<&'a str> {
    let a = &answers[key];
    (a["type"] == "choice")
        .then(|| a["choice"].as_str())
        .flatten()
}
pub trait Clock {
    fn now(&self) -> Duration;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
    }
}
pub(crate) fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        head + "…"
    } else {
        head
    }
}
