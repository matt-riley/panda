use super::*;
use anyhow::Context;
use panda_core::Part;
use std::collections::HashMap;

fn text(f: Family, s: &str) -> Value {
    if f == Family::Anthropic {
        json!({"type":"text","text":s})
    } else {
        json!({"text":s})
    }
}
fn part(f: Family, p: Part, images: bool) -> Result<Value> {
    match p {
        Part::Text(s) => Ok(text(f, s)),
        Part::Image(_) if !images => Ok(text(f, "[image omitted: text-only model]")),
        Part::Image(url) => {
            let (mime, data) = url
                .strip_prefix("data:")
                .and_then(|s| s.split_once(";base64,"))
                .context("native provider images require a base64 data URL")?;
            ensure!(mime.starts_with("image/"), "unsupported image content type");
            Ok(if f == Family::Anthropic {
                json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}})
            } else {
                json!({"inlineData":{"mimeType":mime,"data":data}})
            })
        }
    }
}
fn anthropic_id(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return id.to_owned();
    }
    use ring::digest::{SHA256, digest};
    digest(&SHA256, id.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn turns(api: &NativeApi, h: &[Message]) -> Result<Vec<Value>> {
    let f = api.family;
    let blocks = if f == Family::Anthropic {
        "content"
    } else {
        "parts"
    };
    let mut turns: Vec<Value> = Vec::new();
    let mut replay: &[Value] = &[];
    let mut names = HashMap::new();
    for msg in h {
        let mut parts = Vec::new();
        let assistant = match msg {
            Message::Reasoning(_) | Message::Truncated(_) | Message::CodemodeStore(_) => continue,
            Message::Replay(state) => {
                replay = if state.provider == api.provider
                    && state.model == api.api.model
                    && state.family == f.name()
                {
                    &state.blocks
                } else {
                    &[]
                };
                continue;
            }
            Message::Bash {
                command,
                outcome,
                exclude,
            } => {
                if *exclude {
                    continue;
                }
                parts.push(text(f, &outcome.context(command)));
                false
            }
            Message::User(s) => {
                parts.push(text(f, s));
                false
            }
            Message::BranchSummary(s) => {
                parts.push(text(f, &format!("Summary from another branch:\n{s}")));
                false
            }
            Message::Images { text: s, urls } => {
                if !s.is_empty() {
                    parts.push(text(f, s));
                }
                for url in urls {
                    parts.push(part(f, Part::Image(url), api.images)?);
                }
                false
            }
            Message::Assistant { text: s, calls } => {
                if f == Family::Anthropic {
                    parts.extend(
                        replay
                            .iter()
                            .filter(|b| {
                                matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"))
                            })
                            .cloned(),
                    );
                }
                if f == Family::Gemini {
                    // Preserve signatures on their original text boundaries. Foreign history supplies plain text.
                    let mut offset = 0;
                    for b in replay.iter().filter(|b| b["text"].is_string()) {
                        let (Some(content), Some(signature), Some(start)) = (
                            b["text"].as_str(),
                            b["signature"].as_str(),
                            b["offset"].as_u64().and_then(|n| usize::try_from(n).ok()),
                        ) else {
                            continue;
                        };
                        if let Some(before) = s.get(offset..start)
                            && s[start..].starts_with(if b["thought"] == true {
                                ""
                            } else {
                                content
                            })
                        {
                            if !before.is_empty() {
                                parts.push(text(f, before));
                            }
                            let mut part = json!({"text":content,"thoughtSignature":signature});
                            if b["thought"] == true {
                                part["thought"] = json!(true);
                                offset = start;
                            } else {
                                offset = start + content.len();
                            }
                            parts.push(part);
                        }
                    }
                    if offset < s.len() {
                        parts.push(text(f, &s[offset..]));
                    }
                } else if !s.is_empty() {
                    parts.push(text(f, s));
                }
                for (i, call) in calls.iter().enumerate() {
                    names.insert(&call.id, &call.name);
                    let mut p = if f == Family::Anthropic {
                        json!({"type":"tool_use","id":anthropic_id(&call.id),"name":call.name,"input":call.args})
                    } else {
                        json!({"functionCall":{"name":call.name,"id":call.id,"args":call.args}})
                    };
                    if f == Family::Gemini {
                        if let Some(signature) = replay
                            .iter()
                            .find(|b| b["call"] == call.id)
                            .and_then(|b| b["signature"].as_str())
                        {
                            p["thoughtSignature"] = json!(signature);
                        } else if i == 0 && api.api.model.starts_with("gemini-3") {
                            // Imported or foreign tool calls have no Gemini thought signature.
                            p["thoughtSignature"] = json!("skip_thought_signature_validator");
                        }
                    }
                    parts.push(p);
                }
                replay = &[];
                true
            }
            Message::Result {
                id,
                text: s,
                images,
                details,
                is_error,
                ..
            } => {
                if f == Family::Anthropic {
                    let content = panda_core::parts(s, images, details.as_ref())
                        .into_iter()
                        .map(|p| part(f, p, api.images))
                        .collect::<Result<Vec<_>>>()?;
                    parts.push(json!({"type":"tool_result","tool_use_id":anthropic_id(id),"content":content,"is_error":is_error}));
                } else {
                    let name = names
                        .get(id)
                        .context("Gemini tool result has no matching call")?;
                    let result = if *is_error {
                        json!({"error":s})
                    } else {
                        json!({"output":s})
                    };
                    parts.push(json!({"functionResponse":{"id":id,"name":name,"response":result}}));
                    for url in images {
                        parts.push(part(f, Part::Image(url), api.images)?);
                    }
                }
                false
            }
        };
        if parts.is_empty() {
            continue;
        }
        let role = if !assistant {
            "user"
        } else if f == Family::Anthropic {
            "assistant"
        } else {
            "model"
        };
        if let Some(last) = turns.last_mut().filter(|m| m["role"] == role) {
            last[blocks].as_array_mut().unwrap().extend(parts);
        } else {
            turns.push(json!({"role":role,blocks:parts}));
        }
    }
    Ok(turns)
}

pub(super) fn body(
    api: &NativeApi,
    system: &str,
    h: &[Message],
    tools: &[Value],
    effort: Option<&str>,
) -> Result<Value> {
    let max = api.max_tokens;
    if api.family == Family::Responses {
        let mut body =
            wire::responses_history(&api.api.model, system, h, tools, Some(&api.provider));
        body["max_output_tokens"] = json!(max);
        body["include"] = json!(["reasoning.encrypted_content"]);
        if let Some(e) = effort {
            body["reasoning"] = json!({"effort":e,"summary":"auto"});
        }
        if !api.images {
            for msg in body["input"].as_array_mut().unwrap() {
                let field = if msg["type"] == "function_call_output" {
                    "output"
                } else {
                    "content"
                };
                if let Some(parts) = msg[field].as_array_mut() {
                    for p in parts {
                        if p["type"] == "input_image" {
                            *p = json!({"type":"input_text","text":"[image omitted: text-only model]"});
                        }
                    }
                }
            }
        }
        return Ok(body);
    }
    let turns = turns(api, h)?;
    let defs:Vec<Value>=tools.iter().map(|t|{
        let f=&t["function"];
        if api.family==Family::Anthropic {json!({"name":f["name"],"description":f["description"],"input_schema":f["parameters"]})}
        else{json!({"name":f["name"],"description":f["description"],"parametersJsonSchema":f["parameters"]})}
    }).collect();
    if api.family == Family::Anthropic {
        let mut body = json!({"model":api.api.model,"system":system,"messages":turns,"stream":true,"max_tokens":max});
        if !defs.is_empty() {
            body["tools"] = json!(defs);
        }
        if let Some(e) = effort {
            if matches!(e, "off" | "none") {
                body["thinking"] = json!({"type":"disabled"});
            } else {
                ensure!(
                    max > 1024,
                    "Anthropic thinking requires maxTokens greater than 1024"
                );
                let budget = match e {
                    "low" | "minimal" => 1024,
                    "medium" => 4096,
                    _ => 8192,
                };
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget.min(max-1)});
            }
        }
        Ok(body)
    } else {
        let mut body = json!({"contents":turns,"systemInstruction":{"parts":[{"text":system}]},"generationConfig":{"maxOutputTokens":max}});
        if !defs.is_empty() {
            body["tools"] = json!([{"functionDeclarations":defs}]);
        }
        if let Some(e) = effort {
            body["generationConfig"]["thinkingConfig"] = if api.api.model.starts_with("gemini-3") {
                json!({"thinkingLevel":match e {"off"|"none"|"minimal"=>"minimal","low"=>"low","medium"=>"medium",_=>"high"},"includeThoughts":true})
            } else {
                json!({"thinkingBudget":match e {"off"|"none"=>0,"low"|"minimal"=>1024,"medium"=>4096,_=>8192},"includeThoughts":true})
            };
        }
        Ok(body)
    }
}
