use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::Serialize;

use super::repository::RepositoryId;
use super::{Error, Result, require};
use crate::protocol::EvidenceId;

/// Explicit input keeps path tests independent of process-global environment variables.
#[derive(Debug, Clone, Default)]
pub struct PathContext {
    pub home: Option<PathBuf>,
    pub config_home: Option<PathBuf>,
    pub data_home: Option<PathBuf>,
    pub cache_home: Option<PathBuf>,
}

impl PathContext {
    pub fn from_env() -> Self {
        Self {
            home: env::var_os("HOME").map(PathBuf::from),
            config_home: env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            data_home: env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            cache_home: env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachinePaths {
    pub config_root: PathBuf,
    pub machine_config: PathBuf,
    pub data_root: PathBuf,
    pub database: PathBuf,
    pub cache_root: PathBuf,
}

impl MachinePaths {
    pub fn resolve(context: &PathContext) -> Result<Self> {
        let resolve = |override_path: &Option<PathBuf>, fallback: &str| -> Result<PathBuf> {
            let base = if let Some(path) = override_path.as_ref().filter(|p| p.is_absolute()) {
                path.clone()
            } else {
                let home = context
                    .home
                    .as_ref()
                    .filter(|p| p.is_absolute())
                    .ok_or_else(|| {
                        Error::Invalid("an absolute HOME or absolute XDG paths are required".into())
                    })?;
                home.join(fallback)
            };
            absolute_path(&base)?;
            Ok(base.join("agentctl"))
        };
        let config_root = resolve(&context.config_home, ".config")?;
        let data_root = resolve(&context.data_home, ".local/share")?;
        let cache_root = resolve(&context.cache_home, ".cache")?;
        Ok(Self {
            machine_config: config_root.join("config.toml"),
            database: data_root.join("state.sqlite3"),
            config_root,
            data_root,
            cache_root,
        })
    }

    pub fn create_directories(&self) -> Result<()> {
        for dir in [&self.config_root, &self.data_root, &self.cache_root] {
            ensure_directory(dir)?;
        }
        Ok(())
    }

    /// Convention only: this function does not create or capture artifacts.
    pub fn evidence_directory(&self, repo: &RepositoryId, evidence: &EvidenceId) -> PathBuf {
        self.data_root
            .join("artifacts")
            .join(repo.as_str())
            .join(evidence.as_str())
    }

    pub fn repository_cache(&self, repo: &RepositoryId) -> PathBuf {
        self.cache_root.join(repo.as_str())
    }
}

pub fn project_config(root: &Path) -> PathBuf {
    root.join(".agentctl/project.toml")
}

pub(crate) fn absolute_path(path: &Path) -> Result<()> {
    require(
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        format!(
            "{}: expected an absolute path without traversal",
            path.display()
        ),
    )
}

pub(crate) fn ensure_directory(path: &Path) -> Result<()> {
    absolute_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => require(
            meta.is_dir() && !meta.file_type().is_symlink(),
            format!(
                "{}: expected a directory, refusing symlink/non-directory",
                path.display()
            ),
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
            let meta = fs::symlink_metadata(path)?;
            require(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "directory changed during creation",
            )?;
        }
        Err(e) => return Err(Error::Invalid(format!("{}: {e}", path.display()))),
    }
    Ok(())
}

pub(crate) fn check_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    require(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        format!("{}: refusing symlink/non-directory", path.display()),
    )
}

/// Reject symlinks and special files at owned leaf paths, including SQLite sidecars.
pub(crate) fn check_file(path: &Path, allow_missing: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            require(
                meta.is_file() && !meta.file_type().is_symlink(),
                format!("{}: refusing symlink or non-regular file", path.display()),
            )?;
            Ok(())
        }
        Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Invalid(format!("{}: {e}", path.display()))),
    }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
}

pub(crate) fn read_text(path: &Path) -> Result<String> {
    check_file(path, false)?;
    let file = options()
        .read(true)
        .open(path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    require(
        file.metadata()?.len() <= 1024 * 1024,
        format!("{}: config exceeds 1 MiB", path.display()),
    )?;
    let mut value = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut value)?;
    require(value.len() <= 1024 * 1024, "config exceeds 1 MiB")?;
    Ok(value)
}

/// Publish a fully written default without replacing an existing policy file.
pub(crate) fn create_default(path: &Path, content: &str) -> Result<bool> {
    check_file(path, true)?;
    if path.exists() {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("config needs a parent directory".into()))?;
    ensure_directory(parent)?;
    let mut attempt = 0_u32;
    let (temp, mut file) = loop {
        let temp = parent.join(format!(".agentctl-init-{}-{attempt}", std::process::id()));
        match options().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1
            }
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| -> Result<bool> {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        match fs::hard_link(&temp, path) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    })();
    fs::remove_file(&temp)?;
    File::open(parent)?.sync_all()?;
    result
}

pub(crate) fn create_database_file(path: &Path) -> Result<()> {
    check_file(path, true)?;
    match options().write(true).create_new(true).open(path) {
        Ok(file) => {
            file.sync_all()?;
            File::open(path.parent().expect("absolute database path"))?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => check_file(path, false)?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
