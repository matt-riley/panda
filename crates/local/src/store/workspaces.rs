use super::{Disk, open_database};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use std::path::{Path, PathBuf};

pub struct SessionLocation {
    pub id: i64,
    pub workspace: PathBuf,
    pub name: String,
}

impl Disk {
    pub fn all_workspaces(dir: &Path) -> Result<Vec<SessionLocation>> {
        let file = dir.join("sessions.sqlite3");
        if !file.try_exists()? {
            return Ok(Vec::new());
        }
        let conn = open_database(&file)?;
        let mut query = conn.prepare("SELECT id,repo,coalesce(json_extract(data,'$.name'),'') FROM sessions ORDER BY id DESC")?;
        Ok(query
            .query_map([], |row| {
                Ok(SessionLocation {
                    id: row.get(0)?,
                    workspace: PathBuf::from(row.get::<_, String>(1)?),
                    name: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn continue_here(&mut self, original: &Path, here: &Path) -> Result<()> {
        ensure!(!original.try_exists()?, "original workspace still exists");
        let changed = self.conn.execute(
            "UPDATE sessions SET repo=? WHERE id=? AND repo=?",
            params![
                here.to_str().context("non UTF-8 workspace")?,
                self.id,
                original.to_str().context("non UTF-8 workspace")?
            ],
        )?;
        ensure!(changed == 1, "session workspace changed during selection");
        Ok(())
    }
}
