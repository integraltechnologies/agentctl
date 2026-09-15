use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
        // rev-parse runs no filters/hooks; everything after uses the fully
        // neutralized configuration discovered for this repository.
        let root = canonical_git_path(&Git::base(start), &["rev-parse", "--show-toplevel"])?;
        let git = Git::hardened(&root)?;
        let git_directory = canonical_git_path(&git, &["rev-parse", "--absolute-git-dir"])?;
        let common_directory = canonical_git_path(
            &git,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let repository_id = RepositoryId::for_common_directory(&common_directory);
        let workspace_id = WorkspaceId::for_git_directory(&git_directory);
        let head = git.run(&["rev-parse", "--verify", "HEAD^{commit}"])?;
        let head_commit = if head.status.success() {
            Some(text(&head.stdout)?.trim_end_matches('\n').to_owned())
        } else {
            // A symbolic HEAD with no branch ref is an unborn repository. Other failures are errors.
            let symbolic = git.run(&["symbolic-ref", "-q", "HEAD"])?;
            require(symbolic.status.success(), git_failure(&root, &head))?;
            let reference = text(&symbolic.stdout)?.trim_end_matches('\n').to_owned();
            let exists = git.run(&["show-ref", "--verify", "--quiet", &reference])?;
            require(exists.status.code() == Some(1), git_failure(&root, &head))?;
            None
        };
        let status = git.checked(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ])?;
        let remote_config = git.run(&[
            "config",
            "--local",
            "--null",
            "--get-regexp",
            "^remote\\..*\\.url$",
        ])?;
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

fn canonical_git_path(git: &Git, args: &[&str]) -> Result<PathBuf> {
    let output = git.checked(args)?;
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

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// Filter/driver keys whose values name programs Git would execute.
const DRIVER_KEYS: &str =
    r"^(filter|diff|merge)\..+\.(clean|smudge|process|required|textconv|command|driver)$";

/// agentctl's own Git subprocesses. Every inherited `GIT_*` variable is dropped;
/// hooks, fsmonitor and global attributes are disabled; and every clean/smudge/
/// process filter and diff/merge driver defined in ANY config scope (system,
/// global, local, worktree, includes) is overridden to a no-op, so attributes in
/// a repository cannot make `git status` run repository- or user-configured
/// programs. Overrides travel through `GIT_CONFIG_COUNT` rather than `-c`, so
/// arbitrary subsection names cannot be misparsed, and they propagate to child
/// Git processes (e.g. submodule status).
struct Git {
    root: PathBuf,
    overrides: Vec<(String, String)>,
}

impl Git {
    fn base(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            overrides: vec![],
        }
    }

    fn hardened(root: &Path) -> Result<Self> {
        refuse_privileged(root)?;
        let base = Self::base(root);
        let listed = base.run(&[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            DRIVER_KEYS,
        ])?;
        require(
            listed.status.success() || listed.status.code() == Some(1),
            git_failure(root, &listed),
        )?;
        let mut filters = BTreeSet::new();
        let mut overrides = vec![];
        for key in listed.stdout.split(|b| *b == 0).filter(|k| !k.is_empty()) {
            let key = text(key)?.trim_end_matches('\n');
            let Some((section, rest)) = key.split_once('.') else {
                continue;
            };
            let Some((name, _)) = rest.rsplit_once('.') else {
                continue;
            };
            match section.to_ascii_lowercase().as_str() {
                "filter" => {
                    filters.insert(name.to_owned());
                }
                "diff" => {
                    overrides.push((format!("diff.{name}.textconv"), String::new()));
                    overrides.push((format!("diff.{name}.command"), String::new()));
                }
                "merge" => overrides.push((format!("merge.{name}.driver"), String::new())),
                _ => {}
            }
        }
        for name in filters {
            for variable in ["clean", "smudge", "process"] {
                overrides.push((format!("filter.{name}.{variable}"), String::new()));
            }
            overrides.push((format!("filter.{name}.required"), "false".into()));
        }
        Ok(Self {
            root: root.to_path_buf(),
            overrides,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.command(args).output().map_err(|e| {
            Error::Invalid(format!(
                "could not run Git for {}: {e}",
                self.root.display()
            ))
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .args(["--no-optional-locks", "--no-pager", "-C"])
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        let fixed = [
            ("core.fsmonitor", "false"),
            ("core.hooksPath", NULL_DEVICE),
            ("core.attributesFile", NULL_DEVICE),
        ];
        let config: Vec<(String, String)> = fixed
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain(self.overrides.iter().cloned())
            .collect();
        command.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ATTR_NOSYSTEM", "1");
        command
    }

    fn checked(&self, args: &[&str]) -> Result<Output> {
        let output = self.run(args)?;
        require(output.status.success(), git_failure(&self.root, &output))?;
        Ok(output)
    }
}

/// Largest single `git ls-files -z` record accepted (one repository-relative path).
const LISTING_RECORD_BYTES: u64 = 64 * 1024;

/// `git ls-files -z` for runtime source capture, under the same hardened
/// invocation as discovery. The `core.excludesFile` setting (from any scope) is
/// additionally neutralized, so `--exclude-standard` applies only the
/// repository's own ignore rules: `.gitignore` files at every level and
/// `$GIT_COMMON_DIR/info/exclude`.
pub(crate) struct SourceListing(Git);

impl SourceListing {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        let mut git = Git::hardened(root)?;
        git.overrides
            .push(("core.excludesFile".into(), NULL_DEVICE.into()));
        Ok(Self(git))
    }

    /// The repository-local exclude file that `--exclude-standard` applies, as
    /// Git itself resolves it: `info/exclude` in the common Git directory, which
    /// linked worktrees share. It is never assumed to be
    /// `<worktree>/.git/info/exclude` (in a linked worktree, `.git` is a file).
    pub(crate) fn exclude_file(&self) -> Result<PathBuf> {
        let output = self.0.checked(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])?;
        let path = PathBuf::from(text(&output.stdout)?.trim_end_matches('\n'));
        super::paths::absolute_path(&path)?;
        Ok(path)
    }

    /// Streams each record of `git ls-files -z <args>` to `visit` without
    /// buffering the whole listing. Records are raw path bytes: `-z` output is
    /// never quoted or localized. `visit` returns `Ok(false)` to stop early, in
    /// which case (as on error) Git is killed rather than left to finish its walk.
    pub(crate) fn stream(
        &self,
        args: &[&str],
        mut visit: impl FnMut(&[u8]) -> Result<bool>,
    ) -> Result<()> {
        let git = &self.0;
        let args: Vec<&str> = ["ls-files", "-z"]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        let mut child = git
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                Error::Invalid(format!("could not run Git for {}: {e}", git.root.display()))
            })?;
        let mut stderr = child.stderr.take().expect("piped Git stderr");
        // Drained concurrently so Git can never block on a full stderr pipe.
        let errors = std::thread::spawn(move || {
            let mut kept = vec![];
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut kept);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            kept
        });
        let mut stdout = BufReader::new(child.stdout.take().expect("piped Git stdout"));
        let finished = (|| -> Result<bool> {
            let mut record = vec![];
            loop {
                record.clear();
                if (&mut stdout)
                    .take(LISTING_RECORD_BYTES + 1)
                    .read_until(0, &mut record)?
                    == 0
                {
                    return Ok(true);
                }
                require(
                    record.pop() == Some(0),
                    "Git source listing record is unterminated or oversized",
                )?;
                if !visit(&record)? {
                    return Ok(false);
                }
            }
        })();
        drop(stdout);
        if !matches!(finished, Ok(true)) {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let stderr = errors.join().unwrap_or_default();
        if finished? {
            require(
                status.success(),
                format!(
                    "Git source listing in {} failed: {}",
                    git.root.display(),
                    String::from_utf8_lossy(&stderr).trim()
                ),
            )?;
        }
        Ok(())
    }
}

/// agentctl never drives Git as root against another user's repository.
fn refuse_privileged(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            require(
                fs::metadata(root)?.uid() == 0,
                "refusing to run agentctl Git operations as root on a repository owned by another user",
            )?;
        }
    }
    let _ = root;
    Ok(())
}

fn git_failure(root: &Path, output: &Output) -> String {
    format!(
        "Git discovery in {} failed: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}
