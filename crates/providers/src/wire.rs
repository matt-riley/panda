//! Native SSE decoders for subscription endpoints.
use anyhow::{Context, Result, bail, ensure};
use panda_core::{Call, Emit, Event, LengthStop, Message};
use serde_json::{Value, json};

#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
    total: usize,
}
impl Lines {
    fn feed(&mut self, bytes: &[u8], mut consume: impl FnMut(Value) -> Result<()>) -> Result<()> {
        self.total = self.total.saturating_add(bytes.len());
        ensure!(self.total <= 16 * 1024 * 1024, "response size limit");
        for &b in bytes {
            if b != b'\n' {
                self.partial.push(b);
                continue;
            }
            let line = std::str::from_utf8(&self.partial)?.trim_end();
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                if data != "[DONE]" {
                    consume(serde_json::from_str(data)?)?
                }
            }
            self.partial.clear();
        }
        Ok(())
    }
}
pub(crate) fn string<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing {k}"))
}
#[derive(Default)]
pub struct Responses {
    lines: Lines,
    text: String,
    calls: Vec<Value>,
    complete: bool,
    cutoff: Option<(Option<u64>, Option<u64>)>,
}
impl Responses {
    pub fn push(&mut self, bytes: &[u8], emit: &mut Emit) -> Result<()> {
        self.lines.feed(bytes, |v| {
            ensure!(!self.complete, "event after completion");
            match string(&v, "type")? {
                "response.reasoning_summary_text.delta" => {
                    if let Some(t) = v["delta"].as_str() {
                        emit(Event::Reasoning(t.into()));
                    }
                }
                "response.output_text.delta" => {
                    let t = v["delta"].as_str().context("missing delta")?;
                    self.text += t;
                    emit(Event::Text(t.into()))
                }
                "response.output_item.done" if v["item"]["type"] == "function_call" => {
                    self.calls.push(v["item"].clone());
                }
                "response.incomplete"
                    if v["response"]["incomplete_details"]["reason"] == "max_output_tokens" =>
                {
                    let response = &v["response"];
                    self.complete = true;
                    self.cutoff = Some((
                        response["usage"]["output_tokens"].as_u64(),
                        response["max_output_tokens"].as_u64(),
                    ));
                    if !response["usage"].is_null() {
                        emit(Event::Usage(response["usage"].clone()));
                    }
                }
                "response.completed" => {
                    ensure!(
                        v["response"]["status"] == "completed",
                        "response incomplete"
                    );
                    self.complete = true;
                    if !v["response"]["usage"].is_null() {
                        emit(Event::Usage(v["response"]["usage"].clone()))
                    }
                }
                "error" | "response.failed" | "response.incomplete" => {
                    bail!("subscription response failed or incomplete")
                }
                _ => {}
            }
            Ok(())
        })
    }
    pub fn finish(self) -> Result<(String, Vec<Call>)> {
        ensure!(
            self.complete && self.lines.partial.is_empty(),
            "incomplete Responses stream"
        );
        if let Some((output_tokens, output_limit)) = self.cutoff {
            return Err(LengthStop {
                text: self.text,
                tool_fragments: self.calls,
                output_tokens,
                output_limit,
                ..Default::default()
            }
            .into());
        }
        let calls = self
            .calls
            .iter()
            .map(|item| {
                ensure!(
                    item.get("status").is_none() || item["status"] == "completed",
                    "incomplete function call"
                );
                Ok(Call {
                    id: string(item, "call_id")?.into(),
                    name: string(item, "name")?.into(),
                    args: serde_json::from_str(string(item, "arguments")?)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok((self.text, calls))
    }
}
fn call_id(id: &str) -> &str {
    id.split('|').next().unwrap_or(id)
}
pub(crate) fn responses_body(
    model: &str,
    system: &str,
    history: &[Message],
    tools: &[Value],
) -> Value {
    let mut input = Vec::new();
    for m in history {
        match m {
            Message::Truncated(_) | Message::CodemodeStore(_) => continue,
            Message::BranchSummary(s) => input.push(
                json!({"role":"user","content":format!("Summary from another branch:\n{s}")}),
            ),
            Message::Reasoning(_) => continue,
            Message::Images { text, urls } => {
                let mut parts = vec![json!({"type":"input_text","text":text})];
                parts.extend(
                    urls.iter()
                        .map(|u| json!({"type":"input_image","image_url":u})),
                );
                input.push(json!({"role":"user","content":parts}));
            }
            Message::User(t) => input.push(json!({"role":"user","content":t})),
            Message::Assistant { text, calls } => {
                if !text.is_empty() {
                    input.push(json!({"role":"assistant","content":text}))
                }
                for c in calls {
                    input.push(json!({"type":"function_call","call_id":call_id(&c.id),"name":c.name,"arguments":c.args.to_string()}))
                }
            }
            Message::Result {
                id, text, images, ..
            } => {
                let output = if images.is_empty() {
                    json!(text)
                } else {
                    let mut parts = vec![json!({"type":"input_text","text":text})];
                    parts.extend(
                        images
                            .iter()
                            .map(|url| json!({"type":"input_image","image_url":url})),
                    );
                    Value::Array(parts)
                };
                input.push(
                    json!({"type":"function_call_output","call_id":call_id(id),"output":output}),
                );
            }
        }
    }
    let tools: Vec<_> = tools
        .iter()
        .map(|t| {
            let mut f = t["function"].clone();
            f["type"] = json!("function");
            f["strict"] = json!(false);
            f
        })
        .collect();
    json!({"model":model,"instructions":system,"input":input,"tools":tools,"stream":true,"store":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_tool_history_keeps_each_response_associated_with_its_call() -> Result<()> {
        let url = "data:image/png;base64,iVBORw0KGgo=";
        let history = [
            Message::Assistant {
                text: String::new(),
                calls: ["a", "b"]
                    .into_iter()
                    .map(|id| Call {
                        id: id.into(),
                        name: "read".into(),
                        args: json!({"path":id}),
                    })
                    .collect(),
            },
            Message::Result {
                id: "a".into(),
                text: "picture".into(),
                images: vec![url.into()],
                is_error: false,
                nested_reads: vec![],
            },
            // Transcripts saved before image reads have no images field.
            serde_json::from_value(json!({"Result":{"id":"b","text":"plain","isError":false}}))?,
            Message::User("continue".into()),
        ];
        let saved = serde_json::to_vec(&history)?;
        let resumed: Vec<Message> = serde_json::from_slice(&saved)?;
        let input = responses_body("m", "s", &resumed, &[])["input"].clone();
        assert_eq!(
            input[2],
            json!({"type":"function_call_output","call_id":"a","output":[
                {"type":"input_text","text":"picture"},{"type":"input_image","image_url":url}
            ]})
        );
        assert_eq!(input[3]["output"], "plain");
        assert_eq!(input.to_string().matches(url).count(), 1);
        let chat = crate::messages("s", &resumed);
        assert_eq!(chat[2]["tool_call_id"], "a");
        assert_eq!(chat[3]["tool_call_id"], "b");
        assert_eq!(
            chat[4],
            json!({"role":"user","content":[{"type":"image_url","image_url":{"url":url}}]})
        );
        assert_eq!(chat[5]["content"], "continue");
        Ok(())
    }

    #[test]
    fn resumed_tool_history_uses_matching_ids() {
        let history = [
            Message::Assistant {
                text: String::new(),
                calls: vec![Call {
                    id: "call1|fc1".into(),
                    name: "read".into(),
                    args: json!({}),
                }],
            },
            Message::Result {
                id: "call1|fc1".into(),
                text: "missing".into(),
                images: vec![],
                is_error: true,
                nested_reads: vec![],
            },
        ];
        let body = responses_body("m", "s", &history, &[]);
        assert_eq!(body["input"][0]["call_id"], "call1");
        assert_eq!(body["input"][1]["call_id"], "call1");
    }
}
