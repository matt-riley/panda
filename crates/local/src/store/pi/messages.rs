use anyhow::{Context, Result, bail};
use panda_core::{Call, Message};
use serde_json::{Value, json};

pub(super) fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("expected string {key}"))
}

#[derive(Default)]
struct Content {
    text: String,
    thinking: String,
    images: Vec<String>,
    calls: Vec<Call>,
}

fn content(value: &Value) -> Result<Content> {
    let mut out = Content::default();
    if let Some(text) = value.as_str() {
        out.text = text.to_owned();
        return Ok(out);
    }
    for block in value
        .as_array()
        .context("expected message content blocks")?
    {
        match string(block, "type")? {
            "text" => out.text.push_str(string(block, "text")?),
            "thinking" => out.thinking.push_str(string(block, "thinking")?),
            "image" => out.images.push(format!(
                "data:{};base64,{}",
                string(block, "mimeType")?,
                string(block, "data")?
            )),
            "toolCall" => out.calls.push(Call {
                id: string(block, "id")?.into(),
                name: string(block, "name")?.into(),
                args: block
                    .get("arguments")
                    .context("missing tool call arguments")?
                    .clone(),
            }),
            _ => {} // Preserve unfamiliar blocks in the original JSONL record.
        }
    }
    Ok(out)
}

pub(super) fn project(entry: &Value) -> Result<Vec<Message>> {
    let kind = string(entry, "type")?;
    if kind == "custom" && entry["customType"] == "panda.truncated-reply" {
        return Ok(vec![Message::Truncated(serde_json::from_value(
            entry["data"].clone(),
        )?)]);
    }
    if kind == "branch_summary" {
        return Ok(vec![Message::BranchSummary(
            string(entry, "summary")?.into(),
        )]);
    }
    let (role, m) = match kind {
        "custom_message" => ("user", entry),
        "message" => (string(&entry["message"], "role")?, &entry["message"]),
        _ => return Ok(vec![]),
    };
    let mut messages = Vec::new();
    match role {
        "user" | "custom" => {
            let c = content(&m["content"])?;
            messages.push(if c.images.is_empty() {
                Message::User(c.text)
            } else {
                Message::Images {
                    text: c.text,
                    urls: c.images,
                }
            });
        }
        "assistant" => {
            let c = content(&m["content"])?;
            if !c.thinking.is_empty() {
                messages.push(Message::Reasoning(c.thinking));
            }
            messages.push(Message::Assistant {
                text: c.text,
                calls: c.calls,
            });
        }
        "toolResult" => {
            let c = content(&m["content"])?;
            messages.push(Message::Result {
                id: string(m, "toolCallId")?.into(),
                text: c.text,
                images: c.images,
                is_error: m["isError"].as_bool().unwrap_or(false),
            });
        }
        "bashExecution" if m["excludeFromContext"] != true => {
            messages.push(Message::User(format!(
                "$ {}\n{}",
                string(m, "command")?,
                string(m, "output")?
            )));
        }
        _ => {} // System settings and extensions are archival, never executable.
    }
    Ok(messages)
}

fn blocks(text: &str, images: &[String]) -> Result<Vec<Value>> {
    let mut out = vec![json!({"type":"text","text":text})];
    for url in images {
        let Some((mime, data)) = url
            .strip_prefix("data:")
            .and_then(|v| v.split_once(";base64,"))
        else {
            bail!("Pi images require a base64 data URL")
        };
        out.push(json!({"type":"image","mimeType":mime,"data":data}));
    }
    Ok(out)
}

pub(super) fn encode(
    message: &Message,
    timestamp: i64,
    tool_name: Option<&str>,
    usage: Option<&panda_core::UsageRecord>,
    thinking: Option<&str>,
) -> Result<Value> {
    let mut m = match message {
        Message::Truncated(partial) => {
            return Ok(
                json!({"type":"custom","customType":"panda.truncated-reply","data":partial}),
            );
        }
        Message::User(t) => json!({"role":"user","content":t}),
        Message::Images { text, urls } => json!({"role":"user","content":blocks(text,urls)?}),
        Message::Result {
            id,
            text,
            images,
            is_error,
        } => {
            json!({"role":"toolResult","toolCallId":id,"toolName":tool_name.context("tool result has no matching ancestor call")?,
            "content":blocks(text,images)?,"isError":is_error})
        }
        Message::Assistant { .. } | Message::Reasoning(_) => {
            let (mut parts, calls) = match message {
                Message::Assistant { text, calls } => (blocks(text, &[])?, calls.as_slice()),
                Message::Reasoning(t) => (vec![json!({"type":"thinking","thinking":t})], &[][..]),
                _ => unreachable!(),
            };
            if let Some(text) = thinking {
                parts.insert(0, json!({"type":"thinking","thinking":text}));
            }
            parts.extend(
                calls
                    .iter()
                    .map(|c| json!({"type":"toolCall","id":c.id,"name":c.name,"arguments":c.args})),
            );
            let (input, output) = usage.map_or((0, 0), |u| (u.input, u.output));
            json!({"role":"assistant","content":parts,"api":"openai-completions","provider":"panda","model":"unknown",
                "stopReason":if calls.is_empty() {"stop"} else {"toolUse"},"usage":{"input":input,"output":output,
                "cacheRead":0,"cacheWrite":0,"totalTokens":input.saturating_add(output),"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":usage.and_then(|u|u.cost).unwrap_or(0.)}}})
        }
        Message::BranchSummary(_) => bail!("branch summary must be exported as an entry"),
    };
    m["timestamp"] = timestamp.into();
    Ok(json!({"type":"message","message":m}))
}
