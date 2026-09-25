//! Windows: handle-level reparse-point checks and write-through renames.

use std::fs::{File, OpenOptions};
use std::io;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, Prefix};
use std::process::Child;

use tempfile::NamedTempFile;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub(super) const NO_FOLLOW_OPEN: &str = "open_reparse_point";
pub(super) const DURABLE_PUBLICATION: &str = "movefile_write_through";

/// Whether a repository path component names exactly one directory entry
/// here. Windows reads separators, drive and stream designators and
/// wildcards in a name, strips trailing dots and spaces, maps device names
/// to devices, and lets an 8.3 short name alias another entry (such as
/// `GIT~1` for `.git`), so such names are refused rather than reinterpreted.
pub(crate) fn literal_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|port| {
        stem.strip_prefix(port).is_some_and(|n| {
            let mut n = n.chars();
            matches!(
                (n.next(), n.next()),
                (Some('0'..='9' | '¹' | '²' | '³'), None)
            )
        })
    });
    let short = stem
        .rsplit_once('~')
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    !(name.chars().any(|c| c < ' ' || r#"<>:"/\|?*"#.contains(c))
        || name.ends_with(['.', ' '])
        || device
        || short)
}

/// Whether the executable at `path` runs as itself when spawned. Windows
/// runs a batch script through `cmd.exe`, which reinterprets its arguments,
/// so a `.bat` or `.cmd` file does not.
pub(crate) fn runs_directly(path: &Path) -> bool {
    !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
}

/// Windows distinguishes file from directory symlinks and lets only
/// privileged or developer-mode users create either, so agentctl creates
/// none rather than guess.
pub(crate) fn symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "agentctl does not create symlinks on Windows",
    ))
}

/// Windows cannot ask a process without a console of its own to stop
/// gracefully, so no request is sent; the caller terminates it outright.
pub(crate) fn request_termination(_child: &Child) -> io::Result<bool> {
    Ok(false)
}

/// Opens `path` if it is a regular file; `None` for a symlink, junction or
/// any other reparse point, a directory, or another entry. The final
/// component is opened itself, never followed, and classified by handle.
pub(crate) fn open_regular(path: &Path) -> io::Result<Option<File>> {
    // Backup semantics lets a directory open, so it is classified rather
    // than failing as access denied.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let meta = file.metadata()?;
    // A reparse point that is not a link (a cloud placeholder, say) is still
    // refused: its filter was bypassed, so its bytes are not its content.
    let regular = meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    Ok(regular.then_some(file))
}

/// A new file in `dir`, created with inherited permissions and not marked
/// temporary, removed unless published.
pub(crate) fn stage(dir: &Path) -> io::Result<NamedTempFile> {
    tempfile::Builder::new().make_in(dir, |path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    })
}

/// Writes `file`, and the device's cache of it, to stable storage.
pub(crate) fn write_back(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Moves `staged` to `dest` only if nothing exists there, atomically;
/// `AlreadyExists` otherwise. Durable when this returns.
pub(crate) fn publish_new(staged: NamedTempFile, dest: &Path) -> io::Result<()> {
    rename(staged, dest, MOVEFILE_WRITE_THROUGH)
}

/// Atomically replaces the file or symlink entry at `dest` with `staged`,
/// never a directory. Durable when this returns. A read-only `dest`, or one
/// open without delete sharing, is refused.
pub(crate) fn replace(staged: NamedTempFile, dest: &Path) -> io::Result<()> {
    rename(
        staged,
        dest,
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
}

/// Publication and replacement are written through before they return, so
/// there is nothing left to flush for them. Windows offers no unprivileged
/// flush of a directory, so a removal is durable only once the filesystem
/// commits its journal.
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn rename(mut staged: NamedTempFile, dest: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
    let (from, to) = (wide(staged.path()), wide(dest));
    // SAFETY: both are NUL-terminated UTF-16 paths that outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // The name is now `dest`'s; dropping must not remove it.
    staged.disable_cleanup(true);
    Ok(())
}

/// `path` as a NUL-terminated wide string. Absolute paths are made verbatim
/// (`\\?\`), lifting the `MAX_PATH` limit as the standard library does;
/// agentctl builds them from components it has already validated.
fn wide(path: &Path) -> Vec<u16> {
    let raw = path.as_os_str().encode_wide();
    let verbatim: &[u16] = match path.components().next() {
        Some(Component::Prefix(prefix)) if path.has_root() => match prefix.kind() {
            Prefix::Disk(_) => &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16],
            Prefix::UNC(..) => {
                let unc: Vec<u16> = r"\\?\UNC".encode_utf16().chain(raw.skip(1)).collect();
                return unc.into_iter().chain(iter::once(0)).collect();
            }
            _ => &[],
        },
        _ => &[],
    };
    verbatim
        .iter()
        .copied()
        .chain(raw)
        .chain(iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::symlink_file;

    #[test]
    fn final_symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"secret").unwrap();
        // Requires the symlink privilege or developer mode.
        symlink_file(&target, dir.path().join("link")).unwrap();
        symlink_file(dir.path().join("absent"), dir.path().join("dangling")).unwrap();
        assert!(open_regular(&dir.path().join("link")).unwrap().is_none());
        assert!(
            open_regular(&dir.path().join("dangling"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refuses_names_windows_would_reinterpret() {
        for name in [
            "back\\slash.rs",
            "C:x",
            "a.rs:stream",
            "*.rs",
            "q?",
            "trailing.",
            "trailing ",
            "nul",
            "Con.txt",
            "com1",
            "LPT²",
            "GIT~1",
            "AGENTC~12.x",
            "tab\t",
        ] {
            assert!(!literal_name(name), "{name}");
        }
        for name in [
            ".git",
            "[id].ts",
            "(group)",
            "with space.rs",
            "a~b",
            "console",
            "com10",
        ] {
            assert!(literal_name(name), "{name}");
        }
    }

    #[test]
    fn batch_scripts_do_not_run_directly() {
        for path in [r"C:\bin\codex.cmd", r"C:\bin\codex.CMD", r"x.Bat"] {
            assert!(!runs_directly(Path::new(path)), "{path}");
        }
        for path in [r"C:\bin\codex.exe", r"C:\bin\claude", r"a.cmd.exe"] {
            assert!(runs_directly(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn verbatim_paths_lift_max_path() {
        let text = |p: &str| String::from_utf16(&wide(Path::new(p))).unwrap();
        assert_eq!(text(r"C:\a\b"), "\\\\?\\C:\\a\\b\0");
        assert_eq!(text(r"\\server\share\a"), "\\\\?\\UNC\\server\\share\\a\0");
        assert_eq!(text(r"\\?\C:\a"), "\\\\?\\C:\\a\0");
        assert_eq!(text(r"relative\a"), "relative\\a\0");
    }
}
