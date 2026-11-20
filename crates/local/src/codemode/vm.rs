use super::*;
use rquickjs::{
    CatchResultExt, Context as JsContext, Ctx, Exception, Function, Promise, Runtime,
    function::Constructor,
};
use serde_json::value::RawValue;
use std::{
    cell::{Cell, RefCell},
    io,
    rc::Rc,
};

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

fn run_script(ctx: &Ctx<'_>, code: &str) -> rquickjs::Result<()> {
    let text: Function = ctx.globals().get("text")?;
    let async_function: Constructor = ctx.eval("(async () => {}).constructor")?;
    let script: Function = async_function.construct((format!("\"use strict\";\n{code}"),))?;
    let promise: Promise = script.call(())?;
    let value: rquickjs::Value = promise.finish()?;
    if !value.is_undefined() {
        text.call::<_, ()>((value,))?;
    }
    Ok(())
}

fn finish(result: Result<String>) -> Result<()> {
    let message = WorkerMessage::Finished(result.map_err(|err| err.to_string()));
    send(&mut io::stdout().lock(), &message)
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
        let emitted = Cell::new(false);
        ctx.globals().set(
            "__text",
            Function::new(ctx.clone(), move |ctx: Ctx<'_>, value: String| {
                let mut slot = buffer.borrow_mut();
                let Some(out) = slot.as_mut() else {
                    return Err(Exception::throw_message(&ctx, OUTPUT_LIMIT_ERROR));
                };
                let newline = emitted.replace(true);
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
        let running = Rc::new(Cell::new(true));
        let exit_enabled = running.clone();
        let exit_output = output.clone();
        ctx.globals().set(
            "exit",
            Function::new(ctx.clone(), move || {
                if !exit_enabled.get() {
                    return;
                }
                // The worker serves one script. Finish its protocol before stopping, so
                // catch/finally blocks and queued jobs cannot run tools after exit().
                let sent = finish(exit_output.take().context(OUTPUT_LIMIT_ERROR));
                std::process::exit(if sent.is_ok() { 0 } else { 1 });
            })?,
        )?;
        ctx.eval::<(), _>(include_str!("bindings.js"))?;
        let result = run_script(&ctx, code);
        // Formatting a thrown Error can run JS getters after the script has failed.
        running.set(false);
        result.catch(&ctx).map_err(|err| {
            anyhow::anyhow!("{}", err.to_string().chars().take(1024).collect::<String>())
        })
    })?;
    output.take().context(OUTPUT_LIMIT_ERROR)
}

pub fn worker() -> Result<()> {
    let (script, catalog): (String, Box<RawValue>) =
        receive(&mut io::stdin().lock(), REQUEST_FRAME)?;
    ensure!(script.len() <= SCRIPT_BYTES, "script exceeds 64 KiB");
    finish(evaluate(&script, &catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn returned_text_follows_emitted_items(items in prop::collection::vec(".{0,32}", 1..8)) {
            let values = json!(items);
            let script = format!("const values = {values}; const last = values.pop(); for (const value of values) text(value); return last;");
            let catalog = serde_json::value::to_raw_value(&json!([])).unwrap();
            prop_assert_eq!(evaluate(&script, &catalog).unwrap(), items.join("\n"));
        }

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
