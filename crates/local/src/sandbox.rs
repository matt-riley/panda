mod shell;

use anyhow::{Context, Result, ensure};
use panda_coding::{Workspace, field};
use panda_core::{Cancel, ToolOutput};
use serde_json::Value;
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
        for name in [".git", ".panda", ".agents", ".codex", ".pi"] {
            let path = quoted(&self.root.join(name))?;
            rules.push_str(&format!("(deny file-write* file-link (subpath {path}))"))
        }
        Ok(rules)
    }
    fn command(&self, cmd: &Path, args: &[&str], write: bool) -> Result<Command> {
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
            .process_group(0);
        Ok(proc)
    }

    fn exec_file(&self, name: &str, args: &Value, cancel: &Cancel) -> Result<String> {
        let input = serde_json::to_vec(args)?;
        let limit = match name {
            // An 8 MiB base64 image or 1 MiB of JSON-escaped UTF-8 text.
            "read" => crate::files::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1024,
            // Each replacement copies part of the JSON input and gains two byte offsets.
            "edit" => input
                .len()
                .saturating_mul(5)
                .saturating_add(6 * crate::MAX_TOOL_RESULT_BYTES + 1024),
            _ => 1_100_000,
        };
        let mut proc = self.command(
            &self.worker,
            &["--file-worker", name],
            matches!(name, "write" | "edit"),
        )?;
        cancel.check()?;
        let mut child = proc
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        fn drain(
            mut pipe: impl Read + Send + 'static,
            limit: u64,
        ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.by_ref().take(limit).read_to_end(&mut bytes)?;
                std::io::copy(&mut pipe, &mut std::io::sink())?;
                Ok(bytes)
            })
        }
        let stdout = drain(child.stdout.take().unwrap(), limit as u64);
        let stderr = drain(child.stderr.take().unwrap(), 1_100_000);
        let status = wait_group(child, Duration::from_secs(120), cancel, || Ok(false))?;
        let _ = writer.join();
        let out = stdout.join().unwrap()?;
        let err = stderr.join().unwrap()?;
        let error = String::from_utf8_lossy(&err);
        if matches!(name, "read" | "edit") {
            ensure!(status.success(), "command {status}: {error}");
            return Ok(String::from_utf8(out)?);
        }
        ensure!(
            status.success(),
            "command {status}: {error} {}",
            String::from_utf8_lossy(&out)
        );
        Ok(format!("{}{error}", String::from_utf8_lossy(&out)))
    }
}
// Always terminate the private process group, including on cancellation, timeout
// or an I/O error. Busy output must still yield to the deadline every iteration.
fn wait_group(
    mut child: std::process::Child,
    timeout: Duration,
    cancel: &Cancel,
    mut pump: impl FnMut() -> Result<bool>,
) -> Result<std::process::ExitStatus> {
    let pid = child.id() as i32;
    let started = Instant::now();
    let result = (|| loop {
        cancel.check()?;
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        ensure!(
            started.elapsed() < timeout,
            "tool timed out after {}s",
            timeout.as_secs()
        );
        if !pump()? {
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    child.wait()?;
    result
}

impl Workspace for Native {
    fn invoke(&self, name: &str, args: &Value, cancel: &Cancel) -> Result<String> {
        self.invoke_stream(name, args, cancel, &mut |_| {})
            .map(|output| output.text)
    }
    fn invoke_stream(
        &self,
        name: &str,
        args: &Value,
        cancel: &Cancel,
        emit: &mut panda_core::Emit,
    ) -> Result<ToolOutput> {
        if name == "shell" {
            let timeout = panda_coding::shell_timeout(args)?;
            let proc =
                self.command(Path::new("/bin/sh"), &["-c", field(args, "command")?], true)?;
            return shell::run(proc, &self.root, timeout, cancel, emit).map(Into::into);
        }
        let result = self.exec_file(name, args, cancel)?;
        if matches!(name, "read" | "edit") {
            Ok(serde_json::from_str(&result)?)
        } else {
            Ok(result.into())
        }
    }
    fn instructions(&self) -> Result<Vec<(String, String)>> {
        let state = state_dir()?;
        let mut loaded = Vec::new();
        for dir in
            std::iter::once(state.as_path()).chain(crate::resources::project_dirs(&self.root))
        {
            if let Some((path, text)) = crate::resources::instruction(dir)? {
                loaded.push((path.display().to_string(), text));
            }
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
