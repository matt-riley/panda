//! Native SSE decoders for subscription endpoints.
use anyhow::{Context, Result, bail, ensure};
use panda_core::{Call, Emit, Event, Message};
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
    calls: Vec<Call>,
    complete: bool,
}
impl Responses {
    pub fn push(&mut self, bytes: &[u8], emit: &mut Emit) -> Result<()> {
        self.lines.feed(bytes, |v| {
            ensure!(!self.complete, "event after completion");
            match string(&v, "type")? {
                "response.output_text.delta" => {
                    let t = v["delta"].as_str().context("missing delta")?;
                    self.text += t;
                    emit(Event::Text(t.into()))
                }
                "response.output_item.done" if v["item"]["type"] == "function_call" => {
                    let item = &v["item"];
                    self.calls.push(Call {
                        id: string(item, "call_id")?.into(),
                        name: string(item, "name")?.into(),
                        args: serde_json::from_str(string(item, "arguments")?)?,
                    })
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
        Ok((self.text, self.calls))
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
            Message::User(t) => input.push(json!({"role":"user","content":t})),
            Message::Assistant { text, calls } => {
                if !text.is_empty() {
                    input.push(json!({"role":"assistant","content":text}))
                }
                for c in calls {
                    input.push(json!({"type":"function_call","call_id":call_id(&c.id),"name":c.name,"arguments":c.args.to_string()}))
                }
            }
            Message::Result { id, text, .. } => input
                .push(json!({"type":"function_call_output","call_id":call_id(id),"output":text})),
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
                is_error: true,
            },
        ];
        let body = responses_body("m", "s", &history, &[]);
        assert_eq!(body["input"][0]["call_id"], "call1");
        assert_eq!(body["input"][1]["call_id"], "call1");
    }
}
