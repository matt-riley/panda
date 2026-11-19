use crate::{Call, Checkpoint, Message, Session};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct CheckpointFiles {
    pub read_files: BTreeSet<String>,
    pub modified_files: BTreeSet<String>,
}

impl CheckpointFiles {
    fn record(&mut self, call: &Call) {
        let Some(path) = call.args["path"].as_str() else {
            return;
        };
        match call.name.as_str() {
            "read" if !self.modified_files.contains(path) => {
                self.read_files.insert(path.into());
            }
            "edit" | "write" => {
                self.read_files.remove(path);
                self.modified_files.insert(path.into());
            }
            _ => {}
        }
    }

    pub fn summary_suffix(&self) -> String {
        let mut suffix = String::new();
        for (tag, paths) in [
            ("read-files", &self.read_files),
            ("modified-files", &self.modified_files),
        ] {
            if !paths.is_empty() {
                suffix.push_str(&format!("\n\n<{tag}>\n"));
                for path in paths {
                    suffix.push_str(path);
                    suffix.push('\n');
                }
                suffix.push_str(&format!("</{tag}>"));
            }
        }
        suffix
    }
}

impl Checkpoint {
    pub fn summary_with_files(&self) -> String {
        let mut summary = self.summary.clone();
        if let Some(files) = &self.files {
            summary.push_str(&files.summary_suffix());
        }
        summary
    }
}

impl Session {
    pub(crate) fn files_through(&self, end: usize) -> CheckpointFiles {
        // Backfill legacy checkpoints once from the recorded tool outcomes.
        let (start, mut files) = self
            .checkpoint
            .as_ref()
            .and_then(|cp| cp.files.clone().map(|files| (cp.through, files)))
            .unwrap_or_default();
        // Imported checkpoints can split an assistant's calls from their results.
        let mut calls = self.history[..start]
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { calls, .. } => Some(calls.as_slice()),
                Message::User(_) | Message::Images { .. } => Some(&[][..]),
                _ => None,
            })
            .unwrap_or_default();
        for message in &self.history[start..end] {
            match message {
                Message::Assistant { calls: next, .. } => calls = next,
                Message::User(_) | Message::Images { .. } => calls = &[],
                Message::Result {
                    id,
                    is_error,
                    nested_reads,
                    ..
                } => {
                    if let Some(call) = calls.iter().find(|c| c.id == *id) {
                        if call.name == "codemode" {
                            // Successful nested reads remain real even if the script later fails.
                            files.read_files.extend(
                                nested_reads
                                    .iter()
                                    .filter(|p| !files.modified_files.contains(*p))
                                    .cloned(),
                            );
                        } else if !is_error {
                            files.record(call);
                        }
                    }
                }
                _ => {}
            }
        }
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn checkpoint_boundaries_preserve_successful_file_operations(
            operations in prop::collection::vec((0..4usize, 0..5usize, any::<bool>()), 1..40),
            split in any::<usize>(),
        ) {
            let mut session = Session::default();
            let mut expected = CheckpointFiles::default();
            for (tool, file, success) in operations {
                let name = ["read", "edit", "write", "search"][tool];
                let path = format!("{file}.rs");
                session.history.push(Message::Assistant {
                    text: String::new(),
                    calls: vec![Call { id: "reused".into(), name: name.into(), args: serde_json::json!({"path": path}) }],
                });
                session.history.push(Message::Result {
                    id: "reused".into(), text: String::new(), images: vec![], nested_reads: vec![], is_error: !success,
                });
                if success {
                    match name {
                        "read" => { expected.read_files.insert(path); }
                        "edit" | "write" => { expected.modified_files.insert(path); }
                        _ => {}
                    }
                }
            }
            expected.read_files.retain(|p| !expected.modified_files.contains(p));
            let end = session.history.len();
            let through = split % (end + 1);
            session.checkpoint = Some(Checkpoint { through, files: Some(session.files_through(through)), ..Default::default() });
            prop_assert_eq!(session.files_through(end), expected);
        }
    }
}
