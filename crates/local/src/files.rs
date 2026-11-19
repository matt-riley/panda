use anyhow::{Context, Result, bail, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use panda_coding::field;
use regex::Regex;
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
    if name == "instructions" {
        let mut output = String::new();
        let parents: Vec<_> = Path::new(path)
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .collect();
        for dir in parents.into_iter().rev() {
            let file = dir.join("AGENTS.md");
            if root.join(&file).try_exists()? {
                output.push_str(&format!(
                    "Instructions from {}:\n{}\n",
                    file.display(),
                    read(&root.join(&file))?
                ));
            }
        }
        return Ok(output);
    }
    let path = root.join(path);
    match name {
        "read" if args.get("offset").is_some() || args.get("limit").is_some() => {
            let file = fs::File::open(path)?;
            ensure!(file.metadata()?.is_file(), "expected regular file");
            panda_coding::read_range(std::io::BufReader::new(file), args)
        }
        "read" => read(&path),
        "write" | "edit" => {
            let (text, report) = if name == "edit" {
                let old = field(args, "old")?;
                let current = read(&path)?;
                ensure!(
                    !old.is_empty() && current.matches(old).count() == 1,
                    "expected one exact match"
                );
                let updated = current.replacen(old, field(args, "new")?, 1);
                let report = edit_report(&current, &updated);
                (updated, report)
            } else {
                (field(args, "text")?.into(), "saved".into())
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
            Ok(report)
        }
        _ => bail!("unknown file tool"),
    }
}
// Keep the small diff from the same snapshots used for the atomic write so replay
// can show the original edit after the workspace has changed.
fn edit_report(before: &str, after: &str) -> String {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let common_end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut report = String::from("saved");
    let mut append = |mark: &str, first: usize, lines: &[&str]| {
        use std::fmt::Write;
        for (offset, line) in lines.iter().take(8).enumerate() {
            let mut chars = line.trim_end_matches(['\r', '\n']).chars();
            let excerpt: String = chars.by_ref().take(256).collect();
            let ellipsis = if chars.next().is_some() { "…" } else { "" };
            let _ = write!(
                report,
                "\n{mark} {}  {excerpt}{ellipsis}",
                first + offset + 1
            );
        }
        if lines.len() > 8 {
            report.push_str("\n  …");
        }
    };
    let context = start.saturating_sub(2);
    append(" ", context, &old[context..start]);
    append("−", start, &old[start..old.len() - common_end]);
    append("+", start, &new[start..new.len() - common_end]);
    let tail = new.len() - common_end;
    append(" ", tail, &new[tail..tail + common_end.min(2)]);
    report
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
enum LineQuery<'a> {
    Literal(&'a str),
    Regex(Regex),
}

impl<'a> LineQuery<'a> {
    fn parse(args: &'a Value) -> Result<Self> {
        let query = field(args, "query")?;
        match args.get("regex") {
            None | Some(Value::Bool(false)) => Ok(Self::Literal(query)),
            Some(Value::Bool(true)) => Ok(Self::Regex(
                Regex::new(query).map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?,
            )),
            _ => bail!("regex must be a boolean"),
        }
    }

    fn matches(&self, line: &str) -> bool {
        match self {
            Self::Literal(query) => line.contains(*query),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

fn path_filter(value: &Value) -> Result<GlobSet> {
    let pattern = value.as_str().context("glob must be a string")?;
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| anyhow::anyhow!("invalid glob: {e}"))?;
    Ok(GlobSetBuilder::new().add(glob).build()?)
}

fn page(root: &Path, name: &str, args: &Value) -> Result<String> {
    let offset = args
        .get("offset")
        .map(|v| v.as_u64().ok_or_else(|| anyhow::anyhow!("invalid offset")))
        .transpose()?
        .unwrap_or(0);
    let query = (name == "search")
        .then(|| LineQuery::parse(args))
        .transpose()?;
    let glob = args.get("glob").map(path_filter).transpose()?;
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
        let relative = entry.path().strip_prefix(root)?;
        if glob.as_ref().is_some_and(|g| !g.is_match(relative)) {
            continue;
        }
        let path = relative.to_string_lossy();
        if let Some(q) = &query {
            let Ok(text) = read(entry.path()) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if q.matches(line)
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
