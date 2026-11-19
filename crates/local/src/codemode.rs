//! Optional read-only JavaScript composition for the native host.
mod vm;

use anyhow::{Context, Result, ensure};
use panda_core::{Call, Cancel, Emit, Event, Mode, Toolset};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub use vm::worker;

const TIME_LIMIT: Duration = Duration::from_secs(10);
const SCRIPT_BYTES: usize = 64 * 1024;
const OUTPUT_BYTES: usize = 16 * 1024;
const ARG_BYTES: usize = 16 * 1024;
const MAX_CALLS: usize = 64;
// A JSON-escaped byte can occupy six bytes in the outer protocol record.
const REQUEST_FRAME: usize = 6 * SCRIPT_BYTES + 1024;
const REPLY_FRAME: usize = 6 * 1024 * 1024 + 1024;

#[derive(Serialize, Deserialize)]
enum WorkerMessage {
    Invoke { name: String, args: Value },
    Finished(Result<String, String>),
}

fn send(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut record = serde_json::to_vec(value)?;
    record.push(b'\n');
    out.write_all(&record)?;
    Ok(out.flush()?)
}

fn receive<T: DeserializeOwned>(input: &mut impl BufRead, limit: usize) -> Result<T> {
    let mut bytes = Vec::new();
    input.take(limit as u64).read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.last() == Some(&b'\n'),
        "Codemode worker closed or exceeded frame limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn watch(
    mut child: std::process::Child,
    done: mpsc::Receiver<()>,
    cancel: &Cancel,
    nested_cancel: &Cancel,
) -> Result<()> {
    let start = Instant::now();
    let outcome = loop {
        if let Err(err) = cancel.check() {
            break Err(err);
        }
        if start.elapsed() >= TIME_LIMIT {
            break Err(anyhow::anyhow!("Codemode time limit exceeded"));
        }
        if let Err(mpsc::RecvTimeoutError::Disconnected) =
            done.recv_timeout(Duration::from_millis(10))
        {
            break Ok(());
        }
    };
    nested_cancel.stop();
    let _ = child.kill();
    child.wait()?;
    outcome
}

pub struct Codemode<'a> {
    inner: &'a (dyn Toolset + Sync),
    executable: &'a Path,
}

impl<'a> Codemode<'a> {
    pub fn new(inner: &'a (dyn Toolset + Sync), executable: &'a Path) -> Self {
        Self { inner, executable }
    }

    fn execute(&self, mode: Mode, call: &Call, cancel: &Cancel, emit: &mut Emit) -> Result<String> {
        cancel.check()?;
        let script = panda_coding::field(&call.args, "code")?;
        ensure!(
            script.len() <= SCRIPT_BYTES,
            "Codemode script exceeds 64 KiB"
        );
        let executable = self.executable.canonicalize()?;
        let literal =
            serde_json::to_string(executable.to_str().context("non UTF-8 executable path")?)?;
        let profile = format!(
            r#"(version 1)(deny default)(import "dyld-support.sb")
            (allow process-exec (literal {literal}))
            (allow sysctl-read file-read-metadata)
            (allow file-read* (literal {literal}) (subpath "/System/Library") (subpath "/usr/lib") (literal "/dev/urandom"))"#
        );
        let mut child = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile])
            .arg(executable)
            .arg("--codemode-worker")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start Codemode sandbox")?;
        let mut output = child.stdin.take().unwrap();
        let mut input = BufReader::new(child.stdout.take().unwrap());
        // One watchdog covers compilation, JS, blocked IPC and a nested file worker.
        // The nested token is private so a timeout doesn't cancel the whole turn.
        let nested = Cancel::default();
        std::thread::scope(|scope| {
            let (finished, done) = mpsc::channel::<()>();
            let nested_cancel = nested.clone();
            let watchdog = scope.spawn(move || watch(child, done, cancel, &nested_cancel));
            let result = (|| {
                send(&mut output, &script)?;
                let mut count = 0;
                loop {
                    match receive(&mut input, REQUEST_FRAME)? {
                        WorkerMessage::Finished(result) => {
                            let text = result.map_err(anyhow::Error::msg)?;
                            ensure!(text.len() <= OUTPUT_BYTES, "Codemode output exceeds 16 KiB");
                            return Ok(text);
                        }
                        WorkerMessage::Invoke { name, args } => {
                            cancel.check()?;
                            nested.check()?;
                            count += 1;
                            ensure!(count <= MAX_CALLS, "Codemode nested call limit exceeded");
                            ensure!(
                                matches!(name.as_str(), "read" | "search" | "repo_map"),
                                "unavailable nested tool"
                            );
                            ensure!(
                                args.is_object() && args.to_string().len() <= ARG_BYTES,
                                "invalid nested tool arguments"
                            );
                            let child_call = Call {
                                id: format!("{}/{}", call.id, count),
                                name,
                                args,
                            };
                            emit(Event::NestedCall {
                                parent: call.id.clone(),
                                call: child_call.clone(),
                            });
                            cancel.check()?;
                            let reply = self
                                .inner
                                .run(mode, &child_call, &nested)
                                .and_then(|s| {
                                    ensure!(s.len() <= 1024 * 1024, "nested result exceeds 1 MiB");
                                    Ok(s)
                                })
                                .map_err(|err| err.to_string());
                            send(&mut output, &reply)?;
                        }
                    }
                }
            })();
            drop(finished);
            watchdog.join().expect("Codemode watchdog panicked")?;
            result
        })
    }
}

impl Toolset for Codemode<'_> {
    fn system(&self) -> Result<String> {
        let mut prompt = self.inner.system()?;
        prompt.push_str(
            r#"
For repository-wide match counts or rankings, start with one codemode call. search already covers the workspace. Aggregate each page inside the script and stop when next_offset is null.
Example: count lines matching "FIXME" across the repository:
let offset = 0, count = 0;
do {
    const page = tools.search({query: "FIXME", offset});
    count += page.items.length;
    offset = page.next_offset;
} while (offset !== null);
text(count);
For per-file results, group the items by their path in the same loop and emit only the requested aggregate."#,
        );
        Ok(prompt)
    }
    fn schemas(&self, mode: Mode) -> Vec<Value> {
        let mut schemas = self.inner.schemas(mode);
        schemas.push(json!({"type":"function","function":{
            "name":"codemode",
            "description":"Run isolated JavaScript to compose read-only workspace tools. tools.search({query,offset?}) and tools.repo_map({offset?}) return {items,next_offset}; search items have path,line,text. tools.read({path,offset?,limit?}) returns a string. Calls are synchronous. Use text(value) to emit selected output; only that output is returned. Limits: 64 calls, 10 seconds, 16 KiB output.",
            "parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}
        }}));
        schemas
    }
    fn run(&self, mode: Mode, call: &Call, cancel: &Cancel) -> Result<String> {
        self.run_stream(mode, call, cancel, &mut |_| {})
    }
    fn run_stream(
        &self,
        mode: Mode,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<String> {
        if call.name == "codemode" {
            self.execute(mode, call, cancel, emit)
        } else {
            self.inner.run_stream(mode, call, cancel, emit)
        }
    }
}
