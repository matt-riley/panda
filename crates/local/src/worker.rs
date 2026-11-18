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
