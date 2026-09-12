//! Bounded, lossless file-content observations. Double collection detects observed
//! races; it is deliberately not advertised as an atomic filesystem snapshot.
use super::*;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub hash: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileState {
    pub content: ArtifactRef,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSnapshot {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub head: Option<String>,
    pub dirty: bool,
    pub index_hash: String,
    pub files: BTreeMap<String, FileState>,
}
impl SourceSnapshot {
    pub fn source_ref(&self) -> Result<SourceStateRef> {
        Ok(SourceStateRef {
            revision: self.head.clone().unwrap_or_else(|| "git:unborn".into()),
            worktree_diff_hash: Some(planning::hash(self)?),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub before: Option<FileState>,
    pub after: Option<FileState>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDiff {
    pub plan_id: PlanId,
    pub task_id: Option<TaskId>,
    pub executor_job_id: Option<JobId>,
    pub workspace_id: WorkspaceId,
    pub before: ArtifactRef,
    pub after: ArtifactRef,
    pub changes: Vec<FileChange>,
    pub scope_violations: Vec<String>,
}

pub struct Artifacts {
    root: PathBuf,
}
impl Artifacts {
    pub fn new(root: &Path) -> Result<Self> {
        paths::ensure_directory(root)?;
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }
    pub fn put(&self, bytes: &[u8]) -> Result<ArtifactRef> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let reference = ArtifactRef {
            hash: graph::content_hash(bytes),
            bytes: bytes.len() as u64,
        };
        let path = self.path(&reference)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        if path.exists() {
            require(
                self.get(&reference)? == bytes,
                "artifact hash collision or corruption",
            )?;
            return Ok(reference);
        }
        let (temp, mut file) = loop {
            let temp = self.root.join(format!(
                ".pending-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match options.open(&temp) {
                Ok(file) => break (temp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let publish = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            match fs::hard_link(&temp, &path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    require(
                        self.get(&reference)? == bytes,
                        "artifact hash collision or corruption",
                    )?;
                }
                Err(e) => return Err(e.into()),
            }
            fs::File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        fs::remove_file(temp)?;
        publish?;
        Ok(reference)
    }
    pub fn json(&self, value: &impl Serialize) -> Result<ArtifactRef> {
        self.put(&serde_json::to_vec(value)?)
    }
    pub fn get(&self, reference: &ArtifactRef) -> Result<Vec<u8>> {
        // Atomic publication briefly has two links to the same fully synced
        // inode. Source files remain hardlink-rejected; artifact bytes are also
        // authenticated by length/hash before use.
        let bytes = read_regular_file(&self.path(reference)?, 64 * 1024 * 1024, true)?;
        require(
            bytes.len() as u64 == reference.bytes && graph::content_hash(&bytes) == reference.hash,
            "artifact content/hash mismatch",
        )?;
        Ok(bytes)
    }
    pub fn decode<T: serde::de::DeserializeOwned>(&self, reference: &ArtifactRef) -> Result<T> {
        Ok(serde_json::from_slice(&self.get(reference)?)?)
    }
    pub fn path(&self, reference: &ArtifactRef) -> Result<PathBuf> {
        let hash = reference.hash.strip_prefix("blake3:").unwrap_or("");
        require(
            hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid artifact locator",
        )?;
        Ok(self.root.join(hash))
    }
}

pub(super) fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    read_regular_file(path, limit, false)
}
fn read_regular_file(path: &Path, limit: u64, artifact: bool) -> Result<Vec<u8>> {
    paths::check_file(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    require(file.metadata()?.is_file(), "runtime refuses special files")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        require(
            artifact || file.metadata()?.nlink() == 1,
            "runtime refuses hard-linked files",
        )?;
    }
    let mut bytes = vec![];
    file.take(limit + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= limit,
        "runtime file/artifact exceeds size limit",
    )?;
    Ok(bytes)
}

pub(super) fn capture(root: &Path, artifacts: &Artifacts) -> Result<SourceSnapshot> {
    let a = collect(root, artifacts)?;
    let b = collect(root, artifacts)?;
    require(
        a == b,
        "SOURCE_DRIFT: workspace changed during source capture",
    )?;
    Ok(b)
}
fn collect(root: &Path, artifacts: &Artifacts) -> Result<SourceSnapshot> {
    let info = RepositoryInfo::discover(root)?;
    let policy = ProjectConfig::load(root)?;
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    let mut entries = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            require(
                entries <= 25_000,
                "runtime workspace traversal exceeds 25000 entries",
            )?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_str()
                .ok_or_else(|| Error::Invalid("runtime requires UTF-8 paths".into()))?
                .to_owned();
            if relative == ".git" {
                continue;
            }
            crate::validation::repo_path(&relative)?;
            require(
                !relative.split('/').skip(1).any(|s| s == ".git"),
                "runtime does not support nested repositories/submodules",
            )?;
            let meta = fs::symlink_metadata(&path)?;
            require(
                !meta.file_type().is_symlink(),
                "runtime source capture refuses symlinks",
            )?;
            require(
                relative.split('/').count() <= 64,
                "runtime tree depth exceeds limit",
            )?;
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            require(
                !policy
                    .protected
                    .iter()
                    .any(|p| p.deny_read && inside(&relative, &p.path)),
                "runtime cannot capture protected read-denied source; narrow the checkout",
            )?;
            let bytes = read_file(&path, 2 * 1024 * 1024)?;
            total += bytes.len() as u64;
            require(
                total <= 64 * 1024 * 1024 && files.len() < 20_000,
                "runtime workspace capture exceeds 64 MiB/20000 files",
            )?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = u32::from(meta.permissions().readonly());
            files.insert(
                relative,
                FileState {
                    content: artifacts.put(&bytes)?,
                    mode,
                },
            );
        }
    }
    let index = info.git_directory.join("index");
    let index_hash = if index.exists() {
        graph::content_hash(&read_file(&index, 16 * 1024 * 1024)?)
    } else {
        graph::content_hash(&[])
    };
    Ok(SourceSnapshot {
        repository_id: info.repository_id,
        workspace_id: info.workspace_id,
        head: info.source.head_commit,
        dirty: info.source.dirty,
        index_hash,
        files,
    })
}

pub(super) fn inside(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}
pub(super) fn permits(scope: &ScopePath, path: &str) -> bool {
    match scope {
        ScopePath::File { path: p } => p == path,
        ScopePath::Directory { path: p } => inside(path, p),
    }
}
pub(super) fn diff(
    before: &SourceSnapshot,
    after: &SourceSnapshot,
    plan: &PlanId,
    task: Option<&TaskPacket>,
    job: Option<&JobId>,
    policy: &ProjectConfig,
    artifacts: &Artifacts,
) -> Result<CapturedDiff> {
    require(
        before.repository_id == after.repository_id
            && before.workspace_id == after.workspace_id
            && before.head == after.head
            && before.index_hash == after.index_hash,
        "SOURCE_DRIFT: Git identity/HEAD/index changed during execution",
    )?;
    let paths: BTreeSet<_> = before.files.keys().chain(after.files.keys()).collect();
    let changes: Vec<_> = paths
        .into_iter()
        .filter(|p| before.files.get(*p) != after.files.get(*p))
        .map(|p| FileChange {
            path: p.clone(),
            before: before.files.get(p).cloned(),
            after: after.files.get(p).cloned(),
        })
        .collect();
    let scope_violations = changes
        .iter()
        .filter(|c| {
            task.is_some_and(|t| !t.write_scope.iter().any(|s| permits(s, &c.path)))
                || inside(&c.path, ".agentctl")
                || inside(&c.path, ".codex")
                || inside(&c.path, ".claude")
                || policy
                    .protected
                    .iter()
                    .any(|p| p.deny_write && inside(&c.path, &p.path))
        })
        .map(|c| c.path.clone())
        .collect();
    Ok(CapturedDiff {
        plan_id: plan.clone(),
        task_id: task.map(|t| t.task_id.clone()),
        executor_job_id: job.cloned(),
        workspace_id: after.workspace_id.clone(),
        before: artifacts.json(before)?,
        after: artifacts.json(after)?,
        changes,
        scope_violations,
    })
}
