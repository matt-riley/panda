use super::*;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url, header::HeaderValue};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub(super) const VERSION: &str = "2025-06-18";
const LIMIT: usize = 4 * 1024 * 1024;

pub(super) enum Connection {
    Stdio {
        child: Child,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
        line: Vec<u8>,
    },
    Http {
        client: Client,
        url: Url,
        session: Option<HeaderValue>,
    },
}

impl Connection {
    pub fn open(
        spec: &ServerSpec,
        root: &Path,
        sandbox: Option<&crate::sandbox::Native>,
    ) -> Result<Self> {
        match (&spec.command, &spec.url) {
            (Some(program), None) => {
                let mut c = if let Some(native) = sandbox {
                    let args = spec.args.iter().map(String::as_str).collect::<Vec<_>>();
                    Command::from(native.project_server_command(root, Path::new(program), &args)?)
                } else {
                    let mut c = Command::new(program);
                    c.args(&spec.args).current_dir(root).env_clear();
                    for name in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SystemRoot"] {
                        if let Some(v) = std::env::var_os(name) {
                            c.env(name, v);
                        }
                    }
                    c
                };
                c.envs(&spec.env)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .process_group(0);
                let mut child = c.spawn().context("starting MCP server")?;
                let input = child.stdin.take().context("missing MCP stdin")?;
                let output = BufReader::new(child.stdout.take().context("missing MCP stdout")?);
                Ok(Self::Stdio {
                    child,
                    input,
                    output,
                    line: Vec::new(),
                })
            }
            (None, Some(raw)) => {
                let url = Url::parse(raw)?;
                ensure!(
                    matches!(url.scheme(), "https" | "http")
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "invalid MCP HTTP URL"
                );
                Ok(Self::Http {
                    client: Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?,
                    url,
                    session: None,
                })
            }
            _ => bail!("specify one MCP command or url"),
        }
    }
    async fn write(input: &mut ChildStdin, body: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(body)?;
        ensure!(line.len() <= LIMIT, "MCP request too large");
        line.push(b'\n');
        input.write_all(&line).await?;
        input.flush().await?;
        Ok(())
    }
    pub async fn exchange(&mut self, body: Value, id: Option<u64>) -> Result<Value> {
        match self {
            Self::Stdio {
                input,
                output,
                line,
                ..
            } => {
                Self::write(input, &body).await?;
                let Some(id) = id else { return Ok(Value::Null) };
                loop {
                    loop {
                        let buf = output.fill_buf().await?;
                        ensure!(!buf.is_empty(), "MCP server disconnected");
                        let end = buf.iter().position(|b| *b == b'\n');
                        let n = end.map_or(buf.len(), |i| i + 1);
                        ensure!(line.len() + n <= LIMIT, "MCP frame too large");
                        line.extend_from_slice(&buf[..n]);
                        output.consume(n);
                        if end.is_some() {
                            break;
                        }
                    }
                    let message: Value =
                        serde_json::from_slice(&*line).context("invalid MCP JSON-RPC frame")?;
                    line.clear();
                    if message.get("method").is_some() && message.get("id").is_some() {
                        let reply = if message["method"] == "ping" {
                            json!({"jsonrpc":"2.0","id":message["id"],"result":{}})
                        } else {
                            json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Unsupported client method"}})
                        };
                        Self::write(input, &reply).await?;
                    } else if let Some(payload) = response(&message, id)? {
                        return Ok(payload);
                    }
                }
            }
            Self::Http {
                client,
                url,
                session,
            } => {
                let mut req = client
                    .post(url.clone())
                    .header("accept", "application/json, text/event-stream")
                    .json(&body);
                if body["method"] != "initialize" {
                    req = req.header("MCP-Protocol-Version", VERSION);
                }
                if let Some(s) = session.as_ref() {
                    req = req.header("Mcp-Session-Id", s);
                }
                let mut res = req.send().await.context("MCP HTTP request failed")?;
                ensure!(
                    res.status().is_success(),
                    "MCP HTTP status {}",
                    res.status().as_u16()
                );
                if body["method"] == "initialize" {
                    *session = res.headers().get("Mcp-Session-Id").cloned();
                }
                let Some(id) = id else { return Ok(Value::Null) };
                let sse = res
                    .headers()
                    .get("content-type")
                    .and_then(|s| s.to_str().ok())
                    .is_some_and(|s| s.split(';').next() == Some("text/event-stream"));
                let mut bytes = Vec::new();
                let mut event = String::new();
                while let Some(chunk) = res.chunk().await? {
                    ensure!(
                        bytes.len() + event.len() + chunk.len() <= LIMIT,
                        "MCP HTTP response too large"
                    );
                    bytes.extend_from_slice(&chunk);
                    if sse {
                        while let Some(i) = bytes.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = bytes.drain(..=i).collect();
                            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
                            if line.is_empty() && !event.is_empty() {
                                let message = serde_json::from_str(&event)?;
                                event.clear();
                                if let Some(payload) = response(&message, id)? {
                                    return Ok(payload);
                                }
                            } else if let Some(data) = line.strip_prefix("data:") {
                                event.push_str(data.strip_prefix(' ').unwrap_or(data));
                                event.push('\n');
                            }
                        }
                    }
                }
                if sse {
                    bail!("MCP event stream ended before response")
                }
                response(&serde_json::from_slice(&bytes)?, id)?.context("MCP response ID mismatch")
            }
        }
    }
    pub async fn close(&mut self) {
        match self {
            Self::Stdio { child, input, .. } => {
                let _ = input.shutdown().await;
                if let Some(pid) = child.id() {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let _ = child.wait().await;
            }
            Self::Http {
                client,
                url,
                session: Some(id),
            } => {
                let _ = client
                    .delete(url.clone())
                    .header("Mcp-Session-Id", id.clone())
                    .header("MCP-Protocol-Version", VERSION)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await;
            }
            _ => {}
        }
    }
}
fn response(msg: &Value, id: u64) -> Result<Option<Value>> {
    ensure!(msg["jsonrpc"] == "2.0", "invalid MCP JSON-RPC version");
    if msg.get("method").is_some() || msg["id"].as_u64() != Some(id) {
        return Ok(None);
    }
    if let Some(err) = msg.get("error") {
        bail!(
            "MCP error {}: {}",
            err["code"],
            err["message"].as_str().unwrap_or("request failed")
        )
    }
    Ok(Some(
        msg.get("result")
            .context("MCP response missing result")?
            .clone(),
    ))
}
