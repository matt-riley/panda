use super::{Disk, save_snapshot};
use anyhow::{Context, Result, ensure};
use panda_core::{Cancel, Message, Session, Store};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Prompt,
    Answer,
    Summary,
    Checkpoint,
    Metadata,
    ToolCall,
    ToolResult,
    Reasoning,
    Other,
}
impl EntryKind {
    pub fn navigable(self) -> bool {
        matches!(
            self,
            Self::Prompt | Self::Answer | Self::Summary | Self::Checkpoint | Self::Metadata
        )
    }
}
pub struct TreeEntry {
    pub kind: EntryKind,
    pub id: i64,
    pub branch_depth: usize,
    pub starts_branch: bool,
    pub is_prompt: bool,
    pub text: String,
    pub label: Option<String>,
    pub current: bool,
}

// History is append-only between navigations. Archive its new suffix on demand,
// keeping the normal turn write path and legacy session snapshots unchanged.
pub(super) const ANCESTORS: &str = "WITH RECURSIVE path(id,parent,depth) AS (
    SELECT id,parent,depth FROM tree_nodes WHERE session=?1 AND id=?2
    UNION ALL SELECT n.id,n.parent,n.depth FROM tree_nodes n JOIN path p ON n.id=p.parent
    WHERE n.session=?1) ";

pub(super) fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tree_nodes (
        id INTEGER PRIMARY KEY, session INTEGER NOT NULL, parent INTEGER NOT NULL,
        depth INTEGER NOT NULL, message TEXT NOT NULL, usage TEXT, checkpoint TEXT);
        CREATE TABLE IF NOT EXISTS tree_models(node INTEGER PRIMARY KEY,responder TEXT,preference TEXT,attempts TEXT);
        CREATE INDEX IF NOT EXISTS tree_session ON tree_nodes(session);
        CREATE TABLE IF NOT EXISTS tree_heads(session INTEGER PRIMARY KEY, tip INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS pi_context_edits(node INTEGER PRIMARY KEY,start INTEGER NOT NULL,end INTEGER NOT NULL,replacement TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(super) fn sync(conn: &Connection, id: i64, session: &Session) -> Result<i64> {
    ensure!(
        !session.active,
        "inspect the interrupted turn and use :ack before navigating"
    );
    archive(conn, id, session)
}

pub(super) fn archive(conn: &Connection, id: i64, session: &Session) -> Result<i64> {
    schema(conn)?;
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
    // Usage and compaction boundaries belong to a specific path. In particular,
    // a later summary from a sibling must never enter the next model request.
    let mut stmt = conn.prepare(&(ANCESTORS.to_owned() + "SELECT n.id FROM path JOIN tree_nodes n USING(id) WHERE n.message != 'null' ORDER BY n.id"))?;
    let path = stmt
        .query_map([id, tip], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (&index, model) in &session.responders {
        if let Some(node) = path.get(index) {
            conn.execute("INSERT INTO tree_models(node,responder) VALUES(?,?) ON CONFLICT(node) DO UPDATE SET responder=excluded.responder",params![node,serde_json::to_string(model)?])?;
        }
    }
    for (&through, selection) in &session.preferences {
        if let Some(node) = through.checked_sub(1).and_then(|idx| path.get(idx)) {
            conn.execute(
                "INSERT INTO tree_models(node,preference) VALUES(?,?) ON CONFLICT(node) DO NOTHING",
                params![node, serde_json::to_string(selection)?],
            )?;
            conn.execute(
                "UPDATE tree_models SET preference=coalesce(preference,?) WHERE node=?",
                params![serde_json::to_string(selection)?, node],
            )?;
        }
    }
    for usage in &session.usage {
        if let Some(node) = usage.through.checked_sub(1).and_then(|i| path.get(i)) {
            conn.execute(
                "UPDATE tree_nodes SET usage=? WHERE id=?",
                params![serde_json::to_string(usage)?, node],
            )?;
        }
    }
    if let Some(cp) = &session.checkpoint {
        let raw = serde_json::to_string(cp)?;
        let previous: Option<String> = conn.query_row(&(ANCESTORS.to_owned() +
            "SELECT n.checkpoint FROM path JOIN tree_nodes n USING(id) WHERE n.checkpoint IS NOT NULL ORDER BY n.id DESC LIMIT 1"),
            [id,tip], |r| r.get(0)).optional()?;
        if previous.as_ref() != Some(&raw) {
            // A checkpoint is created on this branch, even when its retained
            // suffix starts at a node shared with a sibling.
            if session.history.len() == depth {
                // The selected node already exists. A new checkpoint becomes
                // its child so existing descendants and raw imports stay intact.
                conn.execute("INSERT INTO tree_nodes(session,parent,depth,message,checkpoint) VALUES (?,?,?,'null',?)",
                    params![id,tip,depth,raw])?;
                tip = conn.last_insert_rowid();
            } else {
                conn.execute(
                    "UPDATE tree_nodes SET checkpoint=? WHERE id=?",
                    params![raw, tip],
                )?;
            }
        }
    }
    if tip != 0 {
        let pref = serde_json::to_string(&(&session.model, &session.reasoning))?;
        let previous: Option<String> = conn.query_row(&(ANCESTORS.to_owned()+"SELECT m.preference FROM path JOIN tree_models m ON m.node=path.id WHERE m.preference IS NOT NULL ORDER BY m.node DESC LIMIT 1"),[id,tip],|r|r.get(0)).optional()?;
        if previous.as_deref() != Some(&pref)
            && (previous.is_some() || session.model.is_some() || session.reasoning.is_some())
        {
            if session.history.len() == depth {
                conn.execute(
                    "INSERT INTO tree_nodes(session,parent,depth,message) VALUES(?,?,?,'null')",
                    params![id, tip, depth],
                )?;
                tip = conn.last_insert_rowid();
            }
            conn.execute("INSERT INTO tree_models(node,preference) VALUES(?,?) ON CONFLICT(node) DO UPDATE SET preference=excluded.preference",params![tip,pref])?;
        }
        // Only append attempts that are absent from this path. A new checkpoint
        // or a late branch summary can carry a charge at an existing history depth.
        let archived:usize = conn.query_row(&(ANCESTORS.to_owned()+"SELECT coalesce(sum(json_array_length(m.attempts)),0) FROM path JOIN tree_models m ON m.node=path.id"),[id,tip],|r|r.get(0))?;
        for attempt in session.attempts.iter().skip(archived) {
            let node = if attempt.through == session.history.len() {
                tip
            } else {
                attempt
                    .through
                    .checked_sub(1)
                    .and_then(|idx| path.get(idx))
                    .copied()
                    .unwrap_or(tip)
            };
            conn.execute("INSERT INTO tree_models(node,attempts) VALUES(?1,json_array(json(?2))) ON CONFLICT(node) DO UPDATE SET attempts=json_insert(coalesce(attempts,'[]'),'$[#]',json(?2))",params![node,serde_json::to_string(attempt)?])?;
        }
    }
    set_head(conn, id, tip)?;
    Ok(tip)
}

pub(super) fn set_head(conn: &Connection, id: i64, tip: i64) -> Result<()> {
    conn.execute("INSERT INTO tree_heads(session,tip) VALUES(?,?) ON CONFLICT(session) DO UPDATE SET tip=excluded.tip",[id,tip])?;
    Ok(())
}

pub(super) fn restore_path(
    conn: &Connection,
    id: i64,
    tip: i64,
    session: &mut Session,
) -> Result<()> {
    session.model = None;
    session.reasoning = None;
    session.preferences.clear();
    session.responders.clear();
    session.attempts.clear();
    session.history.clear();
    session.context_edits.clear();
    session.usage.clear();
    session.checkpoint = None;
    {
        let mut stmt=conn.prepare(&(ANCESTORS.to_owned()+"SELECT n.message,n.usage,n.checkpoint,e.start,e.end,e.replacement,m.responder,m.preference,m.attempts FROM path p JOIN tree_nodes n ON n.id=p.id LEFT JOIN tree_models m ON m.node=p.id LEFT JOIN pi_context_edits e ON e.node=n.id ORDER BY n.id"))?;
        let mut rows = stmt.query([id, tip])?;
        while let Some(r) = rows.next()? {
            if let Some(message) = serde_json::from_str(&r.get::<_, String>(0)?)? {
                session.history.push(message);
            }
            if let Some(raw) = r.get::<_, Option<String>>(6)?
                && let Some(i) = session.history.len().checked_sub(1)
            {
                session.responders.insert(i, serde_json::from_str(&raw)?);
            }
            if let Some(raw) = r.get::<_, Option<String>>(7)? {
                (session.model, session.reasoning) = serde_json::from_str(&raw)?;
                session.preferences.insert(
                    session.history.len(),
                    (session.model.clone(), session.reasoning.clone()),
                );
            }
            if let Some(raw) = r.get::<_, Option<String>>(8)? {
                session
                    .attempts
                    .extend(serde_json::from_str::<Vec<panda_core::Attempt>>(&raw)?);
            }
            if let Some(start) = r.get::<_, Option<usize>>(3)? {
                let end: usize = r.get(4)?;
                for slot in start..end {
                    session.context_edits.insert(slot, vec![]);
                }
                session
                    .context_edits
                    .insert(start, serde_json::from_str(&r.get::<_, String>(5)?)?);
            }
            if let Some(u) = r.get::<_, Option<String>>(1)? {
                session.usage.push(serde_json::from_str(&u)?);
            }
            if let Some(c) = r.get::<_, Option<String>>(2)? {
                session.checkpoint = Some(serde_json::from_str(&c)?);
            }
        }
    }
    // An imported partial tool round has unknown outcomes. Let Panda's usual
    // acknowledgement path close it without ever dispatching historical calls.
    let mut completed = std::collections::HashSet::new();
    for msg in session.history.iter().rev() {
        match msg {
            Message::Result { id, .. } => {
                completed.insert(id);
            }
            Message::Reasoning(_) | Message::CodemodeStore(_) | Message::Replay(_) => {}
            Message::Assistant { calls, .. } => {
                session.active = calls.iter().any(|c| !completed.contains(&c.id));
                break;
            }
            _ => break,
        }
    }
    Ok(())
}

fn destination(conn: &Connection, session: i64, node: i64) -> Result<(i64, Option<Message>)> {
    let (parent, raw): (i64, String) = conn.query_row(
        "SELECT parent,message FROM tree_nodes WHERE session=? AND id=?",
        [session, node],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let Some(message) = serde_json::from_str::<Option<Message>>(&raw)? else {
        return Ok((node, None));
    };
    let (tip, draft) = match message {
        Message::BranchSummary(_) => (node, None),
        Message::User(_) | Message::Images { .. } => (parent, Some(message)),
        Message::Assistant { calls, .. } if calls.is_empty() => (node, None),
        _ => anyhow::bail!("select a prompt or completed answer"),
    };
    Ok((tip, draft))
}
impl Disk {
    /// Branch targets for callers that navigate the returned entries.
    pub fn tree(&mut self) -> Result<Vec<TreeEntry>> {
        Ok(self
            .tree_entries()?
            .into_iter()
            .filter(|entry| entry.kind.navigable())
            .collect())
    }

    pub fn tree_entries(&mut self) -> Result<Vec<TreeEntry>> {
        let s = self.load()?;
        let tx = self.conn.transaction()?;
        let tip = sync(&tx, self.id, &s)?;
        let mut stmt = tx.prepare(
            "SELECT l.node,l.label FROM tree_labels l JOIN tree_nodes n ON n.id=l.node
            WHERE n.session=? AND l.label IS NOT NULL",
        )?;
        let mut labels = stmt
            .query_map([self.id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        drop(stmt);
        let mut children = HashMap::<i64, Vec<(i64, EntryKind, String)>>::new();
        {
            // Extract bounded previews in SQLite so browsing never loads attachment data.
            let mut stmt = tx.prepare("SELECT id,parent,
                (SELECT key FROM json_each(message) LIMIT 1),
                coalesce(json_array_length(message,'$.Assistant.calls'),0),
                checkpoint IS NOT NULL,
                substr(coalesce(json_extract(message,'$.User'),json_extract(message,'$.Images.text'),
                    nullif(json_extract(message,'$.Assistant.text'),''),
                    json_extract(message,'$.Assistant.calls[0].name'),json_extract(message,'$.Result.text'),
                    json_extract(message,'$.BranchSummary'),json_extract(message,'$.Truncated.text'),''),1,512)
                FROM tree_nodes WHERE session=? ORDER BY id DESC")?;
            let mut rows = stmt.query([self.id])?;
            while let Some(row) = rows.next()? {
                let kind = match row.get::<_, Option<String>>(2)?.as_deref() {
                    Some("User" | "Images") => EntryKind::Prompt,
                    Some("Assistant") if row.get::<_, usize>(3)? == 0 => EntryKind::Answer,
                    Some("Assistant") => EntryKind::ToolCall,
                    Some("Result") => EntryKind::ToolResult,
                    Some("BranchSummary") => EntryKind::Summary,
                    Some("Reasoning") => EntryKind::Reasoning,
                    None if row.get::<_, bool>(4)? => EntryKind::Checkpoint,
                    None => EntryKind::Metadata,
                    _ => EntryKind::Other,
                };
                let text: String = row.get(5)?;
                let text = match kind {
                    EntryKind::Metadata => "Session metadata".into(),
                    EntryKind::Checkpoint => "Compaction checkpoint".into(),
                    EntryKind::Summary => format!("Branch summary · {text}"),
                    EntryKind::Reasoning => "Reasoning".into(),
                    EntryKind::Other if text.is_empty() => "Session event".into(),
                    _ => text,
                };
                children
                    .entry(row.get(1)?)
                    .or_default()
                    .push((row.get(0)?, kind, text));
            }
        }
        let mut entries = Vec::new();
        let mut stack = vec![(0, EntryKind::Metadata, String::new(), 0, false)];
        while let Some((id, kind, text, branch_depth, starts_branch)) = stack.pop() {
            if id != 0 {
                entries.push(TreeEntry {
                    id,
                    kind,
                    is_prompt: kind == EntryKind::Prompt,
                    text,
                    label: labels.remove(&id),
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

    pub fn set_tree_label(&mut self, node: i64, text: &str) -> Result<()> {
        let mut session = self.load()?;
        let tx = self.conn.transaction()?;
        schema(&tx)?;
        let label = text.trim();
        let updated = tx.execute(
            "INSERT INTO tree_labels(node,label,stamp)
             SELECT id,NULLIF(?3,''),strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM tree_nodes
             WHERE session=?1 AND id=?2
             ON CONFLICT(node) DO UPDATE SET label=excluded.label,stamp=excluded.stamp",
            params![self.id, node, label],
        )?;
        ensure!(updated == 1, "unknown tree entry");
        save_snapshot(&tx, self.id, &mut session)?;
        tx.commit()?;
        Ok(())
    }

    /// Seed a separate session from the selected prompt's ancestry without moving this tree's head.
    pub fn fork_prompt(&self, dir: &Path, repo: &Path, node: i64) -> Result<(Self, Message)> {
        let mut seed = self.load()?;
        ensure!(!seed.active, "acknowledge interrupted turn before forking");
        let (parent, draft) = destination(&self.conn, self.id, node)?;
        let draft = draft.context("select a user prompt to fork")?;
        restore_path(&self.conn, self.id, parent, &mut seed)?;
        Ok((self.create_fork(dir, repo, seed, "")?, draft))
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
                    tx.prepare(&(ANCESTORS.to_owned() + "SELECT n.id FROM path JOIN tree_nodes n USING(id) WHERE n.message != 'null' ORDER BY n.id"))?;
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
        let text = generate(&session.resolved(common..session.history.len()))?;
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
            restore_path(&tx, self.id, tip, &mut session)?;
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
    fn branch_models_thinking_and_attempts_restore_on_each_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Disk::open(dir.path(), dir.path(), None).unwrap();
        let append = |db: &mut Disk, model: &str, effort: &str| {
            let id = panda_core::ModelId {
                provider: "fixture".into(),
                model: model.into(),
            };
            let mut s = db.load().unwrap();
            s.model = Some(id.clone());
            s.reasoning = Some(effort.into());
            s.history.push(Message::User(format!("ask {model}")));
            s.preferences
                .insert(s.history.len(), (s.model.clone(), s.reasoning.clone()));
            s.responders.insert(s.history.len(), id.clone());
            s.history.push(answer(model));
            s.attempts.push(panda_core::Attempt {
                through: s.history.len(),
                model: Some(id),
                purpose: "turn".into(),
                outcome: "ok".into(),
                usage: None,
                cost: None,
                cost_source: "unknown".into(),
                timestamp: 1,
            });
            db.save(&mut s).unwrap();
            db.tree()
                .unwrap()
                .into_iter()
                .find(|e| e.current)
                .unwrap()
                .id
        };
        let common = append(&mut db, "root", "low");
        let left = append(&mut db, "left", "medium");
        db.navigate(common).unwrap();
        let right = append(&mut db, "right", "high");
        let id = db.id();
        drop(db);
        let mut db = Disk::open(dir.path(), dir.path(), Some(id)).unwrap();
        for (node, name, effort) in [
            (left, "left", "medium"),
            (right, "right", "high"),
            (left, "left", "medium"),
        ] {
            db.navigate(node).unwrap();
            let s = db.load().unwrap();
            assert_eq!(s.model.unwrap().model, name);
            assert_eq!(s.reasoning.as_deref(), Some(effort));
            assert_eq!(
                s.responders
                    .values()
                    .map(|id| id.model.as_str())
                    .collect::<Vec<_>>(),
                ["root", name]
            );
            assert_eq!(
                s.attempts
                    .iter()
                    .map(|a| a.model.as_ref().unwrap().model.as_str())
                    .collect::<Vec<_>>(),
                ["root", name]
            );
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
            ..Default::default()
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
    fn fork_from_a_sibling_prompt_copies_its_ancestors_and_full_image_draft() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut db = Disk::open(root, root, None).unwrap();
        let mut original = db.load().unwrap();
        let prompt = Message::Images {
            text: "image caption ".repeat(100),
            urls: vec!["data:image/png;base64,AA==".into()],
        };
        original.history = vec![
            Message::User("first".into()),
            answer("one"),
            prompt.clone(),
            answer("two"),
        ];
        db.save(&mut original).unwrap();
        let entries = db.tree().unwrap();
        db.navigate(entries[0].id).unwrap();
        let mut sibling = db.load().unwrap();
        sibling.history = vec![Message::User("other".into()), answer("other answer")];
        db.save(&mut sibling).unwrap();
        let current = db
            .tree()
            .unwrap()
            .into_iter()
            .find(|e| e.current)
            .unwrap()
            .id;

        assert!(db.fork_prompt(root, root, current).is_err());
        assert_eq!(db.list(root).unwrap(), [db.id()]);
        let (child, draft) = db.fork_prompt(root, root, entries[2].id).unwrap();
        assert_eq!(draft, prompt);
        let seed = child.load().unwrap();
        assert_eq!(seed.history, &original.history[..2]);
        assert_eq!(seed.parent, Some(db.id()));
        assert_eq!(db.load().unwrap().revision, sibling.revision);
        assert!(
            db.tree()
                .unwrap()
                .iter()
                .any(|e| e.id == current && e.current)
        );
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
