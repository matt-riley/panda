use super::{
    judge::{Clock, Judge, Kind, Question, clip, probability},
    plan::{Role, visible},
};
use crate::{Cancel, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub last_soft_tokens: Option<u64>,
    pub warm_notified: bool,
    pub last_outcome: Option<String>,
    pub rule_sources: Vec<String>,
    pub agent_note: Option<String>,
}
impl Schedule {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Below,
    Warm { notify: bool },
    Deferred,
    Soft,
    Hard,
    Explicit,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Thresholds {
    pub soft: f64,
    pub hard: f64,
    pub cache_share: f64,
    pub cache_age_secs: u64,
}
impl Thresholds {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            soft: 0.5,
            hard: 0.7,
            cache_share: 0.7,
            cache_age_secs: 1800,
        }
    }
}
#[derive(Clone, Copy, Default)]
pub struct CacheSample {
    pub input: u64,
    pub read: Option<u64>,
    pub timestamp: Option<u64>,
}
impl CacheSample {
    pub fn last(s: &Session) -> Self {
        s.usage
            .last()
            .map(|u| Self {
                input: u.input,
                read: u.cache_read,
                timestamp: u.timestamp,
            })
            .unwrap_or_default()
    }
    pub fn warm(self, now: u64) -> bool {
        self.is_warm(now, &Thresholds::default())
    }
    pub fn is_warm(self, now: u64, policy: &Thresholds) -> bool {
        matches!((self.read,self.timestamp),(Some(read),Some(at)) if self.input>0 && at<=now && now-at<policy.cache_age_secs && read as f64 / self.input as f64 >= policy.cache_share)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn trigger(
    s: &mut Session,
    used: u64,
    cache: CacheSample,
    request: &str,
    explicit: bool,
    thresholds: &Thresholds,
    judge: &mut dyn Judge,
    clock: &dyn Clock,
    cancel: &Cancel,
) -> Pressure {
    if s.active {
        return Pressure::Deferred;
    }
    if explicit {
        return Pressure::Explicit;
    }
    let window = s
        .model_window
        .filter(|w| *w > 0)
        .unwrap_or(65536)
        .min(300_000) as f64;
    let hard = thresholds.hard.clamp(0.01, 0.90);
    if used as f64 >= window * hard {
        return Pressure::Hard;
    }
    if (used as f64) < window * thresholds.soft.clamp(0., hard) {
        s.native_schedule.warm_notified = false;
        return Pressure::Below;
    }
    if cache.is_warm(clock.now().as_secs(), thresholds) {
        let notify = !s.native_schedule.warm_notified;
        s.native_schedule.warm_notified = true;
        return Pressure::Warm { notify };
    }
    s.native_schedule.warm_notified = false;
    if s.native_schedule
        .last_soft_tokens
        .is_some_and(|last| used < last.saturating_add(10_000))
    {
        return Pressure::Deferred;
    }
    s.native_schedule.last_soft_tokens = Some(used);
    if !judge.available() {
        return Pressure::Deferred;
    }
    let recent = visible(s, s.history.len());
    let recent_text = recent
        .iter()
        .rev()
        .take(8)
        .rev()
        .map(|s| clip(&judge.mask(&s.text), 500))
        .collect::<Vec<_>>()
        .join("\n");
    let previous = recent
        .iter()
        .rfind(|s| s.role == Role::User)
        .map(|s| clip(&judge.mask(&s.text), 1500));
    let mut q = Question::new(
        Kind::Moment,
        json!({"recent_activity":recent_text,"current_request":clip(&judge.mask(request),600),"previous_work":previous}),
    );
    for (key, ask) in [
        (
            "mid_flight",
            "Does unfinished work depend on recent details?",
        ),
        (
            "debugging",
            "Is the agent actively debugging an unresolved failure?",
        ),
        ("completed", "Was a unit of work completed?"),
        ("switched_gears", "Is the current request a new task?"),
        (
            "needs_history",
            "Does the current request depend on previous work?",
        ),
    ] {
        q.probability(key, ask);
    }
    let start = clock.now();
    let a = judge
        .ask(q, Duration::from_secs(8), cancel)
        .unwrap_or(Value::Null);
    if clock.now().saturating_sub(start) >= Duration::from_secs(8) {
        return Pressure::Deferred;
    }
    let quiet = probability(&a, "mid_flight").is_some_and(|v| v < 0.4)
        && probability(&a, "debugging").is_some_and(|v| v < 0.4);
    let switched = probability(&a, "switched_gears").is_some_and(|v| v >= 0.7)
        && probability(&a, "needs_history").is_some_and(|v| v < 0.5);
    if quiet && (switched || probability(&a, "completed").is_some_and(|v| v >= 0.5)) {
        Pressure::Soft
    } else {
        Pressure::Deferred
    }
}
