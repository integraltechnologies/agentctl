//! Bounded, lossless file-content observations. Double collection detects observed
//! races; it is deliberately not advertised as an atomic filesystem snapshot.
//!
//! A capture observes every file Git walks (see [`source_paths`]), not every file
//! on disk: the contents of directories the repository ignores as a whole (build
//! output, dependency trees, caches) are never traversed, read, or counted.
use super::*;
use crate::local::repository::SourceListing;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
};

/// Aggregate ceiling on workspace source capture (all files combined), distinct from
/// [`ArtifactRef`] readback and from `PlanningLimits.bytes` (which bounds only the
/// frozen `PlannerPacket`, not the full-workspace observation captured alongside it).
const WORKSPACE_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
/// Ceiling on the number of files a single workspace capture may observe.
const WORKSPACE_CAPTURE_FILES: usize = 20_000;
/// Ceiling on the `.git/index` file read during workspace capture.
const GIT_INDEX_BYTES: u64 = 16 * 1024 * 1024;
/// Ceiling on index entries listed during workspace capture. An index within
/// [`GIT_INDEX_BYTES`] cannot hold more: every on-disk entry is at least 62 bytes.
const GIT_INDEX_ENTRIES: usize = (GIT_INDEX_BYTES / 62) as usize;
/// Ceiling on a single content-addressed artifact readback from the local CAS.
const ARTIFACT_READBACK_BYTES: u64 = 64 * 1024 * 1024;

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

/// An individually ignored file. Git reports it while walking the worktree, but
/// the repository declares that its content is not source. It is observed by
/// metadata alone: never read, stored, counted against the content budget, or
/// offered as context. Creating, replacing, or writing it is still a drift and
/// scope change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoredFile {
    pub bytes: u64,
    /// Full `st_mode` (file type and permissions) on Unix; read-only flag elsewhere.
    pub mode: u32,
    pub device: u64,
    pub inode: u64,
    pub modified_ns: i64,
    /// Status-change time: every content or metadata write updates it, and an
    /// unprivileged process cannot set it back.
    pub changed_ns: i64,
}
impl IgnoredFile {
    fn observe(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ns =
                |secs: i64, nanos: i64| secs.saturating_mul(1_000_000_000).saturating_add(nanos);
            Self {
                bytes: meta.len(),
                mode: meta.mode(),
                device: meta.dev(),
                inode: meta.ino(),
                modified_ns: ns(meta.mtime(), meta.mtime_nsec()),
                changed_ns: ns(meta.ctime(), meta.ctime_nsec()),
            }
        }
        #[cfg(not(unix))]
        {
            let modified_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
            Self {
                bytes: meta.len(),
                mode: u32::from(meta.permissions().readonly()),
                device: 0,
                inode: 0,
                modified_ns,
                changed_ns: 0,
            }
        }
    }
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
    /// Individually ignored files, by metadata only (see [`IgnoredFile`]). Omitted
    /// when empty, so a snapshot without any serializes and hashes as before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ignored: BTreeMap<String, IgnoredFile>,
    /// Content hash of the repository-local exclude rules Git applies
    /// (`info/exclude` in the common Git directory, as Git resolves it), or
    /// `None` when there are none. Those rules move the ignore boundary from
    /// outside the worktree, so they belong to the integrity baseline. Only the
    /// hash is recorded: Git control metadata is neither source nor agent context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_hash: Option<String>,
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
    /// Metadata of an individually ignored file on either side; its content is
    /// never captured. Omitted when absent, so source-only changes are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_ignored: Option<IgnoredFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ignored: Option<IgnoredFile>,
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
        require(
            reference.bytes <= ARTIFACT_READBACK_BYTES,
            format!(
                "runtime artifact readback exceeds {ARTIFACT_READBACK_BYTES}-byte limit (recorded={})",
                reference.bytes
            ),
        )?;
        let bytes = read_regular_file(&self.path(reference)?, ARTIFACT_READBACK_BYTES, true)?;
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
    #[cfg(not(unix))]
    let _ = artifact;
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
/// Ceiling on the repository-local `info/exclude` rules read during capture. They
/// are Git control metadata, not source: only their hash is kept.
const GIT_EXCLUDE_BYTES: u64 = 1024 * 1024;
/// What a capture observes, as Git classifies the worktree while walking it.
struct Listing {
    /// Captured by content: every index entry, even one an ignore rule also
    /// matches (Git never ignores tracked content, so neither does capture), and
    /// every untracked file the repository's rules do not ignore.
    source: BTreeSet<String>,
    /// Observed by metadata only (see [`IgnoredFile`]): every ignored file Git
    /// reaches while walking, such as `.env`, a stray `*.log`, or a planted
    /// self-ignoring `.gitignore`. An executor cannot create or rewrite one
    /// outside its scope unnoticed, yet its content never costs budget or
    /// becomes context.
    ignored: BTreeSet<String>,
    /// Hash of the repository-local exclude rules the listing was made under
    /// (see [`SourceSnapshot::exclude_hash`]).
    exclude_hash: Option<String>,
    /// Whether a listing stopped early (see [`source_paths`]).
    truncated: bool,
}
/// Content hash of the repository-local exclude rules at `path`, or `None` when
/// there are none. They live in the Git directory, outside the walked worktree,
/// so they are hashed explicitly. Only the hash is kept; like the index, the
/// file is read bounded and without following links.
fn exclude_rules_hash(path: &Path) -> Result<Option<String>> {
    let Some(meta) = lstat(path)? else {
        return Ok(None);
    };
    let len = meta.len();
    require(
        len <= GIT_EXCLUDE_BYTES,
        format!(
            "runtime git info/exclude read exceeds {GIT_EXCLUDE_BYTES}-byte limit (size={len})"
        ),
    )?;
    Ok(Some(graph::content_hash(&read_file(
        path,
        GIT_EXCLUDE_BYTES,
    )?)))
}
/// Lists the worktree for capture. Nothing inside a directory that an ignore
/// rule matches as a whole, such as `target/` or `node_modules/`, is listed:
/// Git never walks such a directory and cannot re-include anything within it,
/// so the repository has declared its contents opaque generated state, and
/// neither Git nor agentctl traverses it. `.git` is never listed.
///
/// Ignore rules are the repository's own: `.gitignore` files at every level and
/// `$GIT_COMMON_DIR/info/exclude`. `core.excludesFile` is neutralized, so the
/// boundary is a property of the repository rather than of whoever runs
/// agentctl. Every rule file Git consults inside the worktree sits in a walked
/// directory, and `info/exclude` (outside it) is hashed into the snapshot, so
/// any change to the boundary is itself an observed change.
///
/// Index entries are bounded by [`GIT_INDEX_ENTRIES`]. The untracked and ignored
/// listings each stop one entry past [`WORKSPACE_CAPTURE_FILES`]; every such
/// entry must be observed, so that alone guarantees the matching count check
/// fails. A truncation is also reported in case a racing deletion hides it.
fn source_paths(root: &Path) -> Result<Listing> {
    let git = SourceListing::new(root)?;
    // Hashed before listing, like the index: the rules the listing is made under.
    let mut listing = Listing {
        source: BTreeSet::new(),
        ignored: BTreeSet::new(),
        exclude_hash: exclude_rules_hash(&git.exclude_file()?)?,
        truncated: false,
    };
    let mut listed = 0usize;
    git.stream(&["--cached"], |record| {
        listed += 1;
        require(
            listed <= GIT_INDEX_ENTRIES,
            format!("runtime git index lists more than {GIT_INDEX_ENTRIES} entries"),
        )?;
        listing.source.insert(utf8_path(record)?);
        Ok(true)
    })?;
    for ignored in [false, true] {
        let (args, paths): (&[&str], _) = if ignored {
            // Ignored entries as Git classifies them while walking. A file record
            // is an individually ignored file. A `dir/` record is a directory a
            // rule matches as a whole, which `--directory` keeps Git from
            // walking; a wholly ignored directory Git did walk also has its files
            // reported individually, so skipping `dir/` records loses no file.
            (
                &["--others", "--ignored", "--exclude-standard", "--directory"],
                &mut listing.ignored,
            )
        } else {
            // A `dir/` record here is an untracked nested repository, refused later.
            (&["--others", "--exclude-standard"], &mut listing.source)
        };
        let mut kept = 0usize;
        git.stream(args, |record| {
            if ignored && record.ends_with(b"/") {
                return Ok(true);
            }
            paths.insert(utf8_path(record)?);
            kept += 1;
            Ok(kept <= WORKSPACE_CAPTURE_FILES)
        })?;
        listing.truncated |= kept > WORKSPACE_CAPTURE_FILES;
    }
    Ok(listing)
}
fn utf8_path(record: &[u8]) -> Result<String> {
    String::from_utf8(record.to_vec()).map_err(|_| {
        Error::Invalid(format!(
            "runtime requires UTF-8 paths: {}",
            String::from_utf8_lossy(record)
        ))
    })
}
/// `lstat`, with `None` when nothing is there (including below a non-directory).
fn lstat(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
/// Whether every parent of `relative` is a real directory; a missing parent means
/// the path is absent. Git lists index entries by name, so a parent directory
/// replaced by a symlink (which Git reports as an untracked entry, or not at all
/// when ignored) must be refused rather than followed out of the workspace.
fn parents_are_directories(
    root: &Path,
    relative: &str,
    checked: &mut BTreeMap<String, bool>,
) -> Result<bool> {
    for (end, _) in relative.match_indices('/') {
        let parent = &relative[..end];
        let present = match checked.get(parent) {
            Some(present) => *present,
            None => {
                let present = match lstat(&root.join(parent))? {
                    Some(meta) => {
                        require(
                            !meta.file_type().is_symlink(),
                            "runtime source capture refuses symlinks",
                        )?;
                        meta.is_dir()
                    }
                    None => false,
                };
                checked.insert(parent.to_owned(), present);
                present
            }
        };
        if !present {
            return Ok(false);
        }
    }
    Ok(true)
}
/// Validates a listed path and `lstat`s it without following it. `None` means the
/// path is absent: an index entry deleted from the worktree, a sparse-checkout
/// path, or a racing deletion.
fn listed_entry(
    root: &Path,
    relative: &str,
    parents: &mut BTreeMap<String, bool>,
) -> Result<Option<(PathBuf, fs::Metadata)>> {
    // Git reports an untracked nested repository as a directory entry.
    require(
        !relative.ends_with('/') && !relative.split('/').any(|s| s == ".git"),
        "runtime does not support nested repositories/submodules",
    )?;
    crate::validation::repo_path(relative)?;
    require(
        relative.split('/').count() <= 64,
        "runtime tree depth exceeds limit",
    )?;
    if !parents_are_directories(root, relative, parents)? {
        return Ok(None);
    }
    let path = root.join(relative);
    Ok(lstat(&path)?.map(|meta| (path, meta)))
}
fn collect(root: &Path, artifacts: &Artifacts) -> Result<SourceSnapshot> {
    let info = RepositoryInfo::discover(root)?;
    let policy = ProjectConfig::load(root)?;
    // Read before listing: a bounded index also bounds the index listing.
    let index = info.git_directory.join("index");
    let index_hash = if index.exists() {
        let len = fs::metadata(&index)?.len();
        require(
            len <= GIT_INDEX_BYTES,
            format!("runtime git index read exceeds {GIT_INDEX_BYTES}-byte limit (size={len})"),
        )?;
        graph::content_hash(&read_file(&index, GIT_INDEX_BYTES)?)
    } else {
        graph::content_hash(&[])
    };
    let listing = source_paths(&info.root)?;
    let mut parents = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    for relative in listing.source {
        let Some((path, meta)) = listed_entry(&info.root, &relative, &mut parents)? else {
            continue;
        };
        require(
            !meta.file_type().is_symlink(),
            "runtime source capture refuses symlinks",
        )?;
        if meta.is_dir() {
            // Only a gitlink (submodule) index entry names a directory. A populated
            // one is a nested repository; an uninitialized one holds nothing.
            require(
                lstat(&path.join(".git"))?.is_none(),
                "runtime does not support nested repositories/submodules",
            )?;
            continue;
        }
        require(
            !policy
                .protected
                .iter()
                .any(|p| p.deny_read && inside(&relative, &p.path)),
            "runtime cannot capture protected read-denied source; narrow the checkout",
        )?;
        require(
            files.len() < WORKSPACE_CAPTURE_FILES,
            format!(
                "runtime workspace capture exceeds {WORKSPACE_CAPTURE_FILES}-file limit (count={})",
                files.len()
            ),
        )?;
        // A file's individual size is not itself meaningful; only the aggregate
        // capture budget is. Bounding each read by the remaining budget (rather
        // than a fixed per-file ceiling) keeps capture bounded overall while not
        // letting one irrelevant large file fail a capture that fits comfortably
        // within the real 64 MiB workspace limit. `meta.len()` is already in hand
        // from the symlink check above, so this attributes the failure (subsystem,
        // path, captured-so-far, this file's size) before ever touching the file's
        // contents; `read_file` still re-checks the same budget as a TOCTOU backstop.
        let remaining = WORKSPACE_CAPTURE_BYTES.saturating_sub(total);
        let file_len = meta.len();
        require(
            file_len <= remaining,
            format!(
                "runtime workspace capture exceeds {WORKSPACE_CAPTURE_BYTES}-byte limit while reading {relative} (captured={total}, file={file_len})"
            ),
        )?;
        let bytes = read_file(&path, remaining)?;
        total += bytes.len() as u64;
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
    // Individually ignored files are observed by metadata alone. Nothing is read,
    // so symlinks, hardlinks, and read-denied paths among them need no refusal,
    // and none of them counts against the content budget.
    let mut ignored = BTreeMap::new();
    for relative in listing.ignored {
        let Some((_, meta)) = listed_entry(&info.root, &relative, &mut parents)? else {
            continue;
        };
        require(
            ignored.len() < WORKSPACE_CAPTURE_FILES,
            format!(
                "runtime workspace capture exceeds {WORKSPACE_CAPTURE_FILES}-file limit for individually ignored files (count={})",
                ignored.len()
            ),
        )?;
        ignored.insert(relative, IgnoredFile::observe(&meta));
    }
    require(
        !listing.truncated,
        format!(
            "runtime workspace capture exceeds {WORKSPACE_CAPTURE_FILES}-file limit (count>{WORKSPACE_CAPTURE_FILES})"
        ),
    )?;
    Ok(SourceSnapshot {
        repository_id: info.repository_id,
        workspace_id: info.workspace_id,
        head: info.source.head_commit,
        dirty: info.source.dirty,
        index_hash,
        files,
        ignored,
        exclude_hash: listing.exclude_hash,
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
    // The exclude rules can hide new content without any worktree change being
    // observable, so a change to them fails closed rather than being diffed.
    require(
        before.exclude_hash == after.exclude_hash,
        "SOURCE_DRIFT: repository exclude rules (info/exclude) changed during execution; content they now hide cannot be ruled out",
    )?;
    let paths: BTreeSet<_> = before
        .files
        .keys()
        .chain(after.files.keys())
        .chain(before.ignored.keys())
        .chain(after.ignored.keys())
        .collect();
    let changes: Vec<_> = paths
        .into_iter()
        .filter(|p| {
            before.files.get(*p) != after.files.get(*p)
                || before.ignored.get(*p) != after.ignored.get(*p)
        })
        .map(|p| FileChange {
            path: p.clone(),
            before: before.files.get(p).cloned(),
            after: after.files.get(p).cloned(),
            before_ignored: before.ignored.get(p).cloned(),
            after_ignored: after.ignored.get(p).cloned(),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::local::config::ProtectedRule;
    use std::{
        os::unix::{ffi::OsStrExt, fs::symlink},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    /// A throwaway repository with agentctl project policy and a private CAS.
    struct Repo {
        base: PathBuf,
        root: PathBuf,
        artifacts: Artifacts,
    }
    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = std::env::temp_dir().join(format!(
                "agentctl-source-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&base);
            let root = base.join("repo");
            fs::create_dir_all(&root).unwrap();
            let repo = Self {
                artifacts: Artifacts::new(&base.join("artifacts")).unwrap(),
                base,
                root,
            };
            repo.git(&["init", "--quiet"]);
            ProjectConfig::initialize(&repo.root).unwrap();
            repo
        }
        /// Plain Git, isolated from the host's system/global/XDG configuration.
        fn command(&self, directory: &Path, args: &[&str]) -> Command {
            let mut command = Command::new("git");
            command
                .current_dir(directory)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("XDG_CONFIG_HOME", &self.base)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
            command
        }
        fn git_in(&self, directory: &Path, args: &[&str]) {
            let output = self.command(directory, args).output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn git(&self, args: &[&str]) {
            self.git_in(&self.root, args);
        }
        fn commit(&self) {
            self.git(&["add", "."]);
            self.git(&["commit", "--quiet", "-m", "baseline"]);
        }
        /// Git's own classification, independent of agentctl. Tracked files are
        /// never ignored unless `no_index` asks only whether a rule matches.
        fn check_ignore(&self, relative: &str, no_index: bool) -> bool {
            let mut args = vec!["check-ignore", "--quiet"];
            if no_index {
                args.push("--no-index");
            }
            args.extend(["--", relative]);
            self.command(&self.root, &args).status().unwrap().success()
        }
        fn ignored(&self, relative: &str) -> bool {
            self.check_ignore(relative, false)
        }
        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fn capture(&self) -> Result<SourceSnapshot> {
            capture(&self.root, &self.artifacts)
        }
        /// Paths captured by content, and paths observed by metadata only.
        fn tiers(&self) -> (BTreeSet<String>, BTreeSet<String>) {
            let snapshot = self.capture().unwrap();
            (
                snapshot.files.into_keys().collect(),
                snapshot.ignored.into_keys().collect(),
            )
        }
        fn refusal(&self) -> String {
            self.capture().unwrap_err().to_string()
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// Capture follows Git's own classification. Source files are captured by
    /// content: tracked files, even ones an ignore rule matches (including inside
    /// an ignored directory, which Git then walks), and untracked, non-ignored
    /// files. Individually ignored files are observed by metadata only, so even
    /// one larger than the whole budget costs nothing and its bytes never reach
    /// the artifact store. Directories the repository's rules ignore as a whole
    /// are not walked, whether the rule is a root glob, a nested `.gitignore`, or
    /// `.git/info/exclude`, except where a negation re-includes one.
    /// `core.excludesFile` is not the repository's policy and is not applied.
    #[test]
    fn capture_tiers_follow_git_classification() {
        let repo = Repo::new();
        repo.write(".gitignore", "*.log\n/out/\n/gen-*/\n!/gen-keep/\n");
        repo.write("sub/.gitignore", "cache/\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.write("src/keep.log", "tracked although *.log matches\n");
        repo.write("out/vendored.c", "tracked inside an ignored directory\n");
        repo.commit();
        repo.git(&["add", "--force", "src/keep.log", "out/vendored.c"]);
        repo.git(&["commit", "--quiet", "-m", "track ignore-matching files"]);
        for name in [
            "notes/todo.txt",
            "-leading-dash.txt",
            "with space.txt",
            "unicod\u{e9}.txt",
            "#hash !bang.txt",
            "gen-keep/kept.txt",
        ] {
            repo.write(name, "untracked\n");
        }
        // Individually ignored files are walked, so they are observed, but only by
        // metadata: neither size nor content costs anything.
        let secret = "individually ignored, never captured\n";
        repo.write("src/debug.log", secret);
        repo.write(
            "out/junk.o",
            "ignored, but Git walks out/ for its tracked file\n",
        );
        fs::File::create(repo.root.join("src/huge.log"))
            .unwrap()
            .set_len(WORKSPACE_CAPTURE_BYTES + 1)
            .unwrap();
        // Contents of directories ignored as a whole are never walked.
        repo.write("gen-a/big.bin", "root glob rule\n");
        repo.write("sub/cache/blob.bin", "nested rule\n");
        repo.write("scratch/tmp.txt", "info/exclude\n");
        fs::write(repo.root.join(".git/info/exclude"), "/scratch/\n").unwrap();
        // Ignored only by core.excludesFile (set locally here; normally
        // user-global), which capture does not apply.
        let personal = repo.base.join("personal-excludes");
        fs::write(&personal, "personal/\n").unwrap();
        repo.git(&["config", "core.excludesFile", personal.to_str().unwrap()]);
        repo.write("personal/notes.txt", "x\n");
        for path in [
            "src/debug.log",
            "src/huge.log",
            "out/junk.o",
            "gen-a/big.bin",
            "sub/cache/blob.bin",
            "scratch/tmp.txt",
            "personal/notes.txt",
        ] {
            assert!(repo.ignored(path), "fixture: Git itself must ignore {path}");
        }
        assert!(!repo.ignored("src/keep.log") && repo.check_ignore("src/keep.log", true));
        let snapshot = repo.capture().unwrap();
        assert_eq!(
            snapshot.files.keys().cloned().collect::<BTreeSet<_>>(),
            set(&[
                ".agentctl/project.toml",
                ".gitignore",
                "sub/.gitignore",
                "src/lib.rs",
                "src/keep.log",
                "out/vendored.c",
                "notes/todo.txt",
                "-leading-dash.txt",
                "with space.txt",
                "unicod\u{e9}.txt",
                "#hash !bang.txt",
                "gen-keep/kept.txt",
                "personal/notes.txt",
            ])
        );
        assert_eq!(
            snapshot.ignored.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["out/junk.o", "src/debug.log", "src/huge.log"])
        );
        assert_eq!(
            snapshot.ignored["src/huge.log"].bytes,
            WORKSPACE_CAPTURE_BYTES + 1
        );
        // Presence is not content: the ignored file's bytes never reach the store.
        let unstored = ArtifactRef {
            hash: graph::content_hash(secret.as_bytes()),
            bytes: secret.len() as u64,
        };
        assert!(!repo.artifacts.path(&unstored).unwrap().exists());
    }

    /// Cross-checks capture against `git status --ignored=matching`, Git's own
    /// reference for this classification. The layout covers the cases where Git's
    /// listings are subtle: directory, glob, and negated rules; wholly ignored
    /// directories that Git still walks; nested and self-ignoring rule files; and
    /// an ignored directory kept walkable by a tracked file. The content tier must
    /// equal every tracked and untracked (non-ignored) file Git reports, the
    /// metadata tier every individually ignored file, and nothing under a
    /// directory Git reports as ignored may appear in either.
    #[test]
    fn capture_matches_git_status_ignored_matching() {
        let repo = Repo::new();
        repo.write(
            ".gitignore",
            "/build/\ngen/*\n*.log\n/out/\n/gen-*/\n!/gen-keep/\nnode_modules/\n",
        );
        repo.write("main.rs", "fn main() {}\n");
        repo.write("mix/ok.txt", "x");
        repo.write("out/vendored.c", "x");
        repo.commit();
        repo.git(&["add", "--force", "out/vendored.c"]);
        repo.git(&[
            "commit",
            "--quiet",
            "-m",
            "track inside an ignored directory",
        ]);
        for path in [
            "build/deep/x.o",
            "gen/a.txt",
            "gen/sub/b.txt",
            "logs/x.log",
            "logs/y.log",
            "a/b/c.log",
            "t/x.py",
            "m/x.log",
            "m/node_modules/x/i.js",
            "out/junk.o",
            "gen-a/big.bin",
            "gen-keep/k.txt",
            "emptyd/z.log",
            "mix/no.log",
        ] {
            repo.write(path, "x");
        }
        repo.write("t/.gitignore", "*\n");
        let records = |args: &[&str]| -> Vec<String> {
            let output = repo.command(&repo.root, args).output().unwrap();
            assert!(output.status.success());
            output
                .stdout
                .split(|b| *b == 0)
                .filter(|r| !r.is_empty())
                .map(|r| String::from_utf8(r.to_vec()).unwrap())
                .collect()
        };
        let mut content: BTreeSet<String> = records(&["ls-files", "-z"]).into_iter().collect();
        let (mut ignored, mut opaque) = (BTreeSet::new(), BTreeSet::new());
        for record in records(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
        ]) {
            let (code, path) = record.split_at(3);
            match (code, path.ends_with('/')) {
                ("?? ", false) => content.insert(path.to_owned()),
                ("!! ", false) => ignored.insert(path.to_owned()),
                ("!! ", true) => opaque.insert(path.to_owned()),
                _ => panic!("unexpected record in a clean fixture: {record:?}"),
            };
        }
        assert_eq!(
            opaque,
            set(&["build/", "gen-a/", "gen/sub/", "m/node_modules/"])
        );
        assert_eq!(repo.tiers(), (content, ignored));
    }

    /// Ignored trees are never walked, so nothing in them is read, counted, or
    /// refused: a file larger than the whole capture budget, hardlinked build
    /// outputs, symlinks, a vendored checkout with its own `.git`, names capture
    /// could never represent, and rule files Git never consults -- the shape of a
    /// real Cargo `target/` or `node_modules/`.
    #[test]
    fn ignored_trees_are_neither_read_counted_nor_refused() {
        let repo = Repo::new();
        repo.write(".gitignore", "/out/\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        let out = repo.root.join("out");
        fs::create_dir_all(out.join("deep")).unwrap();
        // Sparse: past the aggregate budget without costing disk space or reads.
        fs::File::create(out.join("deep/huge.rlib"))
            .unwrap()
            .set_len(WORKSPACE_CAPTURE_BYTES + 1)
            .unwrap();
        fs::write(out.join("bin"), "x").unwrap();
        fs::hard_link(out.join("bin"), out.join("deep/bin-1a2b")).unwrap();
        symlink("/", out.join("root-link")).unwrap();
        fs::create_dir_all(out.join("vendored")).unwrap();
        repo.git_in(&out.join("vendored"), &["init", "--quiet"]);
        for name in ["tab\there", "new\nline", "glob[*?]{}:\\"] {
            fs::write(out.join(name), "x").unwrap();
        }
        // Not representable on every filesystem (APFS requires UTF-8 names).
        let _ = fs::write(out.join(std::ffi::OsStr::from_bytes(b"non-utf8-\xff")), "x");
        fs::write(out.join(".gitignore"), "!*\n").unwrap();
        assert!(repo.ignored("out/deep/huge.rlib"));
        assert_eq!(
            repo.tiers(),
            (
                set(&[".agentctl/project.toml", ".gitignore", "src/lib.rs"]),
                BTreeSet::new()
            )
        );
    }

    /// ...but the same entries are still refused among captured source, where
    /// capture would read them. Individually ignored, they are observed by
    /// metadata alone, which never follows or reads them; inside a directory the
    /// repository ignores as a whole, they are not observed at all.
    #[test]
    fn symlinks_hardlinks_and_nested_repositories_are_refused_only_among_captured_source() {
        type Case = (&'static str, fn(&Repo), &'static str, Option<&'static str>);
        let cases: [Case; 3] = [
            (
                "box/link",
                |r| {
                    fs::create_dir_all(r.root.join("box")).unwrap();
                    symlink("../src", r.root.join("box/link")).unwrap();
                },
                "refuses symlinks",
                Some("/box/link\n"),
            ),
            (
                "box/pair",
                |r| {
                    r.write("box/pair/a", "x");
                    fs::hard_link(r.root.join("box/pair/a"), r.root.join("box/pair/b")).unwrap();
                },
                "hard-linked",
                Some("/box/pair/*\n"),
            ),
            (
                "box/vendored",
                |r| {
                    fs::create_dir_all(r.root.join("box/vendored")).unwrap();
                    r.git_in(&r.root.join("box/vendored"), &["init", "--quiet"]);
                },
                "nested repositories",
                None,
            ),
        ];
        for (path, setup, refusal, individually_ignored) in cases {
            let repo = Repo::new();
            repo.write("src/lib.rs", "pub fn f() {}\n");
            setup(&repo);
            let message = repo.refusal();
            assert!(message.contains(refusal), "{path}: {message}");
            if let Some(rule) = individually_ignored {
                repo.write(".gitignore", rule);
                let (content, ignored) = repo.tiers();
                assert!(!content.iter().any(|p| p.starts_with(path)), "{path}");
                assert!(
                    ignored.iter().any(|p| p.starts_with(path)),
                    "{path} ({rule:?})"
                );
            }
            repo.write(".gitignore", "/box/\n");
            let (content, ignored) = repo.tiers();
            assert!(
                !content.iter().chain(&ignored).any(|p| p.starts_with("box")),
                "{path}"
            );
        }
    }

    /// A tracked directory replaced by a symlink that the repository then ignores
    /// is refused, never read through. The ignored symlink itself is observed by
    /// metadata only, but the index still names `lib/a.txt` beneath it, and
    /// capture refuses to resolve a captured path through a symlinked parent.
    #[test]
    fn a_tracked_directory_replaced_by_an_ignored_symlink_is_refused() {
        let repo = Repo::new();
        repo.write("lib/a.txt", "inside\n");
        repo.commit();
        let outside = repo.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("a.txt"), "outside the workspace\n").unwrap();
        fs::remove_dir_all(repo.root.join("lib")).unwrap();
        symlink(&outside, repo.root.join("lib")).unwrap();
        fs::write(repo.root.join(".git/info/exclude"), "/lib\n").unwrap();
        // Plain `check-ignore` calls a directory of index entries "tracked".
        assert!(repo.check_ignore("lib", true));
        let message = repo.refusal();
        assert!(message.contains("refuses symlinks"), "{message}");
    }

    /// The guard behind every listed path: capture never resolves one through a
    /// symlinked parent, and a missing or non-directory parent means it is absent.
    #[test]
    fn listed_paths_are_never_resolved_through_symlinked_parents() {
        let repo = Repo::new();
        repo.write("real/a.txt", "x");
        symlink(repo.root.join("real"), repo.root.join("link")).unwrap();
        let mut checked = BTreeMap::new();
        assert!(parents_are_directories(&repo.root, "real/a.txt", &mut checked).unwrap());
        assert!(!parents_are_directories(&repo.root, "gone/a.txt", &mut checked).unwrap());
        assert!(!parents_are_directories(&repo.root, "real/a.txt/b", &mut checked).unwrap());
        let message = parents_are_directories(&repo.root, "link/a.txt", &mut checked)
            .unwrap_err()
            .to_string();
        assert!(message.contains("refuses symlinks"), "{message}");
    }

    /// A worker can hide a new file from Git by planting a `.gitignore` that also
    /// ignores itself. A rule that ignores files individually hides nothing from
    /// capture, because Git still walks them. A rule that ignores a whole directory
    /// does hide that directory's contents, but the rule file sits in a walked
    /// directory, so the plant itself is a captured change: a drift and scope
    /// signal. A rule file inside an already-ignored directory is never consulted.
    #[test]
    fn planted_ignore_rules_are_observed_even_when_they_ignore_themselves() {
        let repo = Repo::new();
        repo.write(".gitignore", "/out/\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.commit();
        let baseline = repo.capture().unwrap();
        repo.write("src/.gitignore", ".gitignore\nsneaky.rs\n");
        repo.write("src/sneaky.rs", "pub fn hidden() {}\n");
        repo.write("tests/.gitignore", ".gitignore\nunit/\n");
        repo.write("tests/unit/conftest.py", "import builtins\n");
        repo.write("out/.gitignore", "!*\n");
        repo.write("out/gen.txt", "x");
        for path in [
            "src/.gitignore",
            "src/sneaky.rs",
            "tests/.gitignore",
            "tests/unit/conftest.py",
        ] {
            assert!(repo.ignored(path), "fixture: Git itself must ignore {path}");
        }
        let (content, ignored) = repo.tiers();
        assert_eq!(content, baseline.files.keys().cloned().collect());
        assert_eq!(
            ignored,
            set(&["src/.gitignore", "src/sneaky.rs", "tests/.gitignore"])
        );
    }

    /// Capture equality is what every drift gate compares. Churn inside an ignored
    /// directory (a concurrent build, a test cache) leaves the snapshot identical.
    /// Changing or deleting a captured file does not, whether it is tracked,
    /// tracked but matching an ignore rule, or untracked. Neither does writing an
    /// individually ignored file: that is detected by metadata, so any write
    /// counts, even one of the same length or one that restores the old bytes.
    #[test]
    fn captured_changes_and_ignored_file_writes_are_drift_but_ignored_directory_churn_is_not() {
        let repo = Repo::new();
        repo.write(".gitignore", "*.log\n/out/\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.write("src/keep.log", "tracked\n");
        repo.commit();
        repo.git(&["add", "--force", "src/keep.log"]);
        repo.git(&["commit", "--quiet", "-m", "track an ignore-matching file"]);
        repo.write("draft.txt", "untracked\n");
        repo.write("src/run.log", "individually ignored\n");
        repo.write("out/cache.bin", "1");
        let baseline = repo.capture().unwrap();
        repo.write("out/cache.bin", "2");
        repo.write("out/new/artifact.bin", "new");
        assert_eq!(repo.capture().unwrap(), baseline);
        for path in ["src/lib.rs", "src/keep.log", "draft.txt"] {
            let original = fs::read(repo.root.join(path)).unwrap();
            fs::write(repo.root.join(path), "changed\n").unwrap();
            assert_ne!(repo.capture().unwrap(), baseline, "{path}");
            fs::write(repo.root.join(path), original).unwrap();
        }
        assert_eq!(repo.capture().unwrap(), baseline);
        repo.write("src/run.log", "individually IGNORED\n");
        let rewritten = repo.capture().unwrap();
        assert_ne!(rewritten, baseline, "a same-length write is observed");
        assert_eq!(rewritten.files, baseline.files);
        repo.write("src/run.log", "individually ignored\n");
        assert_ne!(
            repo.capture().unwrap(),
            baseline,
            "a write restoring the old bytes is still observed"
        );
        fs::remove_file(repo.root.join("src/lib.rs")).unwrap();
        let deleted = repo.capture().unwrap();
        assert!(!deleted.files.contains_key("src/lib.rs"));
        assert_ne!(deleted, baseline);
    }

    /// The metadata tier is bounded too: past 20,000 individually ignored files,
    /// capture fails closed with its own diagnostic, however little content there is.
    #[test]
    fn individually_ignored_files_are_bounded() {
        let repo = Repo::new();
        repo.write(".gitignore", "*.log\n");
        fs::create_dir_all(repo.root.join("logs")).unwrap();
        for i in 0..=WORKSPACE_CAPTURE_FILES {
            fs::write(repo.root.join(format!("logs/{i}.log")), "").unwrap();
        }
        let message = repo.refusal();
        assert!(
            message.contains("20000-file limit for individually ignored files")
                && message.contains("count=20000"),
            "{message}"
        );
    }

    /// Read-denied policy paths block capture only where capture would copy their
    /// contents into the artifact store. An individually ignored read-denied file is
    /// observed by metadata only, and one inside an ignored directory not at all.
    #[test]
    fn read_denied_paths_refuse_capture_only_when_observed() {
        let repo = Repo::new();
        let mut policy = ProjectConfig::load(&repo.root).unwrap();
        policy.protected.push(ProtectedRule {
            path: "secrets".into(),
            deny_read: true,
            deny_write: true,
            reason: "credentials".into(),
        });
        fs::write(
            crate::local::paths::project_config(&repo.root),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        repo.write("secrets/token", "t\n");
        let message = repo.refusal();
        assert!(message.contains("protected read-denied"), "{message}");
        repo.write(".gitignore", "/secrets/token\n");
        let (content, ignored) = repo.tiers();
        assert!(!content.iter().any(|p| p.starts_with("secrets")));
        assert_eq!(ignored, set(&["secrets/token"]));
        repo.write(".gitignore", "/secrets/\n");
        let (content, ignored) = repo.tiers();
        assert!(!content.iter().any(|p| p.starts_with("secrets")));
        assert!(ignored.is_empty());
    }

    /// Git's own resolution of the repository-local exclude file.
    fn exclude_file(repo: &Repo, directory: &Path) -> PathBuf {
        let output = repo
            .command(
                directory,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "info/exclude",
                ],
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
    }
    fn append(path: &Path, line: &str) {
        let prior = fs::read_to_string(path).unwrap_or_default();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{prior}{line}\n")).unwrap();
    }
    /// The executor diff (what scope checks run on) must fail closed as drift.
    fn assert_exclude_drift(before: &SourceSnapshot, after: &SourceSnapshot, root: &Path) {
        let artifacts =
            Artifacts::new(&root.parent().unwrap().join("exclude-drift-artifacts")).unwrap();
        let message = diff(
            before,
            after,
            &PlanId::new("plan:exclude").unwrap(),
            None,
            None,
            &ProjectConfig::load(root).unwrap(),
            &artifacts,
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("SOURCE_DRIFT") && message.contains("info/exclude"),
            "{message}"
        );
    }

    /// `.git/info/exclude` lives outside the walked worktree yet moves the ignore
    /// boundary. Adding a rule there, or creating the file with one, and then
    /// creating content the rule hides changes nothing capture can see in the
    /// worktree: Git prunes the new directory, so its file lands in neither
    /// tier. The snapshot's exclude-rules hash is the one thing that changes, and
    /// the diff that scope checks run on fails closed as drift.
    #[test]
    fn hiding_new_content_through_info_exclude_fails_closed() {
        for initially_present in [true, false] {
            let repo = Repo::new();
            repo.write("src/lib.rs", "pub fn f() {}\n");
            repo.commit();
            let exclude = exclude_file(&repo, &repo.root);
            if initially_present {
                append(&exclude, "# repository-local excludes");
            } else {
                let _ = fs::remove_file(&exclude);
            }
            let baseline = repo.capture().unwrap();
            assert_eq!(baseline.exclude_hash.is_some(), initially_present);
            append(&exclude, "/hidden/");
            repo.write("hidden/conftest.py", "import builtins\n");
            assert!(repo.ignored("hidden/conftest.py"));
            let after = repo.capture().unwrap();
            assert!(
                !after
                    .files
                    .keys()
                    .chain(after.ignored.keys())
                    .any(|p| p.starts_with("hidden")),
                "Git prunes the newly hidden directory"
            );
            assert_ne!(after.exclude_hash, baseline.exclude_hash);
            assert_eq!(
                SourceSnapshot {
                    exclude_hash: baseline.exclude_hash.clone(),
                    ..after.clone()
                },
                baseline,
                "the exclude-rules hash is the only observable change"
            );
            assert_exclude_drift(&baseline, &after, &repo.root);
        }
    }

    /// The ordinary case keeps Issue #3's semantics: with stable exclude rules,
    /// churn inside a directory they ignore as a whole (here a build tree past the
    /// whole content budget) is neither drift nor a diff change.
    #[test]
    fn stable_info_exclude_with_ignored_directory_churn_is_not_drift() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.commit();
        append(&exclude_file(&repo, &repo.root), "/target/");
        repo.write("target/debug/app", "1");
        let baseline = repo.capture().unwrap();
        assert!(baseline.exclude_hash.is_some());
        repo.write("target/debug/app", "2");
        repo.write("target/debug/deps/new.rlib", "new");
        fs::File::create(repo.root.join("target/debug/libhuge.rlib"))
            .unwrap()
            .set_len(WORKSPACE_CAPTURE_BYTES + 1)
            .unwrap();
        let after = repo.capture().unwrap();
        assert_eq!(after, baseline);
        let diff = diff(
            &baseline,
            &after,
            &PlanId::new("plan:exclude").unwrap(),
            None,
            None,
            &ProjectConfig::load(&repo.root).unwrap(),
            &repo.artifacts,
        )
        .unwrap();
        assert!(diff.changes.is_empty());
    }

    /// In a linked worktree `.git` is a file, and Git applies the common Git
    /// directory's `info/exclude`, not one under the worktree's own Git directory.
    /// Capture follows Git's resolution: editing the shared rules is drift for the
    /// worktree, while a stray per-worktree file that Git ignores hides nothing.
    #[test]
    fn linked_worktrees_observe_the_shared_info_exclude() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.commit();
        let wt = repo.base.join("wt");
        repo.git(&["worktree", "add", "--quiet", wt.to_str().unwrap()]);
        assert!(fs::symlink_metadata(wt.join(".git")).unwrap().is_file());
        let shared = exclude_file(&repo, &wt);
        assert_eq!(shared, exclude_file(&repo, &repo.root));
        let observe = || capture(&wt, &repo.artifacts).unwrap();
        let output = repo
            .command(&wt, &["rev-parse", "--absolute-git-dir"])
            .output()
            .unwrap();
        let own = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
            .join("info/exclude");
        assert_ne!(own, shared);
        append(&own, "/only-here/");
        fs::create_dir_all(wt.join("only-here")).unwrap();
        fs::write(wt.join("only-here/x.txt"), "still source\n").unwrap();
        let baseline = observe();
        assert!(baseline.files.contains_key("only-here/x.txt"));
        append(&shared, "/hidden/");
        fs::create_dir_all(wt.join("hidden")).unwrap();
        fs::write(wt.join("hidden/p.txt"), "hidden\n").unwrap();
        let after = observe();
        assert!(!after.files.contains_key("hidden/p.txt"));
        assert_exclude_drift(&baseline, &after, &wt);
    }

    /// A baseline stored by 0.1.0-alpha.2 predates `ignored` and `exclude_hash`
    /// and could hold ignored build output as content. It still decodes, but it
    /// never equals a capture under the current policy, and diffing against it
    /// fails. Every stored-baseline gate (resume, adoption, integration)
    /// therefore fails closed as `SOURCE_DRIFT` instead of trusting it.
    #[test]
    fn an_alpha2_baseline_decodes_and_fails_closed() {
        let repo = Repo::new();
        repo.write(".gitignore", "/target/\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        repo.commit();
        repo.write("target/debug/app", "binary");
        let current = repo.capture().unwrap();
        assert!(current.ignored.is_empty() && current.exclude_hash.is_some());
        let mut stored = serde_json::to_value(&current).unwrap();
        let fields = stored.as_object_mut().unwrap();
        assert!(fields.remove("ignored").is_none());
        fields.remove("exclude_hash");
        stored["files"]["target/debug/app"] = serde_json::to_value(FileState {
            content: repo.artifacts.put(b"binary").unwrap(),
            mode: 0o644,
        })
        .unwrap();
        let alpha2: SourceSnapshot = serde_json::from_value(stored).unwrap();
        assert_ne!(alpha2, current);
        assert_exclude_drift(&alpha2, &current, &repo.root);
    }
}
