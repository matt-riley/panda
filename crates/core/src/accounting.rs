//! Provider request attribution and per-million-token price accounting.
use super::{Call, Cancel, Emit, Event, Message, Provider};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelId {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenPrices {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}
impl TokenPrices {
    pub fn valid(&self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .all(|v| v.is_finite() && v >= 0.0)
    }
    fn price(&self, u: &UsageRecord) -> Option<f64> {
        let read = u.cache_read.unwrap_or(0);
        let write = u.cache_write.unwrap_or(0);
        let input = u.input.checked_sub(read)?.checked_sub(write)?;
        let read_cost = if read == 0 {
            0.0
        } else {
            self.cache_read? * read as f64
        };
        let write_cost = if write == 0 {
            0.0
        } else {
            self.cache_write? * write as f64
        };
        Some(
            (input as f64 * self.input + u.output as f64 * self.output + read_cost + write_cost)
                / 1_000_000.,
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub cost: Option<f64>,
    pub through: usize,
    pub model: Option<ModelId>,
    pub purpose: String,
    pub outcome: String,
    pub usage: Option<UsageRecord>,
    pub cost_source: String,
    pub timestamp: u64,
}
fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default)]
    pub checkpoint_through: usize,
    pub through: usize,
    pub input: u64,
    pub output: u64,
    pub cost: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<u64>,
    #[serde(default)]
    pub cache_write: Option<u64>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}
impl UsageRecord {
    pub fn from_event(u: &Value) -> Option<Self> {
        let read = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .or(u.pointer("/input_tokens_details/cached_tokens"))
            .or(u.get("prompt_cache_hit_tokens"))
            .or(u.get("cache_read_input_tokens"))
            .and_then(Value::as_u64);
        let write = u
            .pointer("/prompt_tokens_details/cache_write_tokens")
            .or(u.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64);
        let mut input = u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64())?;
        if u.get("cache_read_input_tokens").is_some()
            || u.get("cache_creation_input_tokens").is_some()
        {
            input = input
                .checked_add(read.unwrap_or(0))?
                .checked_add(write.unwrap_or(0))?;
        }
        Some(Self {
            cache_read: read,
            cache_write: write,
            timestamp: Some(timestamp()),
            through: 0,
            checkpoint_through: 0,
            input,
            output: u["output_tokens"]
                .as_u64()
                .or(u["completion_tokens"].as_u64())?,
            cost: u["cost"].as_f64(),
        })
    }
}
#[derive(Debug)]
pub struct FailedUsage(pub Value);
impl std::fmt::Display for FailedUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider reported usage on the failed request")
    }
}
pub(crate) fn tracked(
    p: &mut dyn Provider,
    attempts: &mut Vec<Attempt>,
    through: usize,
    purpose: &str,
    emit: &mut Emit,
    send: impl FnOnce(&mut dyn Provider, &mut Emit) -> Result<(String, Vec<Call>)>,
) -> Result<(String, Vec<Call>)> {
    let requested = p.model();
    let mut backing = requested.clone();
    let mut model = requested.clone();
    let mut report = None;
    let result = send(p, &mut |event| {
        if let Event::BackingModel(id) = event {
            backing = Some(id.clone());
            model = Some(id);
            return;
        }
        if let Event::Responder(name) = event {
            if let Some(id) = &mut model
                && !name.is_empty()
            {
                id.model = name;
            }
            return;
        }
        if let Event::Usage(value) = &event {
            report = Some(value.clone());
        }
        emit(event);
    });
    if let Err(e) = &result
        && let Some(FailedUsage(value)) = e.downcast_ref::<FailedUsage>()
    {
        report = Some(value.clone());
    }
    let prices = (model == backing).then(|| p.prices()).flatten();
    attempts.push(Attempt::from_report(
        model,
        purpose,
        through,
        report.as_ref(),
        prices.as_ref(),
        result.is_ok(),
    ));
    result
}

impl Attempt {
    pub fn from_report(
        model: Option<ModelId>,
        purpose: &str,
        through: usize,
        report: Option<&Value>,
        prices: Option<&TokenPrices>,
        ok: bool,
    ) -> Self {
        let mut usage = report.and_then(UsageRecord::from_event);
        let reported = report.and_then(|u| {
            u["cost"]
                .as_f64()
                .or_else(|| u.pointer("/cost/total").and_then(Value::as_f64))
        });
        let calculated = usage
            .as_ref()
            .and_then(|reading| prices.and_then(|p| p.price(reading)));
        let cost = reported.or(calculated);
        let source = if reported.is_some() {
            "reported"
        } else if calculated.is_some() {
            "catalog"
        } else {
            "unknown"
        };
        if let Some(reading) = &mut usage {
            reading.through = through;
            reading.cost = cost;
        }
        Self {
            cost,
            through,
            model,
            purpose: purpose.into(),
            outcome: if ok { "ok" } else { "failed" }.into(),
            usage,
            cost_source: source.into(),
            timestamp: timestamp(),
        }
    }
}
pub struct IdentifiedProvider {
    pub persist_default: bool,
    pub inner: Box<dyn Provider + Send>,
    pub id: ModelId,
    pub prices: Option<TokenPrices>,
}
impl Provider for IdentifiedProvider {
    fn compaction_judge(&mut self) -> Option<&mut dyn crate::native_compaction::Judge> {
        self.inner.compaction_judge()
    }
    fn default_model(&self) -> Option<ModelId> {
        self.persist_default.then(|| self.id.clone())
    }
    fn model(&self) -> Option<ModelId> {
        Some(self.id.clone())
    }
    fn prices(&self) -> Option<TokenPrices> {
        self.prices.clone()
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
