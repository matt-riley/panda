use anyhow::{Context, Result, ensure};
use panda_core::{
    Checkpoint, Message, ModelId, Session, Store,
    sdk::{Entry, SessionHost},
};
use std::collections::BTreeMap;

#[derive(Default)]
struct Record {
    session: Session,
    entries: Vec<Entry>,
    path: Vec<i64>,
    archived: BTreeMap<Vec<i64>, Session>,
    checkpoints: BTreeMap<i64, Checkpoint>,
    preferences: BTreeMap<i64, (Option<ModelId>, Option<String>)>,
}
impl Record {
    fn append(&mut self, message: Option<Message>) -> i64 {
        let id = self.entries.len() as i64 + 1;
        self.entries.push(Entry {
            id,
            parent: self.path.last().copied(),
            message,
            label: None,
            active: false,
        });
        self.path.push(id);
        id
    }
    fn depth(&self, path: &[i64]) -> usize {
        path.iter()
            .filter(|&&id| self.entries[(id - 1) as usize].message.is_some())
            .count()
    }
    fn archive(&mut self) {
        let depth = self.depth(&self.path);
        for index in depth..self.session.history.len() {
            self.append(Some(self.session.history[index].clone()));
        }
        let Some(mut tip) = self.path.last().copied() else {
            return;
        };
        let changed = depth == self.session.history.len();
        if let Some(checkpoint) = self.session.checkpoint.clone()
            && self
                .path
                .iter()
                .rev()
                .find_map(|id| self.checkpoints.get(id))
                != Some(&checkpoint)
        {
            if changed {
                tip = self.append(None);
            }
            self.checkpoints.insert(tip, checkpoint);
        }
        let preference = (self.session.model.clone(), self.session.reasoning.clone());
        let previous = self
            .path
            .iter()
            .rev()
            .find_map(|id| self.preferences.get(id));
        if previous != Some(&preference) && (previous.is_some() || preference != (None, None)) {
            if changed {
                tip = self.append(None);
            }
            self.preferences.insert(tip, preference);
        }
    }
    fn destination(&self, entry: i64) -> Result<(Vec<i64>, Session, Option<Message>)> {
        let (path, session) = std::iter::once((&self.path, &self.session))
            .chain(self.archived.iter())
            .find(|(p, _)| p.contains(&entry))
            .context("unknown entry")?;
        let index = path.iter().position(|&id| id == entry).unwrap();
        let selected = self.entries[(entry - 1) as usize].message.as_ref();
        let (end, draft) = match selected {
            Some(m) if m.user() => (index, Some(m.clone())),
            Some(Message::Assistant { calls, .. }) if calls.is_empty() => (index + 1, None),
            Some(Message::BranchSummary(_)) | None => (index + 1, None),
            _ => anyhow::bail!("select a prompt or completed answer"),
        };
        let path = &path[..end];
        let mut s = session.clone();
        s.truncate_branch(self.depth(path))?;
        s.checkpoint = path
            .iter()
            .rev()
            .find_map(|id| self.checkpoints.get(id))
            .cloned();
        if let Some(preference) = path.iter().rev().find_map(|id| self.preferences.get(id)) {
            (s.model, s.reasoning) = preference.clone();
        }
        Ok((path.to_vec(), s, draft))
    }
}
pub struct MemoryStore {
    id: i64,
    sessions: BTreeMap<i64, Record>,
}
impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            id: 1,
            sessions: [(1, Record::default())].into(),
        }
    }
}
impl MemoryStore {
    fn create(&mut self, session: Session) {
        self.id = self.sessions.last_key_value().unwrap().0 + 1;
        self.sessions.insert(
            self.id,
            Record {
                session,
                ..Default::default()
            },
        );
    }
}
impl Store for MemoryStore {
    fn load(&self) -> Result<Session> {
        Ok(self.sessions[&self.id].session.clone())
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        let record = self.sessions.get_mut(&self.id).unwrap();
        ensure!(record.session.revision == s.revision, "stale revision");
        s.revision += 1;
        record.session = s.clone();
        Ok(())
    }
}
impl SessionHost for MemoryStore {
    fn session_id(&self) -> i64 {
        self.id
    }
    fn entries(&mut self) -> Result<Vec<Entry>> {
        let record = self.sessions.get_mut(&self.id).unwrap();
        record.archive();
        let mut entries = record.entries.clone();
        if let Some(&leaf) = record.path.last() {
            entries[(leaf - 1) as usize].active = true;
        }
        Ok(entries)
    }
    fn switch(&mut self, id: Option<i64>) -> Result<()> {
        if let Some(id) = id {
            ensure!(self.sessions.contains_key(&id), "unknown session {id}");
            self.id = id;
        } else {
            self.create(Session::default())
        }
        Ok(())
    }
    fn create_session(&mut self, seed: Session) -> Result<()> {
        self.create(seed);
        Ok(())
    }
    fn fork_turn(&mut self, turn: Option<usize>, name: &str) -> Result<()> {
        let mut s = self.load()?;
        ensure!(!s.active, "acknowledge interrupted turn before forking");
        if let Some(n) = turn {
            s.truncate_branch(s.after_turn(n)?)?;
        }
        s.revision = 0;
        s.name = name.into();
        s.parent = Some(self.id);
        self.create(s);
        Ok(())
    }
    fn fork_prompt(&mut self, id: i64) -> Result<Message> {
        let record = self.sessions.get_mut(&self.id).unwrap();
        ensure!(
            !record.session.active,
            "acknowledge interrupted turn before forking"
        );
        record.archive();
        let (_, mut s, draft) = record.destination(id)?;
        let draft = draft.context("select a user prompt to fork")?;
        s.revision = 0;
        s.name.clear();
        s.parent = Some(self.id);
        self.create(s);
        Ok(draft)
    }
    fn navigate_entry(&mut self, id: i64) -> Result<Option<Message>> {
        let record = self.sessions.get_mut(&self.id).unwrap();
        ensure!(
            !record.session.active,
            "acknowledge interrupted turn before navigating"
        );
        record.archive();
        let (path, mut session, draft) = record.destination(id)?;
        session.revision = record.session.revision + 1;
        record.archived.insert(
            std::mem::replace(&mut record.path, path),
            std::mem::replace(&mut record.session, session),
        );
        Ok(draft)
    }
}
