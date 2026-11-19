pub use crate::files::run;
#[cfg(test)]
use {
    serde_json::{Value, json},
    std::fs,
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_edit_reports_numbered_context_from_the_actual_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "before\nfont: serif;\nafter\n").unwrap();
        let report = run(
            dir.path(),
            "edit",
            &json!({"path":"file","old":"serif","new":"mono"}),
        )
        .unwrap();
        assert_eq!(
            report,
            "saved\n  1  before\n− 2  font: serif;\n+ 2  font: mono;\n  3  after"
        );
    }

    #[test]
    fn edits_unique_match_and_preserves_ambiguous_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("source");
        fs::write(&file, "aa bb aa").unwrap();
        assert!(
            run(
                dir.path(),
                "edit",
                &json!({"path":"source","old":"aa","new":"x"})
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "aa bb aa");
        run(
            dir.path(),
            "edit",
            &json!({"path":"source","old":"bb","new":"cc"}),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "aa cc aa");
    }
    #[test]
    fn patterns_filter_before_pagination() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        fs::create_dir_all(root.join("src/deep"))?;
        fs::write(root.join("outside.rs"), "id=99")?;
        for n in 0..51 {
            fs::write(
                root.join(format!("src/deep/{n:02}.rs")),
                "id=x\nid=42\nid=.",
            )?;
            fs::write(root.join(format!("src/deep/{n:02}.txt")), "id=11")?;
        }
        let page = |name, args: Value| -> anyhow::Result<Value> {
            Ok(serde_json::from_str(&run(root, name, &args)?)?)
        };
        for (name, mut args) in [
            (
                "search",
                json!({"query":"^id=[0-9]+$", "regex":true, "glob":"src/**/*.rs"}),
            ),
            ("repo_map", json!({"glob":"src/**/*.rs"})),
        ] {
            let first = page(name, args.clone())?;
            assert_eq!(first["items"].as_array().unwrap().len(), 50);
            assert_eq!(first["next_offset"], 50);
            args["offset"] = first["next_offset"].clone();
            let last = page(name, args)?;
            let item = if name == "search" {
                json!({"path":"src/deep/50.rs","line":2,"text":"id=42"})
            } else {
                json!("src/deep/50.rs")
            };
            assert_eq!(last, json!({"items":[item], "next_offset":null}));
        }
        let literal = page("search", json!({"query":"id=.","glob":"src/deep/00.rs"}))?;
        assert_eq!(
            literal["items"],
            json!([{"path":"src/deep/00.rs","line":3,"text":"id=."}])
        );
        assert_eq!(
            page("repo_map", json!({"glob":"src/*.rs"}))?["items"],
            json!([])
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_patterns_and_option_types_on_an_empty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        for (name, args, message) in [
            ("repo_map", json!({"glob":"["}), "glob"),
            ("search", json!({"regex":true, "query":"["}), "regex"),
            ("search", json!({"regex":"true", "query":"a"}), "regex"),
            ("repo_map", json!({"glob":false}), "glob"),
        ] {
            let error = run(dir.path(), name, &args).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn reaches_last_search_hit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "hit\n".repeat(51)).unwrap();
        let first: Value =
            serde_json::from_str(&run(dir.path(), "search", &json!({"query":"hit"})).unwrap())
                .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 50);
        let last: Value = serde_json::from_str(
            &run(
                dir.path(),
                "search",
                &json!({"query":"hit","offset":first["next_offset"]}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(last["items"][0]["line"], 51);
        assert!(last["next_offset"].is_null());
    }
}

#[test]
fn bounded_read_reaches_late_lines_in_large_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("log"),
        "1234567890\n".repeat(110000) + "last\n",
    )
    .unwrap();
    let range = serde_json::json!({"path":"log","offset":110000,"limit":2});
    assert_eq!(
        run(dir.path(), "read", &range).unwrap(),
        "1234567890\nlast\n"
    );
}

#[test]
fn scoped_instructions_apply_only_to_ancestors_of_the_requested_file() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src/gui")).unwrap();
    std::fs::write(repo.path().join("src/AGENTS.md"), "Use the UI fixtures.").unwrap();
    let output = run(
        repo.path(),
        "instructions",
        &serde_json::json!({"path":"src/gui/button.rs"}),
    )
    .unwrap();
    assert!(output.contains("src/AGENTS.md"));
    assert!(output.contains("UI fixtures"));
    assert!(
        run(
            repo.path(),
            "instructions",
            &serde_json::json!({"path":"../escape"})
        )
        .is_err()
    );
}
