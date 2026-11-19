use anyhow::{Context, Result, ensure};
use panda_coding::{Workspace, field};
use panda_core::Cancel;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct Native {
    pub root: PathBuf,
    worker: PathBuf,
    scratch: tempfile::TempDir,
    toolchains: Vec<PathBuf>,
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl Native {
    pub fn new(root: &Path, worker: &Path) -> Result<Self> {
        ensure!(cfg!(target_os = "macos"), "native sandbox requires macOS");
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "expected workspace directory");
        let home =
            PathBuf::from(std::env::var_os("HOME").context("missing HOME")?).canonicalize()?;
        ensure!(
            !home.starts_with(&root),
            "choose a repository below the home directory"
        );
        let state = state_dir()?;
        ensure!(
            !overlaps(&state, &root),
            "workspace overlaps Panda credentials or sessions"
        );
        let mut toolchains = Vec::new();
        if let Some(roots) = std::env::var_os("PANDA_TOOLCHAIN_DIRS") {
            for dir in std::env::split_paths(&roots) {
                let dir = dir.canonicalize()?;
                ensure!(
                    dir.is_dir()
                        && !home.starts_with(&dir)
                        && !dir.starts_with(&root)
                        && !overlaps(&state, &dir),
                    "choose a separate installed toolchain directory"
                );
                toolchains.push(dir);
            }
        }
        Ok(Self {
            root,
            worker: worker.canonicalize()?,
            scratch: tempfile::tempdir()?,
            toolchains,
        })
    }
    pub fn session_dir(&self, path: &Path) -> Result<PathBuf> {
        let dir = private_dir(path, "--session-dir")?;
        ensure!(
            !overlaps(&dir, &self.root),
            "workspace overlaps session directory"
        );
        ensure!(
            self.toolchains.iter().all(|tools| !overlaps(&dir, tools)),
            "toolchain overlaps session directory"
        );
        Ok(dir)
    }
    fn profile(&self, write: bool) -> Result<String> {
        let quoted = |p: &Path| -> Result<String> {
            Ok(serde_json::to_string(
                p.to_str().context("non UTF-8 sandbox path")?,
            )?)
        };
        let root = quoted(&self.root)?;
        let scratch = quoted(&self.scratch.path().canonicalize()?)?;
        let worker = quoted(&self.worker)?;
        let mut rules = format!(
            r#"(version 1)(deny default)(import "dyld-support.sb")
 (allow signal (target same-sandbox))
 (allow process-exec process-fork)
 (allow sysctl-read file-read-metadata)
 (allow file-read* (subpath "/System/Library") (subpath "/usr/lib") (subpath "/usr/libexec") (subpath "/usr/share") (subpath "/usr/bin") (subpath "/usr/sbin") (subpath "/bin") (subpath "/sbin") (subpath "/Library/Developer") (subpath "/Applications/Xcode.app/Contents") (literal "/dev/null") (literal "/dev/urandom") (subpath {root}) (subpath {scratch}) (literal {worker}))
 (allow file-write* (subpath {scratch}) (literal "/dev/null"))
 (allow mach-lookup (global-name "com.apple.system.logger"))
 "#
        );
        for dir in &self.toolchains {
            rules.push_str(&format!("(allow file-read* (subpath {}))", quoted(dir)?))
        }
        if write {
            rules.push_str(&format!("(allow file-write* (subpath {root}))"))
        }
        for name in [".git", ".panda", ".agents", ".codex"] {
            let path = quoted(&self.root.join(name))?;
            rules.push_str(&format!("(deny file-write* (subpath {path}))"))
        }
        Ok(rules)
    }
    fn exec(
        &self,
        cmd: &Path,
        args: &[&str],
        input: &[u8],
        write: bool,
        cancel: &Cancel,
        emit: &mut panda_core::Emit,
    ) -> Result<String> {
        let scratch = self.scratch.path().canonicalize()?;
        let bins = self
            .toolchains
            .iter()
            .map(|p| p.join("bin"))
            .chain(std::env::split_paths(std::ffi::OsStr::new(
                "/Applications/Xcode.app/Contents/Developer/usr/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            )));
        let path = std::env::join_paths(bins)?;
        let mut proc = Command::new("/usr/bin/sandbox-exec");
        proc.arg("-p")
            .arg(self.profile(write)?)
            .arg(cmd)
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &scratch)
            .env("TMPDIR", &scratch)
            .env(
                "DEVELOPER_DIR",
                "/Applications/Xcode.app/Contents/Developer",
            )
            .env("PATH", path)
            .env("LC_ALL", "C")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cancel.check()?;
        let mut child = proc.spawn()?;
        let pid = child.id() as i32;
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let (updates, rx) = std::sync::mpsc::channel();
        fn drain(
            mut pipe: impl Read + Send + 'static,
            updates: Option<std::sync::mpsc::Sender<String>>,
        ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let n = pipe.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    let count = n.min(1_100_000 - out.len());
                    if let Some(tx) = &updates
                        && count > 0
                    {
                        let _ = tx.send(String::from_utf8_lossy(&buffer[..count]).into_owned());
                    }
                    out.extend_from_slice(&buffer[..count]);
                }
                Ok(out)
            })
        }
        let stdout = drain(
            child.stdout.take().unwrap(),
            (cmd == Path::new("/bin/sh")).then(|| updates.clone()),
        );
        let stderr = drain(
            child.stderr.take().unwrap(),
            (cmd == Path::new("/bin/sh")).then_some(updates),
        );
        let start = Instant::now();
        let result = loop {
            for chunk in rx.try_iter() {
                emit(panda_core::Event::ToolOutput(chunk));
            }
            if let Err(e) = cancel.check() {
                break Err(e);
            }
            if start.elapsed() > Duration::from_secs(120) {
                break Err(anyhow::anyhow!("tool timed out"));
            }
            if let Some(s) = child.try_wait()? {
                break Ok(s);
            }
            std::thread::sleep(Duration::from_millis(15))
        };
        // Every command gets a private process group, including shell descendants.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = child.wait();
        let status = result?;
        let _ = writer.join();
        let out = stdout.join().unwrap()?;
        let err = stderr.join().unwrap()?;
        for chunk in rx.try_iter() {
            emit(panda_core::Event::ToolOutput(chunk));
        }
        let error = String::from_utf8_lossy(&err);
        ensure!(
            status.success(),
            "command {status}: {error} {}",
            String::from_utf8_lossy(&out)
        );
        Ok(format!("{}{error}", String::from_utf8_lossy(&out)))
    }
}
impl Workspace for Native {
    fn invoke(&self, name: &str, args: &Value, cancel: &Cancel) -> Result<String> {
        self.invoke_stream(name, args, cancel, &mut |_| {})
    }
    fn invoke_stream(
        &self,
        name: &str,
        args: &Value,
        cancel: &Cancel,
        emit: &mut panda_core::Emit,
    ) -> Result<String> {
        if name == "shell" {
            return self.exec(
                Path::new("/bin/sh"),
                &["-c", field(args, "command")?],
                &[],
                true,
                cancel,
                emit,
            );
        }
        self.exec(
            &self.worker,
            &["--file-worker", name],
            &serde_json::to_vec(args)?,
            matches!(name, "write" | "edit"),
            cancel,
            emit,
        )
    }
    fn instructions(&self) -> Result<Vec<(String, String)>> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut paths = vec![state_dir()?.join("AGENTS.md")];
        if let Some(top) = self.root.ancestors().find(|d| d.join(".git").exists()) {
            let dirs: Vec<_> = self
                .root
                .ancestors()
                .skip(1)
                .take_while(|d| d.starts_with(top))
                .collect();
            paths.extend(dirs.into_iter().rev().map(|d| d.join("AGENTS.md")));
        }
        let mut loaded = Vec::new();
        for p in paths {
            if !p.try_exists()? {
                continue;
            }
            let mut opts = fs::OpenOptions::new();
            opts.read(true);
            #[cfg(target_os = "macos")]
            opts.custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK);
            loaded.push((
                p.display().to_string(),
                super::files::read_file(opts.open(p)?)?,
            ));
        }
        let file = self.root.join("AGENTS.md");
        if file.try_exists()? {
            loaded.push((
                file.display().to_string(),
                self.invoke("read", &json!({"path":"AGENTS.md"}), &Cancel::default())?,
            ));
        }
        Ok(loaded)
    }
}
pub fn state_dir() -> Result<PathBuf> {
    let p = std::env::var_os("PANDA_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(std::env::var_os("HOME").context("missing HOME")?).join(".panda"));
    private_dir(&p, "PANDA_HOME")
}
fn private_dir(p: &Path, label: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .with_context(|| format!("opening {label} at {}", p.display()))?;
    let metadata = fs::metadata(p)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "{label} must be a private directory owned by the current user (0700)"
    );
    Ok(p.canonicalize()?)
}
