//! Accepted source identity and exact, file-granular recovery.
//!
//! Source is what Git considers repository content (tracked, or untracked
//! and not ignored) within the configured source roots, excluding agentctl's
//! `.agentctl/` and anything in a `.git` entry. Only regular files carry
//! source content; symlinks, submodules and other entries are never captured.
//!
//! Paths are opaque literal strings: canonical, `/`-separated and relative to
//! the project root. Characters such as `[slug]`, `(group)`, `*` or `:` are
//! part of a name, never a pattern, and Git is told so.
//!
//! Accepted bytes live in the content-addressed object store; the `Store`
//! records which content, or accepted absence, each tracked path has. Only
//! this module establishes accepted state, and it records content only once
//! its object is durably published.
//!
//! Every path is resolved from the project root through real directories:
//! a symlink (or other reparse point) is never followed, and one at a path,
//! like a directory, is not file content. Checks and uses are separate system
//! calls, so a concurrent process swapping an ancestor directory for a
//! symlink between them is not excluded; closing that requires
//! handle-relative resolution, hence `ConfinedResolution` is only best effort.

mod objects;
mod snapshot;
mod workspace;

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::platform;
use crate::project::{Project, STATE_DIR};
use crate::state::{GenerationId, Store, check_path};
use objects::Objects;
pub(crate) use snapshot::{Snapshot, observe_paths, snapshot, snapshot_paths};
pub(crate) use workspace::{Installation, Preparation, Workspace, restore_candidate};

/// How a path's working state compares with its accepted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    Identical,
    /// Accepted content exists but the path holds different bytes or is not
    /// a regular file.
    Modified,
    /// Accepted content exists but the path does not.
    Missing,
    /// The path is accepted as absent but exists.
    Present,
}

/// Establishes the baseline accepted state of every eligible source file that
/// has no accepted state yet, returning the newly accepted paths. Paths that
/// already have accepted state keep it.
pub fn baseline(project: &Project, store: &mut Store) -> Result<Vec<String>> {
    let objects = Objects::open(&project.root.join(STATE_DIR))?;
    let mut accepted = Vec::new();
    for path in discover(project)? {
        if store.accepted_source(&path)?.is_some() {
            continue;
        }
        let Entry::File(file) = entry(&project.root, &path)? else {
            continue;
        };
        let hash = objects
            .publish(file)
            .with_context(|| format!("capturing `{path}`"))?;
        accepted.push((path, hash));
    }
    if accepted.is_empty() {
        return Ok(Vec::new());
    }
    // State may reference only objects that are already durable.
    objects.sync()?;
    let sources: Vec<(&str, Option<&str>)> = accepted
        .iter()
        .map(|(path, hash)| (path.as_str(), Some(hash.as_str())))
        .collect();
    store.record_baseline(&sources)?;
    Ok(accepted.into_iter().map(|(path, _)| path).collect())
}

/// Accepts eligible paths that do not exist and have no accepted state as
/// absent, so that restoring them removes whatever is later created there.
/// When a path becomes tracked is for the caller's lifecycle to decide.
pub fn accept_absent(project: &Project, store: &mut Store, paths: &[&str]) -> Result<()> {
    for &path in paths {
        check_source(project, path)?;
        ensure!(
            matches!(entry(&project.root, path)?, Entry::Absent),
            "`{path}` exists, so its absence cannot be accepted"
        );
    }
    if let Some(path) = ignored(project, paths)? {
        bail!("`{path}` is ignored by Git, so it is not source");
    }
    let sources: Vec<(&str, Option<&str>)> = paths.iter().map(|&path| (path, None)).collect();
    store.record_baseline(&sources)
}

/// Accepts an active generation that never executed, establishing the
/// current working state of each eligible path in `paths` as its accepted
/// state: a regular file's bytes, or absence. An executor's candidate is
/// accepted only once verified, through `crate::acceptance`, which never
/// reads accepted identities from the working tree.
pub fn accept_generation(
    project: &Project,
    store: &mut Store,
    generation: GenerationId,
    paths: &[&str],
) -> Result<()> {
    for &path in paths {
        check_source(project, path)?;
    }
    if let Some(path) = ignored(project, paths)? {
        bail!("`{path}` is ignored by Git, so it is not source");
    }
    let objects = Objects::open(&project.root.join(STATE_DIR))?;
    let mut hashes = Vec::with_capacity(paths.len());
    for &path in paths {
        hashes.push(match entry(&project.root, path)? {
            Entry::File(file) => Some(
                objects
                    .publish(file)
                    .with_context(|| format!("capturing `{path}`"))?,
            ),
            Entry::Absent => None,
            Entry::Other => bail!("`{path}` is not a regular file"),
        });
    }
    // State may reference only objects that are already durable.
    objects.sync()?;
    let sources: Vec<(&str, Option<&str>)> = paths
        .iter()
        .zip(&hashes)
        .map(|(&path, hash)| (path, hash.as_deref()))
        .collect();
    store.accept_generation(generation, &sources)
}

/// Compares one path's working state with its accepted state by content.
pub fn drift(project: &Project, store: &Store, path: &str) -> Result<Drift> {
    let accepted = accepted(project, store, path)?;
    Ok(match (accepted, entry(&project.root, path)?) {
        (None, Entry::Absent) => Drift::Identical,
        (None, _) => Drift::Present,
        (Some(_), Entry::Absent) => Drift::Missing,
        (Some(_), Entry::Other) => Drift::Modified,
        (Some(hash), Entry::File(file)) => {
            if objects::hash(file)? == hash {
                Drift::Identical
            } else {
                Drift::Modified
            }
        }
    })
}

/// Restores exactly the given paths to their accepted state, touching nothing
/// else: accepted content is written back byte for byte, and whatever exists
/// at an accepted-absent path is removed, unless it is a directory. Each file
/// is replaced atomically, but paths are restored one at a time; on error,
/// earlier paths stay restored.
pub fn restore(project: &Project, store: &Store, paths: &[&str]) -> Result<()> {
    let accepted = paths
        .iter()
        .map(|path| accepted(project, store, path))
        .collect::<Result<Vec<_>>>()?;
    let objects = Objects::open(&project.root.join(STATE_DIR))?;
    for (path, hash) in paths.iter().zip(accepted) {
        match hash {
            Some(hash) => write_object(&project.root, &objects, path, &hash),
            None => remove(&project.root, path),
        }
        .with_context(|| format!("restoring `{path}`"))?;
    }
    Ok(())
}

/// A source path's accepted content, as read from its recovery object and
/// verified against its accepted hash. Only this module constructs it, so
/// holding one means the bytes are the accepted content named by the hash.
#[derive(Debug)]
pub struct AcceptedContent {
    hash: String,
    bytes: Vec<u8>,
}

impl AcceptedContent {
    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Reads the accepted content of a tracked source path from its recovery
/// object, never from the working tree. Fails for accepted absence, and for
/// a missing or corrupt object.
pub fn read_accepted(project: &Project, store: &Store, path: &str) -> Result<AcceptedContent> {
    let hash = accepted(project, store, path)?
        .with_context(|| format!("`{path}` is accepted as absent"))?;
    let mut bytes = Vec::new();
    Objects::open(&project.root.join(STATE_DIR))?.copy_to(&hash, &mut bytes)?;
    Ok(AcceptedContent { hash, bytes })
}

/// The length of the accepted content named `hash`.
pub(crate) fn content_len(project: &Project, hash: &str) -> Result<u64> {
    Objects::open(&project.root.join(STATE_DIR))?.len(hash)
}

/// The bytes of recovery object `hash`, verified, unless there are more
/// than `limit` of them.
pub(crate) fn read_object(project: &Project, hash: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let objects = Objects::open(&project.root.join(STATE_DIR))?;
    if objects.len(hash)? > limit {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    objects.copy_to(hash, &mut bytes)?;
    Ok(Some(bytes))
}

/// Fails unless recovery object `hash` is available and its bytes hash to
/// `hash`.
pub(crate) fn check_object(project: &Project, hash: &str) -> Result<()> {
    Objects::open(&project.root.join(STATE_DIR))?.copy_to(hash, &mut io::sink())
}

/// Makes every recovery object published so far durable, as far as the
/// platform allows.
pub(crate) fn sync_objects(project: &Project) -> Result<()> {
    Objects::open(&project.root.join(STATE_DIR))?.sync()
}

/// The accepted content hash of a tracked source path, `None` for accepted
/// absence.
fn accepted(project: &Project, store: &Store, path: &str) -> Result<Option<String>> {
    check_source(project, path)?;
    let source = store
        .accepted_source(path)?
        .with_context(|| format!("`{path}` has no accepted state"))?;
    Ok(source.hash)
}

fn write_object(root: &Path, objects: &Objects, path: &str, hash: &str) -> Result<()> {
    let mut permissions = None;
    if let Some(dir) = parent(root, path, false)?
        && let Entry::File(file) = open(&dir.join(name(path)))?
    {
        permissions = Some(file.metadata()?.permissions());
        if objects::hash(file)? == hash {
            return Ok(());
        }
    }
    // Verify the accepted bytes in full before touching the working tree.
    let mut staged = objects.stage()?;
    objects.copy_to(hash, staged.as_file_mut())?;
    if let Some(permissions) = permissions {
        staged.as_file().set_permissions(permissions)?;
    }
    platform::write_back(staged.as_file())?;
    let dir = parent(root, path, true)?.context("parent directory vanished")?;
    platform::replace(staged, &dir.join(name(path)))?;
    objects::sync_dir(&dir)
}

fn remove(root: &Path, path: &str) -> Result<()> {
    let Some(dir) = parent(root, path, false)? else {
        return Ok(());
    };
    let target = dir.join(name(path));
    match fs::symlink_metadata(&target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(meta) if meta.is_dir() => bail!("`{path}` is a directory; refusing to remove it"),
        Ok(_) => match fs::remove_file(&target) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    objects::sync_dir(&dir)
}

/// Refuses paths that cannot be source: non-canonical, outside every source
/// root, inside agentctl's or Git's own state, or not addressable as literal
/// names on this platform. ASCII case is ignored for state, as
/// case-insensitive filesystems do.
pub(crate) fn check_source(project: &Project, path: &str) -> Result<()> {
    check_path(path)?;
    ensure!(
        project
            .config
            .codegraph
            .roots
            .iter()
            .any(|root| root.contains_path(path)),
        "`{path}` is outside the configured source roots"
    );
    ensure!(!reserved(path), "`{path}` is agentctl or Git state");
    ensure!(
        path.split('/').all(platform::literal_name),
        "`{path}` cannot be addressed literally on this platform"
    );
    Ok(())
}

fn reserved(path: &str) -> bool {
    let top = path.split('/').next().unwrap_or(path);
    top.eq_ignore_ascii_case(STATE_DIR)
        || path
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".git"))
}

/// Eligible source paths within the configured roots, as Git lists them.
fn discover(project: &Project) -> Result<Vec<String>> {
    let output = git(
        project,
        Command::new("git")
            .args(["--literal-pathspecs", "ls-files", "-z"])
            .args(["--cached", "--others", "--exclude-standard", "--"])
            .args(project.config.codegraph.roots.iter().map(|r| r.as_str())),
        b"",
    )?;
    ensure!(output.status.success(), "{}", stderr(&output));
    let mut paths = Vec::new();
    for raw in output.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| anyhow!("`{}` is not a UTF-8 path", String::from_utf8_lossy(raw)))?;
        // Untracked nested repositories are listed as directories.
        if path.ends_with('/') || reserved(path) {
            continue;
        }
        check_source(project, path)?;
        paths.push(path.to_owned());
    }
    // Unmerged index entries are listed once per stage.
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// The first of `paths` that Git ignores, if any.
fn ignored(project: &Project, paths: &[&str]) -> Result<Option<String>> {
    Ok(ignored_among(project, paths)?.into_iter().next())
}

/// Those of `paths` that Git ignores, in the order given. A tracked path is
/// never ignored.
fn ignored_among(project: &Project, paths: &[&str]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // `check-ignore` matches each path as a name, but refuses the literal
    // pathspec flag. A `./` prefix keeps a leading `:` from being read as
    // pathspec magic.
    let input: String = paths.iter().map(|path| format!("./{path}\0")).collect();
    let output = git(
        project,
        Command::new("git").args(["check-ignore", "-z", "--stdin"]),
        input.as_bytes(),
    )?;
    match output.status.code() {
        Some(0) => Ok(output
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|path| {
                let path = String::from_utf8_lossy(path);
                path.strip_prefix("./").unwrap_or(&path).to_owned()
            })
            .collect()),
        Some(1) => Ok(Vec::new()),
        _ => bail!("{}", stderr(&output)),
    }
}

/// Runs a Git command in the project root with `input` as its standard
/// input, isolated from environment that could redirect it to another
/// repository or read paths as patterns.
fn git(project: &Project, command: &mut Command, input: &[u8]) -> Result<Output> {
    command
        .current_dir(&project.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        command.env_remove(var);
    }
    let mut child = command.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => anyhow!("source discovery requires Git, which was not found"),
        _ => e.into(),
    })?;
    let mut stdin = child.stdin.take().context("no stdin for git")?;
    // Written concurrently, so Git filling its output pipe cannot deadlock.
    let (output, written) = thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(input));
        let output = child.wait_with_output();
        (
            output,
            writer.join().expect("writing to git does not panic"),
        )
    });
    let output = output?;
    if !output.status.success() && stderr(&output).contains("not a git repository") {
        bail!(
            "{} is not in a Git repository; source discovery requires one",
            project.root.display()
        );
    }
    written.with_context(|| format!("writing to git: {}", stderr(&output)))?;
    Ok(output)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

/// A path's working entry, examined without following symlinks.
enum Entry {
    Absent,
    File(File),
    /// A directory, symlink or other non-regular entry.
    Other,
}

fn entry(root: &Path, path: &str) -> Result<Entry> {
    match parent(root, path, false)? {
        Some(dir) => open(&dir.join(name(path))),
        None => Ok(Entry::Absent),
    }
}

fn open(path: &Path) -> Result<Entry> {
    match platform::open_regular(path) {
        Ok(Some(file)) => Ok(Entry::File(file)),
        Ok(None) => Ok(Entry::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Entry::Absent),
        Err(e) => Err(e).with_context(|| format!("opening {}", path.display())),
    }
}

/// The directory holding `path`, reached from `root` through real
/// directories only. `None` when an ancestor is missing or not a directory,
/// so `path` cannot exist; with `create`, missing ancestors are created
/// instead. An ancestor symlink is always refused.
fn parent(root: &Path, path: &str, create: bool) -> Result<Option<PathBuf>> {
    let mut dir = root.to_path_buf();
    let ancestors = path.rsplit_once('/').map_or("", |(ancestors, _)| ancestors);
    for part in ancestors.split('/').filter(|part| !part.is_empty()) {
        dir.push(part);
        loop {
            match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => break,
                Ok(meta) if meta.is_symlink() => {
                    bail!("{} is a symlink; refusing to follow it", dir.display())
                }
                Ok(_) if create => bail!("{} is not a directory", dir.display()),
                Ok(_) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    match fs::create_dir(&dir) {
                        // Whatever now exists is examined again.
                        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                        _ => {}
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(Some(dir))
}

fn name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;
    use crate::state::tests::objective;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        project: Project,
        store: Store,
    }

    impl Fixture {
        /// A Git repository holding a project with the given source roots.
        fn new(roots: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            run_git(dir.path(), &["init", "-q"]);
            let mut config = sample();
            config.codegraph.roots = roots.parse().unwrap();
            let project = Project::create(dir.path(), config).unwrap();
            let store = project.hydrate().unwrap();
            Self {
                _dir: dir,
                project,
                store,
            }
        }

        fn root(&self) -> &Path {
            &self.project.root
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.root().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.root().join(path)).unwrap()
        }

        fn exists(&self, path: &str) -> bool {
            fs::symlink_metadata(self.root().join(path)).is_ok()
        }

        fn baseline(&mut self) -> Vec<String> {
            baseline(&self.project, &mut self.store).unwrap()
        }

        fn drift(&self, path: &str) -> Drift {
            drift(&self.project, &self.store, path).unwrap()
        }

        fn restore(&self, paths: &[&str]) -> Result<()> {
            restore(&self.project, &self.store, paths)
        }

        fn accept_absent(&mut self, paths: &[&str]) -> Result<()> {
            accept_absent(&self.project, &mut self.store, paths)
        }

        /// Starts a generation of a new task, to be accepted.
        fn generation(&mut self) -> GenerationId {
            let plan = self.store.create_plan(&objective("change")).unwrap();
            let task = self.store.add_task(plan, "change", &[]).unwrap();
            self.store.start_generation(task).unwrap()
        }

        fn accept(&mut self, generation: GenerationId, paths: &[&str]) -> Result<()> {
            accept_generation(&self.project, &mut self.store, generation, paths)
        }

        fn objects(&self) -> Vec<(String, Vec<u8>)> {
            let dir = self.root().join(STATE_DIR).join("objects");
            let mut objects: Vec<_> = fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .map(|e| {
                            let e = e.unwrap();
                            (
                                e.file_name().into_string().unwrap(),
                                fs::read(e.path()).unwrap(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            objects.sort();
            objects
        }

        fn object_path(&self, bytes: &[u8]) -> PathBuf {
            self.root()
                .join(STATE_DIR)
                .join("objects")
                .join(sha256(bytes))
        }
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn sorted(paths: &[&str]) -> Vec<String> {
        let mut paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        paths.sort();
        paths
    }

    fn fails(result: Result<impl std::fmt::Debug>, expected: &str) {
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains(expected), "{message}");
    }

    #[test]
    fn baseline_captures_only_eligible_source() {
        let mut fx = Fixture::new("src, crates/core");
        fx.write(".gitignore", b"target/\n*.log\n");
        fx.write("src/main.rs", b"fn main() {}\n");
        fx.write("crates/core/src/lib.rs", b"pub fn core() {}\n");
        run_git(fx.root(), &["add", "src/main.rs", "crates/core/src/lib.rs"]);
        fx.write("src/untracked.rs", b"// new, not ignored\n");
        // The old failure class: a generated artifact beneath a source root.
        let artifact = b"\x7fELF generated build artifact";
        fx.write("crates/core/target/debug/libcore.rlib", artifact);
        fx.write("src/build.log", b"generated log");
        fx.write("docs/guide.md", b"outside the roots");
        fx.write("crates/core2/lib.rs", b"a sibling sharing a prefix");
        fx.write("src/vendor/lib.rs", b"inside a nested repository");
        run_git(&fx.root().join("src/vendor"), &["init", "-q"]);

        let expected = ["crates/core/src/lib.rs", "src/main.rs", "src/untracked.rs"];
        assert_eq!(fx.baseline(), sorted(&expected));
        for path in expected {
            let source = fx.store.accepted_source(path).unwrap().unwrap();
            assert_eq!(source.hash, Some(sha256(&fx.read(path))));
            assert_eq!(source.generation, None);
        }
        for path in [
            "crates/core/target/debug/libcore.rlib",
            "src/build.log",
            "docs/guide.md",
            "crates/core2/lib.rs",
            "src/vendor/lib.rs",
        ] {
            assert_eq!(fx.store.accepted_source(path).unwrap(), None, "{path}");
        }
        assert!(!fx.object_path(artifact).exists());
        assert_eq!(fx.objects().len(), expected.len());

        // A repeated baseline captures only sources that are new since.
        fx.write("src/main.rs", b"changed after acceptance");
        fx.write("src/later.rs", b"added later");
        assert_eq!(fx.baseline(), ["src/later.rs"]);
        assert_eq!(fx.drift("src/main.rs"), Drift::Modified);
    }

    #[test]
    fn state_and_git_directories_are_never_captured() {
        let mut fx = Fixture::new(".");
        fx.write("src/a.rs", b"a");
        run_git(fx.root(), &["add", "-f", ".agentctl/state.db"]);
        fx.write("vendor/lib.rs", b"nested");
        run_git(&fx.root().join("vendor"), &["init", "-q"]);

        assert_eq!(
            fx.baseline(),
            sorted(&[".gitignore", "agentctl.toml", "src/a.rs"])
        );
        for bad in [
            ".agentctl/state.db",
            ".AgentCtl/x",
            "src/.git/config",
            "a/.GIT",
        ] {
            fails(drift(&fx.project, &fx.store, bad), "agentctl or Git state");
            fails(fx.accept_absent(&[bad]), "agentctl or Git state");
        }
    }

    #[test]
    fn requires_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path(), sample()).unwrap();
        let mut store = project.hydrate().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), b"a").unwrap();
        fails(baseline(&project, &mut store), "not in a Git repository");
        assert_eq!(store.accepted_source("src/a.rs").unwrap(), None);
    }

    /// Windows reads `*`, `:` and `\` in names, so it refuses them instead.
    #[cfg(unix)]
    #[test]
    fn literal_paths_round_trip() {
        let mut fx = Fixture::new("src, app/(customer)/s/[slug]");
        let paths = [
            "app/(customer)/s/[slug]/actions.ts",
            "src/[id].ts",
            "src/i.ts",
            "src/@modal/(.)photo/page.tsx",
            "src/with space.rs",
            "src/a+b.rs",
            "src/ünïcødé/文件.rs",
            "src/*.rs",
            "src/:colon.rs",
            "src/back\\slash.rs",
        ];
        for path in paths {
            fx.write(path, format!("accepted {path}").as_bytes());
        }
        // Matches `app/(customer)/s/[slug]` only if the root were a glob.
        fx.write("app/(customer)/s/s/decoy.ts", b"decoy");

        assert_eq!(fx.baseline(), sorted(&paths));
        for path in paths {
            fx.write(path, b"candidate");
            assert_eq!(fx.drift(path), Drift::Modified, "{path}");
        }
        fx.restore(&paths).unwrap();
        for path in paths {
            assert_eq!(fx.read(path), format!("accepted {path}").as_bytes());
            assert_eq!(fx.drift(path), Drift::Identical, "{path}");
        }

        // Restoring `src/*.rs` or `src/[id].ts` touches only that file.
        fx.write("src/a+b.rs", b"unrelated work");
        fx.write("src/i.ts", b"unrelated work");
        fx.write("src/*.rs", b"candidate");
        fx.write("src/[id].ts", b"candidate");
        fx.restore(&["src/*.rs", "src/[id].ts"]).unwrap();
        assert_eq!(fx.read("src/a+b.rs"), b"unrelated work");
        assert_eq!(fx.read("src/i.ts"), b"unrelated work");

        fs::remove_dir_all(fx.root().join("src/ünïcødé")).unwrap();
        assert_eq!(fx.drift("src/ünïcødé/文件.rs"), Drift::Missing);
        fx.restore(&["src/ünïcødé/文件.rs"]).unwrap();
        assert_eq!(
            fx.read("src/ünïcødé/文件.rs"),
            "accepted src/ünïcødé/文件.rs".as_bytes()
        );

        let absent = ["src/[new].ts", "src/:new.rs", "src/(g)/[id]/new page.tsx"];
        fx.accept_absent(&absent).unwrap();
        for path in absent {
            fx.write(path, b"created");
            assert_eq!(fx.drift(path), Drift::Present, "{path}");
        }
        fx.restore(&absent).unwrap();
        for path in absent {
            assert!(!fx.exists(path), "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn names_windows_would_reinterpret_are_refused() {
        let mut fx = Fixture::new("src");
        for path in [
            "src/*.rs",
            "src/:colon.rs",
            "src/back\\slash.rs",
            "src/C:x",
            "src/GIT~1/config",
        ] {
            fails(fx.accept_absent(&[path]), "cannot be addressed");
            fails(fx.restore(&[path]), "cannot be addressed");
        }
        fx.accept_absent(&["src/[new].ts", "src/(g)/[id]/new page.tsx"])
            .unwrap();
    }

    #[test]
    fn detects_drift_and_restores_exact_bytes() {
        let mut fx = Fixture::new("src");
        let a: &[u8] = b"binary\0\xff\xfe\r\nbytes";
        fx.write("src/a.rs", a);
        fx.write("src/nested/b.rs", b"b");
        fx.write("src/c.rs", b"c");
        fx.write("src/unrelated.rs", b"unrelated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = fs::Permissions::from_mode(0o755);
            fs::set_permissions(fx.root().join("src/a.rs"), executable).unwrap();
        }
        fx.baseline();
        fx.accept_absent(&["src/new.rs"]).unwrap();
        for path in ["src/a.rs", "src/nested/b.rs", "src/c.rs", "src/new.rs"] {
            assert_eq!(fx.drift(path), Drift::Identical, "{path}");
        }

        // Same length, different content.
        let mut modified = a.to_vec();
        modified[0] = b'B';
        fx.write("src/a.rs", &modified);
        fs::remove_dir_all(fx.root().join("src/nested")).unwrap();
        fx.write("src/new.rs", b"created by a candidate");
        fx.write("src/unrelated.rs", b"concurrent work");
        assert_eq!(fx.drift("src/a.rs"), Drift::Modified);
        assert_eq!(fx.drift("src/nested/b.rs"), Drift::Missing);
        assert_eq!(fx.drift("src/new.rs"), Drift::Present);

        fx.restore(&["src/a.rs", "src/nested/b.rs", "src/new.rs"])
            .unwrap();
        assert_eq!(fx.read("src/a.rs"), a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(fx.root().join("src/a.rs"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "a replaced file keeps its mode");
        }
        assert_eq!(fx.read("src/nested/b.rs"), b"b");
        assert!(!fx.exists("src/new.rs"));
        assert_eq!(fx.read("src/unrelated.rs"), b"concurrent work");
        assert_eq!(fx.read("src/c.rs"), b"c");
        let staging = fx.root().join(STATE_DIR).join("tmp");
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);

        // A path that is not a file is modified, and never removed.
        fs::remove_file(fx.root().join("src/c.rs")).unwrap();
        fx.write("src/c.rs/inner", b"x");
        assert_eq!(fx.drift("src/c.rs"), Drift::Modified);
        assert!(fx.restore(&["src/c.rs"]).is_err());
        assert_eq!(fx.read("src/c.rs/inner"), b"x");
        fs::create_dir(fx.root().join("src/new.rs")).unwrap();
        assert_eq!(fx.drift("src/new.rs"), Drift::Present);
        fails(fx.restore(&["src/new.rs"]), "is a directory");
        assert!(fx.root().join("src/new.rs").is_dir());

        // Untracked paths have no accepted state to restore to.
        fails(fx.restore(&["src/unrelated2.rs"]), "no accepted state");
        fx.write("src/untracked.rs", b"keep");
        fails(fx.restore(&["src/untracked.rs"]), "no accepted state");
        assert_eq!(fx.read("src/untracked.rs"), b"keep");
        fails(fx.accept_absent(&["src/untracked.rs"]), "exists");
        fails(fx.accept_absent(&["src/a.rs"]), "exists");
        fails(fx.accept_absent(&["src/new.rs"]), "exists");
    }

    #[test]
    fn accepted_generations_publish_content_before_recording_it() {
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        fx.write("src/b.rs", b"b");
        fx.baseline();
        let generation = fx.generation();
        fx.write("src/a.rs", b"changed a");
        fx.write("src/c.rs", b"new c");
        fs::remove_file(fx.root().join("src/b.rs")).unwrap();
        fx.accept(generation, &["src/a.rs", "src/b.rs", "src/c.rs"])
            .unwrap();

        let source = |path| fx.store.accepted_source(path).unwrap().unwrap();
        assert_eq!(source("src/a.rs").hash, Some(sha256(b"changed a")));
        assert_eq!(source("src/a.rs").generation, Some(generation));
        assert_eq!(
            source("src/b.rs").hash,
            None,
            "a deletion is accepted absence"
        );
        assert_eq!(
            fs::read(fx.object_path(b"changed a")).unwrap(),
            b"changed a"
        );
        assert_eq!(fs::read(fx.object_path(b"new c")).unwrap(), b"new c");
        for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            assert_eq!(fx.drift(path), Drift::Identical);
        }

        fx.write("src/a.rs", b"candidate");
        fx.write("src/b.rs", b"recreated");
        fs::remove_file(fx.root().join("src/c.rs")).unwrap();
        assert_eq!(fx.drift("src/b.rs"), Drift::Present);
        fx.restore(&["src/a.rs", "src/b.rs", "src/c.rs"]).unwrap();
        assert_eq!(fx.read("src/a.rs"), b"changed a");
        assert!(!fx.exists("src/b.rs"));
        assert_eq!(fx.read("src/c.rs"), b"new c");
        fails(fx.accept(generation, &[]), "already ended");
    }

    #[test]
    fn unpublished_content_is_never_accepted() {
        let mut fx = Fixture::new("src");
        fx.write(".gitignore", b"target/\n");
        fx.write("src/a.rs", b"a");
        fx.write("src/b.rs", b"b");
        fs::create_dir_all(fx.root().join("src/dir")).unwrap();
        let generation = fx.generation();
        // An object already present under the hash of `src/b.rs` fails
        // verification, so that content has no published object.
        let planted = fx.object_path(b"b");
        fs::create_dir_all(planted.parent().unwrap()).unwrap();
        fs::write(&planted, b"not b").unwrap();

        fails(fx.accept(generation, &["src/a.rs", "src/b.rs"]), "corrupt");
        fails(
            fx.accept(generation, &["src/a.rs", "src/dir"]),
            "not a regular file",
        );
        fails(
            fx.accept(generation, &["src/a.rs", "src/target/x"]),
            "ignored by Git",
        );
        fails(fx.accept(generation, &["src/a.rs", "other/x"]), "outside");
        for path in ["src/a.rs", "src/b.rs"] {
            assert_eq!(fx.store.accepted_source(path).unwrap(), None);
        }
        fx.accept(generation, &["src/a.rs"]).unwrap();
        assert_eq!(fx.drift("src/a.rs"), Drift::Identical);
    }

    #[test]
    fn ignored_paths_cannot_be_accepted_absent() {
        let mut fx = Fixture::new("src");
        fx.write(".gitignore", b"target/\n");
        fails(
            fx.accept_absent(&["src/ok.rs", "src/target/x.o"]),
            "ignored by Git",
        );
        assert_eq!(fx.store.accepted_source("src/ok.rs").unwrap(), None);
    }

    #[test]
    fn identical_contents_share_one_verified_object() {
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"same");
        fx.write("src/b/c.rs", b"same");
        fx.write("src/d.rs", b"");
        fx.write("src/e.rs", b"other");
        fx.baseline();
        let objects = fx.objects();
        assert_eq!(objects.len(), 3);
        for (name, bytes) in &objects {
            assert_eq!(name, &sha256(bytes));
        }
        assert_eq!(
            fx.store.accepted_source("src/a.rs").unwrap(),
            fx.store.accepted_source("src/b/c.rs").unwrap()
        );
    }

    #[test]
    fn rejects_paths_outside_the_source_boundary() {
        let outer = tempfile::tempdir().unwrap();
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        fx.write("docs/a.md", b"docs");
        fx.baseline();
        let escape = format!("{}/victim", outer.path().display());
        fs::write(&escape, b"victim").unwrap();
        for bad in [
            "",
            "/etc/passwd",
            escape.as_str(),
            "../victim",
            "src/../../victim",
            "src/./a.rs",
            "./src/a.rs",
            "src//a.rs",
            "src/",
            "docs/a.md",
            "srcx/a.rs",
        ] {
            assert!(drift(&fx.project, &fx.store, bad).is_err(), "{bad:?}");
            assert!(fx.restore(&[bad]).is_err(), "{bad:?}");
            assert!(fx.accept_absent(&[bad]).is_err(), "{bad:?}");
        }
        // A bad path anywhere in the request restores nothing.
        fx.write("src/a.rs", b"candidate");
        assert!(fx.restore(&["src/a.rs", "../victim"]).is_err());
        assert_eq!(fx.read("src/a.rs"), b"candidate");
        assert_eq!(fs::read(&escape).unwrap(), b"victim");
    }

    /// Exercises the Unix backend end to end; the Windows backend's reparse
    /// handling is tested in `platform::windows`.
    #[cfg(unix)]
    #[test]
    fn symlinks_never_lead_outside_the_project() {
        use std::os::unix::fs::symlink;
        let outer = tempfile::tempdir().unwrap();
        let secret = outer.path().join("secret");
        fs::write(&secret, b"accepted a").unwrap();
        fs::create_dir(outer.path().join("dir")).unwrap();
        fs::write(outer.path().join("dir/b.rs"), b"outside b").unwrap();

        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"accepted a");
        fx.write("src/d/b.rs", b"accepted b");
        symlink(&secret, fx.root().join("src/link.rs")).unwrap();
        symlink(outer.path().join("dir"), fx.root().join("src/linked")).unwrap();
        assert_eq!(fx.baseline(), ["src/a.rs", "src/d/b.rs"]);
        assert_eq!(fx.objects().len(), 2);
        assert!(!fx.object_path(b"outside b").exists());

        // A symlink at an accepted path is not its content, even when its
        // target holds identical bytes; restoring replaces only the link.
        fs::remove_file(fx.root().join("src/a.rs")).unwrap();
        symlink(&secret, fx.root().join("src/a.rs")).unwrap();
        assert_eq!(fx.drift("src/a.rs"), Drift::Modified);
        fs::write(&secret, b"outside").unwrap();
        fx.restore(&["src/a.rs"]).unwrap();
        assert!(
            fs::symlink_metadata(fx.root().join("src/a.rs"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fx.read("src/a.rs"), b"accepted a");
        assert_eq!(fs::read(&secret).unwrap(), b"outside");

        // A directory swapped for a symlink is never traversed.
        fs::remove_dir_all(fx.root().join("src/d")).unwrap();
        symlink(outer.path().join("dir"), fx.root().join("src/d")).unwrap();
        fails(drift(&fx.project, &fx.store, "src/d/b.rs"), "symlink");
        fails(fx.restore(&["src/d/b.rs"]), "symlink");
        fails(fx.accept_absent(&["src/d/new.rs"]), "symlink");
        assert_eq!(
            fs::read(outer.path().join("dir/b.rs")).unwrap(),
            b"outside b"
        );
        assert_eq!(fs::read_dir(outer.path().join("dir")).unwrap().count(), 1);

        // Restoring accepted absence removes a symlink, not its target.
        fx.accept_absent(&["src/new.rs"]).unwrap();
        symlink(&secret, fx.root().join("src/new.rs")).unwrap();
        assert_eq!(fx.drift("src/new.rs"), Drift::Present);
        fx.restore(&["src/new.rs"]).unwrap();
        assert!(!fx.exists("src/new.rs"));
        assert_eq!(fs::read(&secret).unwrap(), b"outside");
    }

    #[test]
    fn failed_publication_records_nothing() {
        // An object already present under a hash must hold those bytes.
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        fx.write("src/b.rs", b"b");
        let planted = fx.object_path(b"b");
        fs::create_dir_all(planted.parent().unwrap()).unwrap();
        fs::write(&planted, b"not b").unwrap();
        fails(baseline(&fx.project, &mut fx.store), "corrupt");
        assert_eq!(fs::read(&planted).unwrap(), b"not b", "never replaced");
        for path in ["src/a.rs", "src/b.rs"] {
            assert_eq!(fx.store.accepted_source(path).unwrap(), None);
        }

        // A source that cannot be read fails the whole baseline, even after
        // other objects were published.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut fx = Fixture::new("src");
            fx.write("src/a.rs", b"a");
            fx.write("src/b.rs", b"b");
            let unreadable = fx.root().join("src/b.rs");
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
            assert!(baseline(&fx.project, &mut fx.store).is_err());
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();
            for path in ["src/a.rs", "src/b.rs"] {
                assert_eq!(fx.store.accepted_source(path).unwrap(), None);
            }
            assert_eq!(fx.baseline(), ["src/a.rs", "src/b.rs"]);
        }
    }

    #[test]
    fn restore_refuses_invalid_objects_without_writing() {
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"accepted a");
        fx.write("src/b.rs", b"accepted b");
        fx.baseline();
        fx.write("src/a.rs", b"candidate a");
        fs::remove_file(fx.root().join("src/b.rs")).unwrap();

        let object = fx.object_path(b"accepted a");
        fs::write(&object, b"tampered").unwrap();
        fails(fx.restore(&["src/a.rs"]), "corrupt");
        assert_eq!(fx.read("src/a.rs"), b"candidate a");

        fs::remove_file(fx.object_path(b"accepted b")).unwrap();
        fails(fx.restore(&["src/b.rs"]), "unavailable");
        assert!(!fx.exists("src/b.rs"));
    }

    #[test]
    fn accepted_state_survives_reopen() {
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        fx.baseline();
        fx.accept_absent(&["src/new.rs"]).unwrap();
        let Fixture {
            _dir,
            project,
            store,
        } = fx;
        drop(store);

        let project = Project::load(&project.root).unwrap();
        let store = project.hydrate().unwrap();
        assert_eq!(
            drift(&project, &store, "src/a.rs").unwrap(),
            Drift::Identical
        );
        fs::remove_file(project.root.join("src/a.rs")).unwrap();
        fs::write(project.root.join("src/new.rs"), b"new").unwrap();
        restore(&project, &store, &["src/a.rs", "src/new.rs"]).unwrap();
        assert_eq!(fs::read(project.root.join("src/a.rs")).unwrap(), b"a");
        assert!(!project.root.join("src/new.rs").exists());
    }

    #[test]
    fn snapshots_observe_repository_content_literally() {
        use crate::state::Content;
        let fx = Fixture::new("src");
        fx.write(".gitignore", b"/.agentctl/\ntarget/\n");
        fx.write("README.md", b"outside the roots, still project content");
        fx.write("src/a.rs", b"a");
        fx.write("src/[id].rs", b"id");
        fx.write("src/target/out.o", b"ignored");
        fx.write("src/vendor/lib.rs", b"nested");
        run_git(&fx.root().join("src/vendor"), &["init", "-q"]);
        fx.write("src/gone.rs", b"tracked, then deleted");
        run_git(fx.root(), &["add", "src/gone.rs"]);
        fs::remove_file(fx.root().join("src/gone.rs")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.rs", fx.root().join("src/link.rs")).unwrap();

        let observed = snapshot(&fx.project, &["src/asked.rs".into()]).unwrap();
        let mut expected = vec![
            (
                ".gitignore",
                Content::File(sha256(b"/.agentctl/\ntarget/\n")),
            ),
            (
                "README.md",
                Content::File(sha256(b"outside the roots, still project content")),
            ),
            (
                "agentctl.toml",
                Content::File(sha256(&fx.read("agentctl.toml"))),
            ),
            ("src/[id].rs", Content::File(sha256(b"id"))),
            ("src/a.rs", Content::File(sha256(b"a"))),
            ("src/asked.rs", Content::Absent),
            ("src/gone.rs", Content::Absent),
        ];
        if cfg!(unix) {
            expected.push(("src/link.rs", Content::Symlink(sha256(b"a.rs"))));
        }
        let entries: Vec<(&str, Content)> = observed
            .entries
            .iter()
            .map(|(path, content)| (path.as_str(), content.clone()))
            .collect();
        assert_eq!(entries, expected);
        assert!(
            observed.head.starts_with("refs/heads/"),
            "{}",
            observed.head
        );
        assert!(observed.head.ends_with(" unborn"), "{}", observed.head);

        // A file replaced by a directory is no file content; behind a
        // symlinked directory there is no entry of the repository.
        fs::remove_file(fx.root().join("src/a.rs")).unwrap();
        fx.write("src/a.rs/inner.rs", b"inner");
        #[cfg(unix)]
        std::os::unix::fs::symlink("vendor", fx.root().join("src/linked")).unwrap();
        let asked = ["src/a.rs".to_string(), "src/linked/lib.rs".to_string()];
        let observed = snapshot(&fx.project, &asked).unwrap();
        let content = |path: &str| {
            let found = observed.entries.iter().find(|(p, _)| p == path);
            found.map(|(_, content)| content.clone())
        };
        assert_eq!(content("src/a.rs"), Some(Content::Other));
        assert_eq!(
            content("src/a.rs/inner.rs"),
            Some(Content::File(sha256(b"inner")))
        );
        #[cfg(unix)]
        assert_eq!(content("src/linked/lib.rs"), Some(Content::Absent));

        // Commits move HEAD.
        run_git(
            fx.root(),
            &[
                "-c",
                "user.name=x",
                "-c",
                "user.email=x@example.com",
                "commit",
                "-q",
                "-m",
                "c",
            ],
        );
        let committed = snapshot(&fx.project, &[]).unwrap().head;
        assert_ne!(committed, observed.head);
        assert!(!committed.ends_with(" unborn"));
    }

    #[test]
    fn workspaces_copy_the_repository_and_nothing_else() {
        let fx = Fixture::new("src");
        fx.write(".gitignore", b"/.agentctl/\ntarget/\n");
        fx.write("src/a.rs", b"a");
        fx.write("src/[id].rs", b"id");
        fx.write("src/target/out.o", b"ignored");
        fx.write("src/vendor/lib.rs", b"nested");
        run_git(&fx.root().join("src/vendor"), &["init", "-q"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::os::unix::fs::symlink("a.rs", fx.root().join("src/link.rs")).unwrap();
            fx.write("src/run.sh", b"#!/bin/sh\n");
            let script = fx.root().join("src/run.sh");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let asked = ["src/new.rs".to_string()];
        let observed = snapshot(&fx.project, &asked).unwrap();
        let workspace = Workspace::stage(&fx.project, &observed).unwrap();
        let root = workspace.root().to_path_buf();
        assert!(!root.starts_with(fx.root()) && !fx.root().starts_with(&root));

        let mut listed = Vec::new();
        let mut dirs = vec![(root.clone(), String::new())];
        while let Some((dir, prefix)) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = format!("{prefix}{}", entry.file_name().to_str().unwrap());
                match entry.file_type().unwrap().is_dir() {
                    true => dirs.push((entry.path(), format!("{path}/"))),
                    false => listed.push(path),
                }
            }
        }
        listed.sort();
        // Neither agentctl's state nor Git's, nothing ignored, and nothing
        // of a nested repository.
        let mut expected = vec![".gitignore", "agentctl.toml", "src/[id].rs", "src/a.rs"];
        if cfg!(unix) {
            expected.extend(["src/link.rs", "src/run.sh"]);
        }
        assert_eq!(listed, expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let link = fs::read_link(root.join("src/link.rs")).unwrap();
            assert_eq!(link, Path::new("a.rs"));
            let mode = fs::metadata(root.join("src/run.sh")).unwrap().permissions();
            assert_eq!(mode.mode() & 0o777, 0o755);
        }
        // Observed by the project's rules, the copy holds what was observed.
        let paths: Vec<String> = observed.entries.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            workspace.observe(&fx.project, &paths).unwrap().entries,
            observed.entries
        );
        drop(workspace);
        assert!(!root.exists());

        // A symlink leading out of the repository is never reproduced.
        #[cfg(unix)]
        for target in ["/etc", "../../outside", "../.agentctl/state.db"] {
            let link = fx.root().join("src/out");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let observed = snapshot(&fx.project, &[]).unwrap();
            let refused = Workspace::stage(&fx.project, &observed);
            fails(refused, "leading out of the repository");
        }
    }

    #[cfg(unix)]
    #[test]
    fn installing_writes_only_where_the_baseline_still_holds() {
        use crate::state::{Change, Content};
        let fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        let observed = snapshot(&fx.project, &[]).unwrap();
        let workspace = Workspace::stage(&fx.project, &observed).unwrap();
        fs::write(workspace.root().join("src/a.rs"), b"b").unwrap();
        let change = Change {
            path: "src/a.rs".into(),
            before: Content::File(sha256(b"a")),
            after: Content::File(sha256(b"b")),
            authorized: true,
        };
        let install = |change: &Change| match workspace
            .prepare(&fx.project, std::slice::from_ref(change))
            .unwrap()
        {
            Preparation::Ready(prepared) => prepared.apply(&fx.project).unwrap(),
            Preparation::Declined(verdict) => verdict,
        };

        // The project's `src` swapped for a symlink to identical bytes
        // elsewhere: never followed, so drifted.
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("a.rs"), b"a").unwrap();
        fs::rename(fx.root().join("src"), fx.root().join("src.moved")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), fx.root().join("src")).unwrap();
        let Installation::Drifted(paths) = install(&change) else {
            panic!()
        };
        assert_eq!(paths, ["src/a.rs"]);
        assert_eq!(fs::read(elsewhere.path().join("a.rs")).unwrap(), b"a");
        fs::remove_file(fx.root().join("src")).unwrap();
        fs::rename(fx.root().join("src.moved"), fx.root().join("src")).unwrap();

        // Symlinks and directories are neither installed nor replaced, and
        // only authorized changes are installed.
        for refused in [
            Change {
                after: Content::Symlink(sha256(b"elsewhere")),
                ..change.clone()
            },
            Change {
                before: Content::Other,
                ..change.clone()
            },
            Change {
                authorized: false,
                ..change.clone()
            },
        ] {
            assert!(matches!(install(&refused), Installation::Refused(_)));
        }
        // Nor what the workspace no longer holds.
        let stale = Change {
            after: Content::File(sha256(b"c")),
            ..change.clone()
        };
        assert!(matches!(install(&stale), Installation::Refused(_)));
        assert_eq!(fx.read("src/a.rs"), b"a");

        // Prepared: both sides kept as recovery objects, nothing written.
        let changes = std::slice::from_ref(&change);
        let Preparation::Ready(prepared) = workspace.prepare(&fx.project, changes).unwrap() else {
            panic!("prepared");
        };
        assert_eq!(fx.read("src/a.rs"), b"a");
        let objects = fx.objects();
        for bytes in [&b"a"[..], b"b"] {
            assert!(
                objects
                    .iter()
                    .any(|(name, kept)| *name == sha256(bytes) && kept == bytes)
            );
        }
        // Installed from those objects alone, the workspace gone.
        let root = workspace.root().to_path_buf();
        drop(workspace);
        assert!(!root.exists());
        assert!(matches!(
            prepared.apply(&fx.project).unwrap(),
            Installation::Installed
        ));
        assert_eq!(fx.read("src/a.rs"), b"b");
    }

    #[test]
    fn captured_deletions_are_installed_only_while_still_absent() {
        use crate::state::{Change, Content};
        let fx = Fixture::new("src");
        fx.write("src/a.rs", b"a");
        let observed = snapshot(&fx.project, &[]).unwrap();
        let workspace = Workspace::stage(&fx.project, &observed).unwrap();
        let staged = workspace.root().join("src/a.rs");
        fs::remove_file(&staged).unwrap();
        let change = Change {
            path: "src/a.rs".into(),
            before: Content::File(sha256(b"a")),
            after: Content::Absent,
            authorized: true,
        };
        let changes = std::slice::from_ref(&change);

        // Still absent: ready, having written nothing.
        assert!(matches!(
            workspace.prepare(&fx.project, changes).unwrap(),
            Preparation::Ready(_)
        ));
        assert_eq!(fx.read("src/a.rs"), b"a");

        // Recreated in any form since capture: refused, nothing written,
        // and the recreated entry left alone.
        let refused = |recreated: &str| {
            let Preparation::Declined(Installation::Refused(why)) =
                workspace.prepare(&fx.project, changes).unwrap()
            else {
                panic!("a deletion recreated as {recreated} was prepared");
            };
            assert!(why.contains("no longer holds what was captured"), "{why}");
            assert_eq!(fx.read("src/a.rs"), b"a");
        };
        fs::write(&staged, b"a").unwrap();
        refused("a regular file");
        assert_eq!(fs::read(&staged).unwrap(), b"a");
        fs::remove_file(&staged).unwrap();

        fs::create_dir(&staged).unwrap();
        refused("a directory");
        assert!(staged.is_dir());
        fs::remove_dir(&staged).unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("b.rs", &staged).unwrap();
            refused("a symlink");
            assert!(fs::symlink_metadata(&staged).unwrap().is_symlink());
            fs::remove_file(&staged).unwrap();
        }

        // Absent again: ready, and installing deletes.
        let Preparation::Ready(prepared) = workspace.prepare(&fx.project, changes).unwrap() else {
            panic!("prepared");
        };
        assert!(matches!(
            prepared.apply(&fx.project).unwrap(),
            Installation::Installed
        ));
        assert!(!fx.root().join("src/a.rs").exists());
    }

    #[test]
    fn accepted_content_is_read_from_verified_objects_only() {
        let mut fx = Fixture::new("src");
        fx.write("src/a.rs", b"accepted");
        fx.baseline();
        fx.accept_absent(&["src/gone.rs"]).unwrap();
        let read = |fx: &Fixture, path| read_accepted(&fx.project, &fx.store, path);

        // The working tree is never consulted.
        fx.write("src/a.rs", b"candidate");
        let content = read(&fx, "src/a.rs").unwrap();
        assert_eq!(content.bytes(), b"accepted");
        assert_eq!(content.hash(), sha256(b"accepted"));

        fails(read(&fx, "src/gone.rs"), "accepted as absent");
        fails(read(&fx, "src/untracked.rs"), "no accepted state");
        fails(read(&fx, "src/../src/a.rs"), "not a canonical");

        let object = fx.object_path(b"accepted");
        fs::write(&object, b"acceptee").unwrap();
        fails(read(&fx, "src/a.rs"), "corrupt");
        fs::remove_file(&object).unwrap();
        fails(read(&fx, "src/a.rs"), "unavailable");
    }
}
