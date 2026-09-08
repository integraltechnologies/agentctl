use super::{Language, content_hash};
use crate::local::{Error, Result, config::ProjectConfig, require};
use ignore::WalkBuilder;
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
};

pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_FILES: usize = 20_000;

/// Deterministic, workspace-local discovery. Never traverses symlink directories.
pub fn discover(root: &Path) -> Result<Vec<String>> {
    let protected = if root.join(".agentctl").try_exists()? {
        ProjectConfig::load(root)?
            .protected
            .into_iter()
            .filter(|p| p.deny_read)
            .map(|p| p.path)
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    let base = root.to_path_buf();
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .follow_links(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_str().unwrap_or("");
            if name == ".git" || name == ".agentctl" {
                return false;
            }
            let relative = entry
                .path()
                .strip_prefix(&base)
                .ok()
                .and_then(Path::to_str)
                .unwrap_or("");
            if protected.iter().any(|p| {
                relative == p || relative.strip_prefix(p).is_some_and(|s| s.starts_with('/'))
            }) {
                return false;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if [
                    "target",
                    "node_modules",
                    ".venv",
                    "vendor",
                    "dist",
                    "build",
                    "__pycache__",
                ]
                .contains(&name)
                {
                    return false;
                }
                if entry.path().join(".git").exists() {
                    return false;
                }
            }
            !entry.file_type().is_some_and(|t| t.is_symlink())
        });
    let mut paths = vec![];
    for (visited, entry) in walk.build().enumerate() {
        require(
            visited < 100_000,
            "index discovery exceeded 100000 entries; narrow the repository using .gitignore",
        )?;
        let entry = entry.map_err(|e| Error::Invalid(format!("index discovery failed: {e}")))?;
        if let Some(error) = entry.error() {
            return Err(Error::Invalid(format!(
                "index ignore policy could not be read: {error}"
            )));
        }
        require(
            entry.depth() <= 64,
            "index discovery exceeded directory depth 64",
        )?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let path = relative
                .to_str()
                .ok_or_else(|| Error::Invalid("index requires UTF-8 paths".into()))?
                .to_string();
            if Language::for_path(&path).is_some() {
                crate::validation::repo_path(&path)?;
                require(
                    paths.len() < MAX_FILES,
                    "index exceeds 20000 supported files",
                )?;
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn read(root: &Path, path: &str) -> Result<(String, String)> {
    crate::validation::repo_path(path)?;
    let mut absolute = root.to_path_buf();
    for component in Path::new(path).components() {
        absolute.push(component);
        require(
            !fs::symlink_metadata(&absolute)?.file_type().is_symlink(),
            "index refuses symlink source paths",
        )?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(absolute)?;
    let metadata = file.metadata()?;
    require(metadata.is_file(), "index source is not a regular file")?;
    require(
        metadata.len() <= MAX_FILE_BYTES,
        "source exceeds 2 MiB indexing limit",
    )?;
    let mut bytes = vec![];
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "source exceeds 2 MiB indexing limit",
    )?;
    let hash = content_hash(&bytes);
    require(!bytes.contains(&0), "binary/NUL content is not indexable")?;
    let source = String::from_utf8(bytes)
        .map_err(|_| Error::Invalid("non-UTF-8 source is not indexable".into()))?;
    Ok((hash, source))
}
