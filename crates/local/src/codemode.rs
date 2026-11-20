//! Optional read-only JavaScript composition for the native host.
mod storage;
mod vm;
use storage::ScriptStore;

use anyhow::{Context, Result, ensure};
use panda_core::{Call, Cancel, Emit, Event, Mode, Session, StoreWrites, ToolOutput, Toolset};
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
const PREVIEW_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = crate::MAX_TOOL_RESULT_BYTES;
const OUTPUT_LIMIT_ERROR: &str = "Codemode text output exceeds 1 MiB";
const ARG_BYTES: usize = 16 * 1024;
const MAX_CALLS: usize = 64;
// Bound the total retained across nested calls, including base64 expansion.
const MAX_IMAGE_URL_BYTES: usize = 12 * 1024 * 1024;
const HELPERS: &[(&str, &str, &str)] = &[
    (
        "ls",
        "{items: {name: string, type: string}[], next_offset: number | null}",
        "tools.ls({path?,offset?}) lists immediate entries (name,type), includes dotfiles, defaults to the root, and returns {items,next_offset}.",
    ),
    (
        "search",
        "{items: {path: string, line: number, text: string, before?: string[], after?: string[]}[], next_offset: number | null}",
        "tools.search({query,regex?,ignoreCase?,context?,glob?,offset?}) returns {items,next_offset} with path,line,text per hit. Search defaults to case-sensitive literal matching. Set regex:true for a Rust regex and ignoreCase:true to ignore case. Positive context adds before and after arrays in file order. Only matches count toward offsets. Search respects .gitignore and accepts workspace-relative globs such as **/*.rs.",
    ),
    (
        "repo_map",
        "{items: string[], next_offset: number | null}",
        "tools.repo_map({glob?,offset?}) returns {items,next_offset} with workspace paths. It respects .gitignore and accepts workspace-relative globs such as **/*.rs.",
    ),
    (
        "read",
        "string",
        "tools.read({path,offset?,limit?}) returns file text or a short image description. Reading a PNG, JPEG or WebP attaches the original image when the script succeeds; only text supports line ranges.",
    ),
];
// A JSON-escaped byte can occupy six bytes in the outer protocol record.
// Include the descriptions and declarations of the permitted workspace helpers.
const REQUEST_FRAME: usize = 6 * SCRIPT_BYTES + 16 * 1024;
const WORKER_FRAME: usize = 6 * storage::TOTAL_CHARS + 1024;
const REPLY_FRAME: usize = 6 * 1024 * 1024 + 1024;
// Finished messages must also fit when every output byte needs JSON escaping.
const _: () = assert!(WORKER_FRAME >= 6 * MAX_OUTPUT_BYTES + 64);

fn finish_output(root: &std::fs::File, text: String) -> Result<String> {
    ensure!(text.len() <= MAX_OUTPUT_BYTES, OUTPUT_LIMIT_ERROR);
    if text.len() <= PREVIEW_BYTES {
        return Ok(text);
    }
    let (path, _) =
        crate::output_log::create_log(root, "codemode", |file| file.write_all(text.as_bytes()))
            .context("could not save full Codemode output")?;
    let middle = "\n[Output truncated]\n";
    let footer = format!("\nFull output: {path}");
    let budget = PREVIEW_BYTES - middle.len() - footer.len();
    let head = text.floor_char_boundary(budget / 2);
    let tail = text.ceil_char_boundary(text.len() - budget.div_ceil(2));
    Ok(format!(
        "{}{middle}{}{footer}",
        &text[..head],
        &text[tail..]
    ))
}

struct NestedHelper<'a> {
    name: &'static str,
    function: &'a Value,
    result_type: &'static str,
    usage: &'static str,
}

#[derive(Serialize)]
struct ToolEntry {
    name: &'static str,
    description: String,
}

impl NestedHelper<'_> {
    fn entry(&self) -> ToolEntry {
        let params = &self.function["parameters"];
        let required = params["required"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut fields = Vec::new();
        if let Some(properties) = params["properties"].as_object() {
            for (name, property) in properties {
                let optional = if required.iter().any(|field| field == name) {
                    ""
                } else {
                    "?"
                };
                let ty = match property["type"].as_str() {
                    Some("integer" | "number") => "number",
                    Some("string") => "string",
                    Some("boolean") => "boolean",
                    _ => "unknown",
                };
                fields.push(format!("{name}{optional}: {ty}"));
            }
        }
        let args = if required.is_empty() { "args?" } else { "args" };
        let declaration = format!(
            "function {}({args}: {{ {} }}): {}",
            self.name,
            fields.join(", "),
            self.result_type
        );
        ToolEntry {
            name: self.name,
            description: format!(
                "{} {}\n{declaration}",
                self.function["description"].as_str().unwrap_or_default(),
                self.usage
            ),
        }
    }
}

#[derive(Serialize, Deserialize)]
enum WorkerMessage {
    Invoke { name: String, args: Value },
    Load { key: String },
    Store { key: String, json: Option<String> },
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
    workspace_root: &'a Path,
}

impl<'a> Codemode<'a> {
    pub fn new(
        inner: &'a (dyn Toolset + Sync),
        executable: &'a Path,
        workspace_root: &'a Path,
    ) -> Self {
        Self {
            inner,
            executable,
            workspace_root,
        }
    }

    fn helpers(schemas: &[Value]) -> impl Iterator<Item = NestedHelper<'_>> {
        HELPERS.iter().filter_map(|&(name, result_type, usage)| {
            let function = schemas
                .iter()
                .map(|s| &s["function"])
                .find(|f| f["name"] == name)?;
            Some(NestedHelper {
                name,
                function,
                result_type,
                usage,
            })
        })
    }

    fn execute(
        &self,
        mode: Mode,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
        mut store: ScriptStore,
    ) -> Result<(ToolOutput, StoreWrites)> {
        cancel.check()?;
        let script = panda_coding::field(&call.args, "code")?;
        let schemas = self.inner.schemas(mode);
        let catalog: Vec<_> = Self::helpers(&schemas)
            .map(|helper| helper.entry())
            .collect();
        ensure!(
            script.len() <= SCRIPT_BYTES,
            "Codemode script exceeds 64 KiB"
        );
        let executable = self.executable.canonicalize()?;
        let root = crate::output_log::open_root(self.workspace_root)?;
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
        let (mut result, writes) = std::thread::scope(|scope| {
            let (finished, done) = mpsc::channel::<()>();
            let nested_cancel = nested.clone();
            let watchdog = scope.spawn(move || watch(child, done, cancel, &nested_cancel));
            let result = (|| {
                send(&mut output, &(script, &catalog))?;
                let mut count = 0;
                let mut images = Vec::new();
                let mut image_bytes = 0;
                loop {
                    match receive(&mut input, WORKER_FRAME)? {
                        WorkerMessage::Load { key } => {
                            send(&mut output, &Ok::<_, String>(store.load(&key)))?;
                        }
                        WorkerMessage::Store { key, json } => {
                            let reply = store
                                .store(key, json)
                                .map(|()| "")
                                .map_err(|e| e.to_string());
                            send(&mut output, &reply)?;
                        }
                        WorkerMessage::Finished(result) => {
                            let text = result.map_err(anyhow::Error::msg)?;
                            return Ok((ToolOutput { text, images }, store.writes));
                        }
                        WorkerMessage::Invoke { name, args } => {
                            cancel.check()?;
                            nested.check()?;
                            count += 1;
                            ensure!(count <= MAX_CALLS, "Codemode nested call limit exceeded");
                            ensure!(
                                catalog.iter().any(|entry| entry.name == name),
                                "unavailable nested tool {name}"
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
                                .run_stream(mode, &child_call, &nested, &mut |_| {})
                                .and_then(|result| {
                                    ensure!(
                                        result.text.len() <= crate::MAX_TOOL_RESULT_BYTES,
                                        "nested result exceeds 1 MiB"
                                    );
                                    let added =
                                        result.images.iter().map(|url| url.len()).sum::<usize>();
                                    ensure!(
                                        image_bytes + added <= MAX_IMAGE_URL_BYTES,
                                        "Codemode image budget exceeded"
                                    );
                                    image_bytes += added;
                                    images.extend(result.images);
                                    Ok(result.text)
                                })
                                .map_err(|err| err.to_string());
                            if reply.is_ok()
                                && child_call.name == "read"
                                && let Some(path) = child_call.args["path"].as_str()
                            {
                                emit(Event::NestedRead(path.to_owned()));
                            }
                            send(&mut output, &reply)?;
                        }
                    }
                }
            })();
            drop(finished);
            watchdog.join().expect("Codemode watchdog panicked")?;
            result
        })?;
        cancel.check()?;
        result.text = finish_output(&root, result.text)?;
        Ok((result, writes))
    }
    pub fn append_guidance(&self, prompt: &mut String) {
        if !Self::helpers(&self.inner.schemas(Mode::Plan)).any(|helper| helper.name == "search") {
            return;
        }
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
    }
}

impl Toolset for Codemode<'_> {
    fn run_in_session(
        &self,
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        if call.name == "codemode" {
            self.execute(
                session.mode,
                call,
                cancel,
                emit,
                ScriptStore::from_history(&session.history),
            )
        } else {
            self.inner.run_in_session(session, call, cancel, emit)
        }
    }

    fn system(&self) -> Result<String> {
        let mut prompt = self.inner.system()?;
        self.append_guidance(&mut prompt);
        Ok(prompt)
    }
    fn schemas(&self, mode: Mode) -> Vec<Value> {
        let mut schemas = self.inner.schemas(mode);
        let helpers = Self::helpers(&schemas)
            .map(|helper| helper.usage)
            .collect::<Vec<_>>()
            .join(" ");
        schemas.push(json!({"type":"function","function":{
            "name":"codemode",
            "description":format!("Run isolated JavaScript to compose read-only workspace tools. {helpers} Calls are synchronous. ALL_TOOLS lists callable helpers as {{name, description}}. searchTools(query, {{limit?}}) ranks them, and describeTool(name) returns a description and typed signature or undefined. Use text(value) to emit selected output. Above 16 KiB the result shows the beginning, end and workspace-relative full log path for line-range reads. store(key,value) and load(key) keep small JSON values on this branch across successful scripts and session resumes. Missing keys return undefined. Storing undefined deletes a key. Limits: 64 calls, 10 seconds, 1 MiB text output, 12 MiB of encoded images."),
            "parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}
        }}));
        schemas
    }
    fn run(&self, mode: Mode, call: &Call, cancel: &Cancel) -> Result<String> {
        self.run_stream(mode, call, cancel, &mut |_| {})
            .map(|output| output.text)
    }
    fn run_stream(
        &self,
        mode: Mode,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<ToolOutput> {
        if call.name == "codemode" {
            self.execute(mode, call, cancel, emit, ScriptStore::default())
                .map(|(output, _)| output)
        } else {
            self.inner.run_stream(mode, call, cancel, emit)
        }
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn utf8_preview_and_log_preserve_the_emitted_bytes(
            pattern in prop::collection::vec(any::<char>(), 1..32),
            byte_target in PREVIEW_BYTES - 8..PREVIEW_BYTES + 256,
        ) {
            let dir = tempfile::tempdir()?;
            let root = std::fs::File::open(dir.path())?;
            let mut text = String::new();
            for ch in pattern.into_iter().cycle() {
                text.push(ch);
                if text.len() >= byte_target { break; }
            }
            let result = finish_output(&root, text.clone()).unwrap();
            if text.len() <= PREVIEW_BYTES {
                prop_assert_eq!(result, text);
                prop_assert!(!dir.path().join(".panda").exists());
            } else {
                let (preview, path) = result.rsplit_once("\nFull output: ").unwrap();
                let (head, tail) = preview.split_once("\n[Output truncated]\n").unwrap();
                prop_assert!(text.starts_with(head) && text.ends_with(tail));
                prop_assert!(head.len().abs_diff(tail.len()) <= 4);
                // Rounding each cut to a UTF-8 boundary costs at most three bytes.
                prop_assert!((PREVIEW_BYTES - 6..=PREVIEW_BYTES).contains(&result.len()));
                prop_assert_eq!(std::fs::read(dir.path().join(path))?, text.as_bytes());
            }
        }
    }
}
