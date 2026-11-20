use super::*;
use panda_core::sdk::Entry;
use std::collections::{BTreeMap, VecDeque};
fn ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub(super) fn message(m: &Message, name: Option<&str>, s: &Session, index: usize) -> Result<Value> {
    if let Message::BranchSummary(t) = m {
        return Ok(json!({"role":"branchSummary","summary":t,"timestamp":0}));
    }
    let encoded = panda_local::store::pi_message(
        m,
        0,
        name,
        s.usage.iter().find(|u| u.through == index + 1),
        if matches!(m, Message::Assistant { .. }) {
            index
                .checked_sub(1)
                .and_then(|i| s.history.get(i))
                .and_then(|m| match m {
                    Message::Reasoning(r) => Some(r.as_str()),
                    _ => None,
                })
        } else {
            None
        },
    )?;
    let mut value = encoded["message"].clone();
    if value["role"] == "assistant"
        && let Some(id) = s.responders.get(&index).or(s.model.as_ref())
    {
        value["provider"] = json!(id.provider);
        value["model"] = json!(id.model);
    }
    Ok(value)
}
pub(super) fn messages(s: &Session) -> Result<Vec<Value>> {
    messages_since(s, 0)
}
fn messages_since(s: &Session, start: usize) -> Result<Vec<Value>> {
    let mut calls = BTreeMap::new();
    let mut messages = vec![];
    for (i, m) in s.history.iter().enumerate() {
        let name = match m {
            Message::Assistant { calls: batch, .. } => {
                for c in batch {
                    calls.insert(&c.id, c.name.as_str());
                }
                None
            }
            Message::Result { id, .. } => calls.get(id).copied(),
            _ => None,
        };
        if i < start
            || (matches!(m, Message::Reasoning(_))
                && matches!(s.history.get(i + 1), Some(Message::Assistant { .. })))
        {
            continue;
        }
        let value = message(m, name, s, i)?;
        if !value.is_null() {
            messages.push(value);
        }
    }
    Ok(messages)
}
pub(super) fn stats(host: &dyn SessionHost) -> Result<Value> {
    let s = host.load()?;
    let totals = sdk::UsageTotals::from(&s);
    let mut prompts = 0;
    let mut answers = 0;
    let mut results = 0;
    let mut calls = 0;
    for m in &s.history {
        match m {
            Message::User(_) | Message::Images { .. } => prompts += 1,
            Message::Assistant { calls: batch, .. } => {
                answers += 1;
                calls += batch.len();
            }
            Message::Result { .. } => results += 1,
            _ => {}
        }
    }
    let mut data = json!({"sessionFile":format!("panda-session:{}",host.session_id()),"sessionId":host.session_id().to_string(),
        "userMessages":prompts,"assistantMessages":answers,"toolCalls":calls,"toolResults":results,"totalMessages":messages(&s)?.len(),
        "tokens":{"input":totals.input.saturating_sub(totals.read).saturating_sub(totals.write),"output":totals.output,"cacheRead":totals.read,
        "cacheWrite":totals.write,"total":totals.input.saturating_add(totals.output)},"cost":totals.cost});
    if let Some(window) = s.model_window.filter(|n| *n > 0) {
        let tokens = s
            .usage
            .last()
            .filter(|u| s.checkpoint.as_ref().is_none_or(|c| u.through > c.through))
            .map(|u| u.input.saturating_add(u.output));
        data["contextUsage"] = json!({"contextWindow":window,"tokens":tokens,"percent":tokens.map(|t|100.*t as f64/window as f64)});
    }
    Ok(data)
}
pub(super) fn active_branch(rows: &[Entry]) -> Vec<&Entry> {
    let by_id: BTreeMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    let mut leaf = rows.iter().find(|r| r.active);
    let mut branch = Vec::new();
    while let Some(entry) = leaf {
        branch.push(entry);
        leaf = entry.parent.and_then(|p| by_id.get(&p).copied());
    }
    branch.reverse();
    branch
}
pub(super) fn entries(
    host: &mut dyn SessionHost,
    tree: bool,
    since: Option<&str>,
) -> Result<Value> {
    let rows = host.entries()?;
    let state = host.load()?;
    let leaf = rows.iter().find(|e| e.active).map(|e| e.id.to_string());
    let mut wire = BTreeMap::new();
    let mut children: BTreeMap<Option<i64>, Vec<i64>> = BTreeMap::new();
    let by_id: BTreeMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    for row in &rows {
        let mut entry = if let Some(m) = &row.message {
            let mut name = None;
            if let Message::Result { id, .. } = m {
                let mut parent = row.parent;
                while let Some(old) = parent.and_then(|p| by_id.get(&p)) {
                    if let Some(Message::Assistant { calls, .. }) = &old.message
                        && let Some(call) = calls.iter().find(|c| c.id == *id)
                    {
                        name = Some(call.name.as_str());
                        break;
                    }
                    parent = old.parent;
                }
            }
            let message = message(m, name, &state, usize::MAX - 1)?;
            if message.is_null() {
                json!({"type":"custom","customType":"panda-event","data":m})
            } else {
                json!({"type":"message","message":message})
            }
        } else {
            json!({"type":"custom","customType":"panda-metadata"})
        };
        entry["id"] = json!(row.id.to_string());
        entry["parentId"] = json!(row.parent.map(|n| n.to_string()));
        wire.insert(row.id, entry);
        children.entry(row.parent).or_default().push(row.id);
    }
    if !tree {
        let cursor = since.map(str::parse::<i64>).transpose()?;
        ensure!(
            cursor.is_none_or(|id| wire.contains_key(&id)),
            "unknown entry cursor"
        );
        return Ok(
            json!({"entries":wire.into_iter().filter(|(id,_)|cursor.is_none_or(|c|*id>c)).map(|(_,v)|v).collect::<Vec<_>>(),"leafId":leaf}),
        );
    }
    let mut nodes = BTreeMap::new();
    for row in rows.iter().rev() {
        let kids: Vec<_> = children
            .remove(&Some(row.id))
            .unwrap_or_default()
            .iter()
            .filter_map(|id| nodes.remove(id))
            .collect();
        let mut node = json!({"entry":wire.remove(&row.id),"children":kids});
        if let Some(label) = &row.label {
            node["label"] = json!(label);
        }
        nodes.insert(row.id, node);
    }
    Ok(
        json!({"tree":children.remove(&None).unwrap_or_default().iter().filter_map(|id|nodes.remove(id)).collect::<Vec<_>>(),"leafId":leaf}),
    )
}
fn usage(u: &UsageRecord) -> Value {
    let read = u.cache_read.unwrap_or(0);
    let write = u.cache_write.unwrap_or(0);
    json!({"input":u.input.saturating_sub(read).saturating_sub(write),"output":u.output,"cacheRead":read,"cacheWrite":write,
        "totalTokens":u.input.saturating_add(u.output),"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":u.cost.unwrap_or(0.)}})
}
pub(super) struct Stream {
    start: usize,
    seen: usize,
    assistant_open: bool,
    turn_open: bool,
    usage: UsageRecord,
    block: Option<(&'static str, String)>,
    block_index: usize,
    calls: VecDeque<Call>,
    message: Value,
    results: Vec<Value>,
    waiting: usize,
}
impl Stream {
    pub(super) fn start(depth: usize) -> Self {
        println!("{}", json!({"type":"agent_start"}));
        println!("{}", json!({"type":"turn_start"}));
        Self {
            start: depth,
            seen: depth,
            assistant_open: false,
            turn_open: true,
            usage: UsageRecord::default(),
            block: None,
            block_index: 0,
            calls: VecDeque::new(),
            message: Value::Null,
            results: vec![],
            waiting: 0,
        }
    }
    fn open_turn(&mut self) {
        if !self.turn_open {
            println!("{}", json!({"type":"turn_start"}));
            self.turn_open = true;
        }
    }
    fn begin(&mut self, m: &Value) {
        self.open_turn();
        println!("{}", json!({"type":"message_start","message":m}));
        self.assistant_open = true;
    }
    fn update(&self, event: Value) {
        println!(
            "{}",
            json!({"type":"message_update","usage":usage(&self.usage),"assistantMessageEvent":event})
        );
    }
    fn end_block(&mut self) {
        if let Some((kind, text)) = self.block.take() {
            self.update(json!({"type":format!("{kind}_end"),"contentIndex":self.block_index,"content":text}));
            self.block_index += 1;
        }
    }
    fn delta(&mut self, kind: &'static str, delta: String) {
        if self.block.as_ref().is_none_or(|(k, _)| *k != kind) {
            self.end_block();
            self.update(json!({"type":format!("{kind}_start"),"contentIndex":self.block_index}));
            self.block = Some((kind, String::new()));
        }
        self.block.as_mut().unwrap().1.push_str(&delta);
        self.update(
            json!({"type":format!("{kind}_delta"),"contentIndex":self.block_index,"delta":delta}),
        );
    }
    fn sync(&mut self, state: &Session) -> Result<()> {
        if self.seen >= state.history.len() {
            return Ok(());
        }
        for index in self.seen..state.history.len() {
            let m = &state.history[index];
            let name = if let Message::Result { id, .. } = m {
                state.history[..index].iter().rev().find_map(|old| {
                    if let Message::Assistant { calls, .. } = old {
                        calls.iter().find(|c| c.id == *id).map(|c| c.name.as_str())
                    } else {
                        None
                    }
                })
            } else {
                None
            };
            let value = message(m, name, state, index)?;
            if value.is_null() || matches!(m, Message::Reasoning(_)) {
                continue;
            }
            self.open_turn();
            if !self.assistant_open || value["role"] != "assistant" {
                println!("{}", json!({"type":"message_start","message":value}));
            }
            if matches!(m, Message::Assistant { .. }) {
                self.end_block();
                self.block_index = 0;
                self.usage = UsageRecord::default();
            }
            println!("{}", json!({"type":"message_end","message":value}));
            match m {
                Message::Assistant { calls, .. } => {
                    self.message = value;
                    self.assistant_open = false;
                    self.waiting = calls.len();
                    self.results.clear();
                }
                Message::Result { .. } => {
                    self.results.push(value);
                    self.waiting = self.waiting.saturating_sub(1);
                }
                _ => continue,
            }
            if self.waiting == 0 {
                self.turn_open = false;
                println!(
                    "{}",
                    json!({"type":"turn_end","message":self.message,"toolResults":self.results})
                );
            }
        }
        self.seen = state.history.len();
        Ok(())
    }
    pub(super) fn event(&mut self, e: Event, store: &dyn Store) {
        match e {
            event @ (Event::Text(_) | Event::Reasoning(_)) => {
                let (ty, delta) = match event {
                    Event::Text(v) => ("text", v),
                    Event::Reasoning(v) => ("thinking", v),
                    _ => unreachable!(),
                };
                if !self.assistant_open {
                    if let Ok(s) = store.load() {
                        let _ = self.sync(&s);
                    }
                    let selected = store.load().ok().and_then(|s| s.model);
                    self.begin(&json!({"role":"assistant","content":[],"timestamp":ms(),"api":"openai-completions",
                        "provider":selected.as_ref().map(|m|m.provider.as_str()), "model":selected.as_ref().map(|m|m.model.as_str()),
                        "stopReason":"pending","usage":usage(&self.usage)}));
                }
                self.delta(ty, delta);
            }
            Event::Usage(value) => {
                if let Some(report) = UsageRecord::from_event(&value) {
                    self.usage = report;
                }
            }
            Event::Calling(c) => {
                if let Ok(s) = store.load() {
                    let _ = self.sync(&s);
                }
                println!(
                    "{}",
                    json!({"type":"tool_execution_start","toolCallId":c.id,"toolName":c.name,"args":c.args})
                );
                self.calls.push_back(c);
            }
            Event::Result(text) => {
                if let Some(c) = self.calls.pop_front() {
                    println!(
                        "{}",
                        json!({"type":"tool_execution_end","toolCallId":c.id,"toolName":c.name,"result":{"content":[{"type":"text","text":text}]},"isError":text.starts_with("tool error:")})
                    );
                }
                if let Ok(s) = store.load() {
                    let _ = self.sync(&s);
                }
            }
            Event::ToolOutput(text) => {
                if let Some(c) = self.calls.front() {
                    println!(
                        "{}",
                        json!({"type":"tool_execution_update","toolCallId":c.id,"toolName":c.name,"partialResult":{"content":[{"type":"text","text":text}]}})
                    );
                }
            }
            Event::Retry { attempt, seconds } => println!(
                "{}",
                json!({"type":"auto_retry_start","attempt":attempt,"delayMs":seconds*1000})
            ),
            _ => {}
        }
    }
    pub(super) fn finish(
        &mut self,
        state: &Session,
        error: Option<&str>,
        aborted: bool,
        settled: bool,
    ) -> Result<()> {
        self.sync(state)?;
        let mut out = messages_since(state, self.start)?;
        if let Some(error) = error {
            let m = json!({"role":"assistant","content":[],"timestamp":ms(),"stopReason":if aborted{"aborted"}else{"error"},"errorMessage":error});
            if !self.assistant_open {
                self.begin(&m);
            }
            self.end_block();
            println!("{}", json!({"type":"message_end","message":m}));
            if self.turn_open {
                println!(
                    "{}",
                    json!({"type":"turn_end","message":m,"toolResults":self.results})
                );
            }
            out.push(m);
        }
        println!(
            "{}",
            json!({"type":"agent_end","messages":out,"willRetry":false})
        );
        if settled {
            println!("{}", json!({"type":"agent_settled","aborted":aborted}));
        }
        Ok(())
    }
}
