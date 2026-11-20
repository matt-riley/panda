use crate::resources::optional_text;
use anyhow::{Context, Result};
use regex::{Captures, Regex};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::LazyLock,
};

enum Source {
    File(PathBuf),
    Markdown(String),
}
pub struct Templates(BTreeMap<String, Source>);
impl Templates {
    pub fn discover(state: &Path, project: &Path, trusted: bool) -> Result<Self> {
        Self::with_packages(state, project, trusted, BTreeMap::new())
    }
    pub fn with_packages(
        state: &Path,
        project: &Path,
        trusted: bool,
        packages: BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut files = BTreeMap::new();
        let user = state.join("prompts");
        let local = project.join(".pi/prompts");
        for dir in std::iter::once(user).chain(trusted.then_some(local)) {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
            };
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_file()
                    && path.extension().is_some_and(|e| e == "md")
                    && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                {
                    files.entry(name.to_owned()).or_insert(Source::File(path));
                }
            }
        }
        for (name, body) in packages {
            files.entry(name).or_insert(Source::Markdown(body));
        }
        Ok(Self(files))
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
    pub fn expand(&self, line: &str) -> Result<Option<String>> {
        let Some(command) = line.strip_prefix('/').or_else(|| line.strip_prefix(':')) else {
            return Ok(None);
        };
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let Some(path) = self.0.get(name) else {
            return Ok(None);
        };
        let raw = match path {
            Source::File(path) => optional_text(path)?
                .with_context(|| format!("missing template {}", path.display()))?,
            Source::Markdown(text) => text.clone(),
        };
        let normalized = raw.replace("\r\n", "\n");
        let body = if let Some(meta) = normalized.strip_prefix("---\n") {
            let end = meta
                .split_inclusive('\n')
                .scan(0usize, |len, line| {
                    *len += line.len();
                    Some((*len, line.trim_end_matches('\n') == "---"))
                })
                .find(|(_, end)| *end)
                .context("unterminated prompt frontmatter")?
                .0;
            &meta[end..]
        } else {
            &normalized
        };
        Ok(Some(substitute(body, &arguments(args))))
    }
}

fn arguments(input: &str) -> Vec<String> {
    let mut chars = input.chars().peekable();
    let mut args = Vec::new();
    while let Some(first) = chars.next() {
        if first.is_whitespace() {
            continue;
        }
        let mut word = String::new();
        if matches!(first, '\'' | '"') {
            let mut quoted = chars.clone();
            let mut closed = false;
            while let Some(c) = quoted.next() {
                if c == first {
                    closed = true;
                    break;
                }
                if c == '\\' && quoted.peek() == Some(&first) {
                    quoted.next();
                    word.push(first)
                } else {
                    word.push(c)
                }
            }
            if closed {
                chars = quoted
            } else {
                word.clear();
                word.push(first)
            }
        } else {
            word.push(first)
        }
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            word.push(c)
        }
        args.push(word);
    }
    args
}

fn substitute(body: &str, args: &[String]) -> String {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\{([0-9]+|ARGUMENTS|@):-([^}]*)\}|\$\{@:([0-9]+)(?::([0-9]+))?\}|\$(ARGUMENTS|@|[0-9]+)").unwrap()
    });
    let all = args.join(" ");
    PATTERN
        .replace_all(body, |cap: &Captures<'_>| {
            if let Some(start) = cap.get(3) {
                let offset = start
                    .as_str()
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)
                    .saturating_sub(1)
                    .min(args.len());
                let count = cap
                    .get(4)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(usize::MAX);
                return args[offset..offset.saturating_add(count).min(args.len())].join(" ");
            }
            let key = cap.get(1).or_else(|| cap.get(5)).unwrap().as_str();
            let value = if matches!(key, "ARGUMENTS" | "@") {
                all.as_str()
            } else {
                key.parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| args.get(i))
                    .map_or("", String::as_str)
            };
            if value.is_empty() {
                cap.get(2).map_or("", |m| m.as_str())
            } else {
                value
            }
            .to_owned()
        })
        .into_owned()
}
