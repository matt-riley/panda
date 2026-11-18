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
fn summary(path: &Path) -> Result<String> {
    let mut lines = BufReader::new(open_skill(path)?.take(8192)).lines();
    if lines.next().transpose()?.as_deref() != Some("---") {
        return Ok(String::new());
    }
    let mut metadata = Vec::new();
    for line in lines {
        let line = line?;
        if line == "---" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("description:")
            .or_else(|| line.strip_prefix("name:"))
        {
            metadata.push(value.trim().to_owned())
        }
    }
    Ok(metadata.join(" "))
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
        let query = query.to_lowercase();
        Ok(self
            .files()?
            .into_iter()
            .filter(|p| {
                p.to_string_lossy().to_lowercase().contains(&query)
                    || summary(p).is_ok_and(|s| s.to_lowercase().contains(&query))
            })
            .take(100)
            .map(|p| p.display().to_string())
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
