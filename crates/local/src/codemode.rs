//! Sandboxed JavaScript composition for the native host.
mod admission;
mod images;
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
const INFERENCE_TIMEOUT_MS: u64 = 180_000;
const SCRIPT_BYTES: usize = 64 * 1024;
const PREVIEW_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = crate::MAX_TOOL_RESULT_BYTES;
const OUTPUT_LIMIT_ERROR: &str = "Codemode text output exceeds 1 MiB";
fn argument_limit(name: &str) -> usize {
    if name == "classify" {
        MAX_IMAGE_URL_BYTES
    } else {
        ARG_BYTES
    }
}
const ARG_BYTES: usize = 16 * 1024;
const MAX_CALLS: usize = 64;
// Bound the total retained across nested calls, including base64 expansion.
const MAX_IMAGE_URL_BYTES: usize = 12 * 1024 * 1024;
const HELPERS: &[(&str, &str, &str)] = &[
    (
        "write",
        "string",
        "tools.write({path,text}) writes UTF-8 text in the workspace.",
    ),
    (
        "edit",
        "string",
        "tools.edit({path,old,new}) or tools.edit({path,edits}) applies exact replacements.",
    ),
    (
        "shell_result",
        "{output: string, exitCode: number | null, elapsedMs?: number, cancelled: boolean, truncated: boolean, fullOutputPath?: string}",
        "tools.shell_result({id}) returns a completed shell result on this branch.",
    ),
    (
        "ls",
        "{items: {name: string, type: string}[], next_offset: number | null}",
        "tools.ls({path?,offset?}) lists immediate entries (name,type), includes dotfiles, defaults to the root, and returns {items,next_offset}.",
    ),
    (
        "search",
        "{items: {path: string, line: number, text: string, before?: string[], after?: string[]}[], next_offset: number | null}",
        "tools.search({query,path?,regex?,ignoreCase?,context?,glob?,offset?}) returns {items,next_offset} with path,line,text per hit. Search defaults to case-sensitive literal matching. Set regex:true for a Rust regex and ignoreCase:true to ignore case. Positive context adds before and after arrays in file order. Only matches count toward offsets. Search respects .gitignore and accepts workspace-relative globs such as **/*.rs.",
    ),
    (
        "repo_map",
        "{items: string[], next_offset: number | null}",
        "tools.repo_map({path?,glob?,offset?}) returns {items,next_offset} with workspace paths. It respects .gitignore and accepts workspace-relative globs such as **/*.rs.",
    ),
    (
        "read",
        "string | ImageBlock",
        "tools.read({path,offset?,limit?}) returns file text or an image block {type, data, mimeType, note}. Use image(block) to select an image in output order. Without image(), nested image reads attach automatically for older scripts; only text supports line ranges.",
    ),
];
// A JSON-escaped byte can occupy six bytes in the outer protocol record.
// Include the descriptions and declarations of the permitted workspace helpers.
const REQUEST_FRAME: usize = 6 * SCRIPT_BYTES + 16 * 1024;
const MAX_ERROR_CHARS: usize = 1024;
const WORKER_FRAME: usize = 6 * storage::TOTAL_CHARS + 6 * MAX_ERROR_CHARS + 1024;
const REPLY_FRAME: usize = MAX_IMAGE_URL_BYTES + 32 * 1024 * 1024 + 8192;
// Finished messages must also fit when every output byte needs JSON escaping.
const _: () = assert!(WORKER_FRAME >= 6 * (MAX_OUTPUT_BYTES + MAX_ERROR_CHARS) + 64);

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ScriptOptions {
    max_output_tokens: usize,
    timeout_ms: u64,
    #[serde(rename = "async")]
    asynchronous: bool,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        Self {
            max_output_tokens: PREVIEW_BYTES / 4,
            timeout_ms: TIME_LIMIT.as_millis() as u64,
            asynchronous: false,
        }
    }
}

impl ScriptOptions {
    fn parse(script: &str) -> Result<Self> {
        let mut options = Self::default();
        for (index, line) in script.lines().enumerate() {
            let Some(comment) = line.trim_start().strip_prefix("//") else {
                continue;
            };
            let Some(body) = comment.trim_start().strip_prefix("@options") else {
                continue;
            };
            ensure!(index == 0, "Codemode @options must be on the first line");
            let json = body
                .trim_start()
                .strip_prefix(':')
                .context("expected // @options: { ... }")?;
            ensure!(
                json.trim_start().starts_with('{'),
                "Codemode @options must be a JSON object"
            );
            options = serde_json::from_str(json).context("invalid Codemode @options")?;
            ensure!(
                (1..=PREVIEW_BYTES / 4).contains(&options.max_output_tokens),
                "max_output_tokens must be between 1 and 4096"
            );
            ensure!(
                (1..=INFERENCE_TIMEOUT_MS).contains(&options.timeout_ms),
                "timeout_ms outside supported range"
            );
        }
        Ok(options)
    }
}

const TRUNCATION: &str = "\n[Output truncated]\n";
struct Preview {
    text: String,
    omitted: Option<std::ops::Range<usize>>,
}
impl Preview {
    fn offset(&self, offset: usize) -> usize {
        match &self.omitted {
            Some(cut) if offset >= cut.end => cut.start + TRUNCATION.len() + offset - cut.end,
            Some(cut) if offset > cut.start => cut.start,
            _ => offset,
        }
    }
}

fn finish_output(root: &std::fs::File, text: String, preview_bytes: usize) -> Result<Preview> {
    ensure!(text.len() <= MAX_OUTPUT_BYTES, OUTPUT_LIMIT_ERROR);
    if text.len() <= preview_bytes {
        return Ok(Preview {
            text,
            omitted: None,
        });
    }
    let (path, _) =
        crate::output_log::create_log(root, "codemode", |file| file.write_all(text.as_bytes()))
            .context("could not save full Codemode output")?;
    let middle = TRUNCATION;
    let footer = format!("\nFull output: {path}");
    let budget = preview_bytes.saturating_sub(middle.len() + footer.len());
    let head = text.floor_char_boundary(budget / 2);
    let tail = text.ceil_char_boundary(text.len() - budget.div_ceil(2));
    Ok(Preview {
        text: format!("{}{middle}{}{footer}", &text[..head], &text[tail..]),
        omitted: Some(head..tail),
    })
}

struct NestedHelper<'a> {
    name: &'static str,
    function: &'a Value,
    result_type: &'static str,
    usage: &'static str,
}

#[derive(Serialize)]
struct ToolEntry {
    name: String,
    description: String,
    namespace: String,
    output: &'static str,
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
            name: self.name.into(),
            namespace: "workspace".into(),
            output: match self.name {
                "read" => "file",
                "write" | "edit" => "text",
                _ => "json",
            },
            description: format!(
                "{} {}\n{declaration}",
                self.function["description"].as_str().unwrap_or_default(),
                self.usage
            ),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ScriptOutput {
    text: String,
    error: Option<String>,
    offsets: Vec<usize>,
    items: Vec<panda_core::OutputItem>,
}

#[derive(Serialize, Deserialize)]
enum WorkerMessage {
    Invoke { name: String, args: Value },
    Batch(Vec<(String, Value)>),
    Load { key: String },
    Store { key: String, json: Option<String> },
    Image { url: String },
    Finished(ScriptOutput),
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
    deadline: Instant,
) -> Result<()> {
    let outcome = loop {
        if let Err(err) = cancel.check() {
            break Err(err);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Err(anyhow::anyhow!("Codemode time limit exceeded"));
        }
        if let Err(mpsc::RecvTimeoutError::Disconnected) =
            done.recv_timeout(remaining.min(Duration::from_millis(10)))
        {
            if Instant::now() >= deadline {
                break Err(anyhow::anyhow!("Codemode time limit exceeded"));
            }
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
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        cancel.check()?;
        panda_core::validate_tool_args(
            &json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}),
            &call.args,
            "codemode",
        )?;
        let script = panda_coding::field(&call.args, "code")?;
        ensure!(
            script.len() <= SCRIPT_BYTES,
            "Codemode script exceeds 64 KiB"
        );
        let options = ScriptOptions::parse(script)?;
        let mut store = ScriptStore::from_history(&session.history);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(options.timeout_ms);
        let definitions = self.inner.nested_definitions(session);
        if options.timeout_ms > TIME_LIMIT.as_millis() as u64 {
            ensure!(
                definitions
                    .iter()
                    .any(|d| d.namespace.as_deref() == Some("models")
                        && d.access == panda_core::Access::Mutate),
                "timeout_ms exceeds 10000 without an enabled inference tool"
            );
        }
        let schemas: Vec<_> = definitions.iter().map(|d| d.schema.clone()).collect();
        let mut catalog: Vec<_> = Self::helpers(&schemas).map(|h| h.entry()).collect();
        for d in &definitions {
            if let Some(namespace) = &d.namespace
                && !catalog.iter().any(|entry| entry.name == d.name())
            {
                catalog.push(ToolEntry {
                    name: d.name().to_owned(),
                    description: d.schema["function"]["description"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    namespace: namespace.clone(),
                    output: match d.output {
                        panda_core::OutputKind::Text => "text",
                        panda_core::OutputKind::Json => "json",
                        panda_core::OutputKind::File => "file",
                    },
                });
            }
        }
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
        let _slot = admission::acquire(cancel, deadline)?;
        cancel.check()?;
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
        let (result, automatic, selected) = std::thread::scope(|scope| {
            let (finished, done) = mpsc::channel::<()>();
            let nested_cancel = nested.clone();
            let watchdog =
                scope.spawn(move || watch(child, done, cancel, &nested_cancel, deadline));
            let result = (|| {
                send(&mut output, &(script, &catalog))?;
                let mut count = 0;
                let mut images = Vec::new();
                let mut image_bytes = 0;
                let mut selected = images::Selected::default();
                loop {
                    match receive(&mut input, WORKER_FRAME.max(MAX_IMAGE_URL_BYTES + 1024))? {
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
                        WorkerMessage::Image { url } => {
                            let reply = selected.push(url).map(|()| "").map_err(|e| e.to_string());
                            send(&mut output, &reply)?;
                        }
                        WorkerMessage::Finished(result) => {
                            return Ok((result, images, selected));
                        }
                        message @ (WorkerMessage::Invoke { .. } | WorkerMessage::Batch(_)) => {
                            let (batch, requests) = match message {
                                WorkerMessage::Invoke { name, args } => (false, vec![(name, args)]),
                                WorkerMessage::Batch(requests) => (true, requests),
                                _ => unreachable!(),
                            };
                            ensure!(
                                !requests.is_empty() && requests.len() <= 4,
                                "invalid nested batch size"
                            );
                            ensure!(
                                count + requests.len() <= MAX_CALLS,
                                "Codemode nested call limit exceeded"
                            );
                            let mut calls = Vec::new();
                            for (name, args) in requests {
                                cancel.check()?;
                                nested.check()?;
                                count += 1;
                                ensure!(
                                    catalog.iter().any(|entry| entry.name == name),
                                    "unavailable nested tool {name}"
                                );
                                ensure!(
                                    args.to_string().len() <= argument_limit(&name),
                                    "nested arguments exceed size limit (16 KiB for ordinary calls)"
                                );
                                let child = Call {
                                    id: format!("{}/{}", call.id, count),
                                    name,
                                    args,
                                };
                                emit(Event::NestedCall {
                                    parent: call.id.clone(),
                                    call: child.clone(),
                                });
                                calls.push(child);
                            }
                            let run = |c: &Call| {
                                let mut attempts = Vec::new();
                                let result = nested.check().and_then(|()| {
                                    panda_core::run_nested_with_events(
                                        self.inner,
                                        session,
                                        c,
                                        &nested,
                                        &mut |e| {
                                            if let Event::ModelAttempt(a) = e {
                                                attempts.push(a);
                                            }
                                        },
                                    )
                                });
                                (result, attempts)
                            };
                            // A mixed batch must observe writes in submission order.
                            let parallel = calls.len() > 1
                                && calls.iter().all(|c| {
                                    definitions.iter().any(|d| {
                                        d.name() == c.name && d.access == panda_core::Access::Read
                                    })
                                });
                            let results: Vec<_> = if parallel {
                                std::thread::scope(|scope| {
                                    let workers: Vec<_> =
                                        calls.iter().map(|c| scope.spawn(|| run(c))).collect();
                                    workers
                                        .into_iter()
                                        .map(|h| {
                                            h.join().unwrap_or_else(|_| {
                                                (
                                                    Err(anyhow::anyhow!("nested worker panicked")),
                                                    Vec::new(),
                                                )
                                            })
                                        })
                                        .collect()
                                })
                            } else {
                                calls.iter().map(run).collect()
                            };
                            let mut replies = Vec::new();
                            for (c, (result, attempts)) in calls.iter().zip(results) {
                                for attempt in attempts {
                                    emit(Event::ModelAttempt(attempt));
                                }
                                let reply = result.and_then(|out| {
                                    if let Some(path) = c.args["path"].as_str() {
                                        match c.name.as_str() {
                                            "read" => emit(Event::NestedRead(path.to_owned())),
                                            "write" | "edit" => emit(Event::NestedWrite(path.to_owned())),
                                            _ => {}
                                        }
                                    }
                                    ensure!(out.text.len() <= crate::MAX_TOOL_RESULT_BYTES, "nested result exceeds 1 MiB");
                                    let added: usize = out.images.iter().map(String::len).sum();
                                    ensure!(image_bytes + added <= MAX_IMAGE_URL_BYTES, "Codemode image budget exceeded");
                                    image_bytes += added;
                                    if catalog.iter().any(|entry| entry.name == c.name && entry.output == "file") {
                                        let response = serde_json::to_string(&json!({"text":out.text,"image":out.images.first()}))?;
                                        images.extend(out.images);
                                        Ok(response)
                                    } else {
                                        Ok(out.text)
                                    }
                                }).map_err(|e| e.to_string());
                                replies.push(reply);
                            }
                            if batch {
                                send(
                                    &mut output,
                                    &Ok::<_, String>(serde_json::to_string(&replies)?),
                                )?;
                            } else {
                                send(&mut output, &replies[0])?;
                            }
                        }
                    }
                }
            })();
            drop(finished);
            watchdog.join().expect("Codemode watchdog panicked")?;
            result
        })?;
        cancel.check()?;
        let preview = finish_output(&root, result.text, options.max_output_tokens * 4);
        if let Some(error) = result.error {
            let text = preview
                .map(|p| p.text)
                .unwrap_or_else(|log_error| log_error.to_string());
            if text.is_empty() {
                anyhow::bail!("{error}");
            }
            anyhow::bail!("{text}\nScript error: {error}");
        }
        let preview = preview?;
        let (images, positions) = selected.finish(&root, automatic, &result.offsets, &preview)?;
        Ok((
            ToolOutput {
                details: Some(panda_core::ToolDetails::Codemode {
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    success: true,
                    images: positions,
                    // The bounded preview includes an omission marker and a full log reference.
                    // Its image positions are mapped separately when original item ranges disappear.
                    items: if preview.omitted.is_none() {
                        result.items
                    } else {
                        vec![]
                    },
                }),
                text: preview.text,
                images,
            },
            store.writes,
        ))
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
    fn definitions(&self, m: Mode) -> Vec<panda_core::ToolDefinition> {
        let mut definitions = self.inner.definitions(m);
        let schema = codemode_schema(&definitions, &self.inner.schemas(m), m);
        definitions.push(panda_core::ToolDefinition {
            schema,
            access: panda_core::Access::Stateful,
            output: panda_core::OutputKind::Text,
            namespace: None,
        });
        definitions
    }
    fn schemas_for(&self, s: &Session) -> Vec<Value> {
        let mut schemas = self.inner.schemas_for(s);
        schemas.push(codemode_schema(
            &self.inner.definitions(s.mode),
            &schemas,
            s.mode,
        ));
        schemas
    }
    fn run(
        &self,
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        if call.name == "codemode" {
            self.execute(session, call, cancel, emit)
        } else {
            self.inner.run(session, call, cancel, emit)
        }
    }

    fn system(&self) -> Result<String> {
        let mut prompt = self.inner.system()?;
        self.append_guidance(&mut prompt);
        Ok(prompt)
    }
}

fn codemode_schema(
    definitions: &[panda_core::ToolDefinition],
    schemas: &[Value],
    mode: Mode,
) -> Value {
    let mut helpers = String::new();
    for d in definitions.iter().filter(|d| {
        d.composable(mode)
            && schemas
                .iter()
                .any(|schema| schema["function"]["name"] == d.name())
    }) {
        let usage = HELPERS
            .iter()
            .find(|(name, _, _)| *name == d.name())
            .map(|(_, _, usage)| (*usage).to_owned())
            .unwrap_or_else(|| {
                format!(
                    "tools.{}: {} ",
                    d.name(),
                    d.schema["function"]["description"]
                        .as_str()
                        .unwrap_or_default()
                )
            });
        // The provider can retrieve the full filtered namespace inside the script.
        if helpers.len() + usage.len() < 3_000 * 4 {
            helpers.push_str(&usage);
            helpers.push(' ');
        }
    }
    json!({"type":"function","function":{
        "name":"codemode",
        "description":format!("Run isolated JavaScript to compose permitted workspace tools. {helpers} Use asyncTools or first-line // @options: {{\"async\":true}} for awaitable tools and Promise.allSettled(). Synchronous tools remain available by default. describeNamespace(name) returns allowed members. ALL_TOOLS lists callable helpers as {{name, description}}. searchTools(query, {{limit?}}) ranks them, and describeTool(name) returns a description and typed signature or undefined. Use text(value) and image(block or local data URL) to emit ordered output. Explicit images are saved under .panda; remote URLs are rejected. A top-level return adds its resolved value after text() unless it is undefined. console.log/info/warn/error/debug lines follow together in one <console_output> block. exit() finishes immediately with the output and stored values so far. If a script throws, emitted text precedes the error and pending store() values are discarded. Completed file edits persist. An optional first line // @options: {{\"max_output_tokens\": 400, \"timeout_ms\": 2000}} reduces the preview (1–4096 tokens, four bytes each) or sets the whole-script deadline. The default is 10 seconds. Enabled inference helpers allow up to 180000 ms. Above the preview limit the result shows the beginning, end and workspace-relative full log path for line-range reads. store(key,value) and load(key) keep small JSON values on this branch across successful scripts and session resumes. Missing keys return undefined. Storing undefined deletes a key. Limits: 64 calls, 1 MiB text output, 12 MiB of encoded images."),
        "parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}
    }})
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
            let result = finish_output(&root, text.clone(), PREVIEW_BYTES).unwrap().text;
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
