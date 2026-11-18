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
        Self::open_seed(dir, repo, id, &Session::default())
    }
    pub fn latest(dir: &Path, repo: &Path) -> Result<Option<i64>> {
        let file = dir.join("sessions.sqlite3");
        if !file.try_exists()? {
            return Ok(None);
        }
        let conn = Connection::open(file)?;
        Ok(conn.query_row(
            "SELECT MAX(id) FROM sessions WHERE repo=?",
            [repo.to_str().context("non UTF-8 workspace")?],
            |row| row.get(0),
        )?)
    }
    pub fn fork(&self, dir: &Path, repo: &Path, turn: Option<usize>, name: &str) -> Result<Self> {
        let mut s = self.load()?;
        ensure!(!s.active, "acknowledge interrupted turn before forking");
        if let Some(n) = turn {
            let end = if n == 0 {
                0
            } else {
                s.history.iter().enumerate().filter(|(_,m)|matches!(m,panda_core::Message::Assistant{calls,..} if calls.is_empty())).nth(n-1).context("unknown completed turn")?.0+1
            };
            s.history.truncate(end);
            s.usage.retain(|u| u.through <= end);
            if s.checkpoint.as_ref().is_some_and(|c| c.through > end) {
                s.checkpoint = None;
            }
        }
        s.name = name.into();
        s.parent = Some(self.id);
        s.revision = 0;
        Self::open_seed(dir, repo, None, &s)
    }
    pub fn catalogue(&self, repo: &Path) -> Result<Vec<(i64, String, Option<i64>)>> {
        let mut stmt=self.conn.prepare("SELECT id,coalesce(json_extract(data,'$.name'),''),json_extract(data,'$.parent') FROM sessions WHERE repo=? ORDER BY id DESC")?;
        Ok(stmt
            .query_map([repo.to_str().context("non UTF-8 workspace")?], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn open_seed(dir: &Path, repo: &Path, id: Option<i64>, seed: &Session) -> Result<Self> {
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
                    params![root, serde_json::to_string(seed)?],
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

#[cfg(test)]
mod branches {
    use super::*;
    use panda_core::Message;
    #[test]
    fn fork_stops_after_selected_answer_and_keeps_parent_intact() {
        let d = tempfile::tempdir().unwrap();
        let mut parent = Disk::open(d.path(), d.path(), None).unwrap();
        let mut s = parent.load().unwrap();
        for word in ["one", "two"] {
            s.history.push(Message::User(word.into()));
            s.history.push(Message::Assistant {
                text: "ok".into(),
                calls: vec![],
            });
        }
        parent.save(&mut s).unwrap();
        let fork = parent
            .fork(d.path(), d.path(), Some(1), "alternative")
            .unwrap();
        assert_eq!(fork.load().unwrap().parent, Some(parent.id()));
        assert_eq!(fork.load().unwrap().history.len(), 2);
        assert_eq!(parent.load().unwrap().history.len(), 4);
    }
}
