use super::{CacheSample, Clock, SystemClock, tokens};
use crate::Session;
use serde_json::{Value, json};

/// Reports the current projection using only provider-visible declarations. No model request is made.
pub fn context_info(session: &Session, system: &str, schemas: &[Value]) -> Value {
    let policy = session.compaction_thresholds;
    let now = SystemClock.now().as_secs();
    let last = CacheSample::last(session);
    let cache = match (last.read, last.timestamp) {
        (Some(read), Some(time)) if last.input > 0 && time <= now => {
            json!({"state":if last.is_warm(now,&policy){"warm"}else{"cold"},"readPercent":100.0*read as f64/last.input as f64,"ageSeconds":now-time})
        }
        _ => json!({"state":"unknown"}),
    };
    let mut sections = Vec::new();
    let mut section_name = "system".to_owned();
    let mut body = String::new();
    for line in system.lines() {
        let heading = if line.starts_with("Instructions from ") && line.ends_with(':') {
            Some(line.trim_end_matches(':'))
        } else if line == "Available skills:" {
            Some("available skills")
        } else {
            None
        };
        if let Some(heading) = heading {
            if !body.trim().is_empty() {
                sections.push((section_name, tokens(&body)));
            }
            body.clear();
            section_name = heading.to_owned();
        }
        body.push_str(line);
        body.push('\n');
    }
    if !body.trim().is_empty() {
        sections.push((section_name, tokens(&body)));
    }
    let start = session
        .checkpoint
        .as_ref()
        .map_or(0, |c| c.through)
        .min(session.history.len());
    if let Some(cp) = &session.checkpoint {
        sections.push(("checkpoint".to_owned(), tokens(&cp.summary_with_files())));
    }
    let visible = super::visible(session, session.history.len());
    let conversation: u64 = visible
        .iter()
        .filter(|s| s.index >= start)
        .map(|s| tokens(&s.text))
        .sum();
    sections.push(("conversation".to_owned(), conversation));
    sections.sort_by(|(a, x), (b, y)| y.cmp(x).then_with(|| a.cmp(b)));
    let sections: Vec<_> = sections
        .into_iter()
        .map(|(section, size)| json!({"section":section,"estimatedTokens":size}))
        .collect();
    let declarations = schemas
        .iter()
        .map(|s| json!({"name":s["function"]["name"],"estimatedTokens":tokens(&s.to_string())}))
        .collect::<Vec<_>>();
    let tool_tokens: u64 = declarations
        .iter()
        .filter_map(|d| d["estimatedTokens"].as_u64())
        .sum();
    let window = session.model_window.unwrap_or(65536).min(300000);
    let used = session.estimated_tokens(system, schemas);
    json!({"estimatedTokens":used,"effectiveWindow":window,"usedPercent":100.0*used as f64/window.max(1) as f64,
        "softPercent":policy.soft*100.0,"hardPercent":policy.hard*100.0,"cacheReadPercent":policy.cache_share*100.0,
        "cacheMinutes":policy.cache_age_secs/60,"cache":cache,"lastUsage":session.usage.last(),"promptSections":sections,"visibleToolCount":declarations.len(),"visibleToolTokens":tool_tokens,"visibleTools":declarations,"bytes":session.context_bytes(),"byteLimit":session.context_budget})
}
