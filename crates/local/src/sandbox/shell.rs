use super::wait_group;
use crate::output_log::{create_log, open_root};
use anyhow::Result;
use panda_core::{Cancel, Emit, Event};
use panda_core::{
    ToolDetails, ToolOutput,
    sdk::{ShellFailure, ShellOutcome},
};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const TAIL_BYTES: usize = 64 * 1024;
const LIVE_BYTES: usize = 1_100_000;

#[derive(Default)]
struct Capture {
    tail: VecDeque<u8>,
    log: Option<io::Result<(String, File)>>,
    live_bytes: usize,
}

impl Capture {
    fn append(&mut self, bytes: &[u8], root: &File, emit: &mut Emit) {
        let excess = (self.tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
        if excess > 0 && self.log.is_none() {
            self.log = Some(create_log(root, "shell", |file| {
                let (first, second) = self.tail.as_slices();
                file.write_all(first)?;
                file.write_all(second)
            }));
        }
        if let Some(Ok((_, file))) = &mut self.log
            && let Err(err) = file.write_all(bytes)
        {
            self.log = Some(Err(err));
        }
        self.tail.drain(..excess.min(self.tail.len()));
        self.tail
            .extend(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]);
        let n = bytes.len().min(LIVE_BYTES - self.live_bytes);
        if n > 0 {
            emit(Event::ToolOutput(
                String::from_utf8_lossy(&bytes[..n]).into_owned(),
            ));
            self.live_bytes += n;
        }
    }

    fn finish(
        mut self,
        status: Result<std::process::ExitStatus>,
        cancelled: bool,
    ) -> Result<ToolOutput> {
        let output = String::from_utf8_lossy(self.tail.make_contiguous()).into_owned();
        let truncated = self.log.is_some();
        let (note, full_output_path) = match self.log {
            None => (String::new(), None),
            Some(Ok((path, _))) => (
                format!("\n[Output truncated to the last 64 KiB]\nFull output: {path}"),
                Some(path),
            ),
            Some(Err(e)) => (
                format!("\n[Output truncated; could not save full log: {e}]"),
                None,
            ),
        };
        let outcome = ShellOutcome {
            output,
            exit_code: status.as_ref().ok().and_then(|s| s.code()),
            cancelled,
            truncated,
            full_output_path,
        };
        match status {
            Ok(s) if s.success() && !truncated => Ok(outcome.output.into()),
            Ok(s) if s.success() => Ok(ToolOutput {
                text: format!("command {s}\n{}{note}", outcome.output),
                images: vec![],
                details: Some(ToolDetails::Shell { outcome }),
            }),
            Ok(s) => Err(ShellFailure {
                diagnostic: format!("command {s}\n{}{note}", outcome.output),
                outcome,
            }
            .into()),
            Err(e) if cancelled => Err(ShellFailure {
                diagnostic: format!("{e}\n{}{note}", outcome.output),
                outcome,
            }
            .into()),
            Err(e) => Err(anyhow::anyhow!("{e}\n{}{note}", outcome.output)),
        }
    }
}

pub(super) fn run(
    mut proc: Command,
    root: &Path,
    timeout: Duration,
    cancel: &Cancel,
    emit: &mut Emit,
) -> Result<ToolOutput> {
    let root = open_root(root)?;
    // A single pipe preserves the order of stdout/stderr writes in the log.
    let (mut reader, writer) = io::pipe()?;
    rustix::io::ioctl_fionbio(&reader, true)?;
    cancel.check()?;
    let child = proc
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()?;
    drop(proc);
    let mut capture = Capture::default();
    let mut buf = [0; 16 * 1024];
    let status = wait_group(child, timeout, cancel, || match reader.read(&mut buf) {
        Ok(n) => {
            capture.append(&buf[..n], &root, emit);
            Ok(n > 0)
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err.into()),
    });
    // Include writes made just before exit. Only drain bytes already in the
    // kernel pipe; an inherited descriptor cannot keep this call alive.
    let remaining = rustix::io::ioctl_fionread(&reader)?;
    let mut buffered = reader.take(remaining);
    loop {
        let n = buffered.read(&mut buf)?;
        if n == 0 {
            break;
        }
        capture.append(&buf[..n], &root, emit);
    }
    capture.finish(status, cancel.check().is_err())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn capture_matches_the_byte_stream_across_chunk_boundaries(
            pattern in prop::collection::vec(any::<u8>(), 1..128),
            size in (TAIL_BYTES - 1024)..(TAIL_BYTES * 2),
            chunk in 1usize..8192
        ) {
            let dir = tempfile::tempdir()?;
            let root = File::open(dir.path())?;
            let bytes: Vec<_> = pattern.into_iter().cycle().take(size).collect();
            let mut capture = Capture::default();
            for part in bytes.chunks(chunk) {
                capture.append(part, &root, &mut |_| {});
            }
            prop_assert_eq!(capture.tail.make_contiguous(), &bytes[size.saturating_sub(TAIL_BYTES)..]);
            match capture.log {
                Some(log) => {
                    let (path, _) = log?;
                    prop_assert!(size > TAIL_BYTES);
                    prop_assert_eq!(std::fs::read(dir.path().join(path))?, bytes);
                }
                None => {
                    prop_assert!(size <= TAIL_BYTES);
                    prop_assert!(!dir.path().join(".panda").exists());
                }
            }
        }
    }

    #[test]
    fn log_directory_symlink_cannot_redirect_a_write() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), root.path().join(".panda"))?;
        assert!(create_log(&File::open(root.path())?, "shell", |_| Ok(())).is_err());
        assert_eq!(outside.path().read_dir()?.count(), 0);
        Ok(())
    }
}
