use crate::{Call, Checkpoint, CheckpointFiles, Message, Session};
use std::{collections::HashMap, sync::LazyLock};
static INLINE_DATA: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)data:[^,\s]+;base64,[a-z0-9+/=]+").unwrap());
fn display_text(text: &str) -> String {
    INLINE_DATA
        .replace_all(text, "[inline data omitted]")
        .into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Attachment,
    Assistant,
    Tool,
    Branch,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct Segment {
    pub source: String,
    pub index: usize,
    pub role: Role,
    pub text: String,
    pub call: Option<String>,
    pub call_index: Option<usize>,
    pub tool: Option<String>,
    pub path: Option<String>,
    pub command: Option<String>,
    pub offset: usize,
    pub lines: usize,
    pub error: bool,
}

impl Segment {
    pub fn pointer(&self) -> Option<String> {
        if self.role != Role::Tool {
            return None;
        }
        if let Some(path) = &self.path {
            if self.tool.as_deref() == Some("read") {
                if self.lines == 0 {
                    return Some(format!(
                        "Read {path} from line {} (empty result)",
                        self.offset
                    ));
                }
                return Some(format!(
                    "Read {path}, lines {}–{}",
                    self.offset,
                    self.offset.saturating_add(self.lines - 1)
                ));
            }
            return Some(format!("{} {path}", self.tool.as_deref().unwrap_or("File")));
        }
        self.command
            .as_ref()
            .map(|cmd| format!("Recorded command (reference only): {cmd}"))
    }
}

/// Matches safe-compact's word, digit and punctuation estimator on the rendered text.
pub fn tokens(text: &str) -> u64 {
    let mut tenths = 0u64;
    let mut letters = 0usize;
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            if letters.is_multiple_of(6) {
                tenths += 10;
            }
            letters += 1;
        } else {
            letters = 0;
            tenths += if c.is_ascii_digit() {
                5
            } else if c.is_whitespace() {
                0
            } else {
                9
            };
        }
    }
    tenths.div_ceil(10)
}

pub fn visible(session: &Session, end: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut calls = HashMap::new();
    for index in 0..end.min(session.history.len()) {
        for (slot, msg) in session.resolved_at(index).iter().enumerate() {
            let mut push =
                |role, text: &str, origin: Option<(usize, &Call)>, id: Option<&str>, error| {
                    let call = origin.map(|(_, c)| c);
                    if text.trim().is_empty() && id.is_none() {
                        return;
                    }
                    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
                    context.update(text.as_bytes());
                    if let Some(c) = call {
                        context.update(&serde_json::to_vec(c).unwrap_or_default());
                    }
                    if let Some(id) = id {
                        context.update(id.as_bytes());
                    }
                    let digest = context.finish();
                    let hash = u64::from_be_bytes(digest.as_ref()[..8].try_into().unwrap());
                    out.push(Segment {
                        source: format!("{index}.{slot}.{role:?}.{hash:016x}"),
                        index,
                        role,
                        text: display_text(text),
                        lines: text.lines().count(),
                        call: id.map(str::to_owned),
                        call_index: origin.map(|(i, _)| i),
                        tool: call.map(|c| c.name.clone()),
                        path: call.and_then(|c| c.args["path"].as_str()).map(display_text),
                        command: call
                            .and_then(|c| c.args["command"].as_str())
                            .map(display_text),
                        offset: call
                            .and_then(|c| c.args["offset"].as_u64())
                            .unwrap_or(1)
                            .max(1) as usize,
                        error,
                    });
                };
            match msg {
                Message::User(text) | Message::Images { text, .. } => {
                    if let Some((user, attachment)) = text.split_once("\n\nAttached file \"") {
                        push(Role::User, user, None, None, false);
                        push(Role::Attachment, attachment, None, None, false);
                    } else {
                        push(Role::User, text, None, None, false);
                    }
                    if let Message::Images { urls, .. } = msg
                        && !urls.is_empty()
                    {
                        let role = if text.trim().is_empty() {
                            Role::User
                        } else {
                            Role::Attachment
                        };
                        push(
                            role,
                            &format!("[{} attached image(s)]", urls.len()),
                            None,
                            None,
                            false,
                        );
                    }
                }
                Message::Assistant { text, calls: batch } => {
                    push(Role::Assistant, text, None, None, false);
                    for call in batch {
                        calls.insert(call.id.as_str(), (index, call));
                        // Only the public operation and locator are needed to explain a call.
                        // Arbitrary tool arguments may contain images, embedded files or credentials.
                        let target = call.args["path"]
                            .as_str()
                            .or_else(|| call.args["command"].as_str())
                            .unwrap_or("");
                        push(
                            Role::Assistant,
                            &format!("Called {} {target}", call.name),
                            Some((index, call)),
                            Some(&call.id),
                            false,
                        );
                    }
                }
                Message::Result {
                    id,
                    text,
                    images,
                    is_error,
                    ..
                } => {
                    let call = calls.get(id.as_str()).copied();
                    push(Role::Tool, text, call, Some(id), *is_error);
                    if !images.is_empty() {
                        push(
                            Role::Attachment,
                            &format!("[{} tool image(s)]", images.len()),
                            call,
                            Some(id),
                            *is_error,
                        );
                    }
                }
                Message::BranchSummary(text) => push(Role::Branch, text, None, None, false),
                Message::Truncated(_)
                | Message::CodemodeStore(_)
                | Message::Reasoning(_)
                | Message::Replay(_) => {}
            }
        }
    }
    out
}

pub struct Plan {
    pub carried_rules: Vec<String>,
    pub through: usize,
    pub segments: Vec<Segment>,
    pub users: Vec<Segment>,
    pub previous: Option<Checkpoint>,
    pub files: CheckpointFiles,
    pub replaced_tokens: u64,
}
pub fn plan(session: &Session) -> Result<Plan, &'static str> {
    let previous = session
        .checkpoint
        .as_ref()
        .filter(|p| p.through <= session.history.len());
    let start = previous.map_or(0, |p| p.through);
    let all = visible(session, session.history.len());
    let active = &all[all.partition_point(|s| s.index < start)..];
    let total: u64 = active.iter().map(|s| tokens(&s.text)).sum();
    if total < 2_000 {
        return Err("insignificant span (under 2,000 tokens)");
    }
    let keep = session
        .invocation_keep_recent_tokens
        .or(session.keep_recent_tokens)
        .unwrap_or(20_000)
        .min(total.saturating_sub(2_000.max(total.div_ceil(4))));
    let mut cuts: Vec<_> = (start..session.history.len())
        .filter(|&i| session.resolved_at(i).first().is_some_and(Message::user))
        .collect();
    if cuts.is_empty() {
        return Err("no whole user exchange");
    }
    // Keep the current exchange even when a zero-token suffix was requested.
    let mut through = *cuts.last().unwrap();
    let mut suffix = 0u64;
    for s in active.iter().rev() {
        suffix += tokens(&s.text);
        if suffix >= keep {
            through = cuts
                .iter()
                .copied()
                .find(|&i| i >= s.index)
                .unwrap_or(through);
            break;
        }
    }
    cuts.retain(|&i| {
        i <= through
            && i > start
            && active.partition_point(|s| s.index < i) <= 400
            && !active.iter().any(|s| {
                s.role == Role::Tool
                    && s.index >= i
                    && s.call_index.is_some_and(|origin| origin < i)
            })
    });
    through = cuts
        .last()
        .copied()
        .ok_or("no whole exchange within 400 segments")?;
    let segments = active[..active.partition_point(|s| s.index < through)].to_vec();
    let summarized: u64 = segments.iter().map(|s| tokens(&s.text)).sum();
    if summarized < 2_000 {
        return Err("insignificant summarized span (under 2,000 tokens)");
    }
    let mut files = previous.and_then(|p| p.files.clone()).unwrap_or_default();
    let from = if previous.is_some_and(|p| p.files.is_some()) {
        start
    } else {
        0
    };
    for s in all
        .iter()
        .filter(|s| s.index >= from && s.index < through && s.role == Role::Tool)
    {
        if s.tool.as_deref() == Some("codemode") {
            for message in session.resolved_at(s.index) {
                if let Message::Result {
                    id, nested_reads, ..
                } = message
                    && s.call.as_ref() == Some(id)
                {
                    files.read_files.extend(
                        nested_reads
                            .iter()
                            .filter(|path| !files.modified_files.contains(*path))
                            .cloned(),
                    );
                }
            }
        } else if !s.error
            && let Some(path) = &s.path
        {
            match s.tool.as_deref() {
                Some("read") if !files.modified_files.contains(path) => {
                    files.read_files.insert(path.clone());
                }
                Some("edit" | "write") => {
                    files.read_files.remove(path);
                    files.modified_files.insert(path.clone());
                }
                _ => {}
            }
        }
    }
    let users = all
        .into_iter()
        .filter(|s| s.index < through && s.role == Role::User)
        .collect();
    Ok(Plan {
        carried_rules: session.native_schedule.rule_sources.clone(),
        through,
        users,
        segments,
        previous: previous.cloned(),
        files,
        replaced_tokens: summarized + previous.map_or(0, |p| tokens(&p.summary_with_files())),
    })
}
