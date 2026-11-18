use anyhow::{Result, ensure};
use panda_coding::Skills;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
fn open_skill(path: &Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "macos")]
    opts.custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK);
    let f = opts.open(path)?;
    ensure!(f.metadata()?.is_file(), "expected a regular skill file");
    Ok(f)
}
fn summary(path: &Path) -> Result<(Option<String>, String)> {
    let mut lines = BufReader::new(open_skill(path)?.take(8192)).lines();
    if lines.next().transpose()?.as_deref() != Some("---") {
        return Ok((None, String::new()));
    }
    let mut name = None;
    let mut description = String::new();
    for line in lines {
        let line = line?;
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let text = value.trim().trim_matches(['"', char::from(39)]).to_owned();
            match key {
                "name" => name = Some(text),
                "description" => description = text,
                _ => {}
            }
        }
    }
    Ok((name, description))
}
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
pub struct Library(Vec<PathBuf>);
impl Library {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self(roots)
    }
    pub fn ranked(&self, query: &str) -> Result<Vec<(String, PathBuf, String)>> {
        let needle = query.to_lowercase();
        let mut entries = Vec::new();
        for path in self.files()? {
            let name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned();
            let (declared, meta) = summary(&path).unwrap_or_default();
            let name = declared.unwrap_or(name);
            let label = name.to_lowercase();
            let score = if label == needle {
                0
            } else if label.starts_with(&needle) {
                1
            } else if label.contains(&needle) {
                2
            } else if meta.to_lowercase().contains(&needle) {
                3
            } else if path.to_string_lossy().to_lowercase().contains(&needle) {
                4
            } else {
                continue;
            };
            entries.push((score, name, path, meta));
        }
        entries.sort();
        Ok(entries
            .into_iter()
            .take(100)
            .map(|(_, n, p, d)| (n, p, d))
            .collect())
    }
    pub fn named(&self, name: &str) -> Result<(PathBuf, String)> {
        let mut matches = self
            .ranked(name)?
            .into_iter()
            .filter(|(n, _, _)| n.eq_ignore_ascii_case(name));
        let (_, p, _) = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("unknown skill {name}"))?;
        ensure!(
            matches.next().is_none(),
            "ambiguous skill {name}; use skill_load with the selected path"
        );
        let text = self.load(
            p.to_str()
                .ok_or_else(|| anyhow::anyhow!("non UTF-8 path"))?,
        )?;
        Ok((p, text))
    }
    fn files(&self) -> Result<BTreeSet<PathBuf>> {
        let mut files = BTreeSet::new();
        for root in &self.0 {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            for e in walkdir::WalkDir::new(root).max_depth(5).follow_links(false) {
                let e = e?;
                if e.file_type().is_file() && e.file_name() == "SKILL.md" {
                    files.insert(e.into_path());
                }
            }
        }
        Ok(files)
    }
}
impl Skills for Library {
    fn search(&self, query: &str) -> Result<String> {
        Ok(self
            .ranked(query)?
            .into_iter()
            .map(|(_, p, desc)| {
                if desc.is_empty() {
                    p.display().to_string()
                } else {
                    format!("{}  {desc}", p.display())
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
    fn load(&self, path: &str) -> Result<String> {
        let p = Path::new(path);
        ensure!(self.files()?.contains(p), "unknown skill path");
        super::files::read_file(open_skill(p)?)
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
