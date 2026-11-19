use super::*;
use rquickjs::{CatchResultExt, Context as JsContext, Ctx, Exception, Function, Runtime};
use std::{cell::RefCell, io, rc::Rc};

fn invoke(name: String, arguments: String) -> Result<String> {
    ensure!(
        arguments.len() <= ARG_BYTES,
        "nested arguments exceed 16 KiB"
    );
    let args = serde_json::from_str(&arguments)?;
    send(
        &mut io::stdout().lock(),
        &WorkerMessage::Invoke { name, args },
    )?;
    let result: Result<String, String> = receive(&mut io::stdin().lock(), REPLY_FRAME)?;
    result.map_err(anyhow::Error::msg)
}

fn evaluate(code: String) -> Result<String> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let context = JsContext::full(&runtime)?;
    let output = Rc::new(RefCell::new(String::new()));
    let buffer = output.clone();
    context.with(|ctx| -> Result<()> {
        ctx.globals().set(
            "__invoke",
            Function::new(ctx.clone(), |ctx: Ctx<'_>, name: String, args: String| {
                invoke(name, args).map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            })?,
        )?;
        ctx.globals().set(
            "__text",
            Function::new(ctx.clone(), move |ctx: Ctx<'_>, value: String| {
                let mut out = buffer.borrow_mut();
                let newline = !out.is_empty();
                if out.len() + usize::from(newline) + value.len() > OUTPUT_BYTES {
                    return Err(Exception::throw_message(
                        &ctx,
                        "Codemode output exceeds 16 KiB",
                    ));
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
    Ok(output.take())
}

pub fn worker() -> Result<()> {
    let script: String = receive(&mut io::stdin().lock(), REQUEST_FRAME)?;
    ensure!(script.len() <= SCRIPT_BYTES, "script exceeds 64 KiB");
    let result = evaluate(script).map_err(|err| err.to_string());
    send(&mut io::stdout().lock(), &WorkerMessage::Finished(result))
}
