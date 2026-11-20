mod edit;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use panda_coding::field;
use panda_core::ToolOutput;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::{
    Value,
    value::{RawValue, to_raw_value},
};
use std::{
    cell::OnceCell,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path},
};
use walkdir::WalkDir;

pub fn run(root: &Path, name: &str, args: &Value) -> Result<String> {
    if matches!(name, "ls" | "repo_map" | "search") {
        let offset = args
            .get("offset")
            .map(|v| v.as_u64().context("invalid offset"))
            .transpose()?
            .unwrap_or(0);
        return if name == "ls" {
            list_directory(root, args, offset)
        } else {
            page(root, name, args, offset)
        };
    }
    if name == "read" {
        return Ok(read_output(root, args)?.text);
    }
    let path = relative_path(args)?;
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
        "write" | "edit" => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
            let write_text = if name == "write" {
                let text = field(args, "text")?;
                fs::create_dir_all(parent)?;
                Some(text)
            } else {
                None
            };
            // A rename changes the file inode. Lock the stable parent while Panda
            // reads, validates and persists so concurrent workers cannot lose edits.
            let directory = fs::File::open(parent)?;
            directory.lock()?;
            let (text, report) = match write_text {
                Some(text) => (text.into(), "saved".into()),
                None => edit::apply(&read(&path)?, args)?,
            };
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
fn relative_path(args: &Value) -> Result<&str> {
    let path = field(args, "path")?;
    ensure!(
        Path::new(path)
            .components()
            .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
        "expected relative workspace path"
    );
    Ok(path)
}

pub(crate) const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

pub fn read_output(root: &Path, args: &Value) -> Result<ToolOutput> {
    use rustix::fs::{Mode, OFlags, openat};

    let path = relative_path(args)?;
    let root = root.canonicalize()?;
    let target = root.join(path).canonicalize()?;
    let relative = target
        .strip_prefix(&root)
        .context("read target outside workspace")?;
    // Follow in-workspace aliases once, then open the resolved components
    // relative to pinned directory descriptors. A swapped symlink fails closed.
    let mut fd = fs::File::open(&root)?.into();
    let mut parts = relative.iter().peekable();
    while let Some(part) = parts.next() {
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if parts.peek().is_some() {
            flags |= OFlags::DIRECTORY;
        }
        fd = openat(&fd, part, flags, Mode::empty())?;
    }
    let file = fs::File::from(fd);
    ensure!(file.metadata()?.is_file(), "expected regular file");
    let mut reader = BufReader::new(file);
    let ranged = args.get("offset").is_some() || args.get("limit").is_some();
    if let Some(mime) = panda_coding::image_mime_type(reader.fill_buf()?) {
        ensure!(!ranged, "offset and limit apply only to text files");
        let mut bytes = Vec::new();
        reader
            .take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image exceeds 8 MiB");
        return Ok(ToolOutput {
            text: format!("Read {mime} ({} bytes)", bytes.len()),
            images: vec![format!("data:{mime};base64,{}", STANDARD.encode(bytes))],
        });
    }
    if ranged {
        return panda_coding::read_range(reader, args).map(Into::into);
    }
    read_text(reader).map(Into::into)
}

pub(super) fn read(path: &Path) -> Result<String> {
    read_file(fs::File::open(path)?)
}
pub(super) fn read_file(f: fs::File) -> Result<String> {
    ensure!(f.metadata()?.is_file(), "expected regular file");
    read_text(f)
}
fn read_text(input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input.take(1_048_577).read_to_end(&mut bytes)?;
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
        let flag = |key: &str| match args.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            _ => bail!("{key} must be a boolean"),
        };
        let regex = flag("regex")?;
        let ignore_case = flag("ignoreCase")?;
        if !regex && !ignore_case {
            return Ok(Self::Literal(query));
        }
        let mut builder = if regex {
            RegexBuilder::new(query)
        } else {
            RegexBuilder::new(&regex::escape(query))
        };
        Ok(Self::Regex(
            builder
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?,
        ))
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

fn directory_rules(dir: &Path) -> Option<Gitignore> {
    // Ignore files are workspace inputs too. Skip symlinks and special files.
    let file = fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(".gitignore"))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    for (i, bytes) in BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .enumerate()
    {
        if let Ok(line) = std::str::from_utf8(&bytes) {
            let line = line.trim_end_matches('\r');
            let line = if i == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            // One malformed pattern does not invalidate the other rules.
            let _ = builder.add_line(None, line);
        }
    }
    builder.build().ok()
}

fn workspace_entries<'a>(
    root: &'a Path,
    start: &'a Path,
) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    let mut rules: Vec<Option<Gitignore>> = Vec::new();
    WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(move |entry| {
            // The walk is depth-first. Keep only the current directory's ancestors.
            rules.truncate(entry.depth());
            // Visit the ancestors for their ignore rules, then only the requested subtree.
            if !start.starts_with(entry.path()) && !entry.path().starts_with(start) {
                return false;
            }
            let is_dir = entry.file_type().is_dir();
            if entry.depth() > 0
                && matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "node_modules")
                )
            {
                return false;
            }
            let decision = rules
                .iter()
                .rev()
                .flatten()
                .map(|rules| rules.matched(entry.path(), is_dir))
                .find(|decision| !decision.is_none());
            if decision.is_some_and(|decision| decision.is_ignore()) {
                return false;
            }
            if is_dir {
                rules.push(directory_rules(entry.path()));
            }
            true
        })
}

fn excerpt(line: &str) -> &str {
    line.char_indices()
        .nth(500)
        .map_or(line, |(end, _)| &line[..end])
}

#[derive(Serialize)]
struct SearchHit<'a> {
    line: usize,
    path: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<&'a [&'a str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<&'a [&'a str]>,
}

#[derive(Default, Serialize)]
struct Page {
    items: Vec<Box<RawValue>>,
    next_offset: Option<u64>,
    #[serde(skip)]
    bytes: usize,
    #[serde(skip)]
    seen: u64,
}

impl Page {
    fn add<T: Serialize>(&mut self, offset: u64, item: impl FnOnce() -> T) -> Result<bool> {
        self.seen += 1;
        if self.seen <= offset {
            return Ok(true);
        }
        let next = offset + self.items.len() as u64;
        if self.items.len() == 50 {
            self.next_offset = Some(next);
            return Ok(false);
        }
        let item = to_raw_value(&item())?;
        // Reserve space for the envelope, the numeric cursor and each comma.
        let bytes = item.get().len() + 1;
        if self.bytes + bytes + 64 > crate::MAX_TOOL_RESULT_BYTES {
            ensure!(
                !self.items.is_empty(),
                "search hit exceeds 1 MiB; reduce context"
            );
            self.next_offset = Some(next);
            return Ok(false);
        }
        self.bytes += bytes;
        self.items.push(item);
        Ok(true)
    }
}

fn list_directory(root: &Path, args: &Value, offset: u64) -> Result<String> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};
    let path = args
        .get("path")
        .map(|v| v.as_str().context("path must be a string"))
        .transpose()?
        .unwrap_or(".");
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    // Open each component relative to the preceding directory descriptor so a
    // symlink or rename cannot redirect a later lookup outside the workspace.
    let mut directory = fs::File::open(root)?.into();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => {
                directory = openat(&directory, name, flags, Mode::empty())
                    .with_context(|| format!("cannot list directory {path:?}"))?;
            }
            Component::CurDir => {}
            _ => bail!("expected relative workspace path"),
        }
    }
    let mut entries = Dir::read_from(&directory)?.collect::<rustix::io::Result<Vec<_>>>()?;
    entries.sort_unstable_by(|a, b| a.file_name().cmp(b.file_name()));
    let mut page = Page::default();
    for entry in entries {
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let file_type = match entry.file_type() {
            FileType::Unknown => FileType::from_raw_mode(
                statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode,
            ),
            known => known,
        };
        let kind = match file_type {
            FileType::Directory => "directory",
            FileType::RegularFile => "file",
            FileType::Symlink => "symlink",
            _ => "other",
        };
        if !page.add(
            offset,
            || serde_json::json!({"name":name.to_string_lossy(),"type":kind}),
        )? {
            break;
        }
    }
    Ok(serde_json::to_string(&page)?)
}

fn page(root: &Path, name: &str, args: &Value, offset: u64) -> Result<String> {
    let query = (name == "search")
        .then(|| LineQuery::parse(args))
        .transpose()?;
    let glob = args.get("glob").map(path_filter).transpose()?;
    let context = match args.get("context") {
        Some(value) if query.is_some() => usize::try_from(
            value
                .as_u64()
                .context("context must be a non-negative integer")?,
        )?,
        _ => 0,
    };
    let mut page = Page::default();
    let start = if args.get("path").is_some() {
        let requested = root.join(relative_path(args)?);
        let target = requested
            .canonicalize()
            .context("resolving discovery path")?;
        root.join(
            target
                .strip_prefix(root.canonicalize()?)
                .context("discovery path outside workspace")?,
        )
    } else {
        root.to_path_buf()
    };
    'files: for entry in workspace_entries(root, &start) {
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
            let excerpts = OnceCell::new();
            for (i, line) in text.lines().enumerate() {
                if !q.matches(line) {
                    continue;
                }
                if !page.add(offset, || {
                    let mut hit = SearchHit {
                        path: &path,
                        line: i + 1,
                        text: excerpt(line),
                        before: None,
                        after: None,
                    };
                    if context > 0 {
                        let lines =
                            excerpts.get_or_init(|| text.lines().map(excerpt).collect::<Vec<_>>());
                        hit.before = Some(&lines[i.saturating_sub(context)..i]);
                        hit.after =
                            Some(&lines[i + 1..(i + 1).saturating_add(context).min(lines.len())]);
                    }
                    hit
                })? {
                    break 'files;
                }
            }
        } else if !page.add(offset, || &path)? {
            break;
        }
    }
    Ok(serde_json::to_string(&page)?)
}
