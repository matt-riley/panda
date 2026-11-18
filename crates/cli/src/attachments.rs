use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use panda_core::Message;
use std::{
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path},
    process::Command,
};

pub fn paths(text: &str) -> Vec<&str> {
    let mut refs = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if let Some(quoted) = rest.strip_prefix("@\"")
            && let Some((name, tail)) = quoted.split_once('"')
        {
            if !name.is_empty() {
                refs.push(name);
            }
            rest = tail;
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if let Some(name) = rest[..end].strip_prefix('@').filter(|s| !s.is_empty()) {
            refs.push(name);
        }
        rest = &rest[end..];
    }
    refs
}

pub fn prepare(root: &Path, input: &str, clipboard: Option<&str>) -> Result<Message> {
    let mut text = input.to_owned();
    let mut urls: Vec<String> = clipboard.iter().map(|s| s.to_string()).collect();
    let mut total = 0;
    for name in paths(input) {
        let path = Path::new(name);
        ensure!(
            path.components()
                .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
            "expected a workspace-relative attachment"
        );
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        #[cfg(target_os = "macos")]
        opts.custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK);
        let file = opts
            .open(root.join(path))
            .with_context(|| format!("attach {name}"))?;
        ensure!(file.metadata()?.is_file(), "expected a regular file");
        let mut bytes = Vec::new();
        file.take(8_388_609).read_to_end(&mut bytes)?;
        total += bytes.len();
        ensure!(total <= 8_388_608, "attachments exceed 8 MiB");
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(&[255, 216, 255]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some("image/webp")
        } else {
            None
        };
        if let Some(mime) = mime {
            urls.push(format!("data:{mime};base64,{}", STANDARD.encode(bytes)));
        } else {
            let content = std::str::from_utf8(&bytes).context("unsupported attachment format")?;
            ensure!(
                content.len() <= 1_048_576,
                "text attachment exceeds 1 MiB; use a line-range read"
            );
            text.push_str(&format!("\n\nAttached file {name:?}:\n{content}"));
        }
    }
    Ok(if urls.is_empty() {
        Message::User(text)
    } else {
        Message::Images { text, urls }
    })
}

/// Read an explicitly requested image from the macOS pasteboard using AppKit.
pub fn clipboard() -> Result<String> {
    read_pasteboard(None)
}
fn read_pasteboard(fixture: Option<&str>) -> Result<String> {
    let script = r#"ObjC.import('AppKit');
function run(args) {
    const board = args.length ? $.NSPasteboard.pasteboardWithName(args[0]) : $.NSPasteboard.generalPasteboard;
    let png = board.dataForType('public.png');
    if (!png.length) {
        const image = $.NSImage.alloc.initWithPasteboard(board);
        png = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation).representationUsingTypeProperties(4, $.NSDictionary.dictionary);
    }
    if (!png.length || png.length > 8388608) throw Error('Expected a clipboard image up to 8 MiB');
    return ObjC.unwrap(png.base64EncodedStringWithOptions(0));
}"#;
    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", script])
        .args(fixture)
        .output()?;
    ensure!(
        output.status.success(),
        "could not read clipboard image: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = std::str::from_utf8(&output.stdout)?.trim();
    ensure!(data.starts_with("iVBOR"), "clipboard conversion failed");
    Ok(format!("data:image/png;base64,{data}"))
}

pub struct Completion(pub std::path::PathBuf);
impl reedline::Completer for Completion {
    fn complete(&mut self, line: &str, pos: usize) -> reedline::CompletionResult {
        let prefix = &line[..pos];
        let start = prefix
            .rfind(char::is_whitespace)
            .map_or(0, |i| i + prefix[i..].chars().next().unwrap().len_utf8());
        let word = &prefix[start..];
        let mut suggestions = Vec::new();
        if let Some(query) = word.strip_prefix('@') {
            for entry in walkdir::WalkDir::new(&self.0)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| {
                    e.depth() == 0
                        || !matches!(
                            e.file_name().to_str(),
                            Some(".git" | "target" | "node_modules")
                        )
                })
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let Some(path) = entry
                    .path()
                    .strip_prefix(&self.0)
                    .ok()
                    .and_then(Path::to_str)
                else {
                    continue;
                };
                if !path.chars().any(|c| c.is_control() || c == '"') && path.contains(query) {
                    suggestions.push(if path.contains(char::is_whitespace) {
                        format!("@\"{path}\"")
                    } else {
                        format!("@{path}")
                    });
                    if suggestions.len() >= 100 {
                        break;
                    }
                }
            }
        } else if start == 0 && word.starts_with('/') {
            suggestions.extend(
                crate::HELP
                    .split(" · ")
                    .filter_map(|s| s.split_whitespace().next())
                    .filter(|s| s.starts_with(word))
                    .map(String::from),
            );
        }
        reedline::CompletionResult::fresh(
            suggestions
                .into_iter()
                .map(|value| reedline::Suggestion {
                    value,
                    span: reedline::Span::new(start, pos),
                    append_whitespace: true,
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn named_pasteboard_can_supply_an_image_without_modifying_the_user_clipboard() {
        let name = format!("com.panda.test.{}", std::process::id());
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/mFsAAAAASUVORK5CYII=";
        let script = "ObjC.import('AppKit'); function run(a){const b=$.NSPasteboard.pasteboardWithName(a[0]);b.clearContents;b.setDataForType($.NSData.alloc.initWithBase64EncodedStringOptions(a[1],0),'public.png');}";
        let status = Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", script, &name, png])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read_pasteboard(Some(&name)).unwrap(),
            format!("data:image/png;base64,{png}")
        );
    }
    #[test]
    fn images_are_typed_and_symlinks_cannot_attach_outside_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join("image"), b"\x89PNG\r\n\x1a\nfixture").unwrap();
        assert!(
            matches!(prepare(&root,"inspect @image",None).unwrap(),Message::Images{urls,..} if urls[0].starts_with("data:image/png;base64,"))
        );
        std::os::unix::fs::symlink("/etc/hosts", root.join("alias")).unwrap();
        assert!(prepare(&root, "@alias", None).is_err());
        assert!(prepare(&root, "@../outside", None).is_err());
    }
    proptest! {
        #[test]
        fn references_are_substrings_of_arbitrary_unicode_input(input in any::<String>()){
            for path in paths(&input){prop_assert!(!path.is_empty());prop_assert!(input.contains(path));}
        }
    }
}
