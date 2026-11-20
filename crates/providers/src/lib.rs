use anyhow::{Context, Result, ensure};
use panda_core::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Stream {
    partial: Vec<u8>,
    text: String,
    calls: BTreeMap<u64, (String, String, String)>,
    done: bool,
    finished: bool,
    length_stop: bool,
    output_tokens: Option<u64>,
    total: usize,
}
impl Stream {
    pub fn push(&mut self, bytes: &[u8], emit: &mut Emit) -> Result<()> {
        self.total = self.total.saturating_add(bytes.len());
        ensure!(self.total <= 16 * 1024 * 1024, "response size limit");
        for &byte in bytes {
            if byte != b'\n' {
                self.partial.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.partial);
            let line = std::str::from_utf8(&line)?.trim_end();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            ensure!(!self.done, "event after stream end");
            if data == "[DONE]" {
                self.done = true;
                continue;
            }
            let event: Value = serde_json::from_str(data)?;
            ensure!(
                event.get("error").is_none(),
                "provider reported a streaming error"
            );
            if let Some(usage) = event.get("usage").filter(|v| !v.is_null()) {
                self.output_tokens = usage["completion_tokens"].as_u64();
                emit(Event::Usage(usage.clone()))
            }
            let Some(choice) = event["choices"].get(0) else {
                continue;
            };
            ensure!(!self.finished, "choice after finish reason");
            if let Some(reason) = choice["finish_reason"].as_str() {
                ensure!(
                    matches!(reason, "tool_calls" | "stop" | "length"),
                    "provider stopped: {reason}"
                );
                self.finished = true;
                self.length_stop = reason == "length";
            }
            let d = &choice["delta"];
            if let Some(t) = d["reasoning_content"].as_str().or(d["reasoning"].as_str()) {
                emit(Event::Reasoning(t.into()));
            }
            if let Some(t) = d["content"].as_str() {
                self.text.push_str(t);
                emit(Event::Text(t.into()))
            }
            if let Some(parts) = d["tool_calls"].as_array() {
                for part in parts {
                    let index = part["index"].as_u64().context("missing tool index")?;
                    ensure!(index < 128, "too many tool calls");
                    let (id, name, args) = self.calls.entry(index).or_default();
                    for (dst, src) in [
                        (id, &part["id"]),
                        (name, &part["function"]["name"]),
                        (args, &part["function"]["arguments"]),
                    ] {
                        if let Some(v) = src.as_str() {
                            dst.push_str(v)
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<(String, Vec<Call>)> {
        self.finish_with_limit(None)
    }
    fn finish_with_limit(self, limit: Option<u64>) -> Result<(String, Vec<Call>)> {
        ensure!(
            self.done && self.finished && self.partial.is_empty(),
            "incomplete stream"
        );
        if self.length_stop {
            return Err(LengthStop {
                text: self.text,
                tool_fragments: self
                    .calls
                    .into_values()
                    .map(|(id, name, arguments)| json!({"id":id,"name":name,"arguments":arguments}))
                    .collect(),
                output_tokens: self.output_tokens,
                output_limit: limit,
                ..Default::default()
            }
            .into());
        }
        let calls = self
            .calls
            .into_values()
            .map(|(id, name, args)| {
                ensure!(!id.is_empty() && !name.is_empty(), "missing tool identity");
                Ok(Call {
                    id,
                    name,
                    args: serde_json::from_str(&args)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok((self.text, calls))
    }
}
pub struct Api {
    pub endpoint: String,
    pub model: String,
    pub key: String,
}
struct ChatProvider<A> {
    api: A,
    max_tokens: Option<u64>,
}
impl Api {
    pub fn with_output_limit(self, max_tokens: Option<u64>) -> impl Provider + Send {
        ChatProvider {
            api: self,
            max_tokens,
        }
    }
}
fn messages(system: &str, history: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    let mut thought = None;
    // Chat tool messages accept text. Finish every outstanding tool response
    // before sending its images in a multimodal user message.
    let mut image_parts = Vec::new();
    let mut history = history.iter().peekable();
    while let Some(msg) = history.next() {
        let message = match msg {
            Message::Truncated(_) | Message::CodemodeStore(_) => continue,
            Message::BranchSummary(s) => {
                json!({"role":"user","content":format!("Summary from another branch:\n{s}")})
            }
            Message::Reasoning(t) => {
                thought = Some(t);
                continue;
            }
            Message::Images { text, urls } => {
                let mut parts = vec![json!({"type":"text","text":text})];
                parts.extend(
                    urls.iter()
                        .map(|u| json!({"type":"image_url","image_url":{"url":u}})),
                );
                json!({"role":"user","content":parts})
            }
            Message::User(text) => json!({"role":"user","content":text}),
            Message::Result {
                id,
                text,
                images,
                details,
                ..
            } => {
                if let Some(panda_core::ToolDetails::Codemode {
                    images: positions, ..
                }) = details
                    && !positions.is_empty()
                {
                    image_parts.extend(
                        panda_core::parts(text, images, details.as_ref())
                            .into_iter()
                            .map(|part| match part {
                                panda_core::Part::Text(text) => json!({"type":"text","text":text}),
                                panda_core::Part::Image(url) => {
                                    json!({"type":"image_url","image_url":{"url":url}})
                                }
                            }),
                    );
                    let paths = positions
                        .iter()
                        .map(|p| p.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    json!({"role":"tool","tool_call_id":id,"content":format!("Codemode output follows. Saved images: {paths}")})
                } else {
                    image_parts.extend(
                        images
                            .iter()
                            .map(|url| json!({"type":"image_url","image_url":{"url":url}})),
                    );
                    json!({"role":"tool","tool_call_id":id,"content":text})
                }
            }
            Message::Assistant { text, calls } => {
                let mut m = json!({"role":"assistant","content":text});
                if let Some(t) = thought.take() {
                    m["reasoning_content"] = json!(t);
                }
                if !calls.is_empty() {
                    m["tool_calls"]=Value::Array(calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.args.to_string()}})).collect())
                }
                m
            }
        };
        out.push(message);
        if !image_parts.is_empty() && !matches!(history.peek(), Some(Message::Result { .. })) {
            out.push(json!({"role":"user","content":std::mem::take(&mut image_parts)}));
        }
    }
    out
}
impl Provider for Api {
    fn reply(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.reply_with_effort(system, h, tools, cancel, emit, None)
    }
    fn reply_with_effort(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        ChatProvider {
            api: &*self,
            max_tokens: None,
        }
        .reply_with_effort(system, h, tools, cancel, emit, effort)
    }
}
impl<A: std::borrow::Borrow<Api>> Provider for ChatProvider<A> {
    fn reply(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.reply_with_effort(system, h, tools, cancel, emit, None)
    }
    fn reply_with_effort(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        let api = self.api.borrow();
        cancel.check()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let request = async {
            let mut body = json!({"model": api.model, "messages": messages(system,h), "stream": true, "stream_options":{"include_usage":true}});
            let deepseek = api.endpoint == "https://api.deepseek.com/chat/completions";
            let limit = self.max_tokens.or_else(|| {
                // Pin the documented DeepSeek default so the length comparison
                // uses the allowance actually sent on this request.
                (deepseek
                    && matches!(
                        api.model.as_str(),
                        "deepseek-flash" | "deepseek-v4-pro" | "deepseek-v4-flash"
                    ))
                .then_some(match effort {
                    Some("none") => 8192,
                    Some("max") => 131072,
                    _ => 65536,
                })
            });
            if let Some(limit) = limit {
                body["max_tokens"] = json!(limit);
            }
            if let Some(effort) = effort {
                if api.endpoint.contains("api.deepseek.com") {
                    body["reasoning_effort"] = json!(effort);
                } else {
                    body["reasoning"] = json!({"effort":effort});
                }
            }
            if !tools.is_empty() {
                body["tools"] = tools.into();
            }
            let response = reqwest::Client::new()
                .post(&api.endpoint)
                .bearer_auth(&api.key)
                .json(&body)
                .send()
                .await
                .map_err(transport_error)?;
            let mut response = checked(response).await?;
            let mut stream = Stream::default();
            while let Some(chunk) = response.chunk().await.map_err(|e| {
                if stream.length_stop {
                    e.without_url().into()
                } else {
                    transport_error(e)
                }
            })? {
                cancel.check()?;
                stream.push(&chunk, emit)?;
            }
            stream.finish_with_limit(limit)
        };
        rt.block_on(async {
            tokio::pin!(request);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(40));
            loop {
                tokio::select! {
                    result = &mut request => return result,
                    _ = tick.tick() => cancel.check()?,
                }
            }
        })
    }
}
pub mod subscription;

fn transport_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_connect() || error.is_timeout() || error.is_body() {
        return TransientFailure {
            reason: "provider connection interrupted".into(),
            retry_after: None,
        }
        .into();
    }
    error.without_url().into()
}
fn retry_after(value: &str) -> Option<std::time::Duration> {
    use std::time::{Duration, SystemTime};
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
        })
}

pub(crate) async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let delay = response
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after);
    let error = response.json::<Value>().await.unwrap_or(Value::Null);
    let code = error["error"]["code"].as_str().unwrap_or_default();
    let message = error["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    if matches!(code, "context_length_exceeded" | "context_window_exceeded")
        || (status.as_u16() == 400
            && message.contains("context")
            && (message.contains("exceed") || message.contains("maximum")))
    {
        return Err(panda_core::ContextOverflow.into());
    }
    if matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
        && !matches!(
            code,
            "insufficient_quota" | "billing_hard_limit_reached" | "usage_limit_reached"
        )
    {
        return Err(TransientFailure {
            reason: format!("provider returned HTTP {status}"),
            retry_after: delay,
        }
        .into());
    }
    anyhow::bail!("provider returned HTTP {status} {code}")
}

pub mod models;

#[cfg(test)]
mod tests {
    use super::*;
    proptest::proptest! {
        #[test]
        fn length_stop_survives_fragmented_terminal_delta(width in 1..100usize, used in 1..150u64) {
            let terminal = json!({"choices":[{"finish_reason":"length","delta":{
                "content":"partial café", "tool_calls":[{"index":0,"function":{"arguments":"{"}}]
            }}]});
            let usage = json!({"choices":[],"usage":{"completion_tokens":used}});
            let wire = format!("data: {terminal}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
            let mut stream = Stream::default();
            for chunk in wire.as_bytes().chunks(width) {
                stream.push(chunk, &mut |_| {}).unwrap();
            }
            let cutoff = stream.finish_with_limit(Some(100)).unwrap_err().downcast::<LengthStop>().unwrap();
            proptest::prop_assert_eq!(cutoff.text, "partial café");
            proptest::prop_assert_eq!(cutoff.tool_fragments[0]["arguments"].as_str(), Some("{"));
            proptest::prop_assert_eq!((cutoff.output_tokens, cutoff.output_limit), (Some(used), Some(100)));
        }
    }
    #[test]
    fn fragmented_call_at_every_chunk_width() {
        let data = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"é\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
        );
        for width in 1..=data.len() {
            let mut s = Stream::default();
            for chunk in data.as_bytes().chunks(width) {
                s.push(chunk, &mut |_| {}).unwrap()
            }
            let (_, calls) = s.finish().unwrap();
            assert_eq!(
                calls,
                vec![Call {
                    id: "abc".into(),
                    name: "read".into(),
                    args: json!({"path":"é"})
                }]
            );
        }
    }
}
mod auth;
pub mod wire;
