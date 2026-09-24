//! macOS and Linux: POSIX primitives, identical in semantics on both.

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use tempfile::NamedTempFile;

pub(super) const NO_FOLLOW_OPEN: &str = "o_nofollow";
pub(super) const DURABLE_PUBLICATION: &str = if cfg!(target_vendor = "apple") {
    "rename_f_fullfsync_directory"
} else {
    "rename_fsync_directory"
};

/// Whether a repository path component names exactly one directory entry
/// here: on Unix, every name without `/` or NUL does.
pub(crate) fn literal_name(_name: &str) -> bool {
    true
}

/// Opens `path` if it is a regular file; `None` for a symlink, directory or
/// other entry. The final component is never followed.
pub(crate) fn open_regular(path: &Path) -> io::Result<Option<File>> {
    // Non-blocking, so an entry swapped for a FIFO cannot stall the open.
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    match opened {
        Ok(file) => Ok(file.metadata()?.is_file().then_some(file)),
        // `O_NOFOLLOW` refuses a symlink.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A new file in `dir`, created as an ordinary file would be (subject to
/// the umask), removed unless published.
pub(crate) fn stage(dir: &Path) -> io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o666))
        .tempfile_in(dir)
}

/// Writes `file` to its device: durable, except that Apple platforms may hold
/// it in the drive's cache until [`sync_dir`]'s full flush. One flush per
/// batch instead of per file keeps publishing many files fast.
pub(crate) fn write_back(file: &File) -> io::Result<()> {
    // SAFETY: the descriptor is owned by `file` and open for this call.
    check(|| unsafe { libc::fsync(file.as_raw_fd()) })
}

/// Moves `staged` to `dest` only if nothing exists there, atomically;
/// `AlreadyExists` otherwise. Durable after [`sync_dir`] on its directory.
pub(crate) fn publish_new(staged: NamedTempFile, dest: &Path) -> io::Result<()> {
    staged
        .persist_noclobber(dest)
        .map(drop)
        .map_err(|e| e.error)
}

/// Atomically replaces the file or symlink entry at `dest` with `staged`,
/// never a directory. Durable after [`sync_dir`] on its directory.
pub(crate) fn replace(staged: NamedTempFile, dest: &Path) -> io::Result<()> {
    staged.persist(dest).map(drop).map_err(|e| e.error)
}

/// Makes entries published, replaced or removed in `dir` durable. On Apple
/// platforms this is `F_FULLFSYNC`, which also flushes the drive's cache of
/// everything [`write_back`] wrote to it.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    full_sync(&File::open(dir)?)
}

/// `fsync`, except on Apple platforms, where `fsync` may stop at the drive's
/// cache: there `F_FULLFSYNC`, which flushes that cache to stable storage.
fn full_sync(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: the descriptor is owned by `file` and open for this call.
    #[cfg(target_vendor = "apple")]
    return check(|| unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) });
    #[cfg(not(target_vendor = "apple"))]
    return check(|| unsafe { libc::fsync(fd) });
}

/// Runs a call that returns 0 on success and sets `errno` otherwise,
/// retrying if a signal interrupts it.
fn check(call: impl Fn() -> libc::c_int) -> io::Result<()> {
    loop {
        if call() == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    #[test]
    fn final_symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, dir.path().join("link")).unwrap();
        symlink(dir.path().join("absent"), dir.path().join("dangling")).unwrap();
        assert!(open_regular(&dir.path().join("link")).unwrap().is_none());
        assert!(
            open_regular(&dir.path().join("dangling"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_durability_barrier_is_full_fsync_only_on_apple() {
        // On macOS `/dev/null` accepts `fsync` but refuses `F_FULLFSYNC`.
        let null = File::open("/dev/null").unwrap();
        // SAFETY: the descriptor is owned by `null` and open for this call.
        let plain = check(|| unsafe { libc::fsync(null.as_raw_fd()) });
        let full = full_sync(&null);
        if cfg!(target_vendor = "apple") {
            plain.unwrap();
            // The refusal surfaces rather than degrading to `fsync`.
            assert!(full.is_err());
        } else {
            assert_eq!(
                full.map_err(|e| e.raw_os_error()),
                plain.map_err(|e| e.raw_os_error())
            );
        }
        let dir = tempfile::tempdir().unwrap();
        sync_dir(dir.path()).unwrap();
    }

    #[test]
    fn a_fifo_is_not_a_file_and_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(open_regular(&fifo).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
