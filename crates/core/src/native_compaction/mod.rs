//! Native, source-backed compaction. Planning and composing a draft never mutate the session.
mod handoff;
mod judge;
mod plan;
mod trigger;
use crate::{Cancel, Emit, Event, Message, Session, Store};
use anyhow::{Context, Result};
pub use handoff::compose;
pub use judge::{Clock, Judge, JudgeStats, Kind, Question, SystemClock, choice, probability};
pub use plan::{Plan, Role, Segment, plan, tokens, visible};
pub use trigger::{CacheSample, Pressure, Schedule, Thresholds, trigger};
#[derive(Debug)]
pub(crate) struct CheckpointWrite;
impl std::fmt::Display for CheckpointWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("saving native checkpoint")
    }
}

pub(crate) fn adopt(
    store: &mut dyn Store,
    s: &mut Session,
    note: &str,
    judge: &mut dyn Judge,
    cancel: &Cancel,
) -> Result<()> {
    let plan = plan(s).map_err(anyhow::Error::msg)?;
    let checkpoint = compose(&plan, note, judge, &SystemClock, cancel)
        .map_err(|error| anyhow::anyhow!("{error}. {}", statistics(judge)))?;
    cancel.check()?;
    s.native_schedule.last_soft_tokens = None;
    s.native_schedule.warm_notified = false;
    s.native_schedule.last_outcome = Some(format!(
        "Compacted through {}. {}",
        checkpoint.through,
        statistics(judge)
    ));
    s.native_schedule.rule_sources = handoff::rule_sources(&checkpoint.summary);
    s.checkpoint = Some(checkpoint);
    store.save_checkpoint(s).context(CheckpointWrite)
}

/// A judge runs only before the new user message is committed. No tool dispatch can be in flight.
pub(crate) fn before_turn(
    s: &mut Session,
    store: &mut dyn Store,
    judge: &mut dyn Judge,
    used: u64,
    input: &Message,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<()> {
    judge.bind(store.compaction_scope());
    let cache = s
        .usage
        .last()
        .map(|u| CacheSample {
            input: u.input,
            read: u.cache_read,
            timestamp: u.timestamp,
        })
        .unwrap_or_default();
    let request = match input {
        Message::User(text) | Message::Images { text, .. } => {
            text.split("\n\nAttached file ").next().unwrap_or("")
        }
        _ => "",
    };
    let pressure = if s
        .context_budget
        .is_some_and(|bytes| s.context_bytes() > bytes)
    {
        Pressure::Hard
    } else {
        trigger(
            s,
            used,
            cache,
            request,
            false,
            &Thresholds::default(),
            judge,
            &SystemClock,
            cancel,
        )
    };
    match pressure {
        Pressure::Warm { notify: true } => {
            emit(Event::CompactionNotice(
                "Compaction deferred while the prompt cache is warm.".into(),
            ));
        }
        Pressure::Soft | Pressure::Hard => {
            if let Err(error) = adopt(store, s, "", judge, cancel) {
                // Planning and judging are optional at a clean boundary. Storage failures
                // after installing a candidate must abort before starting a new turn.
                cancel.check()?;
                if error.is::<CheckpointWrite>() {
                    return Err(error);
                }
                s.native_schedule.last_outcome = Some(error.to_string());
            }
        }
        _ => {}
    }
    cancel.check()
}

fn statistics(judge: &dyn Judge) -> String {
    let stats = judge.stats();
    let mut message = format!(
        "Judged {}, failed {}, reused {}.",
        stats.judged, stats.failed, stats.reused
    );
    if !stats.usage.is_empty() {
        let sum = |key: &str| {
            stats
                .usage
                .iter()
                .filter_map(|u| u[key].as_u64())
                .fold(0u64, u64::saturating_add)
        };
        message.push_str(&format!(
            " Reported usage: {} input, {} output tokens.",
            sum("input_tokens"),
            sum("output_tokens")
        ));
    }
    message
}
