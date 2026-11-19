use anyhow::{Context, Result, ensure};
use panda_core::Message;
use std::io::Write;
use std::process::{Command, Stdio};

fn last_reply(history: &[Message]) -> Option<&str> {
    history.iter().rev().find_map(|message| match message {
        Message::Assistant { text, calls } if calls.is_empty() && !text.trim().is_empty() => {
            Some(text.as_str())
        }
        _ => None,
    })
}

pub fn copy_last(history: &[Message]) -> Result<&'static str> {
    let Some(text) = last_reply(history) else {
        return Ok("No assistant reply to copy yet");
    };
    let mut cmd = Command::new("/usr/bin/osascript");
    cmd.args(["-l", "JavaScript", "-e", r#"ObjC.import('AppKit');
function run(args) {
    const board = args.length ? $.NSPasteboard.pasteboardWithName(args[0]) : $.NSPasteboard.generalPasteboard;
    const data = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
    board.clearContents;
    if (!board.setDataForType(data, 'public.utf8-plain-text')) throw Error('Could not write clipboard');
}"#]);
    #[cfg(test)]
    if let Some(board) = std::env::var_os("PANDA_TEST_PASTEBOARD") {
        cmd.arg(board);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("open macOS clipboard")?;
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "clipboard write failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    written.context("send reply to clipboard")?;
    Ok("Copied last reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn copy_preserves_original_reply(entries in prop::collection::vec((0u8..7, any::<String>()), 0..40)) {
            let mut expected = None;
            let mut history = Vec::new();
            for (kind, text) in entries {
                history.push(match kind {
                    0 => {
                        if !text.trim().is_empty() { expected = Some(text.clone()); }
                        Message::Assistant { text, calls: vec![] }
                    }
                    1 => Message::BranchSummary(text),
                    2 => Message::Reasoning(text),
                    3 => Message::Result {id: "call".into(), text, images: vec![], is_error: false},
                    4 => Message::Assistant {text, calls: vec![panda_core::Call { id:"call".into(), name:"read".into(), args: serde_json::Value::Null }]},
                    5 => Message::Assistant {text: " \n".into(), calls: vec![]},
                    _ => Message::User(text),
                });
            }
            prop_assert_eq!(last_reply(&history), expected.as_deref());
        }
    }
}
