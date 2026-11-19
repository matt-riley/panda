use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use panda_coding::Coding;
use panda_core::{Cancel, Mode, Store, turn};
use panda_local::{codemode::Codemode, sandbox::Native};
use panda_memory::{EmptySkills, MemoryStore};
use panda_providers::Api;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::fs::symlink,
    path::Path,
    thread,
};

// Exercise the real worker binaries, turn loop and HTTP request serializer.
fn capture(root: &Path, args: Value, nested: bool) -> Result<Vec<Value>> {
    let exe = Path::new(env!("CARGO_BIN_EXE_panda"));
    let coding = Coding {
        workspace: Native::new(root, exe)?,
        skills: EmptySkills,
    };
    let codemode = Codemode::new(&coding, exe);
    let server = TcpListener::bind("127.0.0.1:0")?;
    let mut api = Api {
        endpoint: format!("http://{}", server.local_addr()?),
        model: "fixture".into(),
        key: "fixture".into(),
    };
    let (name, arguments) = if nested {
        (
            "codemode",
            json!({"code":format!("text(tools.read({args}))")}),
        )
    } else {
        ("read", args)
    };
    let capture = thread::spawn(move || -> Result<Vec<Value>> {
        let mut requests = Vec::new();
        for choice in [
            json!({"delta":{"tool_calls":[{"index":0,"id":"read1","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}),
            json!({"delta":{"content":"done"},"finish_reason":"stop"}),
        ] {
            let (mut conn, _) = server.accept()?;
            let mut reader = BufReader::new(&mut conn);
            let mut length = None;
            for line in reader.by_ref().lines() {
                let line = line?;
                if line.is_empty() {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let mut body = vec![0; length.context("missing content length")?];
            reader.read_exact(&mut body)?;
            requests.push(serde_json::from_slice(&body)?);
            let response = format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[choice]}));
            write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",
                response.len()
            )?;
        }
        Ok(requests)
    });
    let mut store = MemoryStore::default();
    let mut session = store.load()?;
    session.mode = Mode::Plan;
    store.save(&mut session)?;
    turn(
        &mut api,
        &mut store,
        if nested { &codemode } else { &coding },
        "inspect".into(),
        &Cancel::default(),
        &mut |_| {},
    )?;
    capture.join().unwrap()
}

#[test]
#[ignore = "requires native macOS Seatbelt and loopback HTTP"]
fn plan_and_codemode_read_the_same_image_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A legal unknown RIFF chunk takes the encoded WebP past the text
    // worker pipe limit and the default context compaction threshold.
    let mut large_webp = include_bytes!("images/pixel.webp").to_vec();
    large_webp.extend(b"JUNK");
    large_webp.extend(900_000_u32.to_le_bytes());
    large_webp.resize(large_webp.len() + 900_000, 0);
    let riff_size = (large_webp.len() as u32 - 8).to_le_bytes();
    large_webp[4..8].copy_from_slice(&riff_size);
    for (mime, bytes) in [
        ("image/png", include_bytes!("images/pixel.png").as_slice()),
        ("image/jpeg", include_bytes!("images/pixel.jpg").as_slice()),
        ("image/webp", large_webp.as_slice()),
    ] {
        // No extension: MIME must come from the bytes.
        fs::write(dir.path().join("picture"), bytes)?;
        for nested in [false, true] {
            let requests = capture(dir.path(), json!({"path":"picture"}), nested)?;
            let messages = requests[1]["messages"].as_array().unwrap();
            assert_eq!(
                messages.len(),
                5,
                "expected one image for {mime} (nested={nested})"
            );
            assert_eq!(messages[3]["tool_call_id"], "read1");
            let parts = messages[4]["content"].as_array().unwrap();
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0]["type"], "image_url");
            let url = parts[0]["image_url"]["url"].as_str().unwrap();
            let (header, data) = url.split_once(',').unwrap();
            assert_eq!(header, format!("data:{mime};base64"));
            assert!(
                STANDARD.decode(data)? == bytes,
                "altered {mime} (nested={nested})"
            );
            assert_eq!(requests[1].to_string().matches(data).count(), 1);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires native macOS Seatbelt and loopback HTTP"]
fn text_ranges_and_external_symlinks_in_both_read_paths() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("note.png"), "one\r\ntwo\r\nthree")?;
    // Seatbelt allows the system binaries themselves. Workspace containment
    // must still reject both a leaf link and a linked ancestor.
    symlink("/bin/sh", dir.path().join("leaf"))?;
    symlink("/bin", dir.path().join("directory"))?;
    for nested in [false, true] {
        for (path, success) in [("note.png", true), ("leaf", false), ("directory/sh", false)] {
            let requests = capture(
                dir.path(),
                json!({"path":path,"offset":2,"limit":1}),
                nested,
            )?;
            let messages = requests[1]["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 4);
            let text = messages[3]["content"].as_str().unwrap();
            if success {
                assert_eq!(text, "two\r\n");
            } else {
                assert!(text.contains("outside workspace"), "{text}");
            }
        }
    }
    Ok(())
}
