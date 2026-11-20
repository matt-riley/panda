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

fn finish(result: Result<ScriptOutput>) -> Result<()> {
    let output = result.unwrap_or_else(|err| ScriptOutput {
        text: String::new(),
        error: Some(err.to_string()),
    });
    send(&mut io::stdout().lock(), &WorkerMessage::Finished(output))
}

const CONSOLE_OPEN: &str = "<console_output>\n";
const CONSOLE_CLOSE: &str = "\n</console_output>";

#[derive(Default)]
struct OutputBuffer {
    text: Option<String>,
    console: Option<String>,
}

impl OutputBuffer {
    fn append(&mut self, value: String, console: bool) -> Result<()> {
        let wrapper = if console || self.console.is_some() {
            CONSOLE_OPEN.len() + CONSOLE_CLOSE.len()
        } else {
            0
        };
        // Each existing group contributes its bytes and one separator in the new result.
        let size = value.len()
            + wrapper
            + self.text.as_ref().map_or(0, |s| s.len() + 1)
            + self.console.as_ref().map_or(0, |s| s.len() + 1);
        ensure!(size <= MAX_OUTPUT_BYTES, OUTPUT_LIMIT_ERROR);
        let group = if console {
            &mut self.console
        } else {
            &mut self.text
        };
        if let Some(lines) = group {
            lines.push('\n');
            lines.push_str(&value);
        } else {
            *group = Some(value);
        }
        Ok(())
    }

    fn render(self) -> String {
        let Some(console) = self.console else {
            return self.text.unwrap_or_default();
        };
        match self.text {
            Some(text) => format!("{text}\n{CONSOLE_OPEN}{console}{CONSOLE_CLOSE}"),
            None => format!("{CONSOLE_OPEN}{console}{CONSOLE_CLOSE}"),
        }
    }
}

fn evaluate(code: &str, catalog: &RawValue) -> Result<ScriptOutput> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let context = JsContext::full(&runtime)?;
    // None latches an overflow, even if the script catches the text() error.
    let output = Rc::new(RefCell::new(Some(OutputBuffer::default())));
    let buffer = output.clone();
    context.with(|ctx| -> Result<ScriptOutput> {
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
            "__emit",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, value: String, console: bool| {
                    let mut slot = buffer.borrow_mut();
                    let Some(out) = slot.as_mut() else {
                        return Err(Exception::throw_message(&ctx, OUTPUT_LIMIT_ERROR));
                    };
                    if out.append(value, console).is_err() {
                        *slot = None;
                        return Err(Exception::throw_message(&ctx, OUTPUT_LIMIT_ERROR));
                    }
                    Ok(())
                },
            )?,
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
                let sent = finish(
                    exit_output
                        .take()
                        .context(OUTPUT_LIMIT_ERROR)
                        .map(|buffer| ScriptOutput {
                            text: buffer.render(),
                            error: None,
                        }),
                );
                std::process::exit(if sent.is_ok() { 0 } else { 1 });
            })?,
        )?;
        ctx.eval::<(), _>(include_str!("bindings.js"))?;
        let result = run_script(&ctx, code);
        // Freeze output before formatting an exception, which may run JS getters.
        running.set(false);
        let text = output.take().context(OUTPUT_LIMIT_ERROR)?.render();
        let error = result
            .catch(&ctx)
            .err()
            .map(|err| err.to_string().chars().take(MAX_ERROR_CHARS).collect());
        Ok(ScriptOutput { text, error })
    })
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
        fn mixed_output_fits_exactly_at_the_rendered_byte_limit(
            items in prop::collection::vec((any::<bool>(), ".{0,12}"), 0..8),
            last_is_console in any::<bool>(),
            over in any::<bool>(),
        ) {
            let mut buffer = OutputBuffer::default();
            let (mut text, mut console) = (Vec::new(), Vec::new());
            for (log, value) in items {
                buffer.append(value.clone(), log).unwrap();
                if log { console.push(value) } else { text.push(value) }
            }
            let last = if last_is_console { &mut console } else { &mut text };
            last.push(String::new());
            let mut groups = Vec::new();
            if !text.is_empty() { groups.push(text.join("\n")); }
            if !console.is_empty() { groups.push(format!("<console_output>\n{}\n</console_output>", console.join("\n"))); }
            let padding = MAX_OUTPUT_BYTES - groups.join("\n").len() + usize::from(over);
            let added = buffer.append("x".repeat(padding), last_is_console);
            prop_assert_eq!(added.is_err(), over);
            if !over { prop_assert_eq!(buffer.render().len(), MAX_OUTPUT_BYTES); }
        }

        #[test]
        fn completion_preserves_emitted_items(items in prop::collection::vec(".{0,32}", 1..8), fails in any::<bool>()) {
            let values = json!(items);
            let completion = if fails { "throw Error('unfinished')" } else { "return last" };
            let script = format!("const values = {values}; const last = values.pop(); for (const value of values) text(value); {completion};");
            let catalog = serde_json::value::to_raw_value(&json!([])).unwrap();
            let output = evaluate(&script, &catalog).unwrap();
            if fails {
                prop_assert!(output.error.unwrap().contains("unfinished"));
                prop_assert_eq!(output.text, items[..items.len()-1].join("\n"));
            } else {
                prop_assert_eq!(output.error, None);
                prop_assert_eq!(output.text, items.join("\n"));
            }
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
            prop_assert_eq!(output.error, None);
            prop_assert_eq!(output.text, serde_json::to_string(&names).unwrap());
        }
    }
}
