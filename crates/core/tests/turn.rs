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

#[test]
fn steering_follows_tool_results_and_followups_wait_until_idle() {
    struct Steered(Vec<Vec<Message>>);
    impl Provider for Steered {
        fn reply(
            &mut self,
            _: &str,
            h: &[Message],
            _: &[Value],
            _: &Cancel,
            _: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            self.0.push(h.to_vec());
            if self.0.len() == 1 {
                return Ok((
                    "".into(),
                    vec![Call {
                        id: "r".into(),
                        name: "read".into(),
                        args: json!({}),
                    }],
                ));
            }
            Ok(("done".into(), vec![]))
        }
    }
    let mut p = Steered(vec![]);
    let mut store = Memory::default();
    let mut steer = Some("correct".to_owned());
    let mut next = Some("later".to_owned());
    turn_with_input(
        &mut p,
        &mut store,
        &Read,
        "first".into(),
        &Cancel::default(),
        &mut |_| {},
        &mut |idle| {
            if idle { next.take() } else { steer.take() }
                .into_iter()
                .collect()
        },
    )
    .unwrap();
    assert!(matches!(&p.0[1][..], [..,Message::Result{..},Message::User(s)] if s=="correct"));
    assert!(
        matches!(&p.0[2][..], [..,Message::Assistant{calls,..},Message::User(s)] if calls.is_empty() && s=="later")
    );
}

#[test]
fn compaction_preserves_transcript_and_projects_summary() {
    let history = vec![
        Message::User("why?".into()),
        Message::Assistant {
            text: "because".into(),
            calls: vec![],
        },
    ];
    let mut m = Memory(Session {
        history: history.clone(),
        ..Default::default()
    });
    struct Summary;
    impl Provider for Summary {
        fn reply(
            &mut self,
            _: &str,
            _: &[Message],
            tools: &[Value],
            _: &Cancel,
            _: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            assert!(tools.is_empty());
            Ok(("decision".into(), vec![]))
        }
    }
    compact(&mut Summary, &mut m, &Cancel::default()).unwrap();
    assert_eq!(m.0.history, history);
    assert_eq!(m.0.projected().len(), 1);
    assert!(matches!(&m.0.projected()[0],Message::User(s) if s.contains("decision")));
}

#[test]
fn summary_reads_the_tail_of_large_messages_and_failure_is_atomic() {
    struct Recorder {
        seen: String,
        fail: bool,
    }
    impl Provider for Recorder {
        fn reply(
            &mut self,
            _: &str,
            h: &[Message],
            _: &[Value],
            _: &Cancel,
            _: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            if let Message::User(t) = &h[0] {
                self.seen += t;
            }
            if self.fail {
                anyhow::bail!("summary unavailable");
            }
            Ok(("short summary".into(), vec![]))
        }
    }
    let mut m = Memory(Session {
        history: vec![Message::User("x".repeat(40000) + "final requirement")],
        ..Default::default()
    });
    let mut p = Recorder {
        seen: String::new(),
        fail: true,
    };
    assert!(compact(&mut p, &mut m, &Cancel::default()).is_err());
    assert!(m.0.checkpoint.is_none());
    p.fail = false;
    compact(&mut p, &mut m, &Cancel::default()).unwrap();
    assert!(p.seen.contains("final requirement"));
}
