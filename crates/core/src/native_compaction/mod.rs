//! Source-backed compaction and the existing provider summary share a single commit path.
mod handoff;
mod info;
mod judge;
mod plan;
mod trigger;
pub use handoff::compose;
pub use info::context_info;
pub use judge::{Clock, Judge, JudgeStats, Kind, Question, SystemClock, choice, probability};
pub use plan::{Plan, Role, Segment, plan, tokens, visible};
pub use trigger::{CacheSample, Pressure, Schedule, Thresholds, trigger};

use crate::{Cancel, Checkpoint, Emit, Event, Message, Provider, Session, Store};
use anyhow::{Context, Result, ensure};

#[derive(Debug)]
struct CheckpointWrite;
impl std::fmt::Display for CheckpointWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("saving native checkpoint")
    }
}

pub(crate) struct Request<'a> {
    limit: usize,
    retain: u64,
    user: &'a str,
    fallback: bool,
}
impl<'a> Request<'a> {
    pub fn manual(limit: usize, user: &'a str) -> Self {
        Self {
            limit,
            retain: 0,
            user,
            fallback: true,
        }
    }
    pub fn automatic(limit: usize, retain: u64) -> Self {
        Self {
            limit,
            retain,
            user: "",
            fallback: true,
        }
    }
}
struct Draft {
    verified: bool,
    checkpoint: Checkpoint,
    outcome: String,
}
fn draft(
    p: &mut dyn Provider,
    s: &mut Session,
    scope: Option<String>,
    request: Request<'_>,
    note: &str,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<Option<Draft>> {
    cancel.check()?;
    let native = if let Some(judge) = p.compaction_judge().filter(|j| j.available()) {
        judge.bind(scope);
        let attempt = plan::plan_before(s, request.limit)
            .map_err(anyhow::Error::msg)
            .and_then(|plan| {
                handoff::compose_guided(&plan, request.user, note, judge, &SystemClock, cancel)
            });
        match attempt {
            Ok(checkpoint) => {
                return Ok(Some(Draft {
                    verified: true,
                    outcome: format!("Verified handoff. {}", statistics(judge)),
                    checkpoint,
                }));
            }
            Err(error) => format!("{error}. {}", statistics(judge)),
        }
    } else {
        "Jev is unconfigured".into()
    };
    cancel.check()?;
    if !request.fallback {
        anyhow::bail!("{native}");
    }
    let end = s.compact_through(request.limit, request.retain);
    if end <= s.checkpoint.as_ref().map_or(0, |c| c.through) {
        return Ok(None);
    }
    let prompt = crate::summary_prompt(crate::COMPACTION_PROMPT, request.user);
    let checkpoint = crate::provider_checkpoint(p, s, end, &prompt, note, cancel, emit)
        .with_context(|| format!("provider fallback after {native}"))?;
    Ok(Some(Draft {
        verified: false,
        checkpoint,
        outcome: format!("Provider summary. {native}."),
    }))
}

/// Preview works on a detached session and never calls either Store save method.
pub fn preview(
    p: &mut dyn Provider,
    session: &Session,
    scope: Option<String>,
    guidance: &str,
    cancel: &Cancel,
) -> Result<String> {
    ensure!(!session.active, "acknowledge interrupted turn first");
    let mut copy = session.clone();
    let request = Request::manual(copy.history.len(), guidance);
    let draft = draft(p, &mut copy, scope, request, "", cancel, &mut |_| {})?
        .context("no eligible exchanges for compaction")?;
    Ok(format!(
        "{}\n{}",
        draft.outcome,
        draft.checkpoint.summary_with_files()
    ))
}
pub(crate) fn commit(
    p: &mut dyn Provider,
    store: &mut dyn Store,
    s: &mut Session,
    request: Request<'_>,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<bool> {
    let note = s.native_schedule.agent_note.take();
    let attempts = s.attempts.len();
    let outcome = draft(
        p,
        s,
        store.compaction_scope(),
        request,
        note.as_deref().unwrap_or(""),
        cancel,
        emit,
    );
    match outcome {
        Ok(Some(draft)) => {
            cancel.check()?;
            s.native_schedule.last_soft_tokens = None;
            s.native_schedule.warm_notified = false;
            s.native_schedule.last_outcome = Some(format!(
                "Compacted through {}. {}",
                draft.checkpoint.through, draft.outcome
            ));
            if draft.verified {
                s.native_schedule.rule_sources = handoff::rule_sources(&draft.checkpoint.summary);
            }
            s.checkpoint = Some(draft.checkpoint);
            store.save_checkpoint(s).context(CheckpointWrite)?;
            Ok(true)
        }
        Ok(None) => {
            if note.is_some() {
                store.save(s)?;
            }
            Ok(false)
        }
        Err(error) => {
            if note.is_some() || s.attempts.len() != attempts {
                store.save(s)?;
            }
            Err(error)
        }
    }
}

pub(crate) fn before_turn(
    s: &mut Session,
    store: &mut dyn Store,
    p: &mut dyn Provider,
    used: u64,
    input: &Message,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<bool> {
    let thresholds = s.compaction_thresholds;
    let cache = CacheSample::last(s);
    let request = match input {
        Message::User(text) | Message::Images { text, .. } => {
            text.split("\n\nAttached file ").next().unwrap_or("")
        }
        _ => "",
    };
    let pressure = if s.native_schedule.agent_note.is_some() {
        Pressure::Explicit
    } else if s
        .context_budget
        .is_some_and(|bytes| s.context_bytes() > bytes)
    {
        Pressure::Hard
    } else if let Some(judge) = p.compaction_judge() {
        judge.bind(store.compaction_scope());
        trigger(
            s,
            used,
            cache,
            request,
            false,
            &thresholds,
            judge,
            &SystemClock,
            cancel,
        )
    } else if used as f64 >= thresholds.hard * s.model_window.unwrap_or(65536).min(300000) as f64 {
        Pressure::Hard
    } else {
        Pressure::Below
    };
    cancel.check()?;
    match pressure {
        Pressure::Warm { notify: true } => emit(Event::CompactionNotice(
            "Compaction deferred while the prompt cache is warm.".into(),
        )),
        Pressure::Soft | Pressure::Hard | Pressure::Explicit => {
            let mut request =
                Request::automatic(s.history.len(), (s.token_budget() / 4).min(20000));
            request.fallback = pressure != Pressure::Soft;
            match commit(p, store, s, request, cancel, emit) {
                Err(error) if pressure == Pressure::Soft && !error.is::<CheckpointWrite>() => {
                    cancel.check()?;
                    s.native_schedule.last_outcome = Some(error.to_string());
                }
                result => return result,
            }
        }
        _ => {}
    }
    Ok(false)
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
