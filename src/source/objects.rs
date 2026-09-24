//! Content-addressed recovery objects (`.agentctl/objects/`).
//!
//! An object is a file named by the lowercase hex SHA-256 of its bytes. It
//! appears under that name only once complete, and is never replaced. The
//! store holds bytes only: which content is accepted where is canonical state
//! in the `Store`.

use std::fs::{self, File, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::state::check_hash;

pub(super) struct Objects {
    dir: PathBuf,
    staging: PathBuf,
}

impl Objects {
    /// Opens the store within the state directory, creating it if absent.
    pub(super) fn open(state: &Path) -> Result<Self> {
        let objects = Self {
            dir: state.join("objects"),
            staging: state.join("tmp"),
        };
        for dir in [&objects.dir, &objects.staging] {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(objects)
    }

    /// A private file, on the state directory's filesystem, for content that
    /// is renamed into place once complete. Abandoned files never become
    /// objects or source.
    pub(super) fn stage(&self) -> Result<NamedTempFile> {
        tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(&self.staging)
            .with_context(|| format!("staging in {}", self.staging.display()))
    }

    /// Copies `source` into the store and returns its hash. A new object is
    /// durable only after [`Objects::sync`]; an existing one is reused once
    /// verified, never replaced.
    pub(super) fn publish(&self, source: impl Read) -> Result<String> {
        let mut staged = self.stage()?;
        let hash = copy(source, staged.as_file_mut())?;
        let path = self.path(&hash)?;
        if !path.try_exists()? {
            fsync(staged.as_file())?;
            match staged.persist_noclobber(&path) {
                Ok(_) => return Ok(hash),
                Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
        }
        self.copy_to(&hash, &mut io::sink())?;
        Ok(hash)
    }

    /// Makes every object published so far durable. Canonical state may
    /// reference an object only after this.
    pub(super) fn sync(&self) -> Result<()> {
        // On Apple platforms this is `F_FULLFSYNC`, which also flushes the
        // drive's cache of every object `fsync` already wrote to it.
        sync_dir(&self.dir)
    }

    /// Writes the bytes of object `hash` to `out`, failing unless they hash
    /// to `hash`. On failure `out` holds unverified bytes and must be
    /// discarded.
    pub(super) fn copy_to(&self, hash: &str, out: &mut impl Write) -> Result<()> {
        let path = self.path(hash)?;
        let file =
            File::open(&path).with_context(|| format!("recovery object {hash} is unavailable"))?;
        ensure!(
            copy(file, out)? == hash,
            "recovery object {hash} is corrupt"
        );
        Ok(())
    }

    fn path(&self, hash: &str) -> Result<PathBuf> {
        check_hash(hash)?;
        Ok(self.dir.join(hash))
    }
}

/// The content hash of everything `source` reads.
pub(super) fn hash(source: impl Read) -> Result<String> {
    copy(source, &mut io::sink())
}

/// Copies `from` to `to`, returning the content hash of the bytes copied.
fn copy(mut from: impl Read, to: &mut impl Write) -> Result<String> {
    let mut hashing = Hashing(Sha256::new(), to);
    io::copy(&mut from, &mut hashing)?;
    Ok(hashing
        .0
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

struct Hashing<W>(Sha256, W);

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.1.write(buf)?;
        self.0.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}

/// Writes `file` to its device: durable, except that Apple platforms may hold
/// it in the drive's cache until a full flush such as [`sync_dir`]. One flush
/// per batch instead of per object keeps publishing many objects fast.
fn fsync(file: &File) -> io::Result<()> {
    // SAFETY: the descriptor is owned by `file` and open for this call.
    match unsafe { libc::fsync(file.as_raw_fd()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Makes entries created or removed in `dir` durable.
pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))
}
