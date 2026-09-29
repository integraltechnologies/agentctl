//! Sessions: which agentctl process recorded work that may still be live,
//! and whether that process still runs.
//!
//! A process records an invocation, a journaled action, a scheduler claim
//! or an acceptance under its session: an identity recorded once, the
//! first time the process writes to the store, together with a lock file
//! of the same name beside the store, which the process holds exclusively
//! for as long as it runs. The operating system gives that lock up when
//! the process ends, however it ends, so taking it proves the session
//! ended: nothing it recorded is still being worked on, and whatever it
//! left unresolved is for recovery to settle (see `crate::recovery`).
//! Whether a process id is in use never says anything about a session.

use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};

use super::{Store, now};

/// This process's session in each store it wrote to, by the store's
/// canonical path, with the lock it holds until the process ends.
static SESSIONS: Mutex<Vec<(PathBuf, String, File)>> = Mutex::new(Vec::new());

/// Whether the process of a session still runs.
#[derive(Debug)]
pub(crate) enum Liveness {
    /// It does: what it recorded is its own work.
    Running,
    /// It ended, as holding its lock proves, for as long as this is held.
    Ended(Ended),
    /// Neither can be established, for the reason given.
    Unknown(String),
}

/// Proof that a session's process ended: its lock, held until dropped, so
/// that no other recovery acts on the session meanwhile.
#[derive(Debug)]
pub struct Ended {
    id: String,
    path: PathBuf,
    _lock: File,
}

impl Ended {
    pub fn session(&self) -> &str {
        &self.id
    }

    /// Removes the session's lock file, once nothing it recorded is
    /// unresolved: no one asks about the session again.
    pub(crate) fn retire(self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 128 bits from the operating system's randomness, as lowercase hex:
/// unique, never secret.
fn random_hex() -> String {
    let word = |salt: u64| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", word(1), word(2))
}

/// Where the lock files of the sessions of the store at `store` are.
fn directory(store: &Path) -> PathBuf {
    store.with_file_name("sessions")
}

/// This process's session in the store at `store`, reached through
/// `conn`: recorded, with its lock taken, the first time it is needed.
pub(super) fn own(store: &Path, conn: &Connection) -> Result<String> {
    let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, id, _)) = sessions.iter().find(|(path, ..)| path == store) {
        return Ok(id.clone());
    }
    let dir = directory(store);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let id = random_hex();
    let path = dir.join(&id);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating session lock {}", path.display()))?;
    let locked = lock.try_lock().map_err(|e| match e {
        TryLockError::Error(e) => e,
        TryLockError::WouldBlock => io::Error::other("it is locked already"),
    });
    let recorded = locked.map_err(anyhow::Error::from).and_then(|()| {
        conn.execute(
            "INSERT INTO sessions (id, started_at) VALUES (?1, ?2)",
            params![id, now()],
        )?;
        Ok(())
    });
    if let Err(e) = recorded {
        let _ = fs::remove_file(&path);
        return Err(e.context(format!("starting session {id}")));
    }
    sessions.push((store.to_path_buf(), id.clone(), lock));
    Ok(id)
}

impl Store {
    /// Whether the process of `session` still runs, as its lock alone
    /// establishes. This process's own session runs.
    pub(crate) fn liveness(&self, session: &str) -> Result<Liveness> {
        ensure!(
            session.len() == 32 && session.bytes().all(|b| b.is_ascii_hexdigit()),
            "`{session}` is not a session"
        );
        if self.session.as_deref() == Some(session) {
            return Ok(Liveness::Running);
        }
        let path = directory(&self.path).join(session);
        let lock = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Liveness::Unknown(format!(
                    "the lock of session {session} is missing, so whether its process \
                     still runs cannot be established"
                )));
            }
            Err(e) => return Err(e).context(format!("opening {}", path.display())),
        };
        Ok(match lock.try_lock() {
            Ok(()) => Liveness::Ended(Ended {
                id: session.to_owned(),
                path,
                _lock: lock,
            }),
            Err(TryLockError::WouldBlock) => Liveness::Running,
            Err(TryLockError::Error(e)) => Liveness::Unknown(format!(
                "the lock of session {session} cannot be taken: {e}"
            )),
        })
    }
}
