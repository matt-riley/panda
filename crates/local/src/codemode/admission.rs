use super::*;
use rustix::fs::{Mode as Permissions, OFlags, mkdirat, open, openat};
use std::{fs::File, os::unix::fs::MetadataExt};

/// Kernel locks survive independent CLI processes and are released on process death.
pub(super) fn acquire(cancel: &Cancel, deadline: Instant) -> Result<File> {
    acquire_in(&std::env::temp_dir(), cancel, deadline)
}
fn acquire_in(root: &Path, cancel: &Cancel, deadline: Instant) -> Result<File> {
    let temp = open(
        root,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Permissions::empty(),
    )?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let name = format!("panda-codemode-{uid}");
    match mkdirat(&temp, name.as_str(), Permissions::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    let dir = File::from(openat(
        &temp,
        name.as_str(),
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY,
        Permissions::empty(),
    )?);
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == uid && meta.mode() & 0o077 == 0,
        "Codemode admission directory must be private"
    );
    let deadline = deadline.min(Instant::now() + Duration::from_secs(3));
    loop {
        cancel.check()?;
        ensure!(
            Instant::now() < deadline,
            "Codemode worker admission timed out"
        );
        for slot in 0..4 {
            let f = File::from(openat(
                &dir,
                slot.to_string(),
                OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Permissions::RUSR | Permissions::WUSR,
            )?);
            match f.try_lock() {
                Ok(()) => return Ok(f),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_admission_releases_slots_and_observes_cancellation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path();
        let deadline = Instant::now() + Duration::from_secs(3);
        let cancel = Cancel::default();
        let mut slots = (0..4)
            .map(|_| acquire_in(path, &cancel, deadline))
            .collect::<Result<Vec<_>>>()?;
        let err =
            acquire_in(path, &cancel, Instant::now() + Duration::from_millis(30)).unwrap_err();
        assert!(err.to_string().contains("admission timed out"));
        std::thread::scope(|s| -> Result<()> {
            let cancelled = Cancel::default();
            let token = cancelled.clone();
            let waiting = s.spawn(move || acquire_in(path, &token, deadline));
            let other = s.spawn(|| acquire_in(path, &cancel, deadline));
            cancelled.stop();
            assert!(
                waiting
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled")
            );
            assert!(!other.is_finished());
            drop(slots.pop());
            drop(other.join().unwrap()?);
            Ok(())
        })
    }
}
