use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, unlinkat};
use std::{fs::File, io, path::Path};

pub(crate) fn open_root(root: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = File::options();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    #[cfg(target_os = "macos")]
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY);
    options.open(root)
}

// The sandbox denies writes to .panda. The host must also avoid following
// workspace-controlled symlinks while creating a log outside the child.
pub(crate) fn create_log(
    root: &File,
    prefix: &str,
    initialize: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<(String, File)> {
    match mkdirat(root, ".panda", Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(err) => return Err(err.into()),
    }
    let dir = openat(
        root,
        ".panda",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    loop {
        let name = format!("{prefix}-{:016x}.log", fastrand::u64(..));
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&dir, name.as_str(), flags, Mode::RUSR | Mode::WUSR) {
            Ok(fd) => {
                let mut file = File::from(fd);
                if let Err(err) = initialize(&mut file) {
                    let _ = unlinkat(&dir, name.as_str(), AtFlags::empty());
                    return Err(err);
                }
                return Ok((format!(".panda/{name}"), file));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(err) => return Err(err.into()),
        }
    }
}
