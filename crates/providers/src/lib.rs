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
                emit(Event::Usage(usage.clone()))
            }
            let Some(choice) = event["choices"].get(0) else {
                continue;
            };
            if let Some(reason) = choice["finish_reason"].as_str() {
                ensure!(
                    matches!(reason, "tool_calls" | "stop"),
                    "provider stopped: {reason}"
                );
                self.finished = true;
            }
            let d = &choice["delta"];
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
        ensure!(
            self.done && self.finished && self.partial.is_empty(),
            "incomplete stream"
        );
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
fn messages(system: &str, history: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    for msg in history {
        let message = match msg {
            Message::User(text) => json!({"role":"user","content":text}),
            Message::Result { id, text, .. } => {
                json!({"role":"tool","tool_call_id":id,"content":text})
            }
            Message::Assistant { text, calls } => {
                let mut m = json!({"role":"assistant","content":text});
                if !calls.is_empty() {
                    m["tool_calls"]=Value::Array(calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.args.to_string()}})).collect())
                }
                m
            }
        };
        out.push(message);
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
        cancel.check()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let request = async {
            let mut body =
                json!({"model": self.model, "messages": messages(system,h), "stream": true});
            if !tools.is_empty() {
                body["tools"] = tools.into();
            }
            let mut response = reqwest::Client::new()
                .post(&self.endpoint)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
            let mut stream = Stream::default();
            while let Some(chunk) = response.chunk().await? {
                cancel.check()?;
                stream.push(&chunk, emit)?;
            }
            stream.finish()
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

#[cfg(test)]
mod tests {
    use super::*;
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
