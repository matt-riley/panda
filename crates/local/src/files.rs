use anyhow::{Result, bail, ensure};
use panda_coding::field;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path},
};
use walkdir::WalkDir;

pub fn run(root: &Path, name: &str, args: &Value) -> Result<String> {
    if name == "repo_map" || name == "search" {
        return page(root, name, args);
    }
    let path = field(args, "path")?;
    ensure!(
        Path::new(path)
            .components()
            .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
        "expected relative workspace path"
    );
    let path = root.join(path);
    match name {
        "read" => read(&path),
        "write" | "edit" => {
            let text = if name == "edit" {
                let old = field(args, "old")?;
                let current = read(&path)?;
                ensure!(
                    !old.is_empty() && current.matches(old).count() == 1,
                    "expected one exact match"
                );
                current.replacen(old, field(args, "new")?, 1)
            } else {
                field(args, "text")?.into()
            };
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
            fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            if let Ok(meta) = path.metadata() {
                temp.as_file().set_permissions(meta.permissions())?
            }
            temp.write_all(text.as_bytes())?;
            temp.persist(path)?;
            Ok("saved".into())
        }
        _ => bail!("unknown file tool"),
    }
}
pub(super) fn read(path: &Path) -> Result<String> {
    read_file(fs::File::open(path)?)
}
pub(super) fn read_file(f: fs::File) -> Result<String> {
    ensure!(f.metadata()?.is_file(), "expected regular file");
    let mut bytes = Vec::new();
    f.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "file exceeds 1 MiB");
    Ok(String::from_utf8(bytes)?)
}
fn page(root: &Path, name: &str, args: &Value) -> Result<String> {
    let offset = args
        .get("offset")
        .map(|v| v.as_u64().ok_or_else(|| anyhow::anyhow!("invalid offset")))
        .transpose()?
        .unwrap_or(0);
    let query = if name == "search" {
        Some(field(args, "query")?)
    } else {
        None
    };
    let mut seen = 0u64;
    let mut items = Vec::new();
    let mut more = false;
    let mut add = |v| {
        seen += 1;
        if seen <= offset {
            return true;
        }
        if items.len() == 50 {
            more = true;
            return false;
        }
        items.push(v);
        true
    };
    let files = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !matches!(
                    e.file_name().to_str(),
                    Some(".git" | "target" | "node_modules")
                )
        });
    'files: for entry in files {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().strip_prefix(root)?.to_string_lossy();
        if let Some(q) = query {
            let Ok(text) = read(entry.path()) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if line.contains(q)
                    && !add(
                        json!({"path":path,"line":i+1,"text":line.chars().take(500).collect::<String>()}),
                    )
                {
                    break 'files;
                }
            }
        } else if !add(json!(path)) {
            break;
        }
    }
    Ok(json!({"items":items,"next_offset":more.then(||offset+50)}).to_string())
}
