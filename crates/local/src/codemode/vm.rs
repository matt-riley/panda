use super::*;
use rquickjs::{CatchResultExt, Context as JsContext, Ctx, Exception, Function, Runtime};
use serde_json::value::RawValue;
use std::{cell::RefCell, io, rc::Rc};

fn invoke(name: String, arguments: String) -> Result<String> {
    ensure!(
        arguments.len() <= ARG_BYTES,
        "nested arguments exceed 16 KiB"
    );
    let args = serde_json::from_str(&arguments)?;
    exchange(WorkerMessage::Invoke { name, args })
}

fn exchange(message: WorkerMessage) -> Result<String> {
    send(&mut io::stdout().lock(), &message)?;
    let result: Result<String, String> = receive(&mut io::stdin().lock(), REPLY_FRAME)?;
    result.map_err(anyhow::Error::msg)
}

fn evaluate(code: &str, catalog: &RawValue) -> Result<String> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let context = JsContext::full(&runtime)?;
    // None latches an overflow, even if the script catches the text() error.
    let output = Rc::new(RefCell::new(Some(String::new())));
    let buffer = output.clone();
    context.with(|ctx| -> Result<()> {
        ctx.globals()
            .set("__catalog", ctx.json_parse(catalog.get())?)?;
        ctx.globals().set(
            "__load",
            Function::new(ctx.clone(), |ctx: Ctx<'_>, key: String| {
                if key.encode_utf16().count() > storage::TOTAL_CHARS {
                    return Ok(String::new());
                }
                exchange(WorkerMessage::Load { key })
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            })?,
        )?;
        ctx.globals().set(
            "__store",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'_>, key: String, json: Option<String>| {
                    let size = json.as_deref().map_or(0, |s| s.encode_utf16().count());
                    if size > storage::VALUE_CHARS {
                        return Err(Exception::throw_message(
                            &ctx,
                            "Codemode store value exceeds 256 Ki characters",
                        ));
                    }
                    if key.encode_utf16().count() + size > storage::TOTAL_CHARS {
                        return Err(Exception::throw_message(
                            &ctx,
                            "Codemode store exceeds 1 Mi characters",
                        ));
                    }
                    exchange(WorkerMessage::Store { key, json })
                        .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
                },
            )?,
        )?;
        ctx.globals().set(
            "__invoke",
            Function::new(ctx.clone(), |ctx: Ctx<'_>, name: String, args: String| {
                invoke(name, args).map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            })?,
        )?;
        ctx.globals().set(
            "__text",
            Function::new(ctx.clone(), move |ctx: Ctx<'_>, value: String| {
                let mut slot = buffer.borrow_mut();
                let Some(out) = slot.as_mut() else {
                    return Err(Exception::throw_message(&ctx, OUTPUT_LIMIT_ERROR));
                };
                let newline = !out.is_empty();
                if out.len() + usize::from(newline) + value.len() > MAX_OUTPUT_BYTES {
                    *slot = None;
                    return Err(Exception::throw_message(&ctx, OUTPUT_LIMIT_ERROR));
                }
                if newline {
                    out.push('\n');
                }
                out.push_str(&value);
                Ok(())
            })?,
        )?;
        ctx.eval::<(), _>(include_str!("bindings.js"))?;
        ctx.eval_promise(code)
            .and_then(|p| p.finish::<()>())
            .catch(&ctx)
            .map_err(|err| {
                anyhow::anyhow!("{}", err.to_string().chars().take(1024).collect::<String>())
            })
    })?;
    output.take().context(OUTPUT_LIMIT_ERROR)
}

pub fn worker() -> Result<()> {
    let (script, catalog): (String, Box<RawValue>) =
        receive(&mut io::stdin().lock(), REQUEST_FRAME)?;
    ensure!(script.len() <= SCRIPT_BYTES, "script exceeds 64 KiB");
    let result = evaluate(&script, &catalog).map_err(|err| err.to_string());
    send(&mut io::stdout().lock(), &WorkerMessage::Finished(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn discovery_respects_catalog_and_limit(
            mask in 0u8..16, query in ".{0,64}", limit in 1usize..9,
        ) {
            let entries: Vec<_> = HELPERS.iter().enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, (name, _, usage))| json!({"name":name,"description":usage}))
                .collect();
            let catalog = serde_json::value::to_raw_value(&entries).unwrap();
            let code = format!(r#"
                const query = {}, limit = {limit};
                const names = ALL_TOOLS.map(tool => tool.name);
                for (const q of [query, "offset", ...names]) {{
                    const hits = searchTools(q, {{limit}});
                    if (hits.length > limit || hits.some(hit => !names.includes(hit.name)))
                        throw Error("invalid result set");
                    if (names.includes(q) && hits[0]?.name !== q)
                        throw Error("exact name did not rank first");
                    for (const hit of hits) {{
                        if (describeTool(hit.name) !== hit.description) throw Error("metadata mismatch");
                    }}
                }}
                text(names);
            "#, json!(query));
            let output = evaluate(&code, &catalog).unwrap();
            let names: Vec<_> = entries.iter().map(|tool| &tool["name"]).collect();
            prop_assert_eq!(output, serde_json::to_string(&names).unwrap());
        }
    }
}
