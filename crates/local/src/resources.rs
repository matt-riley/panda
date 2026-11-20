//! Project resources are text files. Opening through a symlink must never expose private state.
use anyhow::{Context, Result};
use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub(crate) fn open_resource(path: &Path) -> Result<Option<fs::File>> {
    let mut options = fs::File::options();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(target_os = "macos")]
    options.custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "expected a regular resource file: {}",
        path.display()
    );
    Ok(Some(file))
}
pub fn optional_text(path: &Path) -> Result<Option<String>> {
    let Some(file) = open_resource(path)? else {
        return Ok(None);
    };
    Ok(Some(
        crate::files::read_file(file).with_context(|| format!("reading {}", path.display()))?,
    ))
}

pub fn project_root(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
}

pub fn project_dirs(cwd: &Path) -> Vec<&Path> {
    let root = project_root(cwd);
    let mut dirs: Vec<_> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .collect();
    dirs.reverse();
    dirs
}

pub fn instruction(dir: &Path) -> Result<Option<(PathBuf, String)>> {
    for name in [
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ] {
        let path = dir.join(name);
        if let Some(text) = optional_text(&path)? {
            return Ok(Some((path, text)));
        }
    }
    Ok(None)
}
