//! User-owned decisions and installed Markdown resources. Read-only startup creates no database.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{os::unix::fs::MetadataExt, path::Path, time::Duration};

pub fn database(state: &Path, create: bool) -> Result<Option<Connection>> {
    let file = state.join("resources.sqlite3");
    if !create && !file.try_exists()? {
        return Ok(None);
    }
    let db = if create {
        Connection::open(file)?
    } else {
        Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    };
    db.busy_timeout(Duration::from_secs(5))?;
    if create {
        db.execute_batch("CREATE TABLE IF NOT EXISTS trust(scope TEXT PRIMARY KEY,allowed INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS packages(scope TEXT NOT NULL,name TEXT NOT NULL,source TEXT NOT NULL,
        filter TEXT NOT NULL,bundle TEXT NOT NULL,version INTEGER NOT NULL, PRIMARY KEY(scope,name));")?;
    }
    Ok(Some(db))
}

/// Canonical path and directory identity bind approval to this checkout. Replacing or
/// moving a project requires a new grant, while a second path to the same directory works.
pub fn scope(project: &Path) -> Result<String> {
    let path = project.canonicalize()?;
    let meta = path.metadata()?;
    ensure!(meta.is_dir(), "expected project directory");
    Ok(format!(
        "{}:{}:{}",
        meta.dev(),
        meta.ino(),
        path.to_str().context("non UTF-8 project path")?
    ))
}
pub fn trusted(state: &Path, project: &Path) -> Result<Option<bool>> {
    let Some(db) = database(state, false)? else {
        return Ok(None);
    };
    Ok(db
        .query_row(
            "SELECT allowed FROM trust WHERE scope=?",
            [scope(project)?],
            |r| r.get(0),
        )
        .optional()?)
}
pub fn decide(state: &Path, project: &Path, action: &str) -> Result<()> {
    let value = match action {
        "allow" => Some(true),
        "deny" => Some(false),
        "revoke" => None,
        _ => anyhow::bail!("use allow, deny, revoke or status"),
    };
    let db = database(state, true)?.unwrap();
    let key = scope(project)?;
    if let Some(allowed) = value {
        db.execute("INSERT INTO trust VALUES(?,?) ON CONFLICT(scope) DO UPDATE SET allowed=excluded.allowed",params![key,allowed])?;
    } else {
        db.execute("DELETE FROM trust WHERE scope=?", [key])?;
    }
    Ok(())
}
