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
            !state.starts_with(&root) && !root.starts_with(&state),
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
                        && !state.starts_with(&dir)
                        && !dir.starts_with(&state),
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
        fn drain(
            mut pipe: impl Read + Send + 'static,
        ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let n = pipe.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buffer[..n.min(1_100_000 - out.len())]);
                }
                Ok(out)
            })
        }
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let start = Instant::now();
        let result = loop {
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
        if name == "shell" {
            return self.exec(
                Path::new("/bin/sh"),
                &["-c", field(args, "command")?],
                &[],
                true,
                cancel,
            );
        }
        self.exec(
            &self.worker,
            &["--file-worker", name],
            &serde_json::to_vec(args)?,
            matches!(name, "write" | "edit"),
            cancel,
        )
    }
    fn instructions(&self) -> Result<Vec<(String, String)>> {
        let file = self.root.join("AGENTS.md");
        if !file.try_exists()? {
            return Ok(vec![]);
        }
        Ok(vec![(
            file.display().to_string(),
            self.invoke("read", &json!({"path":"AGENTS.md"}), &Cancel::default())?,
        )])
    }
}
pub fn state_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let p = std::env::var_os("PANDA_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(std::env::var_os("HOME").context("missing HOME")?).join(".panda"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&p)?;
    let metadata = fs::metadata(&p)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "PANDA_HOME must be a private directory owned by the current user (0700)"
    );
    Ok(p.canonicalize()?)
}
