use super::{Disk, save_snapshot};
use anyhow::{Context, Result};
use panda_core::Session;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

fn capture(tx: &Transaction, id: i64, s: &mut Session) -> Result<()> {
    let mut bytes = Vec::new();
    for message in &s.history {
        serde_json::to_writer(&mut bytes, message)?;
        bytes.push(b'\n');
    }
    save_bytes(tx, id, s, &bytes)
}

fn save_bytes(tx: &Transaction, id: i64, s: &mut Session, bytes: &[u8]) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS recovery_transcripts(
        session INTEGER NOT NULL, revision INTEGER NOT NULL, pointer TEXT UNIQUE NOT NULL,
        jsonl BLOB NOT NULL, PRIMARY KEY(session,revision))",
    )?;
    let cp = s.checkpoint.as_mut().context("missing checkpoint")?;
    let revision = s.revision + 1;
    let pointer = format!("panda-{id}-{revision}");
    tx.execute(
        "INSERT INTO recovery_transcripts VALUES(?,?,?,?)",
        params![id, revision, pointer, bytes],
    )?;
    tx.execute("DELETE FROM recovery_transcripts WHERE session=?1 AND revision NOT IN (
        SELECT revision FROM recovery_transcripts WHERE session=?1 ORDER BY revision DESC LIMIT 50)",[id])?;
    cp.recovery = Some(pointer);
    Ok(())
}
impl Disk {
    pub(super) fn commit_with_transcript(&mut self, session: &mut Session) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut candidate = session.clone();
        capture(&tx, self.id, &mut candidate)?;
        save_snapshot(&tx, self.id, &mut candidate)?;
        tx.commit()?;
        *session = candidate;
        Ok(())
    }
    pub fn recover_transcript(&self, pointer: &str) -> Result<String> {
        self.transcript_if_present(pointer)?
            .context("unknown or expired transcript for this session")
    }

    pub(super) fn transcript_if_present(&self, pointer: &str) -> Result<Option<String>> {
        if !self.conn.table_exists(None, "recovery_transcripts")? {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT jsonl FROM recovery_transcripts WHERE session=? AND pointer=?",
                params![self.id, pointer],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes.map(String::from_utf8).transpose()?)
    }
}
pub(super) fn seed_fork(tx: &Transaction, id: i64, seed: &Session, bytes: &[u8]) -> Result<()> {
    let mut s = seed.clone();
    save_bytes(tx, id, &mut s, bytes)?;
    save_snapshot(tx, id, &mut s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use panda_core::{Checkpoint, Message, Store};

    #[test]
    fn checkpointing_after_tree_navigation_excludes_sibling_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = Disk::open(dir.path(), dir.path(), None).unwrap();
        let answer = |text: &str| Message::Assistant {
            text: text.into(),
            calls: vec![],
        };
        let mut s = disk.load().unwrap();
        s.history = vec![
            Message::User("root".into()),
            answer("shared answer"),
            Message::User("sibling-only note".into()),
            answer("sibling-only reply"),
        ];
        disk.save(&mut s).unwrap();
        let root_answer = disk
            .tree()
            .unwrap()
            .iter()
            .find(|e| e.text.contains("shared answer"))
            .unwrap()
            .id;
        disk.navigate(root_answer).unwrap();
        let mut active = disk.load().unwrap();
        active.history.extend([
            Message::User("new branch error 731".into()),
            answer("acknowledged"),
        ]);
        active.checkpoint = Some(Checkpoint {
            through: 4,
            summary: "reviewed the error".into(),
            ..Default::default()
        });
        disk.save_checkpoint(&mut active).unwrap();
        drop(disk);
        let disk = Disk::open(dir.path(), dir.path(), Some(1)).unwrap();
        let cp = disk.load().unwrap().checkpoint.unwrap();
        let jsonl = disk
            .recover_transcript(cp.recovery.as_deref().unwrap())
            .unwrap();
        let actual = jsonl
            .lines()
            .map(|row| serde_json::from_str::<Message>(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, active.history);
        assert!(jsonl.contains("new branch error 731") && !jsonl.contains("sibling-only"));
    }

    #[test]
    fn recovery_survives_reopen_and_rejects_stale_writes_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let mut disk = Disk::open(repo, repo, None).unwrap();
        let mut s = disk.load().unwrap();
        s.history = vec![Message::User(
            "exact mid-turn error: worker returned 23".into(),
        )];
        s.checkpoint = Some(Checkpoint {
            through: 1,
            summary: "failure".into(),
            ..Default::default()
        });
        let mut stale = s.clone();
        disk.save_checkpoint(&mut s).unwrap();
        let pointer = s.checkpoint.as_ref().unwrap().recovery.clone().unwrap();
        stale.revision += 8;
        assert!(disk.save_checkpoint(&mut stale).is_err());
        assert!(stale.checkpoint.unwrap().recovery.is_none());
        let count: i64 = disk
            .conn
            .query_row("SELECT COUNT(*) FROM recovery_transcripts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        s.history.push(Message::User("later message".into()));
        disk.save(&mut s).unwrap();
        let sibling = disk.fork(repo, repo, None, "copy").unwrap();
        let copied = sibling
            .load()
            .unwrap()
            .checkpoint
            .unwrap()
            .recovery
            .unwrap();
        assert_ne!(copied, pointer);
        assert!(sibling.recover_transcript(&pointer).is_err());
        assert_eq!(
            sibling.recover_transcript(&copied).unwrap(),
            disk.recover_transcript(&pointer).unwrap()
        );
        drop(disk);
        let mut disk = Disk::open(repo, repo, Some(1)).unwrap();
        let exact = disk.recover_transcript(&pointer).unwrap();
        assert_eq!(
            exact,
            format!("{}\n", serde_json::to_string(&s.history[0]).unwrap())
        );
        for _ in 0..50 {
            disk.save_checkpoint(&mut s).unwrap();
        }
        assert!(disk.recover_transcript(&pointer).is_err());
        assert!(sibling.recover_transcript(&copied).is_ok());
        let count: i64 = disk
            .conn
            .query_row(
                "SELECT COUNT(*) FROM recovery_transcripts WHERE session=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 50);
    }
}
