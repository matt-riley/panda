use super::*;
use anyhow::Context;
use panda_core::LengthStop;
use std::collections::BTreeMap;

pub(super) struct Decoder {
    family: Family,
    text: String,
    calls: Vec<Call>,
    replay: Vec<Value>,
    blocks: BTreeMap<u64, Block>,
    tool_blocks: Vec<Block>,
    usage: Value,
    done: bool,
    stop: String,
    history_len: usize,
    limit: u64,
}
struct Block {
    raw: Value,
    arguments: String,
}
fn append(field: &mut Value, delta: &str) {
    if let Value::String(current) = field {
        current.push_str(delta);
    } else {
        *field = json!(delta);
    }
}

impl Decoder {
    pub fn new(family: Family, history_len: usize, limit: u64) -> Self {
        Self {
            family,
            history_len,
            limit,
            text: String::new(),
            calls: vec![],
            replay: vec![],
            blocks: BTreeMap::new(),
            tool_blocks: Vec::new(),
            usage: json!({}),
            done: false,
            stop: String::new(),
        }
    }
    pub fn push(&mut self, v: Value, emit: &mut Emit) -> Result<()> {
        ensure!(
            v.get("error").is_none(),
            "provider reported a streaming error"
        );
        if self.family == Family::Anthropic {
            self.anthropic(&v, emit)
        } else {
            self.gemini(&v, emit)
        }
    }
    fn anthropic(&mut self, v: &Value, emit: &mut Emit) -> Result<()> {
        ensure!(!self.done, "event after message_stop");
        let idx = || {
            v["index"]
                .as_u64()
                .filter(|i| *i < 1024)
                .context("invalid content block index")
        };
        match wire::string(v, "type")? {
            "message_start" => {
                let m = &v["message"];
                if let Some(model) = m["model"].as_str() {
                    emit(Event::Responder(model.into()));
                }
                if let Some(u) = m["usage"].as_object() {
                    self.usage = json!(u);
                }
            }
            "content_block_start" => {
                let raw = v["content_block"].clone();
                let block = Block {
                    raw,
                    arguments: String::new(),
                };
                ensure!(
                    self.blocks.insert(idx()?, block).is_none(),
                    "duplicate content block"
                );
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let b = self
                    .blocks
                    .get_mut(&idx()?)
                    .context("delta without content block")?;
                match wire::string(d, "type")? {
                    "text_delta" => {
                        let t = wire::string(d, "text")?;
                        self.text.push_str(t);
                        emit(Event::Text(t.into()));
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().context("expected thinking delta")?;
                        append(&mut b.raw["thinking"], t);
                        emit(Event::Reasoning(t.into()));
                    }
                    "signature_delta" => {
                        let sig = wire::string(d, "signature")?;
                        append(&mut b.raw["signature"], sig);
                    }
                    "input_json_delta" => b
                        .arguments
                        .push_str(d["partial_json"].as_str().context("invalid input delta")?),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let b = self
                    .blocks
                    .remove(&idx()?)
                    .context("stop without content block")?;
                match b.raw["type"].as_str() {
                    Some("tool_use") => self.tool_blocks.push(b),
                    Some("thinking") if b.raw["signature"].is_string() => self.replay.push(b.raw),
                    Some("redacted_thinking") => self.replay.push(b.raw),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(stop) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = stop.into();
                }
                if let Some(u) = v["usage"].as_object() {
                    self.usage.as_object_mut().unwrap().extend(u.clone());
                }
            }
            "message_stop" => self.done = true,
            "error" => bail!("Anthropic stream failed"),
            _ => {}
        }
        Ok(())
    }
    fn gemini(&mut self, v: &Value, emit: &mut Emit) -> Result<()> {
        if let Some(model) = v["modelVersion"].as_str() {
            emit(Event::Responder(model.into()));
        }
        if let Some(u) = v.get("usageMetadata") {
            let input = u["promptTokenCount"].as_u64().unwrap_or(0);
            let output = u["candidatesTokenCount"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(u["thoughtsTokenCount"].as_u64().unwrap_or(0));
            self.usage = json!({"input_tokens":input,"output_tokens":output,"input_tokens_details":{"cached_tokens":u["cachedContentTokenCount"]}});
        }
        if let Some(r) = v
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            bail!("Gemini blocked prompt: {r}");
        }
        let Some(c) = v["candidates"].get(0) else {
            return Ok(());
        };
        ensure!(!self.done, "candidate after finish reason");
        if let Some(parts) = c.pointer("/content/parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(t) = p["text"].as_str() {
                    if let Some(sig) = p["thoughtSignature"].as_str() {
                        self.replay.push(json!({"text":t,"thought":p["thought"]==true,"signature":sig,"offset":self.text.len()}));
                    }
                    if p["thought"] == true {
                        emit(Event::Reasoning(t.into()));
                    } else {
                        self.text += t;
                        emit(Event::Text(t.into()));
                    }
                }
                if let Some(f) = p.get("functionCall") {
                    ensure!(
                        f.get("partialArgs").is_none()
                            && f.get("willContinue") != Some(&Value::Bool(true)),
                        "unexpected incremental Gemini arguments"
                    );
                    let name = wire::string(f, "name")?;
                    let id = f["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                        format!("gemini-{}-{}", self.history_len, self.calls.len())
                    });
                    let args = f.get("args").cloned().unwrap_or_else(|| json!({}));
                    ensure!(args.is_object(), "invalid Gemini function arguments");
                    if let Some(sig) = p["thoughtSignature"].as_str() {
                        self.replay.push(json!({"call":id,"signature":sig}));
                    }
                    self.calls.push(Call {
                        id,
                        name: name.to_owned(),
                        args,
                    });
                }
            }
        }
        if let Some(finish) = c["finishReason"].as_str() {
            self.stop = finish.into();
            self.done = true;
        }
        Ok(())
    }
    pub fn finish(mut self, emit: &mut Emit) -> Result<(String, Vec<Call>, Vec<Value>)> {
        if !self.usage.as_object().unwrap().is_empty() {
            emit(Event::Usage(self.usage.clone()));
        }
        ensure!(self.done, "incomplete native stream");
        if matches!(self.stop.as_str(), "MAX_TOKENS" | "max_tokens") {
            return Err(LengthStop {
                text: self.text,
                tool_fragments: self
                    .tool_blocks
                    .into_iter()
                    .chain(self.blocks.into_values())
                    .filter(|b| b.raw["type"] == "tool_use")
                    .map(|b| json!({"id":b.raw["id"],"name":b.raw["name"],"arguments":b.arguments}))
                    .collect(),
                output_limit: Some(self.limit),
                output_tokens: self.usage["output_tokens"].as_u64(),
                ..Default::default()
            }
            .into());
        }
        ensure!(
            self.blocks.is_empty()
                && matches!(
                    self.stop.as_str(),
                    "end_turn" | "stop_sequence" | "tool_use" | "STOP"
                ),
            "provider stopped: {}",
            self.stop
        );
        for b in self.tool_blocks {
            let args = if b.arguments.is_empty() {
                b.raw["input"].clone()
            } else {
                serde_json::from_str(&b.arguments)?
            };
            ensure!(args.is_object(), "invalid tool input");
            self.calls.push(Call {
                id: wire::string(&b.raw, "id")?.into(),
                name: wire::string(&b.raw, "name")?.into(),
                args,
            });
        }
        Ok((self.text, self.calls, self.replay))
    }
}
