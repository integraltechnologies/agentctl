use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Error, Result, now_ms, require};

/// Logical local repository identity, derived from the canonical Git common directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepositoryId(String);

impl RepositoryId {
    pub fn for_common_directory(path: &Path) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"agentctl-local-git-v1\0");
        digest.update(path.as_os_str().as_encoded_bytes());
        Self(format!("repo-{:x}", digest.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RepositoryId {
    type Error = String;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.len() != 69
            || !value.starts_with("repo-")
            || !value[5..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("repository ID must be repo- followed by 64 lowercase hex digits".into());
        }
        Ok(Self(value))
    }
}

impl From<RepositoryId> for String {
    fn from(value: RepositoryId) -> Self {
        value.0
    }
}

/// Concrete checkout identity, independent of repository-level ownership.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn for_git_directory(path: &Path) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"agentctl-workspace-v1\0");
        digest.update(path.as_os_str().as_encoded_bytes());
        Self(format!("workspace-{:x}", digest.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WorkspaceId {
    type Error = String;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let valid = value.strip_prefix("workspace-").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if !valid {
            return Err(
                "workspace ID must be workspace- followed by 64 lowercase hex digits".into(),
            );
        }
        Ok(Self(value))
    }
}

impl From<WorkspaceId> for String {
    fn from(value: WorkspaceId) -> Self {
        value.0
    }
}

/// This is a local observation envelope; the accepted SourceStateRef wire type is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySourceState {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub head_commit: Option<String>,
    pub dirty: bool,
    /// Always None in Stage 1. Even clean Git status is not a whole-filesystem fingerprint.
    pub worktree_fingerprint: Option<String>,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInfo {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub root: PathBuf,
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
    pub git_directory_identity: Option<DirectoryIdentity>,
    pub common_directory_identity: Option<DirectoryIdentity>,
    pub remotes: BTreeMap<String, Vec<String>>,
    pub source: RepositorySourceState,
}

/// Diagnostic replacement detection on Unix; not a distributed identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

impl RepositoryInfo {
    pub fn discover(start: &Path) -> Result<Self> {
        let start = fs::canonicalize(start)
            .map_err(|e| Error::Invalid(format!("{}: {e}", start.display())))?;
        let start = if start.is_file() {
            start.parent().expect("canonical file parent")
        } else {
            &start
        };
        let root = canonical_git_path(start, &["rev-parse", "--show-toplevel"])?;
        let git_directory = canonical_git_path(&root, &["rev-parse", "--absolute-git-dir"])?;
        let common_directory = canonical_git_path(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let repository_id = RepositoryId::for_common_directory(&common_directory);
        let workspace_id = WorkspaceId::for_git_directory(&git_directory);
        let head = git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        let head_commit = if head.status.success() {
            Some(text(&head.stdout)?.trim_end_matches('\n').to_owned())
        } else {
            // A symbolic HEAD with no branch ref is an unborn repository. Other failures are errors.
            let symbolic = git(&root, &["symbolic-ref", "-q", "HEAD"])?;
            require(symbolic.status.success(), git_failure(&root, &head))?;
            let reference = text(&symbolic.stdout)?.trim_end_matches('\n').to_owned();
            let exists = git(&root, &["show-ref", "--verify", "--quiet", &reference])?;
            require(exists.status.code() == Some(1), git_failure(&root, &head))?;
            None
        };
        let status = checked_git(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )?;
        let remote_config = git(
            &root,
            &[
                "config",
                "--local",
                "--null",
                "--get-regexp",
                "^remote\\..*\\.url$",
            ],
        )?;
        require(
            remote_config.status.success() || remote_config.status.code() == Some(1),
            git_failure(&root, &remote_config),
        )?;
        let mut remotes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in remote_config
            .stdout
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry = text(entry)?;
            let (key, url) = entry
                .split_once('\n')
                .ok_or_else(|| Error::Invalid("malformed Git remote metadata".into()))?;
            let name = key
                .strip_prefix("remote.")
                .and_then(|v| v.strip_suffix(".url"))
                .ok_or_else(|| Error::Invalid("malformed Git remote name".into()))?;
            remotes.entry(name.into()).or_default().push(url.into());
        }
        let source = RepositorySourceState {
            repository_id: repository_id.clone(),
            workspace_id: workspace_id.clone(),
            head_commit,
            dirty: !status.stdout.is_empty(),
            worktree_fingerprint: None,
            observed_at_ms: now_ms()?,
        };
        let git_directory_identity = directory_identity(&git_directory)?;
        let common_directory_identity = directory_identity(&common_directory)?;
        Ok(Self {
            repository_id,
            workspace_id,
            root,
            git_directory,
            common_directory,
            git_directory_identity,
            common_directory_identity,
            remotes,
            source,
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for path in [&self.root, &self.git_directory, &self.common_directory] {
            super::paths::absolute_path(path)?;
        }
        require(
            self.repository_id == RepositoryId::for_common_directory(&self.common_directory)
                && self.workspace_id == WorkspaceId::for_git_directory(&self.git_directory),
            "repository/workspace identity does not match its Git directory",
        )?;
        require(
            self.source.repository_id == self.repository_id
                && self.source.workspace_id == self.workspace_id
                && self.source.worktree_fingerprint.is_none(),
            "invalid Stage 1 source-state envelope",
        )
    }
}

fn directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path)?;
        Ok(Some(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn canonical_git_path(root: &Path, args: &[&str]) -> Result<PathBuf> {
    let output = checked_git(root, args)?;
    let value = text(&output.stdout)?
        .strip_suffix('\n')
        .unwrap_or(text(&output.stdout)?);
    let path =
        fs::canonicalize(value).map_err(|e| Error::Invalid(format!("Git path {value:?}: {e}")))?;
    require(
        path.to_str().is_some(),
        "Stage 1 repository metadata requires UTF-8 filesystem paths",
    )?;
    Ok(path)
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|e| Error::Invalid(format!("non-UTF-8 Git metadata: {e}")))
}

fn git(root: &Path, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(args);
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(variable);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| Error::Invalid(format!("could not run Git for {}: {e}", root.display())))
}

fn checked_git(root: &Path, args: &[&str]) -> Result<Output> {
    let output = git(root, args)?;
    require(output.status.success(), git_failure(root, &output))?;
    Ok(output)
}

fn git_failure(root: &Path, output: &Output) -> String {
    format!(
        "Git discovery in {} failed: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}
