//! TypeSafe System One client for source-backed judgments. No provider credential is sent as state.
use anyhow::{Context, Result, ensure};
use panda_core::{
    Call, Cancel, Emit, Message, Provider,
    native_compaction::{Judge, JudgeStats, Kind, Question, choice, probability},
};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

static SHAPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is:-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----)|\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|sk-[A-Za-z0-9_-]{20,}|(?:AKIA|ASIA)[A-Z0-9]{16}|xox[baprs]-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,})\b|(?i:\bBearer\s+[A-Za-z0-9._~+/=-]{8,}|[A-Za-z0-9_.-]*(?:key|token|secret|password|credential)[A-Za-z0-9_.-]*["']?\s*[:=]\s*["']?[^\s"',;<]{8,}|\b[a-z][a-z0-9+.-]*://[^/\s?#<>]+@)|data:[^\s,]+;base64,[a-zA-Z0-9+/=]+"#).unwrap()
});

// The API bounds serialized state at 60,000 characters. Retain every typed item and
// shrink only its text if an unusually large verification batch crosses that limit.
fn cap_strings(value: &mut Value, limit: usize) {
    match value {
        Value::String(s) if s.chars().count() > limit => {
            *s = s.chars().take(limit).collect();
            s.push('…');
        }
        Value::Array(items) => items.iter_mut().for_each(|v| cap_strings(v, limit)),
        Value::Object(items) => items.values_mut().for_each(|v| cap_strings(v, limit)),
        _ => {}
    }
}

type Memo = Arc<Mutex<HashMap<[u8; 32], Value>>>;
static SCOPES: LazyLock<Mutex<HashMap<String, Memo>>> = LazyLock::new(Default::default);
pub struct Jev {
    base: String,
    key: String,
    model: String,
    secrets: Vec<String>,
    client: reqwest::Client,
    memo: Memo,
    stats: JudgeStats,
}
impl Jev {
    pub fn new(base: &str, key: &str, model: &str, secrets: Vec<String>) -> Result<Self> {
        let url = reqwest::Url::parse(base)?;
        let loopback = url
            .host_str()
            .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "Jev needs HTTPS or loopback HTTP"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid Jev base URL"
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            key: key.trim().into(),
            model: model.into(),
            secrets,
            client,
            memo: Default::default(),
            stats: Default::default(),
        })
    }
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("TYPESAFE_API_KEY").unwrap_or_default();
        let secrets = std::env::vars()
            .filter(|(k, v)| {
                v.len() >= 8
                    && ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
                        .iter()
                        .any(|s| k.ends_with(s))
            })
            .map(|(_, v)| v)
            .collect();
        Self::new(
            &std::env::var("TYPESAFE_BASE_URL")
                .unwrap_or_else(|_| "https://api.typesafe.ai/v1".into()),
            &key,
            &std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| "jev-1.13.0".into()),
            secrets,
        )
    }
    fn sanitize(&self, data: &mut Value) {
        match data {
            Value::String(s) => *s = self.mask(s),
            Value::Array(items) => items.iter_mut().for_each(|item| self.sanitize(item)),
            Value::Object(items) => items.values_mut().for_each(|item| self.sanitize(item)),
            _ => {}
        }
    }
    fn request(&self, body: &Value, timeout: Duration, cancel: &Cancel) -> Result<Value> {
        ensure!(!self.key.is_empty(), "Jev is unconfigured");
        ensure!(!timeout.is_zero(), "Jev deadline elapsed");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(super::subscription::cancellable(cancel, async {
            let response = self
                .client
                .post(format!("{}/systemone", self.base))
                .bearer_auth(&self.key)
                .timeout(timeout.min(Duration::from_secs(30)))
                .json(body)
                .send()
                .await?
                .error_for_status()?;
            Ok(response.json().await?)
        }))
    }
}
fn digest(data: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .try_into()
        .unwrap()
}
impl Judge for Jev {
    fn mask(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for literal in std::iter::once(&self.key)
            .chain(self.secrets.iter())
            .filter(|s| !s.is_empty())
        {
            text = text.replace(literal, "<REDACTED>");
        }
        SHAPES.replace_all(&text, "<REDACTED>").into_owned()
    }
    fn bind(&mut self, scope: Option<String>) {
        if let Some(scope) = scope {
            let mut scopes = SCOPES.lock().expect("Jev scope lock");
            if scopes.len() >= 64 && !scopes.contains_key(&scope) {
                scopes.clear();
            }
            self.memo = scopes.entry(scope).or_default().clone();
        } else {
            self.memo = Default::default();
        }
    }
    fn stats(&self) -> JudgeStats {
        self.stats.clone()
    }
    fn ask(&mut self, mut q: Question, timeout: Duration, cancel: &Cancel) -> Result<Value> {
        cancel.check()?;
        let cache_key = digest(&serde_json::to_vec(&(
            &self.base,
            &self.model,
            digest(self.key.as_bytes()),
            q.kind,
            q.scoring_tool,
            &q.state,
            &q.questions,
        ))?);
        if let Some(a) = self.memo.lock().expect("Jev memo lock").get(&cache_key) {
            self.stats.reused += 1;
            return Ok(a.clone());
        }
        self.sanitize(&mut q.state);
        if q.kind == Kind::Score {
            if q.scoring_tool
                && let Some(field) = q.state.pointer_mut("/segment/text")
                && let Some(text) = field.as_str()
            {
                let chars: Vec<_> = text.chars().collect();
                if chars.len() > 800 {
                    *field = json!(format!(
                        "{}\n[… {} chars trimmed for scoring …]\n{}",
                        chars[..520].iter().collect::<String>(),
                        chars.len() - 800,
                        chars[chars.len() - 280..].iter().collect::<String>()
                    ));
                }
            }
            if let Some(next) = q.state.get_mut("next")
                && let Some(s) = next.as_str()
            {
                *next = json!(s.chars().take(400).collect::<String>());
            }
        }
        if q.kind == Kind::Score
            && let Some(text) = q.state.pointer_mut("/segment/text")
        {
            cap_strings(text, 1500);
        }
        for cap in [6000, 1500, 600, 240, 100, 40, 16] {
            if q.state.to_string().chars().count() <= 60000 {
                break;
            }
            cap_strings(&mut q.state, cap);
        }
        let mut questions = json!(q.questions);
        self.sanitize(&mut questions);
        let body = json!({"model":self.model,"state":q.state,"questions":questions});
        let result = self.request(&body, timeout, cancel);
        let Ok(response) = result else {
            self.stats.failed += 1;
            return result;
        };
        if let Some(usage) = response.get("usage") {
            self.stats.usage.push(usage.clone());
        }
        let answers = response.get("answers").cloned().unwrap_or(Value::Null);
        let complete = q.questions.iter().all(|(k, q)| match q["type"].as_str() {
            Some("noul") => probability(&answers, k).is_some(),
            Some("choice") => {
                choice(&answers, k).is_some_and(|selected| q["criteria"].get(selected).is_some())
            }
            _ => false,
        });
        if complete {
            self.stats.judged += 1;
            let mut memo = self.memo.lock().expect("Jev memo lock");
            if memo.len() >= 2048 {
                memo.clear();
            }
            memo.insert(cache_key, answers.clone());
        } else {
            self.stats.failed += 1;
        }
        // Preserve usable verification answers; absence is distinct from contradiction.
        if complete || matches!(q.kind, Kind::Faithfulness | Kind::Coverage) {
            Ok(answers)
        } else {
            None.context("Jev returned incomplete typed answers")
        }
    }
}

pub struct NativeProvider {
    pub inner: Box<dyn Provider + Send>,
    pub judge: Jev,
}
impl Provider for NativeProvider {
    fn default_model(&self) -> Option<panda_core::ModelId> {
        self.inner.default_model()
    }
    fn model(&self) -> Option<panda_core::ModelId> {
        self.inner.model()
    }
    fn prices(&self) -> Option<panda_core::TokenPrices> {
        self.inner.prices()
    }
    fn compaction_judge(&mut self) -> Option<&mut dyn Judge> {
        Some(&mut self.judge)
    }
    fn reply(
        &mut self,
        system: &str,
        history: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.inner.reply(system, history, tools, cancel, emit)
    }
    fn reply_with_effort(
        &mut self,
        system: &str,
        history: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        self.inner
            .reply_with_effort(system, history, tools, cancel, emit, effort)
    }
}
