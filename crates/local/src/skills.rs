use anyhow::{Context, Result, ensure};
use panda_coding::Skills;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
struct Metadata {
    name: Option<String>,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    disabled: bool,
}

struct Entry {
    name: String,
    path: PathBuf,
    description: String,
    disabled: bool,
}

fn entry(path: &Path) -> Result<Entry> {
    use std::io::{BufReader, Read};
    let file = crate::resources::open_resource(path)?.context("skill disappeared")?;
    from_reader(path, BufReader::new(file.take(16 * 1024)))
}
fn from_reader(path: &Path, mut reader: impl std::io::BufRead) -> Result<Entry> {
    let mut raw = String::new();
    reader.read_line(&mut raw)?;
    if raw.trim_end() == "---" {
        loop {
            let start = raw.len();
            if reader.read_line(&mut raw)? == 0 || raw[start..].trim_end() == "---" {
                break;
            }
        }
    }
    let normalized = raw.replace("\r\n", "\n");
    let metadata = if let Some(rest) = normalized.strip_prefix("---\n") {
        let end = rest
            .lines()
            .position(|s| s == "---")
            .context("unterminated skill frontmatter")?;
        let yaml = rest.lines().take(end).collect::<Vec<_>>().join("\n");
        let meta: Metadata = serde_yaml_ng::from_str(&yaml)?;
        ensure!(
            !meta.description.trim().is_empty() && meta.description.chars().count() <= 1024,
            "description must contain 1–1024 characters"
        );
        if let Some(n) = &meta.name {
            ensure!(
                !n.is_empty()
                    && n.len() <= 64
                    && !n.starts_with('-')
                    && !n.ends_with('-')
                    && !n.contains("--")
                    && n.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "invalid skill name"
            );
        }
        meta
    } else {
        Metadata::default()
    }; // Keep supporting Panda's plain Markdown skills.
    let name = metadata
        .name
        .or_else(|| path.parent()?.file_name()?.to_str().map(str::to_owned))
        .context("missing skill name")?;
    Ok(Entry {
        name,
        path: path.to_owned(),
        description: metadata.description,
        disabled: metadata.disabled,
    })
}

pub struct Library(Vec<PathBuf>, BTreeMap<PathBuf, String>);
impl Library {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self(
            roots
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
            BTreeMap::new(),
        )
    }

    pub fn with_packages(mut self, resources: BTreeMap<PathBuf, String>) -> Self {
        self.1 = resources;
        self
    }
    fn body(&self, path: &Path) -> Result<String> {
        if let Some(body) = self.1.get(path) {
            return Ok(body.clone());
        }
        crate::resources::optional_text(path)?.context("skill disappeared")
    }

    fn catalog(&self) -> Result<(Vec<Entry>, Vec<String>)> {
        let mut seen = BTreeSet::new();
        let mut catalog = BTreeMap::<String, Entry>::new();
        let mut diagnostics = Vec::new();
        for root in &self.0 {
            if !root.is_dir() {
                continue;
            }
            for result in walkdir::WalkDir::new(root)
                .max_depth(5)
                .follow_root_links(false)
                .sort_by_file_name()
            {
                let file = result?;
                if !file.file_type().is_file() || file.file_name() != "SKILL.md" {
                    continue;
                }
                let path = file.into_path();
                if !seen.insert(path.clone()) {
                    continue;
                }
                match entry(&path) {
                    Ok(e) => {
                        if let Some(first) = catalog.get(&e.name) {
                            diagnostics.push(format!(
                                "Duplicate skill {}: using {}, ignoring {}",
                                e.name,
                                first.path.display(),
                                path.display()
                            ));
                        } else {
                            catalog.insert(e.name.clone(), e);
                        }
                    }
                    Err(e) => diagnostics.push(format!("Invalid skill {}: {e:#}", path.display())),
                }
            }
        }
        for (id, body) in &self.1 {
            match from_reader(id, std::io::Cursor::new(body.as_bytes())) {
                Ok(item) => {
                    catalog.entry(item.name.clone()).or_insert(item);
                }
                Err(e) => {
                    diagnostics.push(format!("Invalid package skill {}: {e:#}", id.display()))
                }
            }
        }
        Ok((catalog.into_values().collect(), diagnostics))
    }

    pub fn diagnostics(&self) -> Result<Vec<String>> {
        Ok(self.catalog()?.1)
    }
    fn matches(&self, query: &str, explicit: bool) -> Result<Vec<Entry>> {
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for entry in self.catalog()?.0 {
            if entry.disabled && !explicit {
                continue;
            }
            let label = entry.name.to_lowercase();
            let score = if label == needle {
                0
            } else if label.starts_with(&needle) {
                1
            } else if label.contains(&needle) {
                2
            } else if entry.description.to_lowercase().contains(&needle) {
                3
            } else if entry
                .path
                .to_string_lossy()
                .to_lowercase()
                .contains(&needle)
            {
                4
            } else {
                continue;
            };
            matches.push((score, entry));
        }
        matches.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)));
        Ok(matches.into_iter().map(|(_, e)| e).collect())
    }
    pub fn ranked(&self, query: &str) -> Result<Vec<(String, PathBuf, String)>> {
        Ok(self
            .matches(query, true)?
            .into_iter()
            .take(100)
            .map(|e| (e.name, e.path, e.description))
            .collect())
    }
    pub fn named(&self, name: &str) -> Result<(PathBuf, String)> {
        let e = self
            .matches(name, true)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("unknown skill {name}"))?;
        let body = self.body(&e.path)?;
        Ok((e.path, body))
    }
    pub fn startup_index(&self) -> Result<String> {
        Ok(self
            .catalog()?
            .0
            .into_iter()
            .filter(|e| !e.disabled)
            .map(|e| format!("{} — {} ({})", e.name, e.description, e.path.display()))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
impl Skills for Library {
    fn search(&self, query: &str) -> Result<String> {
        Ok(self
            .matches(query, false)?
            .into_iter()
            .take(100)
            .map(|e| {
                if e.description.is_empty() {
                    e.path.display().to_string()
                } else {
                    format!("{}  {}", e.path.display(), e.description)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
    fn load(&self, path: &str) -> Result<String> {
        let p = Path::new(path);
        ensure!(
            self.catalog()?.0.iter().any(|e| e.path == p && !e.disabled),
            "unknown or explicitly invoked skill path"
        );
        self.body(p)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_only_discovered_skills() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("SKILL.md");
        std::fs::write(&p, "# Skill").unwrap();
        let library = Library::new(vec![d.path().into()]);
        let found = library.search("skill").unwrap();
        assert_eq!(library.load(&found).unwrap(), "# Skill");
        assert!(library.load("/etc/passwd").is_err());
    }
}
#[cfg(test)]
mod discovery_tests {
    use super::*;
    #[test]
    fn finds_skill_by_description_without_loading_body_into_results() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("SKILL.md");
        std::fs::write(
            &p,
            "---\nname: fix\ndescription: Diagnose flaky builds\n---\nPrivate instructions.",
        )
        .unwrap();
        let result = Library::new(vec![d.path().into()])
            .search("flaky builds")
            .unwrap();
        assert!(result.contains("SKILL.md"));
        assert!(!result.contains("Private instructions"));
    }
}

#[test]
fn explicit_skill_uses_declared_name_and_returns_only_its_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SKILL.md");
    std::fs::write(
        &path,
        "---\nname: review-docs\ndescription: Check docs\n---\nRead README first.",
    )
    .unwrap();
    let lib = Library::new(vec![dir.path().into()]);
    let (found, text) = lib.named("review-docs").unwrap();
    assert_eq!(found, path.canonicalize().unwrap());
    assert!(text.contains("Read README"));
}
