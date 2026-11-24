//! Versioned JSONL adapters. Both wire dialects drive the same session and run loop.
mod pi;
mod shared;
use super::{Host, ModelControls};
use anyhow::{Context, Result, bail, ensure};
use panda_core::{
    sdk::{Delivery, Queue, SessionHost, SessionSdk},
    *,
};
use serde_json::{Value, json};
use shared::Shared;
use std::{
    io::{self, BufRead},
    sync::{Mutex, mpsc},
    time::Duration,
};

fn reply(pi: bool, req: &Value, result: Result<Value>) {
    let mut out = match result {
        Ok(data) if pi => {
            let mut out = json!({"type":"response","command":req["type"],"success":true});
            if !data.is_null() {
                out["data"] = data;
            }
            out
        }
        Ok(data) => data,
        Err(e) if pi => {
            json!({"type":"response","command":req["type"],"success":false,"error":e.to_string()})
        }
        Err(e) => json!({"type":"error","error":e.to_string()}),
    };
    if let Some(id) = req.get("id") {
        out["id"] = id.clone();
    }
    println!("{out}");
}
fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .with_context(|| format!("expected string {name}"))
}
fn prompt_input(req: &Value, key: &str, root: Option<&std::path::Path>) -> Result<Message> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let text = field(req, key)?;
    let base = match root {
        Some(root) => crate::attachments::prepare(root, text, None)?,
        None => Message::User(text.to_string()),
    };
    let Some(images) = req.get("images") else {
        return Ok(base);
    };
    let (text, mut urls) = match base {
        Message::User(t) => (t, vec![]),
        Message::Images { text, urls } => (text, urls),
        _ => unreachable!(),
    };
    let mut encoded_bytes: usize = urls.iter().map(String::len).sum();
    for image in images.as_array().context("expected images array")? {
        ensure!(image["type"] == "image", "expected image content");
        let mime = field(image, "mimeType")?;
        let data = field(image, "data")?;
        encoded_bytes = encoded_bytes.saturating_add(data.len());
        ensure!(
            encoded_bytes <= 8_388_608usize.div_ceil(3) * 4,
            "images exceed 8 MiB"
        );
        let decoded = STANDARD.decode(data)?;
        ensure!(
            panda_coding::image_mime_type(&decoded) == Some(mime),
            "image content does not match MIME type"
        );
        urls.push(format!("data:{mime};base64,{data}"));
    }
    Ok(if urls.is_empty() {
        Message::User(text)
    } else {
        Message::Images { text, urls }
    })
}
fn queue_text(messages: Vec<Message>) -> Vec<String> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            Message::User(t) | Message::Images { text: t, .. } => Some(t),
            _ => None,
        })
        .collect()
}
fn mode(one: bool) -> &'static str {
    if one { "one-at-a-time" } else { "all" }
}
fn state(
    host: &dyn SessionHost,
    queue: &Mutex<Queue<Message>>,
    active: Option<&str>,
    models: &ModelControls,
) -> Result<Value> {
    let s = host.load()?;
    let q = queue.lock().unwrap();
    let mut out = json!({"sessionId":host.session_id().to_string(),"sessionFile":format!("panda-session:{}",host.session_id()),
        "thinkingLevel":s.reasoning.as_deref().unwrap_or("off"),"messageCount":s.history.len(),
        "steeringMode":mode(q.delivery.single_steer),"followUpMode":mode(q.delivery.single_follow),
        "isStreaming":matches!(active,Some("prompt"|"retry")),"isCompacting":active==Some("compact"),"autoCompactionEnabled":!s.disable_auto_compaction,"autoRetryEnabled":!s.disable_auto_retry,"pendingMessageCount":q.steer.len()+q.follow.len()});
    if let Some(id) = &s.model {
        out["model"] = (models.describe)(id)?;
    }
    if !s.name.is_empty() {
        out["sessionName"] = s.name.into();
    }
    Ok(out)
}
fn session_id(v: &Value) -> Result<i64> {
    v.as_i64().map(Ok).unwrap_or_else(|| {
        let text = v.as_str().context("expected a session ID or sessionPath")?;
        Ok(text
            .strip_prefix("panda-session:")
            .unwrap_or(text)
            .parse()?)
    })
}
fn entry(v: &Value) -> Result<i64> {
    v.as_i64()
        .map(Ok)
        .unwrap_or_else(|| Ok(v.as_str().context("expected entryId")?.parse()?))
}
fn control(
    host: &mut dyn SessionHost,
    queue: &Mutex<Queue<Message>>,
    req: &Value,
    active: Option<&str>,
    cancel: &Cancel,
    models: &ModelControls,
    pi: bool,
) -> Result<Value> {
    let kind = field(req, "type")?;
    let data = match kind {
        "session" if !pi => {
            return Ok(
                json!({"type":"session","session_id":host.session_id(),"session":host.load()?}),
            );
        }
        "get_state" => state(host, queue, active, models)?,
        "get_session_stats" => pi::stats(host)?,
        "get_messages" => json!({"messages":pi::messages(&host.load()?)?}),
        "get_tree" | "get_entries" => pi::entries(host, kind == "get_tree", req["since"].as_str())?,
        "get_fork_messages" => {
            let prompts = pi::active_branch(&host.entries()?)
                .into_iter()
                .filter_map(|row| {
                    let text = match row.message.as_ref()? {
                        Message::User(t) | Message::Images { text: t, .. } => t,
                        _ => return None,
                    };
                    Some(json!({"entryId":row.id.to_string(),"text":text}))
                })
                .collect::<Vec<_>>();
            json!({"messages":prompts})
        }
        "get_last_assistant_text" => {
            json!({"text":host.load()?.history.iter().rev().find_map(|m|if let Message::Assistant{text,..}=m{Some(text)}else{None})})
        }
        "get_available_models" => {
            json!({"models":(models.list)()?.iter().map(models.describe).collect::<Result<Vec<_>>>()?})
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            let one = match field(req, "mode")? {
                "one-at-a-time" => true,
                "all" => false,
                _ => bail!("invalid queue mode"),
            };
            let mut q = queue.lock().unwrap();
            if kind == "set_steering_mode" {
                q.delivery.single_steer = one
            } else {
                q.delivery.single_follow = one
            };
            Value::Null
        }
        "clear_queue" => {
            let mut q = queue.lock().unwrap();
            json!({"steering":queue_text(std::mem::take(&mut q.steer)),"followUp":queue_text(std::mem::take(&mut q.follow))})
        }
        "cancel" | "abort" | "abort_bash" | "abort_retry" => {
            if kind == "abort_bash" {
                ensure!(active == Some("bash"), "no bash command running");
            }
            if active.is_some() {
                cancel.stop();
            }
            Value::Null
        }
        "steer" | "follow_up" | "prompt" if active.is_some() => {
            ensure!(
                matches!(active, Some("prompt" | "retry")),
                "no active prompt"
            );
            let steering = match kind {
                "steer" => true,
                "follow_up" => false,
                _ => match req["streamingBehavior"].as_str() {
                    Some("steer") => true,
                    Some("followUp") => false,
                    _ => bail!("prompt already active; specify streamingBehavior"),
                },
            };
            let input = prompt_input(req, if pi { "message" } else { "text" }, None)?;
            let mut q = queue.lock().unwrap();
            if steering {
                &mut q.steer
            } else {
                &mut q.follow
            }
            .push(input);
            if !pi {
                return Ok(json!({"type":"queued"}));
            }
            json!({"disposition":"queued"})
        }
        _ if active.is_some() => bail!("turn active"),
        "ack" => {
            acknowledge(host)?;
            Value::Null
        }
        "new_session" | "switch_session" | "clone" | "fork" | "navigate" => {
            let mut result = json!({"cancelled":false});
            match kind {
                "new_session" => SessionSdk { host }.new_session(req["parentSession"].as_str())?,
                "switch_session" => {
                    if let Some(text) = req["sessionPath"].as_str() {
                        host.switch_reference(text)?;
                    } else {
                        host.switch(Some(session_id(&req["sessionPath"])?))?;
                    }
                }
                "clone" => host.fork_turn(None, req["name"].as_str().unwrap_or(""))?,
                "fork" => {
                    let draft = host.fork_prompt(entry(&req["entryId"])?)?;
                    if let Message::User(t) | Message::Images { text: t, .. } = draft {
                        result["text"] = t.into();
                    }
                }
                _ => {
                    result["draft"] = json!(host.navigate_entry(entry(&req["entryId"])?)?);
                }
            }
            let mut q = queue.lock().unwrap();
            q.steer.clear();
            q.follow.clear();
            result
        }
        "set_auto_compaction" | "set_auto_retry" => {
            let enabled = req["enabled"]
                .as_bool()
                .context("expected boolean enabled")?;
            let mut s = host.load()?;
            if kind == "set_auto_retry" {
                s.disable_auto_retry = !enabled;
            } else {
                s.disable_auto_compaction = !enabled;
            }
            host.save(&mut s)?;
            Value::Null
        }
        "set_session_name" => {
            let mut s = host.load()?;
            s.name = field(req, "name")?.into();
            host.save(&mut s)?;
            Value::Null
        }
        "get_available_thinking_levels" | "set_thinking_level" | "cycle_thinking_level" => {
            let mut s = host.load()?;
            let selected = s.model.as_ref().context("no model selected")?;
            let levels = (models.effort)(selected)?;
            if kind == "get_available_thinking_levels" {
                json!({"levels":levels})
            } else {
                let current = s.reasoning.as_deref().unwrap_or("off");
                let level = if kind == "set_thinking_level" {
                    field(req, "level")?
                } else if levels.is_empty() {
                    current
                } else {
                    &levels[levels
                        .iter()
                        .position(|s| s == current)
                        .map_or(0, |i| (i + 1) % levels.len())]
                };
                ensure!(
                    levels.iter().any(|s| s == level),
                    "unsupported thinking level {level}"
                );
                let data = if kind == "cycle_thinking_level" {
                    json!({"level":level})
                } else {
                    Value::Null
                };
                s.reasoning = (level != "off").then(|| level.into());
                host.save(&mut s)?;
                data
            }
        }
        "set_model" | "cycle_model" => {
            let available = (models.list)()?;
            let selected = if kind == "cycle_model" {
                let s = host.load()?;
                if available.len() <= 1 {
                    return Ok(if pi {
                        Value::Null
                    } else {
                        json!({"type":"ok","data":null})
                    });
                }
                let i = s
                    .model
                    .and_then(|id| available.iter().position(|m| m == &id))
                    .map_or(0, |i| (i + 1) % available.len());
                available[i].clone()
            } else {
                let id = ModelId {
                    provider: field(req, "provider")?.into(),
                    model: field(req, "modelId")?.into(),
                };
                ensure!(
                    available.contains(&id) || host.load()?.model.as_ref() == Some(&id),
                    "unknown or unconfigured model"
                );
                id
            };
            let descriptor = (models.describe)(&selected)?;
            let keep = (models.retention)(&selected);
            SessionSdk { host }.select_model_with_limits(
                selected,
                None,
                Some((descriptor["contextWindow"].as_u64(), keep)),
            )?;
            if kind == "cycle_model" {
                json!({"model":descriptor,"thinkingLevel":host.load()?.reasoning.unwrap_or_else(||"off".into()),"isScoped":false})
            } else {
                descriptor
            }
        }
        _ => bail!("unknown RPC command {kind}"),
    };
    Ok(if pi {
        data
    } else if data.is_null() {
        json!({"type":"ok"})
    } else {
        json!({"type":"ok","data":data})
    })
}

pub(super) fn serve(host: Host<'_>, delivery: Delivery, pi: bool) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let queue = Mutex::new(Queue {
        delivery,
        ..Default::default()
    });
    let shared = Mutex::new(host.db);
    let mut store = Shared(&shared);
    let cancel = host.cancel;
    let tools = host.tools;
    let decode = |line: String| match serde_json::from_str::<Value>(&line) {
        Ok(r) => Some(r),
        Err(e) => {
            reply(pi, &Value::Null, Err(e.into()));
            None
        }
    };
    let mut continuation: Option<Message> = None;
    loop {
        let resumed = continuation.is_some();
        let req = if resumed {
            json!({"type":"prompt"})
        } else {
            let Ok(line) = rx.recv() else { break };
            let Some(req) = decode(line) else { continue };
            req
        };
        let kind = req["type"].as_str().unwrap_or_default();
        if !matches!(kind, "prompt" | "retry" | "compact" | "bash") {
            let data = control(&mut store, &queue, &req, None, cancel, &host.models, pi);
            reply(pi, &req, data);
            continue;
        }
        let preparation = (|| -> Result<_> {
            let s = store.load()?;
            let mut input = None;
            if kind == "prompt" {
                ensure!(!s.active, "acknowledge interrupted turn first");
                input = Some(match continuation.take() {
                    Some(message) => message,
                    None => {
                        prompt_input(&req, if pi { "message" } else { "text" }, Some(host.root))?
                    }
                });
            }
            if kind == "bash" {
                field(&req, "command")?;
            }
            let provider = if kind == "bash" {
                None
            } else {
                Some((host.connect)(s.model.as_ref())?)
            };
            Ok((s, provider, input))
        })();
        let (initial, mut provider, input) = match preparation {
            Ok(p) => p,
            Err(e) => {
                reply(pi, &req, Err(e));
                continue;
            }
        };
        let prompt = matches!(kind, "prompt" | "retry");
        cancel.reset();
        if pi && prompt && !resumed {
            reply(true, &req, Ok(json!({"disposition":"started"})));
        }
        let mut stream = (pi && prompt).then(|| pi::Stream::start(initial.history.len()));
        let result = std::thread::scope(|scope| -> Result<Value> {
            let mut worker_store = Shared(&shared);
            let snapshot = Shared(&shared);
            let dispatch = &queue;
            let run_req = &req;
            let stream = &mut stream;
            let worker = scope.spawn(move || -> Result<Value> {
                let mut sdk = SessionSdk { host: &mut worker_store };
                let mut emit = |event: Event| {
                    if matches!(event, Event::ResultImages(_)) {
                        return;
                    }
                    if let Some(stream) = stream {
                        stream.event(event, &snapshot);
                    } else if pi {
                        if let Event::ToolOutput(delta) = event {
                            let update = json!({"type":"bash_execution_update","id":run_req["id"],"delta":delta});
                            println!("{update}");
                        }
                    } else {
                        let record = json!({"id":run_req["id"],"type":"event","event":event});
                        println!("{record}");
                    }
                };
                if kind == "bash" {
                    let exclude = run_req["excludeFromContext"].as_bool().unwrap_or(false);
                    let out = sdk.shell(tools,field(run_req, "command")?,exclude,cancel,&mut emit)?;
                    return Ok(serde_json::to_value(out)?);
                }
                let provider = provider.as_deref_mut().context("missing run provider")?;
                match kind {
                    "prompt" => sdk.prompt(provider,tools,input.context("missing prompt")?,cancel,&mut emit,dispatch)?,
                    "retry" => sdk.retry(provider,tools,cancel,&mut emit,dispatch)?,
                    "compact" => {
                        let system = tools.system()?;
                        let schemas = tools.schemas_for(&sdk.state()?);
                        let before = sdk.state()?.estimated_tokens(&system, &schemas);
                        sdk.compact(provider,run_req["customInstructions"].as_str().unwrap_or(""),cancel)?;
                        let state = sdk.state()?;
                        let checkpoint = state.checkpoint.as_ref().context("no compaction checkpoint")?;
                        let entries = sdk.host.entries()?;
                        let first = pi::active_branch(&entries).into_iter().filter(|e| e.message.is_some())
                            .nth(checkpoint.through).map(|e|e.id.to_string());
                        return Ok(json!({"summary":checkpoint.summary,"firstKeptEntryId":first,"tokensBefore":before,
                            "estimatedTokensAfter":state.estimated_tokens(&system,&schemas)}));
                    }
                    _ => unreachable!(),
                }
                Ok(Value::Null)
            });
            while !worker.is_finished() {
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(line) => {
                        if let Some(other) = decode(line) {
                            let data = control(
                                &mut store,
                                &queue,
                                &other,
                                Some(kind),
                                cancel,
                                &host.models,
                                pi,
                            );
                            reply(pi, &other, data);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("session worker failed"))?
        });
        if pi && prompt {
            let aborted = cancel.check().is_err();
            let s = store.load()?;
            let err = result.err().map(|e| e.to_string());
            if aborted {
                acknowledge(&mut store)?;
                let mut q = queue.lock().unwrap();
                let messages = if !q.steer.is_empty() {
                    &mut q.steer
                } else {
                    &mut q.follow
                };
                if !messages.is_empty() {
                    continuation = Some(messages.remove(0));
                }
            }
            stream
                .as_mut()
                .unwrap()
                .finish(&s, err.as_deref(), aborted, continuation.is_none())?;
        } else if !pi && prompt {
            let mut q = queue.lock().unwrap();
            let mut undelivered = std::mem::take(&mut q.steer);
            undelivered.append(&mut q.follow);
            let undelivered = queue_text(undelivered);
            let response = match result {
                Ok(_) => {
                    json!({"type":"done","session_id":host.session_id.map(|_|store.session_id()),"undelivered":undelivered})
                }
                Err(e) => json!({"type":"error","error":e.to_string(),"undelivered":undelivered}),
            };
            reply(false, &req, Ok(response));
        } else {
            reply(
                pi,
                &req,
                result.map(|v| if pi { v } else { json!({"type":"ok","data":v}) }),
            );
        }
    }
    Ok(())
}
