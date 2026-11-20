use anyhow::{Context, Result};
use panda_core::Session;
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn collect(state: &Path, id: i64, session: &Session, bug_report: bool) -> Result<PathBuf> {
    // Every value is generated here or typed as a number, boolean or enum. Paths,
    // environment variables, account settings and arbitrary session strings stay private.
    let mut report = json!({
        "schemaVersion": 1,
        "version": include_str!("../../../version.txt").trim(),
        "os": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "session": { "id": id, "mode": session.mode, "active": session.active,
            "messages": session.history.len(), "revision": session.revision,
            "hasCheckpoint": session.checkpoint.is_some() }
    });
    let prefix = if bug_report {
        report["reproduction"] = json!({"steps": [], "expected": "", "observed": ""});
        "bug-report-"
    } else {
        "diagnostics-"
    };
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".json")
        .tempfile_in(state)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &report)?;
    file.as_file().sync_all()?;
    Ok(file.keep().context("saving private report")?.1)
}

pub fn changelog(version: Option<&str>) -> Result<&'static str> {
    let log = include_str!("../../../CHANGELOG.md");
    let mut releases = log.split("\n## ").skip(1);
    match version {
        None => releases.next(),
        Some(v) => {
            releases.find(|entry| entry.starts_with(&format!("[{}]", v.trim_start_matches('v'))))
        }
    }
    .context("release not found in the bundled changelog")
}
