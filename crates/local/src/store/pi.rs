//! Pi v3 entries are archived verbatim. SQLite nodes carry only the messages
//! and checkpoints Panda needs to project the selected path.
mod export;
mod messages;

use super::{Disk, save_snapshot, tree};
use anyhow::{Context, Result, ensure};
use messages::string;
use panda_core::{Checkpoint, CheckpointFiles, Message, Session, Store, UsageRecord};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Write},
    path::Path,
};

pub(super) fn schema(conn: &Connection) -> Result<()> {
    tree::schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pi_headers(session INTEGER PRIMARY KEY,raw TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS pi_records(node INTEGER PRIMARY KEY,external_id TEXT,raw TEXT);
        CREATE TABLE IF NOT EXISTS pi_export_times(node INTEGER PRIMARY KEY, stamp TEXT NOT NULL, millis INTEGER NOT NULL);",
    )?;
    Ok(())
}

// A null stamp means the original label event is already in pi_records.
// Local edits get their own timestamp and are emitted once on export.
pub(super) fn migrate_labels(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE tree_labels(node INTEGER PRIMARY KEY,label TEXT,stamp TEXT)")?;
    if conn.table_exists(None, "pi_records")? {
        conn.execute_batch(
            "INSERT OR REPLACE INTO tree_labels(node,label)
             SELECT t.node,NULLIF(json_extract(e.raw,'$.label'),'') FROM pi_records e
             JOIN tree_nodes event ON event.id=e.node
             JOIN pi_records t ON t.external_id=json_extract(e.raw,'$.targetId')
             JOIN tree_nodes target ON target.id=t.node AND target.session=event.session
             WHERE json_extract(e.raw,'$.type')='label'
               AND (json_type(e.raw,'$.label') IS NULL OR json_type(e.raw,'$.label') IN ('text','null'))
             ORDER BY e.node",
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Position {
    node: i64,
    depth: usize,
    before: usize,
}

fn import(conn: &Connection, sid: i64, mut input: impl BufRead) -> Result<()> {
    schema(conn)?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let header: Value = serde_json::from_str(line.trim_start_matches('\u{feff}'))
        .context("invalid Pi session header")?;
    ensure!(
        header["type"] == "session" && header["version"] == 3,
        "expected Pi v3 session header"
    );
    for field in ["id", "cwd", "timestamp"] {
        string(&header, field)?;
    }
    let external_id = string(&header, "id")?;
    let valid_end = |c: Option<&u8>| c.is_some_and(u8::is_ascii_alphanumeric);
    ensure!(
        valid_end(external_id.as_bytes().first())
            && valid_end(external_id.as_bytes().last())
            && external_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid Pi session id"
    );
    conn.execute(
        "INSERT INTO pi_headers VALUES (?,?)",
        params![sid, line.trim().trim_start_matches('\u{feff}')],
    )?;
    let mut entries = HashMap::new();
    let mut tip = Position::default();
    for (index, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        tip = import_entry(conn, sid, &line, &mut entries)
            .with_context(|| format!("Pi session line {}", index + 2))?;
    }
    let mut state = Session::default();
    tree::restore_path(conn, sid, tip.node, &mut state)?;
    state.name = imported_name(conn, sid)?;
    tree::set_head(conn, sid, tip.node)?;
    save_snapshot(conn, sid, &mut state)
}

fn import_entry(
    conn: &Connection,
    sid: i64,
    line: &str,
    entries: &mut HashMap<String, Position>,
) -> Result<Position> {
    let e: Value = serde_json::from_str(line)?;
    let id = string(&e, "id")?;
    ensure!(
        !id.is_empty() && !entries.contains_key(id),
        "empty or duplicate entry id {id:?}"
    );
    let kind = string(&e, "type")?;
    ensure!(kind != "session", "unexpected second header");
    let mut pos = match e.get("parentId").context("missing parentId")? {
        Value::Null => Position::default(),
        Value::String(p) => *entries
            .get(p)
            .with_context(|| format!("unknown parent {p}"))?,
        _ => anyhow::bail!("invalid parentId"),
    };
    let ancestor = |key: &str| -> Result<Position> {
        let target = entries
            .get(string(&e, key)?)
            .with_context(|| format!("unknown {key}"))?;
        let present: bool = conn.query_row(
            &(tree::ANCESTORS.to_owned() + "SELECT EXISTS(SELECT 1 FROM path WHERE id=?3)"),
            [sid, pos.node, target.node],
            |r| r.get(0),
        )?;
        ensure!(present, "{key} is not on this branch");
        Ok(*target)
    };
    let edit = if kind == "context_edit" {
        let target = ancestor("targetId")?;
        let raw: String = conn.query_row(
            "SELECT raw FROM pi_records WHERE node=?",
            [target.node],
            |r| r.get(0),
        )?;
        let mut source: Value = serde_json::from_str(&raw)?;
        ensure!(
            source["type"] == "custom_message"
                || (source["type"] == "message"
                    && matches!(
                        source["message"]["role"].as_str(),
                        Some("user" | "assistant" | "toolResult" | "custom")
                    )),
            "invalid context edit target"
        );
        let replacement = e.get("replacement").context("missing replacement")?;
        let messages = if replacement.is_null() {
            vec![]
        } else {
            let content = replacement
                .get("content")
                .context("missing replacement content")?
                .clone();
            if source["type"] == "message" {
                source["message"]["content"] = content;
            } else {
                source["content"] = content;
            }
            messages::project(&source)?
        };
        Some((target, serde_json::to_string(&messages)?))
    } else {
        None
    };
    let cp = if kind == "compaction" {
        let kept = string(&e, "firstKeptEntryId")?;
        let through = if kept == id {
            pos.depth
        } else {
            ancestor("firstKeptEntryId")?.before
        };
        // Extension checkpoints may carry unrelated details. Their raw record is retained.
        let details = &e["details"];
        let files: Option<CheckpointFiles> =
            if details["readFiles"].is_array() || details["modifiedFiles"].is_array() {
                serde_json::from_value(details.clone()).ok()
            } else {
                None
            };
        let summary = string(&e, "summary")?;
        let suffix = files
            .as_ref()
            .map(CheckpointFiles::summary_suffix)
            .unwrap_or_default();
        Some(Checkpoint {
            recovery: None,
            through,
            summary: summary.strip_suffix(&suffix).unwrap_or(summary).into(),
            files,
        })
    } else {
        None
    };
    pos.before = pos.depth;
    let projected = messages::project(&e)?;
    let count = projected.len().max(1);
    for index in 0..count {
        let message = projected.get(index);
        let last = index + 1 == count;
        pos.depth += usize::from(message.is_some());
        conn.execute(
            "INSERT INTO tree_nodes(session,parent,depth,message,checkpoint) VALUES(?,?,?,?,?)",
            params![
                sid,
                pos.node,
                pos.depth,
                serde_json::to_string(&message)?,
                cp.as_ref().map(serde_json::to_string).transpose()?
            ],
        )?;
        pos.node = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO pi_records VALUES(?,?,?)",
            params![pos.node, last.then_some(id), last.then_some(line)],
        )?;
    }
    if let Some((target, replacement)) = edit {
        conn.execute(
            "INSERT INTO pi_context_edits VALUES (?,?,?,?)",
            params![pos.node, target.before, target.depth, replacement],
        )?;
    }
    if kind == "label"
        && let Some(target) = e["targetId"].as_str().and_then(|id| entries.get(id))
        && (e["label"].is_string() || e["label"].is_null())
    {
        conn.execute(
            "INSERT OR REPLACE INTO tree_labels(node,label) VALUES (?,NULLIF(?,''))",
            params![target.node, e["label"].as_str()],
        )?;
    }
    entries.insert(id.to_owned(), pos);
    Ok(pos)
}

fn imported_name(conn: &Connection, sid: i64) -> Result<String> {
    Ok(conn.query_row("SELECT json_extract(raw,'$.name') FROM pi_records JOIN tree_nodes ON id=node WHERE session=?
        AND json_extract(raw,'$.type')='session_info' AND json_type(raw,'$.name')='text' ORDER BY node DESC LIMIT 1",
        [sid],|r|r.get(0)).optional()?.unwrap_or_default())
}

impl Disk {
    pub fn import_pi(dir: &Path, repo: &Path, input: impl BufRead) -> Result<Self> {
        Self::open_initialized(dir, repo, None, &Session::default(), |tx, id| {
            import(tx, id, input)
        })
    }

    pub fn export_pi(&mut self, mut out: impl Write) -> Result<()> {
        let state = self.load()?;
        let tx = self.conn.transaction()?;
        schema(&tx)?;
        let tip = tree::archive(&tx, self.id, &state)?;
        let (now, ms): (String, i64) = tx.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now'),CAST(unixepoch('subsec')*1000 AS INTEGER)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let header: Option<String> = tx
            .query_row(
                "SELECT raw FROM pi_headers WHERE session=?",
                [self.id],
                |r| r.get(0),
            )
            .optional()?;
        let header = match header {
            Some(h) => {
                let cwd: String =
                    tx.query_row("SELECT repo FROM sessions WHERE id=?", [self.id], |r| {
                        r.get(0)
                    })?;
                let mut header: Value = serde_json::from_str(&h)?;
                if header["cwd"] == cwd {
                    h
                } else {
                    header["cwd"] = cwd.into();
                    header.to_string()
                }
            }
            None => {
                let (cwd, id): (String, String) = tx.query_row(
                    "SELECT repo,lower(hex(randomblob(16))) FROM sessions WHERE id=?",
                    [self.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let h = json!({"type":"session","version":3,"id":id,"cwd":cwd,"timestamp":now})
                    .to_string();
                tx.execute("INSERT INTO pi_headers VALUES (?,?)", params![self.id, h])?;
                h
            }
        };
        writeln!(out, "{header}")?;
        export::write_entries(&tx, self.id, tip, &now, ms, &state.name, &mut out)?;
        out.flush()?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
