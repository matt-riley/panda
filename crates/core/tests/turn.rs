use anyhow::Result;
use panda_core::*;
use serde_json::{Value, json};
#[derive(Default)]
struct Memory(Session);
impl Store for Memory {
    fn load(&self) -> Result<Session> {
        Ok(self.0.clone())
    }
    fn save(&mut self, s: &mut Session) -> Result<()> {
        self.0 = s.clone();
        Ok(())
    }
}
struct Read;
impl Toolset for Read {
    fn system(&self) -> Result<String> {
        Ok("repo".into())
    }
    fn schemas(&self, _: Mode) -> Vec<Value> {
        vec![]
    }
    fn run(&self, _: Mode, c: &Call, _: &Cancel) -> Result<String> {
        assert_eq!(c.name, "read");
        Ok("hello".into())
    }
}
struct Script;
impl Provider for Script {
    fn reply(
        &mut self,
        _: &str,
        h: &[Message],
        _: &[Value],
        _: &Cancel,
        _: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        match h.last().unwrap() {
            Message::User(_) => Ok((
                "".into(),
                vec![Call {
                    id: "1".into(),
                    name: "read".into(),
                    args: json!({}),
                }],
            )),
            Message::Result { id, text, .. } => {
                assert_eq!((id.as_str(), text.as_str()), ("1", "hello"));
                Ok(("finished".into(), vec![]))
            }
            _ => panic!(),
        }
    }
}
#[test]
fn reads_and_finishes() {
    let mut m = Memory::default();
    turn(
        &mut Script,
        &mut m,
        &Read,
        "ask".into(),
        &Cancel::default(),
        &mut |_| {},
    )
    .unwrap();
    assert!(
        matches!(m.0.history.last(),Some(Message::Assistant{text,calls}) if text=="finished" && calls.is_empty())
    );
    assert!(!m.0.active);
}
#[test]
fn incomplete_tool_exchange_requires_explicit_recovery() {
    let mut m = Memory(Session {
        active: true,
        history: vec![Message::Assistant {
            text: "".into(),
            calls: vec![Call {
                id: "pending".into(),
                name: "shell".into(),
                args: json!({}),
            }],
        }],
        ..Session::default()
    });
    assert!(
        turn(
            &mut Script,
            &mut m,
            &Read,
            "continue".into(),
            &Cancel::default(),
            &mut |_| {}
        )
        .is_err()
    );
    acknowledge(&mut m).unwrap();
    assert!(
        matches!(m.0.history.last(),Some(Message::Result{id,text,..}) if id=="pending" && text.contains("unknown"))
    );
    let history = m.0.history.clone();
    acknowledge(&mut m).unwrap();
    assert_eq!(m.0.history, history);
}
