use super::*;
use panda_core::{
    Message,
    sdk::{Entry, SessionHost},
};
impl SessionHost for Disk {
    fn session_id(&self) -> i64 {
        self.id
    }
    fn entries(&mut self) -> Result<Vec<Entry>> {
        let s = self.load()?;
        let tx = self.conn.transaction()?;
        let leaf = tree::archive(&tx, self.id, &s)?;
        let mut query=tx.prepare("SELECT n.id,n.parent,n.message,l.label FROM tree_nodes n LEFT JOIN tree_labels l ON n.id=l.node WHERE n.session=? ORDER BY n.id")?;
        let mut rows = query.query([self.id])?;
        let mut entries = vec![];
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let parent: i64 = row.get(1)?;
            entries.push(Entry {
                id,
                parent: (parent != 0).then_some(parent),
                message: serde_json::from_str(&row.get::<_, String>(2)?)?,
                label: row.get(3)?,
                active: id == leaf,
            });
        }
        drop(rows);
        drop(query);
        tx.commit()?;
        Ok(entries)
    }
    fn switch(&mut self, id: Option<i64>) -> Result<()> {
        if id == Some(self.id) {
            return Ok(());
        }
        let mut next = Self::open(&self.directory, &self.workspace, id)?;
        next.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        *self = next;
        Ok(())
    }
    fn create_session(&mut self, seed: Session) -> Result<()> {
        let mut next = Self::open_seed(&self.directory, &self.workspace, None, &seed)?;
        next.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        *self = next;
        Ok(())
    }
    fn switch_reference(&mut self, reference: &str) -> Result<()> {
        let key = reference
            .strip_prefix("panda-session:")
            .unwrap_or(reference);
        if let Ok(id) = key.parse::<i64>() {
            return SessionHost::switch(self, Some(id));
        }
        let path = Path::new(reference);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            self.workspace.join(path)
        };
        ensure!(
            path.extension().is_some_and(|s| s == "jsonl"),
            "expected a Panda session ID or a Pi JSONL session"
        );
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Pi session must be a regular file"
        );
        let mut next = Disk::import_pi(
            &self.directory,
            &self.workspace,
            std::io::BufReader::new(file),
        )?;
        next.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        *self = next;
        Ok(())
    }
    fn fork_turn(&mut self, turn: Option<usize>, name: &str) -> Result<()> {
        let mut next = self.fork(&self.directory, &self.workspace, turn, name)?;
        next.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        *self = next;
        Ok(())
    }
    fn fork_prompt(&mut self, entry: i64) -> Result<Message> {
        let (mut next, draft) = Disk::fork_prompt(self, &self.directory, &self.workspace, entry)?;
        next.invocation_keep_recent_tokens = self.invocation_keep_recent_tokens;
        *self = next;
        Ok(draft)
    }
    fn navigate_entry(&mut self, entry: i64) -> Result<Option<Message>> {
        self.navigate(entry)
    }
}
