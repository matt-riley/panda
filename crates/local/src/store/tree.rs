use super::{Disk, save_snapshot};
use anyhow::{Result, ensure};
use panda_core::{Cancel, Message, Session, Store};
use rusqlite::{Connection, params};
use std::collections::HashMap;

pub struct TreeEntry {
    pub id: i64,
    pub branch_depth: usize,
    pub starts_branch: bool,
    pub is_prompt: bool,
    pub text: String,
    pub current: bool,
}

// History is append-only between navigations. Archive its new suffix on demand,
// keeping the normal turn write path and legacy session snapshots unchanged.
const ANCESTORS: &str = "WITH RECURSIVE path(id,parent,depth) AS (
    SELECT id,parent,depth FROM tree_nodes WHERE session=?1 AND id=?2
    UNION ALL SELECT n.id,n.parent,n.depth FROM tree_nodes n JOIN path p ON n.id=p.parent
    WHERE n.session=?1) ";

fn sync(conn: &Connection, id: i64, session: &Session) -> Result<i64> {
    ensure!(
        !session.active,
        "inspect the interrupted turn and use :ack before navigating"
    );
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tree_nodes (
        id INTEGER PRIMARY KEY, session INTEGER NOT NULL, parent INTEGER NOT NULL,
        depth INTEGER NOT NULL, message TEXT NOT NULL, usage TEXT, checkpoint TEXT);
        CREATE INDEX IF NOT EXISTS tree_session ON tree_nodes(session);
        CREATE TABLE IF NOT EXISTS tree_heads(session INTEGER PRIMARY KEY, tip INTEGER NOT NULL);",
    )?;
    let (mut tip, depth): (i64,usize) = conn.query_row(
        "SELECT coalesce(h.tip,0), coalesce(n.depth,0) FROM sessions s
         LEFT JOIN tree_heads h ON h.session=s.id LEFT JOIN tree_nodes n ON n.id=h.tip WHERE s.id=?",
        [id], |r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        depth <= session.history.len(),
        "tree head exceeds active history"
    );
    for (i, message) in session.history.iter().enumerate().skip(depth) {
        conn.execute(
            "INSERT INTO tree_nodes(session,parent,depth,message) VALUES(?,?,?,?)",
            params![id, tip, i + 1, serde_json::to_string(message)?],
        )?;
        tip = conn.last_insert_rowid();
    }
    set_head(conn, id, tip)?;
    // Usage and compaction boundaries belong to a specific path. In particular,
    // a later summary from a sibling must never enter the next model request.
    let mut stmt = conn.prepare(&(ANCESTORS.to_owned() + "SELECT id FROM path ORDER BY depth"))?;
    let path = stmt
        .query_map([id, tip], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for usage in &session.usage {
        if let Some(node) = usage.through.checked_sub(1).and_then(|i| path.get(i)) {
            conn.execute(
                "UPDATE tree_nodes SET usage=? WHERE id=?",
                params![serde_json::to_string(usage)?, node],
            )?;
        }
    }
    if let Some(cp) = &session.checkpoint
        && let Some(node) = cp.through.checked_sub(1).and_then(|i| path.get(i))
    {
        conn.execute(
            "UPDATE tree_nodes SET checkpoint=? WHERE id=?",
            params![serde_json::to_string(cp)?, node],
        )?;
    }
    Ok(tip)
}

fn set_head(conn: &Connection, id: i64, tip: i64) -> Result<()> {
    conn.execute("INSERT INTO tree_heads(session,tip) VALUES(?,?) ON CONFLICT(session) DO UPDATE SET tip=excluded.tip",[id,tip])?;
    Ok(())
}

fn destination(conn: &Connection, session: i64, node: i64) -> Result<(i64, Option<Message>)> {
    let (parent, raw): (i64, String) = conn.query_row(
        "SELECT parent,message FROM tree_nodes WHERE session=? AND id=?",
        [session, node],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let message: Message = serde_json::from_str(&raw)?;
    let (tip, draft) = match message {
        Message::BranchSummary(_) => (node, None),
        Message::User(_) | Message::Images { .. } => (parent, Some(message)),
        Message::Assistant { calls, .. } if calls.is_empty() => (node, None),
        _ => anyhow::bail!("select a prompt or completed answer"),
    };
    Ok((tip, draft))
}
impl Disk {
    pub fn tree(&mut self) -> Result<Vec<TreeEntry>> {
        let s = self.load()?;
        let tx = self.conn.transaction()?;
        let tip = sync(&tx, self.id, &s)?;
        let mut children: HashMap<i64, Vec<(i64, bool, Option<String>)>> = HashMap::new();
        {
            let mut stmt=tx.prepare("SELECT id,parent, json_type(message,'$.User') IS NOT NULL OR json_type(message,'$.Images') IS NOT NULL,
                substr(coalesce('Branch summary · ' || json_extract(message,'$.BranchSummary'),json_extract(message,'$.User'), json_extract(message,'$.Images.text'),
                CASE WHEN json_array_length(message,'$.Assistant.calls')=0 THEN json_extract(message,'$.Assistant.text') END),1,512)
                FROM tree_nodes WHERE session=? ORDER BY id DESC")?;
            let mut rows = stmt.query([self.id])?;
            while let Some(row) = rows.next()? {
                children.entry(row.get(1)?).or_default().push((
                    row.get(0)?,
                    row.get(2)?,
                    row.get(3)?,
                ));
            }
        }
        let mut entries = Vec::new();
        let mut stack = vec![(0, false, None, 0, false)];
        while let Some((id, is_prompt, text, branch_depth, starts_branch)) = stack.pop() {
            if let Some(text) = text {
                entries.push(TreeEntry {
                    id,
                    is_prompt,
                    text,
                    branch_depth,
                    starts_branch,
                    current: id == tip,
                });
            }
            if let Some(kids) = children.remove(&id) {
                let split = kids.len() > 1;
                let indent = branch_depth + usize::from(split);
                stack.extend(
                    kids.into_iter()
                        .map(|(id, prompt, text)| (id, prompt, text, indent, split)),
                );
            }
        }
        tx.commit()?;
        Ok(entries)
    }

    /// A prompt becomes an editable draft at its parent. A completed answer
    /// becomes the active tip. Both changes share the snapshot transaction.
    pub fn navigate(&mut self, node: i64) -> Result<Option<Message>> {
        self.switch(node, None)
    }

    pub fn navigate_summarized(
        &mut self,
        node: i64,
        cancel: &Cancel,
        generate: impl FnOnce(&[Message]) -> Result<String>,
    ) -> Result<Option<Message>> {
        let session = self.load()?;
        let common = {
            let tx = self.conn.transaction()?;
            let source = sync(&tx, self.id, &session)?;
            let (target, _) = destination(&tx, self.id, node)?;
            let ancestors = |tip| -> Result<Vec<i64>> {
                let mut stmt =
                    tx.prepare(&(ANCESTORS.to_owned() + "SELECT id FROM path ORDER BY depth"))?;
                Ok(stmt
                    .query_map([self.id, tip], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            };
            let shared = ancestors(source)?
                .iter()
                .zip(ancestors(target)?)
                .take_while(|(a, b)| **a == *b)
                .count();
            tx.commit()?;
            shared
        };
        cancel.check()?;
        if common == session.history.len() {
            return self.navigate(node);
        }
        let text = generate(&session.history[common..])?;
        ensure!(!text.trim().is_empty(), "empty branch summary");
        cancel.check()?;
        self.switch(node, Some((text, cancel)))
    }

    fn switch(&mut self, node: i64, summary: Option<(String, &Cancel)>) -> Result<Option<Message>> {
        let mut session = self.load()?;
        let tx = self.conn.transaction()?;
        let previous = sync(&tx, self.id, &session)?;
        let (tip, draft) = destination(&tx, self.id, node)?;
        if tip != previous {
            session.history.clear();
            session.usage.clear();
            session.checkpoint = None;
            {
                let mut stmt=tx.prepare(&(ANCESTORS.to_owned()+"SELECT n.message,n.usage,n.checkpoint FROM path p JOIN tree_nodes n ON n.id=p.id ORDER BY p.depth"))?;
                let mut rows = stmt.query([self.id, tip])?;
                while let Some(r) = rows.next()? {
                    session
                        .history
                        .push(serde_json::from_str(&r.get::<_, String>(0)?)?);
                    if let Some(u) = r.get::<_, Option<String>>(1)? {
                        session.usage.push(serde_json::from_str(&u)?);
                    }
                    if let Some(c) = r.get::<_, Option<String>>(2)? {
                        session.checkpoint = Some(serde_json::from_str(&c)?);
                    }
                }
            }
            set_head(&tx, self.id, tip)?;
            if let Some((text, cancel)) = &summary {
                session.history.push(Message::BranchSummary(text.clone()));
                sync(&tx, self.id, &session)?;
                cancel.check()?;
            }
            save_snapshot(&tx, self.id, &mut session)?;
        }
        if let Some((_, cancel)) = summary {
            cancel.check()?;
        }
        tx.commit()?;
        Ok(draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panda_core::{Checkpoint, UsageRecord};

    fn answer(text: &str) -> Message {
        Message::Assistant {
            text: text.into(),
            calls: vec![],
        }
    }

    #[test]
    fn pi_user_selection_edits_at_parent_and_answer_selection_sets_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Disk::open(dir.path(), dir.path(), None).unwrap();
        let mut s = db.load().unwrap();
        let image = Message::Images {
            text: "Second".into(),
            urls: vec!["data:fixture".into()],
        };
        s.history = vec![
            Message::User("First".into()),
            answer("one"),
            image.clone(),
            answer("two"),
        ];
        s.checkpoint = Some(Checkpoint {
            through: 4,
            summary: "original summary".into(),
        });
        s.usage.push(UsageRecord {
            through: 4,
            output: 7,
            ..Default::default()
        });
        db.save(&mut s).unwrap();
        let entries = db.tree().unwrap();
        assert_eq!(
            db.navigate(entries[0].id).unwrap(),
            Some(Message::User("First".into()))
        );
        assert!(db.load().unwrap().history.is_empty());
        assert_eq!(db.navigate(entries[2].id).unwrap(), Some(image));
        let mut alt = db.load().unwrap();
        assert_eq!(alt.history, &s.history[..2]);
        assert!(alt.checkpoint.is_none() && alt.usage.is_empty());
        alt.history
            .extend([Message::User("Alternative".into()), answer("three")]);
        db.save(&mut alt).unwrap();
        assert_eq!(db.navigate(entries[3].id).unwrap(), None);
        let restored = db.load().unwrap();
        assert_eq!(restored.history, s.history);
        assert_eq!(restored.usage[0].output, 7);
        assert_eq!(restored.checkpoint.unwrap().summary, "original summary");
        assert_eq!(db.navigate(entries[3].id).unwrap(), None);
        assert_eq!(db.load().unwrap().revision, restored.revision);
        let entries = db.tree().unwrap();
        assert_eq!(entries.len(), 6);
        assert_eq!(entries.iter().filter(|e| e.current).count(), 1);
        assert!(entries[4].branch_depth > entries[1].branch_depth);
    }

    #[test]
    fn pi_summary_for_nested_prompt_is_attached_to_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Disk::open(dir.path(), dir.path(), None).unwrap();
        let mut s = db.load().unwrap();
        s.history = vec![
            Message::User("shared".into()),
            answer("shared answer"),
            Message::User("edit me".into()),
            answer("finding"),
        ];
        db.save(&mut s).unwrap();
        let entries = db.tree().unwrap();
        let cancel = Cancel::default();
        assert!(
            db.navigate_summarized(entries[2].id, &cancel, |_| anyhow::bail!("offline"))
                .is_err()
        );
        assert_eq!(db.load().unwrap().revision, s.revision);
        let draft = db
            .navigate_summarized(entries[2].id, &cancel, |source| {
                assert_eq!(source, &s.history[2..]);
                Ok("carry this".into())
            })
            .unwrap();
        assert_eq!(draft, Some(s.history[2].clone()));
        let expected = [
            s.history[0].clone(),
            s.history[1].clone(),
            Message::BranchSummary("carry this".into()),
        ];
        assert_eq!(db.load().unwrap().projected(), expected);
        let leaf = db.tree().unwrap().into_iter().find(|e| e.current).unwrap();
        db.navigate_summarized(leaf.id, &cancel, |_| {
            panic!("same branch must skip provider")
        })
        .unwrap();
        db.navigate(entries[3].id).unwrap();
        assert_eq!(db.load().unwrap().history, s.history);
        db.navigate(leaf.id).unwrap();
        assert_eq!(db.load().unwrap().history, expected);
    }

    #[test]
    fn interrupted_and_foreign_nodes_cannot_move_the_active_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Disk::open(dir.path(), dir.path(), None).unwrap();
        let mut s = db.load().unwrap();
        s.history.push(Message::User("First".into()));
        db.save(&mut s).unwrap();
        let node = db.tree().unwrap()[0].id;
        let mut other = Disk::open(dir.path(), dir.path(), None).unwrap();
        assert!(other.navigate(node).is_err());
        s.active = true;
        db.save(&mut s).unwrap();
        assert!(db.navigate(node).is_err());
        assert_eq!(db.load().unwrap().history, s.history);
    }
}
