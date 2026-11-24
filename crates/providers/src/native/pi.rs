//! Radius accepts Pi transcript messages, including tool declarations on the system message.
use super::*;
use anyhow::Context;
use panda_core::{Part, parts};
use std::collections::BTreeMap;

fn content(
    text: &str,
    images: &[String],
    details: Option<&panda_core::ToolDetails>,
    accept_images: bool,
) -> Result<Vec<Value>> {
    parts(text, images, details)
        .into_iter()
        .map(|part| match part {
            Part::Text(t) => Ok(json!({"type":"text","text":t})),
            Part::Image(_) if !accept_images => {
                Ok(json!({"type":"text","text":"[image omitted: text-only model]"}))
            }
            Part::Image(url) => {
                let (mime, data) = url
                    .strip_prefix("data:")
                    .and_then(|s| s.split_once(";base64,"))
                    .context("expected base64 image")?;
                Ok(json!({"type":"image","mimeType":mime,"data":data}))
            }
        })
        .collect()
}
pub(super) fn body(
    api: &NativeApi,
    system: &str,
    history: &[Message],
    tools: &[Value],
    effort: Option<&str>,
) -> Result<Value> {
    let tools: Vec<_> = tools.iter().map(|v| &v["function"]).collect();
    let mut messages =
        vec![json!({"role":"system","content":system,"toolsAdded":tools,"timestamp":0})];
    let mut names = BTreeMap::new();
    for m in history {
        let mut msg = match m {
            Message::User(t) => json!({"role":"user","content":t}),
            Message::Images { text, urls } => {
                json!({"role":"user","content":content(text,urls,None,api.images)?})
            }
            Message::BranchSummary(t) => {
                json!({"role":"user","content":format!("Summary from another branch:\n{t}")})
            }
            Message::Bash {
                command,
                outcome,
                exclude: false,
            } => json!({"role":"user","content":outcome.context(command)}),
            Message::Assistant { text, calls } => {
                let mut blocks = content(text, &[], None, false)?;
                for c in calls {
                    names.insert(&c.id, &c.name);
                    blocks.push(
                        json!({"type":"toolCall","id":c.id,"name":c.name,"arguments":c.args}),
                    );
                }
                json!({"role":"assistant","content":blocks,"api":"pi-messages","provider":api.provider,"model":api.api.model,
                    "stopReason":if calls.is_empty(){"stop"}else{"toolUse"},"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}})
            }
            Message::Result {
                id,
                text,
                images,
                details,
                is_error,
                ..
            } => {
                json!({"role":"toolResult","toolCallId":id,"toolName":names.get(id).context("missing ancestor tool call")?,"content":content(text,images,details.as_ref(),api.images)?,"isError":is_error})
            }
            _ => continue,
        };
        msg["timestamp"] = json!(0);
        messages.push(msg);
    }
    let mut options = json!({"maxTokens":api.max_tokens});
    if let Some(e) = effort {
        options["reasoning"] = json!(e);
    }
    Ok(json!({"model":api.api.model,"context":{"messages":messages},"options":options}))
}
