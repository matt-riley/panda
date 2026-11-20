//! Namespaced Model Context Protocol resources and tools share the ordinary Panda dispatcher.
mod transport;
use anyhow::{Context, Result, ensure};
use panda_core::{Access, Cancel, OutputKind, ToolDefinition, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};
use transport::{Connection, VERSION};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FileConfig {
    mcp_servers: BTreeMap<String, ServerSpec>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ServerSpec {
    command: Option<String>,
    url: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    read_only_tools: BTreeSet<String>,
}
struct Server {
    runtime: Runtime,
    connection: Mutex<Connection>,
    id: std::sync::atomic::AtomicU64,
}
impl Server {
    fn connect(
        spec: &ServerSpec,
        root: &Path,
        sandbox: Option<&crate::sandbox::Native>,
    ) -> Result<(Self, Value)> {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let connection = {
            let _guard = runtime.enter();
            Connection::open(spec, root, sandbox)?
        };
        let server = Self {
            runtime,
            connection: Mutex::new(connection),
            id: 1.into(),
        };
        let hello = json!({
            "protocolVersion": VERSION,
            "capabilities": {},
            "clientInfo": {"name": "panda", "version": env!("CARGO_PKG_VERSION")}
        });
        let init = server.call("initialize", hello, &Cancel::default())?;
        ensure!(
            init["protocolVersion"] == VERSION,
            "unsupported MCP protocol {}",
            init["protocolVersion"]
        );
        let mut conn = server.connection.lock().unwrap();
        server.runtime.block_on(tokio_timeout(
            &mut conn,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            None,
        ))?;
        drop(conn);
        Ok((server, init["capabilities"].clone()))
    }
    fn call(&self, method: &str, params: Value, cancel: &Cancel) -> Result<Value> {
        cancel.check()?;
        let id = self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut conn = loop {
            match self.connection.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::WouldBlock) => {
                    cancel.check()?;
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => anyhow::bail!("{e}"),
            }
        };
        self.runtime.block_on(async {
            let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let cancelled = async {
                while cancel.check().is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let outcome = tokio::select! {
                result = tokio_timeout(&mut conn, body, Some(id)) => result,
                _ = cancelled => Err(anyhow::anyhow!("cancelled")),
            };
            if cancel.check().is_err()
                || outcome
                    .as_ref()
                    .is_err_and(|e| e.is::<tokio::time::error::Elapsed>())
            {
                let notice = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": "client cancelled"}
                });
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), conn.exchange(notice, None)).await;
            }
            outcome
        })
    }
}
async fn tokio_timeout(conn: &mut Connection, body: Value, id: Option<u64>) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(30), conn.exchange(body, id)).await?
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(conn) = self.connection.get_mut() {
            self.runtime.block_on(conn.close());
        }
    }
}
struct Operation {
    definition: ToolDefinition,
    server: String,
    method: &'static str,
    remote: Option<String>,
}
#[derive(Default)]
pub struct Servers {
    servers: BTreeMap<String, Server>,
    operations: BTreeMap<String, Operation>,
}
impl Servers {
    pub fn load(
        state: &Path,
        project: &Path,
        trusted: bool,
        native: &crate::sandbox::Native,
    ) -> Result<Self> {
        let mut registry = Self::default();
        for (path, sandbox) in std::iter::once((state.join("mcp.json"), None))
            .chain(trusted.then(|| (project.join(".panda/mcp.json"), Some(native))))
        {
            let Some(raw) = crate::resources::optional_text(&path)? else {
                continue;
            };
            let file: FileConfig = serde_json::from_str(&raw)
                .with_context(|| format!("invalid {}", path.display()))?;
            for (name, spec) in file.mcp_servers {
                ensure!(
                    !registry.servers.contains_key(&name),
                    "duplicate MCP server {name}"
                );
                ensure!(valid(&name), "invalid MCP server name {name}");
                let (server, caps) = Server::connect(&spec, project, sandbox)
                    .with_context(|| format!("connecting MCP server {name}"))?;
                if caps["tools"].is_object() {
                    let mut params = json!({});
                    let mut cursors = BTreeSet::new();
                    loop {
                        let page = server.call("tools/list", params, &Cancel::default())?;
                        for t in page["tools"]
                            .as_array()
                            .context("invalid MCP tools response")?
                        {
                            let tool = t["name"]
                                .as_str()
                                .filter(|s| valid(s))
                                .context("invalid MCP tool name")?;
                            ensure!(
                                t["inputSchema"]["type"] == "object",
                                "MCP tool {tool} needs object inputSchema"
                            );
                            registry.add(
                                &name,
                                "tools/call",
                                Some(tool),
                                tool,
                                t["description"].as_str().unwrap_or(tool),
                                t["inputSchema"].clone(),
                                spec.read_only_tools.contains(tool),
                            )?;
                        }
                        let Some(cursor) = page["nextCursor"].as_str() else {
                            break;
                        };
                        ensure!(
                            cursors.len() < 256 && cursors.insert(cursor.to_owned()),
                            "invalid MCP tool pagination"
                        );
                        params = json!({"cursor":cursor});
                    }
                }
                if caps["resources"].is_object() {
                    for (name_part, method, arg, required) in [
                        ("resources_list", "resources/list", "cursor", false),
                        (
                            "resources_templates",
                            "resources/templates/list",
                            "cursor",
                            false,
                        ),
                        ("resources_read", "resources/read", "uri", true),
                    ] {
                        let schema = json!({
                            "type": "object",
                            "properties": {(arg): {"type":"string"}},
                            "required": if required {vec![arg]} else {vec![]},
                            "additionalProperties": false
                        });
                        registry.add(&name, method, None, name_part, method, schema, true)?;
                    }
                }
                registry.servers.insert(name, server);
            }
        }
        Ok(registry)
    }
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        server: &str,
        method: &'static str,
        remote: Option<&str>,
        suffix: &str,
        desc: &str,
        params: Value,
        read: bool,
    ) -> Result<()> {
        let name = format!("mcp_{server}_{suffix}");
        ensure!(
            self.operations.len() < 1024
                && !self.operations.contains_key(&name)
                && name.len() <= 64,
            "duplicate, too long or too many MCP tools: {name}"
        );
        let definition = ToolDefinition {
            schema: json!({"type":"function","function":{"name":name,"description":desc,"parameters":params}}),
            access: if read { Access::Read } else { Access::Mutate },
            output: OutputKind::Json,
            namespace: read.then(|| format!("mcp.{server}")),
        };
        self.operations.insert(
            name,
            Operation {
                definition,
                server: server.into(),
                method,
                remote: remote.map(str::to_owned),
            },
        );
        Ok(())
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.operations
            .values()
            .map(|o| o.definition.clone())
            .collect()
    }
    pub fn invoke(&self, name: &str, args: &Value, cancel: &Cancel) -> Result<ToolOutput> {
        let op = self.operations.get(name).context("unknown MCP operation")?;
        let params = if let Some(remote) = &op.remote {
            json!({"name":remote,"arguments":args})
        } else {
            args.clone()
        };
        let result = self.servers[&op.server].call(op.method, params, cancel)?;
        ensure!(
            result["isError"] != true,
            "MCP tool error: {}",
            result["content"]
        );
        Ok(serde_json::to_string(&result)?.into())
    }
}
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
