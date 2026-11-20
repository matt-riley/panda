// Lock only around store operations. Provider calls can block while clients inspect
// committed messages or cancel the running request.
use super::*;
pub(super) struct Shared<'a, 'b>(pub &'a Mutex<&'b mut dyn SessionHost>);
impl Store for Shared<'_, '_> {
    fn load(&self) -> Result<Session> {
        self.0.lock().unwrap().load()
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        self.0.lock().unwrap().save(s)
    }
    fn compaction_scope(&self) -> Option<String> {
        self.0.lock().unwrap().compaction_scope()
    }
    fn save_checkpoint(&mut self, s: &mut Session) -> Result<()> {
        self.0.lock().unwrap().save_checkpoint(s)
    }
}
impl SessionHost for Shared<'_, '_> {
    fn session_id(&self) -> i64 {
        self.0.lock().unwrap().session_id()
    }
    fn entries(&mut self) -> Result<Vec<sdk::Entry>> {
        self.0.lock().unwrap().entries()
    }
    fn switch(&mut self, id: Option<i64>) -> Result<()> {
        self.0.lock().unwrap().switch(id)
    }
    fn create_session(&mut self, seed: Session) -> Result<()> {
        self.0.lock().unwrap().create_session(seed)
    }
    fn switch_reference(&mut self, reference: &str) -> Result<()> {
        self.0.lock().unwrap().switch_reference(reference)
    }
    fn fork_turn(&mut self, n: Option<usize>, name: &str) -> Result<()> {
        self.0.lock().unwrap().fork_turn(n, name)
    }
    fn fork_prompt(&mut self, id: i64) -> Result<Message> {
        self.0.lock().unwrap().fork_prompt(id)
    }
    fn navigate_entry(&mut self, id: i64) -> Result<Option<Message>> {
        self.0.lock().unwrap().navigate_entry(id)
    }
}
