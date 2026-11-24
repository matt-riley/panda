//! Namespaced Model Context Protocol resources and tools share the ordinary Panda dispatcher.
mod auth;
mod management;
mod policy;
pub use management::manage;
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
    #[serde(default)]
    exposure: Visibility,
    #[serde(default)]
    tool_exposure: BTreeMap<String, Visibility>,
    oauth: Option<auth::OAuth>,
    command: Option<String>,
    url: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    read_only_tools: BTreeSet<String>,
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Visibility {
    #[default]
    Direct,
    Deferred,
    Hidden,
}
impl ServerSpec {
    fn visibility(&self, tool: &str) -> Visibility {
        self.tool_exposure
            .get(tool)
            .copied()
            .unwrap_or(self.exposure)
    }
}

struct Server {
    runtime: Runtime,
    connection: Mutex<Connection>,
    id: std::sync::atomic::AtomicU64,
}
fn initialize_request(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{
        "protocolVersion": VERSION, "capabilities": {},
        "clientInfo": {"name": "panda", "version": env!("CARGO_PKG_VERSION")}
    }})
}
impl Server {
    fn connect(
        spec: &ServerSpec,
        credentials: Option<auth::Credentials>,
        root: &Path,
        sandbox: Option<&crate::sandbox::Native>,
    ) -> Result<(Self, Value)> {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let connection = {
            let _guard = runtime.enter();
            Connection::open(spec, root, sandbox, credentials)?
        };
        let server = Self {
            runtime,
            connection: Mutex::new(connection),
            id: 1.into(),
        };
        let capabilities = {
            let mut conn = server.connection.lock().unwrap();
            server.runtime.block_on(server.initialize(&mut conn))?
        };
        Ok((server, capabilities))
    }

    async fn initialize(&self, conn: &mut Connection) -> Result<Value> {
        let id = self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let init = tokio_timeout(conn, initialize_request(id), Some(id)).await?;
        ensure!(
            init["protocolVersion"] == VERSION,
            "unsupported MCP protocol version"
        );
        tokio_timeout(
            conn,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            None,
        )
        .await?;
        Ok(init["capabilities"].clone())
    }

    fn call(&self, method: &str, params: Value, cancel: &Cancel, read_only: bool) -> Result<Value> {
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
            let exchange = async {
                let first = tokio_timeout(&mut conn, body.clone(), Some(id)).await;
                if read_only
                    && first
                        .as_ref()
                        .is_err_and(|err| err.is::<transport::Disconnected>())
                    && let Connection::Http { session, .. } = &mut *conn
                {
                    *session = None;
                    cancel.check()?;
                    self.initialize(&mut conn).await?;
                    cancel.check()?;
                    return tokio_timeout(&mut conn, body, Some(id)).await;
                }
                first
            };
            let outcome = tokio::select! {
                result = exchange => result,
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
    visibility: Visibility,
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
        for (name, (spec, local)) in management::configured(state, project, trusted)? {
            if let Err(err) =
                registry.connect(&name, &spec, state, project, local.then_some(native))
            {
                if !err.is::<transport::LoginRequired>() {
                    return Err(err);
                }
                registry.operations.retain(|_, op| op.server != name);
            }
        }
        Ok(registry)
    }

    fn connect(
        &mut self,
        name: &str,
        spec: &ServerSpec,
        state: &Path,
        project: &Path,
        sandbox: Option<&crate::sandbox::Native>,
    ) -> Result<()> {
        let credentials = auth::Credentials::new(state, name, spec, sandbox.is_some())?;
        if let Some(auth) = &credentials
            && auth.token()?.is_none()
        {
            return Ok(());
        }
        let (server, caps) = Server::connect(spec, credentials, project, sandbox)
            .with_context(|| format!("connecting MCP server {name}"))?;
        if caps["tools"].is_object() {
            let mut params = json!({});
            let mut cursors = BTreeSet::new();
            loop {
                let page = server.call("tools/list", params, &Cancel::default(), true)?;
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
                    self.add(
                        name,
                        spec.visibility(tool),
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
                self.add(
                    name,
                    spec.visibility(name_part),
                    method,
                    None,
                    name_part,
                    method,
                    schema,
                    true,
                )?;
            }
        }
        self.servers.insert(name.into(), server);
        Ok(())
    }
    pub fn deferred(&self, name: &str) -> bool {
        self.operations
            .get(name)
            .is_some_and(|op| op.visibility == Visibility::Deferred)
    }
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        server: &str,
        visibility: Visibility,
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
                visibility,
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
            .filter(|op| op.visibility != Visibility::Hidden)
            .map(|o| o.definition.clone())
            .collect()
    }
    pub fn invoke(&self, name: &str, args: &Value, cancel: &Cancel) -> Result<ToolOutput> {
        let op = self
            .operations
            .get(name)
            .filter(|op| op.visibility != Visibility::Hidden)
            .context("unknown MCP operation")?;
        let params = if let Some(remote) = &op.remote {
            json!({"name":remote,"arguments":args})
        } else {
            args.clone()
        };
        let result = self.servers[&op.server].call(
            op.method,
            params,
            cancel,
            op.definition.access == Access::Read,
        )?;
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
