mod pi;
mod tree;
use anyhow::{Context, Result, ensure};
use panda_core::{Session, Store};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{fs, path::Path};
pub use tree::{EntryKind, TreeEntry};
pub struct SessionPreview {
    pub id: i64,
    pub name: String,
    pub activity: i64,
    pub messages: Vec<(&'static str, String)>,
}
pub struct Disk {
    conn: Connection,
    id: i64,
    _lock: fs::File,
    pub invocation_keep_recent_tokens: Option<u64>,
}

fn open_database(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? < 3 {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another process may have migrated the store while we waited for the lock.
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, repo TEXT NOT NULL,
                     revision INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL);
                 ALTER TABLE sessions ADD COLUMN modified_seq INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX sessions_by_activity ON sessions(repo, modified_seq DESC, id DESC);",
            )?;
        }
        if version < 2 {
            pi::migrate_labels(&tx)?;
        }
        if version < 3 {
            tx.execute_batch("CREATE TABLE session_id_floor(id INTEGER PRIMARY KEY CHECK(id=1), retired INTEGER NOT NULL);
                              INSERT INTO session_id_floor VALUES(1,0)")?;
            tx.pragma_update(None, "user_version", 3)?;
        }
        tx.commit()?;
    }
    Ok(conn)
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
        let conn = open_database(&file)?;
        Ok(conn
            .query_row(
                "SELECT id FROM sessions WHERE repo=? ORDER BY modified_seq DESC, id DESC LIMIT 1",
                [repo.to_str().context("non UTF-8 workspace")?],
                |row| row.get(0),
            )
            .optional()?)
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
        self.create_fork(dir, repo, s, name)
    }

    fn create_fork(&self, dir: &Path, repo: &Path, mut s: Session, name: &str) -> Result<Self> {
        if !s.context_edits.is_empty() {
            // A fork starts a new tree with the visible path as its seed.
            let mut boundaries = vec![0];
            let mut history = Vec::with_capacity(s.history.len());
            for (i, message) in s.history.drain(..).enumerate() {
                if let Some(replacement) = s.context_edits.remove(&i) {
                    history.extend(replacement);
                } else {
                    history.push(message);
                }
                boundaries.push(history.len());
            }
            if let Some(cp) = &mut s.checkpoint {
                cp.through = boundaries[cp.through];
            }
            for u in &mut s.usage {
                u.through = boundaries[u.through];
                u.checkpoint_through = boundaries[u.checkpoint_through];
            }
            s.history = history;
            s.context_edits.clear();
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
    /// Read searchable conversation text without opening session locks or
    /// transferring tool payloads, reasoning traces, or image data to the UI.
    pub fn previews(&self, repo: &Path) -> Result<Vec<SessionPreview>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, coalesce(json_extract(s.data, '$.name'), ''),
                    json_extract(m.value, '$.User'),
                    json_extract(m.value, '$.Images.text'),
                    json_extract(m.value, '$.Assistant.text'), s.modified_seq
             FROM sessions AS s LEFT JOIN json_each(s.data, '$.history') AS m
             WHERE s.repo = ? ORDER BY s.id DESC, m.key",
        )?;
        let mut rows = stmt.query([repo.to_str().context("non UTF-8 workspace")?])?;
        let mut sessions: Vec<SessionPreview> = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get(0)?;
            if sessions.last().is_none_or(|s| s.id != id) {
                sessions.push(SessionPreview {
                    id,
                    name: row.get(1)?,
                    activity: row.get(5)?,
                    messages: Vec::new(),
                });
            }
            for (col, role) in [(2, "You"), (3, "You"), (4, "Panda")] {
                if let Some(text) = row.get::<_, Option<String>>(col)?
                    && !text.trim().is_empty()
                {
                    sessions.last_mut().unwrap().messages.push((role, text));
                }
            }
        }
        Ok(sessions)
    }
    fn open_seed(dir: &Path, repo: &Path, id: Option<i64>, seed: &Session) -> Result<Self> {
        Self::open_initialized(dir, repo, id, seed, |_, _| Ok(()))
    }

    fn open_initialized(
        dir: &Path,
        repo: &Path,
        id: Option<i64>,
        seed: &Session,
        initialize: impl FnOnce(&rusqlite::Transaction, i64) -> Result<()>,
    ) -> Result<Self> {
        let path = dir.join("sessions.sqlite3");
        if let Some(id) = id {
            ensure!(path.try_exists()?, "unknown session {id}");
        }
        let mut conn = open_database(&path)?;
        let root = repo.to_str().context("non UTF-8 workspace")?;
        let tx = conn.transaction()?;
        let id = match id {
            Some(id) => {
                let owner: String = tx
                    .query_row("SELECT repo FROM sessions WHERE id=?", [id], |r| r.get(0))
                    .optional()?
                    .with_context(|| format!("unknown session {id}"))?;
                ensure!(owner == root, "session belongs to another workspace");
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO sessions(id,repo,data,modified_seq)
                     SELECT max((SELECT coalesce(max(id),0) FROM sessions),retired)+1, ?1, ?2,
                           (SELECT coalesce(max(modified_seq),0)+1 FROM sessions WHERE repo=?1)
                     FROM session_id_floor",
                    params![root, serde_json::to_string(seed)?],
                )?;
                tx.last_insert_rowid()
            }
        };
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("session-{id}.lock")))?;
        lock.try_lock().context("session already open")?;
        initialize(&tx, id)?;
        tx.commit()?;
        Ok(Self {
            conn,
            id,
            _lock: lock,
            invocation_keep_recent_tokens: None,
        })
    }
    pub fn id(&self) -> i64 {
        self.id
    }
    pub fn delete(self) -> Result<()> {
        // Hold the same exclusive session lock as a writer until every archive row is removed.
        let mut conn = self.conn;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        pi::schema(&tx)?;
        for table in [
            "pi_context_edits",
            "pi_records",
            "pi_export_times",
            "tree_labels",
        ] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE node IN (SELECT id FROM tree_nodes WHERE session=?)"
                ),
                [self.id],
            )?;
        }
        for table in ["pi_headers", "tree_heads", "tree_nodes"] {
            tx.execute(&format!("DELETE FROM {table} WHERE session=?"), [self.id])?;
        }
        tx.execute(
            "UPDATE session_id_floor SET retired=max(retired,?)",
            [self.id],
        )?;
        ensure!(
            tx.execute("DELETE FROM sessions WHERE id=?", [self.id])? == 1,
            "unknown session"
        );
        tx.commit()?;
        Ok(())
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
        let mut session: Session = serde_json::from_str(&self.conn.query_row(
            "SELECT data FROM sessions WHERE id=?",
            [self.id],
            |r| r.get::<_, String>(0),
        )?)?;
        session.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        Ok(session)
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        save_snapshot(&self.conn, self.id, s)
    }
}
fn save_snapshot(conn: &Connection, id: i64, s: &mut Session) -> Result<()> {
    let mut next = s.clone();
    next.revision += 1;
    let updated = conn.execute(
        "UPDATE sessions SET revision=?, data=?,
             modified_seq=(SELECT max(previous.modified_seq)+1 FROM sessions AS previous
                           WHERE previous.repo=sessions.repo)
         WHERE id=? AND revision=?",
        params![next.revision, serde_json::to_string(&next)?, id, s.revision],
    )?;
    ensure!(updated == 1, "stale session revision");
    s.revision = next.revision;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_sessions_keep_creation_order_until_a_successful_save() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let db = Connection::open(repo.join("sessions.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id INTEGER PRIMARY KEY, repo TEXT NOT NULL,
                 revision INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL)",
        )
        .unwrap();
        let seed = serde_json::to_string(&Session::default()).unwrap();
        for _ in 0..2 {
            db.execute(
                "INSERT INTO sessions(repo,data) VALUES (?,?)",
                params![repo.to_str(), seed],
            )
            .unwrap();
        }
        drop(db);
        let latest = || Disk::latest(dir.path(), repo).unwrap().unwrap();
        assert_eq!(latest(), 2);
        let mut first = Disk::open(dir.path(), repo, Some(1)).unwrap();
        let mut state = first.load().unwrap();
        first.save(&mut state).unwrap();
        assert_eq!(latest(), 1);
        let newest = Disk::open(dir.path(), repo, None).unwrap();
        assert_eq!(latest(), newest.id());
        first.previews(repo).unwrap();
        assert!(first.save(&mut Session::default()).is_err());
        assert_eq!(latest(), newest.id());
    }

    #[test]
    fn previews_are_workspace_scoped_and_read_only_while_sessions_are_open() {
        use panda_core::Message;
        let dir = tempfile::tempdir().unwrap();
        let mut disk = Disk::open(dir.path(), dir.path(), None).unwrap();
        let mut session = disk.load().unwrap();
        session.name = "same name".into();
        session.history = vec![
            Message::Images {
                text: "image caption".into(),
                urls: vec!["hidden attachment".into()],
            },
            Message::Reasoning("hidden reasoning".into()),
            Message::Result {
                details: None,
                id: "1".into(),
                text: "hidden tool output".into(),
                images: vec![],
                is_error: false,
                nested_reads: vec![],
            },
            Message::Assistant {
                text: "visible reply".into(),
                calls: vec![],
            },
        ];
        disk.save(&mut session).unwrap();
        let empty = Disk::open(dir.path(), dir.path(), None).unwrap();
        let _other = Disk::open(dir.path(), &dir.path().join("elsewhere"), None).unwrap();
        let items = disk.previews(dir.path()).unwrap();
        assert_eq!(
            items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [empty.id(), disk.id()]
        );
        assert!(items[0].messages.is_empty());
        assert_eq!(items[1].name, "same name");
        assert_eq!(
            items[1].messages,
            [
                ("You", "image caption".into()),
                ("Panda", "visible reply".into())
            ]
        );
        assert_eq!(disk.load().unwrap().revision, session.revision);
    }
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
