use super::*;
use panda_core::Call;

const FIXTURE: &str = include_str!("../../../../cli/tests/fixtures/pi-v3.jsonl");

fn export(disk: &mut Disk) -> Result<Vec<Value>> {
    let mut file = Vec::new();
    disk.export_pi(&mut file)?;
    std::str::from_utf8(&file)?
        .lines()
        .map(|s| Ok(serde_json::from_str(s)?))
        .collect()
}

#[test]
fn malformed_imports_roll_back_every_session_and_tree_row() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Disk::open(dir.path(), dir.path(), None)?;
    for extra in [
        "bad JSON",
        r#"{"type":"custom","id":"root","parentId":null}"#,
        r#"{"type":"custom","id":"bad","parentId":"missing"}"#,
        r#"{"type":"compaction","id":"bad","parentId":"b-answer","firstKeptEntryId":"keep","summary":"foreign"}"#,
    ] {
        let input = format!("{FIXTURE}{extra}\n");
        assert!(Disk::import_pi(dir.path(), dir.path(), input.as_bytes()).is_err());
        assert_eq!(db.list(dir.path())?, [db.id()]);
    }
    Ok(())
}

#[test]
fn switching_branches_restores_only_their_own_edits_and_compaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let edit = json!({"type":"context_edit","id":"edit","parentId":"a-answer","targetId":"result","replacement":{"content":"replacement"},"timestamp":"2026-01-02T00:00:00Z"});
    let input = format!("{FIXTURE}{edit}\n");
    let mut db = Disk::import_pi(dir.path(), dir.path(), input.as_bytes())?;
    let state = db.load()?;
    assert!(
        state
            .projected()
            .iter()
            .any(|m| matches!(m,Message::Result{text,..} if text=="replacement"))
    );
    let tree = db.tree()?;
    let sibling = tree.iter().find(|e| e.text == "sibling answer").unwrap().id;
    let current = tree.iter().find(|e| e.current).unwrap().id;
    db.navigate(sibling)?;
    let b = db.load()?;
    assert!(b.checkpoint.is_none() && b.context_edits.is_empty());
    assert_eq!(b.history.len(), 3);
    let records = export(&mut db)?;
    let marker = records.last().unwrap();
    assert_eq!(marker["parentId"], "b-answer");
    assert_eq!(marker["type"], "custom");
    db.navigate(current)?;
    assert_eq!(db.load()?.projected(), state.projected());
    Ok(())
}

#[test]
fn labels_from_existing_imports_are_migrated_by_session_and_original_target() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let import = |label: &str| {
        let event = json!({"type":"label","id":"label","parentId":"a-answer","targetId":"sibling","label":label});
        Disk::import_pi(root, root, format!("{FIXTURE}{event}\n").as_bytes())
    };
    let a = import("first")?.id();
    let b = import("second")?.id();
    let conn = Connection::open(root.join("sessions.sqlite3"))?;
    conn.execute_batch("DROP TABLE tree_labels; PRAGMA user_version=1;")?;
    for (id, label) in [(a, "first"), (b, "second")] {
        let mut db = Disk::open(root, root, Some(id))?;
        let labeled: Vec<_> = db
            .tree()?
            .into_iter()
            .filter(|e| e.label.is_some())
            .collect();
        assert_eq!(labeled.len(), 1);
        assert_eq!(labeled[0].label.as_deref(), Some(label));
        assert_eq!(labeled[0].text, "sibling only");
        assert_eq!(
            export(&mut db)?
                .iter()
                .filter(|e| e["type"] == "label")
                .count(),
            1
        );
    }
    Ok(())
}

#[test]
fn exports_native_checkpoint_images_and_matching_tool_name() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = Disk::open(dir.path(), dir.path(), None)?;
    let mut s = db.load()?;
    s.name = "Review".into();
    s.history = vec![
        Message::User("root".into()),
        Message::Reasoning("think".into()),
        Message::Assistant {
            text: String::new(),
            calls: vec![Call {
                id: "call".into(),
                name: "read".into(),
                args: json!({"path":"image"}),
            }],
        },
        Message::Result {
            id: "call".into(),
            text: "image".into(),
            images: vec!["data:image/png;base64,YQ==".into()],
            is_error: false,
            nested_reads: vec![],
        },
        Message::Assistant {
            text: "done".into(),
            calls: vec![],
        },
    ];
    s.checkpoint = Some(Checkpoint {
        through: 1,
        summary: "summary".into(),
        ..Default::default()
    });
    db.save(&mut s)?;
    let records = export(&mut db)?;
    std::thread::sleep(std::time::Duration::from_millis(3));
    let again = export(&mut db)?;
    // The synthetic session name is attached to the active leaf at export.
    assert_eq!(records[..records.len() - 1], again[..again.len() - 1]);
    assert_eq!(records[3]["message"]["toolName"], "read");
    assert_eq!(records[5]["firstKeptEntryId"], records[2]["id"]);
    let input = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let copy = Disk::import_pi(dir.path(), dir.path(), input.as_bytes())?.load()?;
    assert_eq!(copy.name, s.name);
    assert_eq!(copy.projected(), s.projected());
    let prompt = db.tree()?.into_iter().find(|e| e.is_prompt).unwrap();
    db.set_tree_label(prompt.id, "native")?;
    let labeled = export(&mut db)?;
    let edit = labeled.iter().find(|e| e["type"] == "label").unwrap();
    assert_eq!(edit["targetId"], labeled[1]["id"]);
    assert_eq!(edit["label"], "native");
    Ok(())
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn opaque_payloads_survive_without_entering_context(
        values in proptest::collection::vec((proptest::prelude::any::<i64>(), ".{0,48}"), 0..12)
    ) {
        let root=tempfile::tempdir().unwrap();
        let opaque=json!({"type":"custom","customType":"example.future","id":"panda-10",
            "parentId":"a-answer","timestamp":"2026-01-02T00:00:00Z","data":values});
        let mut db=Disk::import_pi(root.path(),root.path(),format!("{FIXTURE}{opaque}\n").as_bytes()).unwrap();
        let before=db.load().unwrap().projected();
        proptest::prop_assert_eq!(before.len(),5);
        let records=export(&mut db).unwrap();
        proptest::prop_assert_eq!(records.last(),Some(&opaque));
    }
}

#[test]
fn unfinished_tool_calls_are_archived_without_execution() -> Result<()> {
    let root = tempfile::tempdir()?;
    let partial = FIXTURE
        .lines()
        .take_while(|line| !line.contains("\"id\": \"result\""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut db = Disk::import_pi(root.path(), root.path(), partial.as_bytes())?;
    assert!(db.load()?.active);
    assert_eq!(export(&mut db)?.last().unwrap()["id"], "call");
    panda_core::acknowledge(&mut db)?;
    assert!(!db.load()?.active);
    assert_eq!(export(&mut db)?.last().unwrap()["message"]["isError"], true);
    Ok(())
}
