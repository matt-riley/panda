use anyhow::{Context, Result, ensure};
use panda_core::{Session, Store};
use rusqlite::{Connection, params};
use std::{fs, path::Path};
pub struct Disk {
    conn: Connection,
    id: i64,
    _lock: fs::File,
}
impl Disk {
    pub fn open(dir: &Path, repo: &Path, id: Option<i64>) -> Result<Self> {
        let conn = Connection::open(dir.join("sessions.sqlite3"))?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, repo TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL)")?;
        let root = repo.to_str().context("non UTF-8 workspace")?;
        let id = match id {
            Some(id) => {
                ensure!(
                    conn.query_row("SELECT repo FROM sessions WHERE id=?", [id], |r| r
                        .get::<_, String>(0))?
                        == root,
                    "session belongs to another workspace"
                );
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO sessions(repo,data) VALUES (?,?)",
                    params![root, serde_json::to_string(&Session::default())?],
                )?;
                conn.last_insert_rowid()
            }
        };
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("session-{id}.lock")))?;
        lock.try_lock().context("session already open")?;
        Ok(Self {
            conn,
            id,
            _lock: lock,
        })
    }
    pub fn id(&self) -> i64 {
        self.id
    }
    pub fn list(&self, repo: &Path) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE repo=? ORDER BY id DESC")?;
        Ok(stmt
            .query_map([repo.to_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
}
impl Store for Disk {
    fn load(&self) -> Result<Session> {
        Ok(serde_json::from_str(&self.conn.query_row(
            "SELECT data FROM sessions WHERE id=?",
            [self.id],
            |r| r.get::<_, String>(0),
        )?)?)
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        let mut next = s.clone();
        next.revision += 1;
        let updated = self.conn.execute(
            "UPDATE sessions SET revision=?,data=? WHERE id=? AND revision=?",
            params![
                next.revision,
                serde_json::to_string(&next)?,
                self.id,
                s.revision
            ],
        )?;
        ensure!(updated == 1, "stale session revision");
        s.revision = next.revision;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_snapshot_refuses_stale_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Disk::open(dir.path(), dir.path(), None).unwrap();
        let id = a.id();
        let mut stale = a.load().unwrap();
        let mut current = stale.clone();
        current.active = true;
        a.save(&mut current).unwrap();
        drop(a);
        let mut b = Disk::open(dir.path(), dir.path(), Some(id)).unwrap();
        assert!(b.load().unwrap().active);
        assert!(b.save(&mut stale).is_err());
    }
}
