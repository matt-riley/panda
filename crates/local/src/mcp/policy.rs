//! Personal exposure preferences are independent of the server's configuration file.
use super::*;
use rusqlite::{Connection, params};

fn database(state: &Path, create: bool) -> Result<Option<Connection>> {
    let path = state.join("mcp-policy.sqlite3");
    if !create && !path.try_exists()? {
        return Ok(None);
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS exposure(scope TEXT NOT NULL, server TEXT NOT NULL,
        tool TEXT NOT NULL, visibility TEXT NOT NULL, PRIMARY KEY(scope,server,tool))",
    )?;
    Ok(Some(db))
}
fn scope(project: &Path, local: bool) -> Result<String> {
    if local {
        crate::catalog::scope(project)
    } else {
        Ok("personal".into())
    }
}
pub(super) fn apply(
    state: &Path,
    project: &Path,
    configs: &mut BTreeMap<String, (ServerSpec, bool)>,
) -> Result<()> {
    let Some(db) = database(state, false)? else {
        return Ok(());
    };
    let mut query =
        db.prepare("SELECT tool,visibility FROM exposure WHERE scope=? AND server=?")?;
    for (name, (spec, local)) in configs {
        let mut rows = query.query(params![scope(project, *local)?, name])?;
        while let Some(row) = rows.next()? {
            let tool: String = row.get(0)?;
            let value = parse(&row.get::<_, String>(1)?)?;
            if tool.is_empty() {
                spec.exposure = value;
            } else {
                spec.tool_exposure.insert(tool, value);
            }
        }
    }
    Ok(())
}
fn parse(value: &str) -> Result<Visibility> {
    match value {
        "direct" => Ok(Visibility::Direct),
        "deferred" => Ok(Visibility::Deferred),
        "hidden" => Ok(Visibility::Hidden),
        _ => anyhow::bail!("choose direct, deferred, hidden or reset"),
    }
}
pub(super) fn set(
    state: &Path,
    project: &Path,
    local: bool,
    name: &str,
    selection: &[String],
) -> Result<()> {
    let (tool, value) = match selection {
        [value] => ("", value),
        [tool, value] if valid(tool) => (tool.as_str(), value),
        _ => anyhow::bail!("use mcp exposure SERVER [TOOL] direct|deferred|hidden|reset"),
    };
    let scope = scope(project, local)?;
    if value == "reset" {
        if let Some(db) = database(state, false)? {
            db.execute(
                "DELETE FROM exposure WHERE scope=? AND server=? AND tool=?",
                params![scope, name, tool],
            )?;
        }
    } else {
        parse(value)?;
        let db = database(state, true)?.unwrap();
        db.execute(
            "INSERT INTO exposure VALUES(?,?,?,?) ON CONFLICT(scope,server,tool)
            DO UPDATE SET visibility=excluded.visibility",
            params![scope, name, tool, value],
        )?;
    }
    Ok(())
}
