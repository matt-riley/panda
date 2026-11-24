//! The terminal and standalone CLI use the same configuration and lifecycle.
use super::*;
use crate::sandbox::Native;

pub(super) fn configured(
    state: &Path,
    project: &Path,
    trusted: bool,
) -> Result<BTreeMap<String, (ServerSpec, bool)>> {
    let mut configs = BTreeMap::new();
    for (path, local) in std::iter::once((state.join("mcp.json"), false))
        .chain(trusted.then(|| (project.join(".panda/mcp.json"), true)))
    {
        let Some(raw) = crate::resources::optional_text(&path)? else {
            continue;
        };
        let file: FileConfig =
            serde_json::from_str(&raw).with_context(|| format!("invalid {}", path.display()))?;
        for (name, spec) in file.mcp_servers {
            ensure!(valid(&name), "invalid MCP server name");
            ensure!(!configs.contains_key(&name), "duplicate MCP server {name}");
            configs.insert(name, (spec, local));
        }
    }
    policy::apply(state, project, &mut configs)?;
    Ok(configs)
}

pub fn manage(
    state: &Path,
    project: &Path,
    trusted: bool,
    native: Option<&Native>,
    args: &[String],
    show: &mut dyn FnMut(&str),
) -> Result<String> {
    let configs = configured(state, project, trusted)?;
    let status = |name: &str, spec: &ServerSpec, local: bool| -> Result<String> {
        let credentials = auth::Credentials::new(state, name, spec, local)?;
        let login = match credentials {
            Some(auth) if auth.token()?.is_some() => "signed in",
            Some(_) => "signed out",
            None => "no OAuth",
        };
        let transport = if spec.url.is_some() { "HTTP" } else { "stdio" };
        let scope = if local { "project" } else { "personal" };
        Ok(format!("{name} · {scope} {transport} · {login}"))
    };
    if args.is_empty() || args == ["list"] || args == ["status"] {
        let lines = configs
            .iter()
            .map(|(name, (spec, local))| status(name, spec, *local))
            .collect::<Result<Vec<_>>>()?;
        return Ok(if lines.is_empty() {
            "No MCP servers configured".into()
        } else {
            lines.join("\n")
        });
    }
    if let [action, name, selection @ ..] = args
        && action == "exposure"
    {
        let (_, local) = configs.get(name).context("unknown MCP server")?;
        policy::set(state, project, *local, name, selection)?;
        if let Some(native) = native {
            let configs = configured(state, project, trusted)?;
            let (spec, _) = configs
                .get(name)
                .context("MCP server configuration changed")?;
            let prefix = format!("mcp_{name}_");
            for (key, op) in native
                .mcp
                .write()
                .unwrap()
                .operations
                .iter_mut()
                .filter(|(_, op)| &op.server == name)
            {
                op.visibility = spec.visibility(&key[prefix.len()..]);
            }
        }
        return Ok(format!("Updated exposure for {name}"));
    }
    let [action, name] = args else {
        anyhow::bail!("use mcp list or mcp status|login|logout|reconnect|diagnostics SERVER")
    };
    let (spec, local) = configs.get(name).context("unknown MCP server")?;
    if action == "status" {
        return status(name, spec, *local);
    }
    let credentials = auth::Credentials::new(state, name, spec, *local)?;
    match action.as_str() {
        "logout" => {
            let auth = credentials.context("server has no OAuth configuration")?;
            if let Some(native) = native {
                let mut registry = native.mcp.write().unwrap();
                registry.servers.remove(name);
                registry.operations.retain(|_, op| op.server != *name);
            }
            auth.logout()?;
            return Ok(format!("{name} signed out"));
        }
        "login" => {
            let auth = credentials.context("server has no OAuth configuration")?;
            Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(auth.login(
                    spec.oauth.as_ref().and_then(|oauth| oauth.scope.as_deref()),
                    show,
                ))?;
        }
        "reconnect" | "diagnostics" => {}
        _ => anyhow::bail!("use mcp status, login, logout, reconnect or diagnostics"),
    }
    if action == "login" && native.is_none() {
        return Ok(format!("{name} signed in"));
    }
    // Standalone commands need a sandbox only to start a project stdio server.
    let standalone;
    let sandbox = if *local && native.is_none() {
        standalone = Native::new(project, &std::env::current_exe()?)?;
        Some(&standalone)
    } else {
        native.filter(|_| *local)
    };
    let mut new = Servers::default();
    new.connect(name, spec, state, project, sandbox)?;
    ensure!(
        new.servers.contains_key(name),
        "MCP login required for {name}"
    );
    let count = new
        .operations
        .values()
        .filter(|op| op.visibility != Visibility::Hidden)
        .count();
    let summary = format!(
        "{} · connected · {count} tools",
        status(name, spec, *local)?
    );
    let Some(native) = native.filter(|_| action != "diagnostics") else {
        return Ok(summary);
    };
    let mut registry = native.mcp.write().unwrap();
    ensure!(
        new.operations.keys().all(|key| registry
            .operations
            .get(key)
            .is_none_or(|op| op.server == *name)),
        "MCP tool name collides with another server"
    );
    ensure!(
        registry
            .operations
            .values()
            .filter(|op| op.server != *name)
            .count()
            + new.operations.len()
            <= 1024,
        "too many MCP tools"
    );
    registry.operations.retain(|_, op| op.server != *name);
    registry.operations.append(&mut new.operations);
    registry.servers.append(&mut new.servers);
    Ok(summary)
}
