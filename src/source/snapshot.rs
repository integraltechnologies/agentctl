//! Observing the repository as it stands, so that what changed between two
//! observations is established from the repository itself, never from what
//! anyone says they changed.
//!
//! An observation covers every path under the project root that Git lists
//! as repository content (tracked, or untracked and not ignored) outside
//! agentctl's and Git's own state, and any further paths asked for, such as
//! those of an earlier observation. Each path's entry is read literally from
//! the filesystem, never through a symlink. Ignored paths, the contents of
//! nested repositories and Git's state other than HEAD are not observed.
//!
//! A copy of the repository outside the project, such as an executor's
//! workspace, is observed by the same rules, as the project's own Git
//! applies them, rather than rules found within the copy, which its writer
//! could rewrite. Anything there in agentctl's or Git's state is observed
//! too, since the copy holds none of either.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::{git, ignored_among, name, objects, reserved, stderr};
use crate::platform;
use crate::project::Project;
use crate::state::{Content, check_path};

/// The repository as observed at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// The entry at each observed path, in strict path order.
    pub entries: Vec<(String, Content)>,
    /// Git's HEAD: the branch it names or `detached`, and its commit or
    /// `unborn`.
    pub head: String,
}

/// Observes the repository, including every path in `also`.
pub(crate) fn snapshot(project: &Project, also: &[String]) -> Result<Snapshot> {
    let output = git(
        project,
        Command::new("git")
            .args(["--literal-pathspecs", "ls-files", "-z"])
            .args(["--cached", "--others", "--exclude-standard"]),
        b"",
    )?;
    ensure!(output.status.success(), "{}", stderr(&output));
    let mut paths = also.to_vec();
    for raw in output.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| anyhow!("`{}` is not a UTF-8 path", String::from_utf8_lossy(raw)))?;
        // Untracked nested repositories are listed as directories.
        if path.ends_with('/') || reserved(path) {
            continue;
        }
        check_path(path)?;
        ensure!(
            path.split('/').all(platform::literal_name),
            "`{path}` cannot be addressed literally on this platform"
        );
        paths.push(path.to_owned());
    }
    // Unmerged index entries are listed once per stage.
    observe(project, &project.root, paths)
}

/// Observes exactly `paths` in the project's working tree, whatever Git
/// makes of them.
pub(crate) fn snapshot_paths(project: &Project, paths: &[String]) -> Result<Snapshot> {
    observe(project, &project.root, paths.to_vec())
}

/// The entry at each of `paths` in the project's working tree, in the
/// order given, without consulting Git.
pub(crate) fn observe_paths(project: &Project, paths: &[String]) -> Result<Vec<(String, Content)>> {
    paths
        .iter()
        .map(|path| {
            check_path(path)?;
            Ok((path.clone(), identify(&project.root, path)?))
        })
        .collect()
}

/// Observes `tree`, a copy of the repository outside the project, including
/// every path in `also`: every entry beneath it, reached through real
/// directories only, that the project's Git takes for repository content
/// (tracked, or not ignored by the project's rules), or that lies in
/// agentctl's or Git's state. HEAD is the project's.
pub(crate) fn snapshot_tree(project: &Project, tree: &Path, also: &[String]) -> Result<Snapshot> {
    let mut found = Vec::new();
    walk(&mut tree.to_path_buf(), "", &mut found)?;
    let asked: BTreeSet<&str> = also.iter().map(String::as_str).collect();
    let unasked: Vec<&str> = found
        .iter()
        .map(String::as_str)
        .filter(|path| !asked.contains(path) && !reserved(path))
        .collect();
    let ignored: BTreeSet<String> = ignored_among(project, &unasked)?.into_iter().collect();
    let mut paths = also.to_vec();
    paths.extend(found.into_iter().filter(|path| !ignored.contains(path)));
    observe(project, tree, paths)
}

/// Every entry beneath `dir` other than a directory, as a repository path
/// under `prefix`; a symlink is never followed.
fn walk(dir: &mut PathBuf, prefix: &str, found: &mut Vec<String>) -> Result<()> {
    let listed = fs::read_dir(&*dir).with_context(|| format!("listing {}", dir.display()))?;
    for entry in listed {
        let entry = entry?;
        let raw = entry.file_name();
        let name = raw
            .to_str()
            .with_context(|| format!("`{prefix}{}` is not a UTF-8 path", raw.to_string_lossy()))?;
        let path = format!("{prefix}{name}");
        if entry.file_type()?.is_dir() {
            dir.push(name);
            walk(dir, &format!("{path}/"), found)?;
            dir.pop();
        } else {
            found.push(path);
        }
    }
    Ok(())
}

/// The entry at each of `paths` in `root`, which holds the repository, in
/// strict path order; and the project's HEAD.
fn observe(project: &Project, root: &Path, mut paths: Vec<String>) -> Result<Snapshot> {
    paths.sort_unstable();
    paths.dedup();
    let entries = paths
        .into_iter()
        .map(|path| {
            check_path(&path)?;
            let content = identify(root, &path)?;
            Ok((path, content))
        })
        .collect::<Result<_>>()?;
    Ok(Snapshot {
        entries,
        head: head(project)?,
    })
}

/// The entry at `path`, reached from `root` through real directories only.
/// Behind a symlink or a file there is no entry of the repository itself.
pub(super) fn identify(root: &Path, path: &str) -> Result<Content> {
    let observing = || format!("observing `{path}`");
    let mut dir = root.to_path_buf();
    if let Some((ancestors, _)) = path.rsplit_once('/') {
        for part in ancestors.split('/') {
            dir.push(part);
            match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(Content::Absent),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Content::Absent),
                Err(e) => return Err(e).with_context(observing),
            }
        }
    }
    let full = dir.join(name(path));
    let meta = match platform::open_regular(&full) {
        Ok(Some(file)) => return Ok(Content::File(objects::hash(file).with_context(observing)?)),
        Ok(None) => fs::symlink_metadata(&full),
        Err(e) => Err(e),
    };
    match meta {
        Ok(meta) if meta.is_symlink() => {
            let target = fs::read_link(&full).with_context(observing)?;
            let hash = objects::hash(target.as_os_str().as_encoded_bytes())?;
            Ok(Content::Symlink(hash))
        }
        Ok(_) => Ok(Content::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Content::Absent),
        Err(e) => Err(e).with_context(observing),
    }
}

/// Identifies Git's HEAD by the branch it names and the commit it resolves
/// to, so that commits, checkouts and resets are observed.
fn head(project: &Project) -> Result<String> {
    let ask = |args: &[&str], otherwise: &str| -> Result<String> {
        let output = git(project, Command::new("git").args(args), b"")?;
        match output.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
            Some(1) => Ok(otherwise.to_owned()),
            _ => bail!("{}", stderr(&output)),
        }
    };
    let branch = ask(&["symbolic-ref", "-q", "HEAD"], "detached")?;
    let commit = ask(&["rev-parse", "-q", "--verify", "HEAD"], "unborn")?;
    Ok(format!("{branch} {commit}"))
}
