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

#[test]
fn retry_reuses_saved_tool_result() {
    struct Flaky(bool);
    impl Provider for Flaky {
        fn reply(
            &mut self,
            sys: &str,
            h: &[Message],
            t: &[Value],
            c: &Cancel,
            e: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            if !self.0 && matches!(h.last(), Some(Message::Result { .. })) {
                self.0 = true;
                return Err(TransientFailure {
                    reason: "503".into(),
                    retry_after: Some(std::time::Duration::ZERO),
                }
                .into());
            }
            Script.reply(sys, h, t, c, e)
        }
    }
    let mut store = Memory::default();
    let mut calls = 0;
    let mut retries = 0;
    turn(
        &mut Flaky(false),
        &mut store,
        &Read,
        "test".into(),
        &Cancel::default(),
        &mut |e| match e {
            Event::Calling(_) => calls += 1,
            Event::Retry { .. } => retries += 1,
            _ => {}
        },
    )
    .unwrap();
    assert_eq!(
        (calls, retries, store.load().unwrap().history.len()),
        (1, 1, 4)
    );
}

#[test]
fn reported_usage_triggers_compaction_after_tools() {
    struct Large;
    impl Provider for Large {
        fn reply(
            &mut self,
            sys: &str,
            h: &[Message],
            t: &[Value],
            c: &Cancel,
            e: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            if sys.starts_with("Summarize") {
                assert!(h[0].summary_text().contains("hello"));
                return Ok(("tests passed".into(), vec![]));
            }
            if h[0].summary_text().contains("tests passed") {
                return Ok(("finished".into(), vec![]));
            }
            e(Event::Usage(
                json!({"input_tokens":880,"output_tokens":110}),
            ));
            Script.reply(sys, h, t, c, e)
        }
    }
    let mut store = Memory(Session {
        model_window: Some(1000),
        ..Default::default()
    });
    turn(
        &mut Large,
        &mut store,
        &Read,
        "ask".into(),
        &Cancel::default(),
        &mut |_| {},
    )
    .unwrap();
    assert!(store.0.checkpoint.is_some());
    assert_eq!(store.0.history.len(), 4);
}

proptest::proptest! {
    #[test]
    fn estimated_tail_after_checkpoint(input in 1..10000u64, output in 0..5000u64, text in ".{0,500}") {
        let mut session = Session {
            history: vec![Message::User("old".into()),Message::Assistant{text:"reply".into(),calls:vec![]}],
            usage:vec![UsageRecord {through:2,input,output,..Default::default()}],
            ..Default::default()
        };
        proptest::prop_assert_eq!(session.estimated_tokens("",&[]),input+output);
        session.history.push(Message::User(text));
        proptest::prop_assert!(session.estimated_tokens("",&[])>input+output);
        session.checkpoint=Some(Checkpoint{through:1,summary:"short".into()});
        let estimate=session.estimated_tokens("",&[]);
        session.usage.clear();
        proptest::prop_assert_eq!(estimate,session.estimated_tokens("",&[]));
    }
}

#[test]
fn large_tool_result_triggers_compaction_with_small_reported_usage() {
    struct Log;
    impl Toolset for Log {
        fn system(&self) -> Result<String> {
            Ok(String::new())
        }
        fn schemas(&self, _: Mode) -> Vec<Value> {
            vec![]
        }
        fn run(&self, _: Mode, _: &Call, _: &Cancel) -> Result<String> {
            Ok("passing\n".repeat(800))
        }
    }
    struct Build;
    impl Provider for Build {
        fn reply(
            &mut self,
            sys: &str,
            h: &[Message],
            schemas: &[Value],
            cancel: &Cancel,
            emit: &mut Emit,
        ) -> Result<(String, Vec<Call>)> {
            if sys.starts_with("Summarize") {
                return Ok(("build passed".into(), vec![]));
            }
            if h.len() == 1 && h[0].summary_text().contains("build passed") {
                return Ok(("done".into(), vec![]));
            }
            assert!(matches!(h.last(), Some(Message::User(_))));
            emit(Event::Usage(json!({"input_tokens":22,"output_tokens":10})));
            Script.reply(sys, h, schemas, cancel, emit)
        }
    }
    let mut db = Memory(Session {
        model_window: Some(1024),
        ..Default::default()
    });
    turn(
        &mut Build,
        &mut db,
        &Log,
        "test".into(),
        &Cancel::default(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(&db.0.history[2],Message::Result{text,..} if text.len()==6400));
    assert_eq!(db.0.checkpoint.unwrap().through, 3);
}
