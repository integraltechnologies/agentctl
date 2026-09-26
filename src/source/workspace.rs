//! An executor's workspace: a disposable copy of the repository outside the
//! project, in which an executor may edit anything, and installing what it
//! changed there into the project's working tree.
//!
//! The copy holds what a snapshot of the repository observed, as ordinary
//! independent files: never agentctl's state, never Git's, nothing ignored,
//! and no entry aliasing anything outside the copy, since a symlink is
//! reproduced only when its target stays within it. Nothing links the copy
//! to the project, so nothing written there reaches the project except by
//! installing.
//!
//! Installing writes a candidate's changes into the working tree only while
//! every changed path still holds what the baseline observed there: a path
//! that drifted belongs to whoever changed it, and nothing is written. Each
//! path is written atomically, but the paths one at a time; should writing
//! fail, what was written is restored. Installing is prepared first,
//! writing nothing in the working tree: every change is validated, and both
//! the candidate's bytes and the bytes they replace are kept as durable
//! recovery objects, which is all writing then reads. Only a prepared
//! install may be recorded as attempted, and only then written, so that a
//! crash part way leaves every path recoverable without the workspace.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use tempfile::TempDir;

use super::snapshot::{Snapshot, identify, snapshot_tree};
use super::{Entry, Objects, entry, objects, parent, remove, reserved, write_object};
use crate::platform;
use crate::project::{Project, STATE_DIR};
use crate::state::{Change, Content};

/// A disposable copy of the repository, removed when dropped.
#[derive(Debug)]
pub(crate) struct Workspace {
    root: PathBuf,
    _dir: TempDir,
}

/// How installing a candidate ended.
#[derive(Debug)]
pub(crate) enum Installation {
    /// Every change was written.
    Installed,
    /// These paths no longer held what the baseline observed, so nothing
    /// was written, or what was written was restored.
    Drifted(Vec<String>),
    /// Nothing was written, for the reason given.
    Refused(String),
    /// Installing failed, and whatever it wrote was restored.
    Failed(anyhow::Error),
}

/// What preparing to install a candidate found.
pub(crate) enum Preparation<'a> {
    /// Installing may begin, once recorded as attempted.
    Ready(Prepared<'a>),
    /// Nothing is to be written: `Drifted` or `Refused`.
    Declined(Installation),
}

/// An install validated and made recoverable, which has written nothing
/// yet: exactly the changes validated, each of whose candidate and baseline
/// bytes is a durable recovery object, so that writing them depends on
/// nothing in the workspace.
pub(crate) struct Prepared<'a> {
    objects: Objects,
    changes: &'a [Change],
}

impl Prepared<'_> {
    /// Writes the prepared changes into the project's working tree, one
    /// path at a time, each from its recovery object, and only while it
    /// still holds what the baseline observed; should writing stop short,
    /// what was written is restored. Must be recorded as attempted first.
    /// Fails only when the working tree may hold part of the candidate:
    /// when restoring what was written failed.
    pub(crate) fn apply(self, project: &Project) -> Result<Installation> {
        let root = &project.root;
        let mut written: Vec<&Change> = Vec::new();
        for change in self.changes {
            // Checked again right before writing, which narrows but cannot
            // close the window in which another writer could be overwritten.
            let verdict = match current(root, &change.path) {
                Ok(found) if found.as_ref() == Some(&change.before) => {
                    written.push(change);
                    match write(root, &self.objects, &change.path, &change.after) {
                        Ok(()) => continue,
                        Err(e) => Installation::Failed(e),
                    }
                }
                Ok(_) => Installation::Drifted(vec![change.path.clone()]),
                Err(e) => Installation::Failed(e),
            };
            // Nothing of the candidate may stay written.
            for change in written.iter().rev() {
                write(root, &self.objects, &change.path, &change.before).with_context(|| {
                    format!(
                        "restoring `{}` once installing stopped ({verdict:?})",
                        change.path
                    )
                })?;
            }
            return Ok(verdict);
        }
        Ok(Installation::Installed)
    }
}

impl Workspace {
    /// Copies everything `baseline` observed of the project into a new
    /// temporary directory outside it: each regular file with its bytes and
    /// permissions, a directory wherever the baseline found one, and each
    /// symlink whose target stays within the copy. Fails, having copied
    /// nothing anyone else can find, unless the copy is then observed to
    /// hold exactly what the baseline observed.
    pub(crate) fn stage(project: &Project, baseline: &Snapshot) -> Result<Self> {
        Self::compose(project, baseline, &BTreeSet::new())
    }

    /// [`Workspace::stage`], except that the regular file at each path in
    /// `recovered` is copied from its recovery object instead of the
    /// working tree, keeping the permissions of any regular file the
    /// working tree holds there: a view of the repository that holds bytes
    /// the working tree need not hold.
    pub(crate) fn compose(
        project: &Project,
        baseline: &Snapshot,
        recovered: &BTreeSet<String>,
    ) -> Result<Self> {
        let objects = match recovered.is_empty() {
            true => None,
            false => Some(Objects::open(&project.root.join(STATE_DIR))?),
        };
        let dir = tempfile::Builder::new()
            .prefix("agentctl-workspace-")
            .tempdir()
            .context("creating an executor workspace")?;
        // Canonical, so that no symlink lies on the way in.
        let root = dir.path().canonicalize()?;
        let project_root = project.root.canonicalize()?;
        ensure!(
            !root.starts_with(&project_root) && !project_root.starts_with(&root),
            "the temporary directory {} overlaps the project",
            root.display()
        );
        let links: HashSet<&str> = baseline
            .entries
            .iter()
            .filter(|(_, content)| matches!(content, Content::Symlink(_)))
            .map(|(path, _)| path.as_str())
            .collect();
        let mut symlinks = Vec::new();
        for (path, content) in &baseline.entries {
            let copying = || format!("copying `{path}` into the workspace");
            match content {
                Content::Absent => {}
                Content::File(hash) if recovered.contains(path) => {
                    let objects = objects.as_ref().context("recovering needs objects")?;
                    recover_file(&project.root, objects, &root, path, hash).with_context(copying)?
                }
                Content::File(hash) => {
                    copy_file(&project.root, &root, path, hash).with_context(copying)?
                }
                Content::Other => fs::create_dir_all(within(&root, path)).with_context(copying)?,
                Content::Symlink(hash) => {
                    let target = read_link(&project.root, path).with_context(copying)?;
                    let bytes = target.as_os_str().as_encoded_bytes();
                    ensure!(
                        objects::hash(bytes)? == *hash,
                        "`{path}` changed while it was copied"
                    );
                    ensure!(
                        confined(path, &target, &links),
                        "`{path}` is a symlink leading out of the repository, \
                         which no executor workspace reproduces"
                    );
                    symlinks.push((path, target));
                }
            }
        }
        // Last, so that no symlink lies on the way to anything copied.
        for (path, target) in symlinks {
            let link = within(&root, path);
            let linking = || format!("reproducing the symlink `{path}` in the workspace");
            fs::create_dir_all(link.parent().context("a path has a parent")?)
                .with_context(linking)?;
            platform::symlink(&target, &link).with_context(linking)?;
        }
        let workspace = Self { root, _dir: dir };
        let paths: Vec<String> = baseline.entries.iter().map(|(p, _)| p.clone()).collect();
        let staged = workspace.observe(project, &paths)?;
        ensure!(
            staged.entries == baseline.entries,
            "the workspace does not hold the repository as observed; it changed meanwhile"
        );
        Ok(workspace)
    }

    /// The workspace's absolute, canonical root directory.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Observes the workspace as the repository it copies, including every
    /// path in `also`.
    pub(crate) fn observe(&self, project: &Project, also: &[String]) -> Result<Snapshot> {
        snapshot_tree(project, &self.root, also)
    }

    /// Prepares installing `changes`, a candidate's authorized changes as
    /// captured in this workspace, into the project's working tree, writing
    /// nothing there; see the module documentation. Changes to or from
    /// symlinks and directories are refused. Ready only once every change
    /// was validated against both this workspace and the working tree, and
    /// every byte installing may write, candidate or baseline, is a durable
    /// recovery object. Fails, having written nothing in the working tree,
    /// when that could not be established.
    pub(crate) fn prepare<'a>(
        &self,
        project: &Project,
        changes: &'a [Change],
    ) -> Result<Preparation<'a>> {
        for change in changes {
            let plain = |content: &Content| matches!(content, Content::Absent | Content::File(_));
            if !change.authorized || !plain(&change.before) || !plain(&change.after) {
                return Ok(Preparation::Declined(Installation::Refused(format!(
                    "`{}` is not an authorized change of a regular file",
                    change.path
                ))));
            }
        }
        let objects = Objects::open(&project.root.join(STATE_DIR))?;
        Ok(match self.preserve(&project.root, &objects, changes)? {
            Some(verdict) => Preparation::Declined(verdict),
            None => Preparation::Ready(Prepared { objects, changes }),
        })
    }

    /// Keeps each change's candidate bytes, read from this workspace, and
    /// the baseline bytes they replace, read from the working tree at
    /// `root`, as durable recovery objects. A verdict instead when the
    /// workspace no longer holds what was captured, or when any path in the
    /// working tree no longer holds what the baseline observed.
    fn preserve(
        &self,
        root: &Path,
        objects: &Objects,
        changes: &[Change],
    ) -> Result<Option<Installation>> {
        let kept = |root: &Path, path: &str| -> Result<Option<String>> {
            match entry(root, path)? {
                Entry::File(file) => Ok(Some(objects.publish(file)?)),
                _ => Ok(None),
            }
        };
        let mut drifted = Vec::new();
        for change in changes {
            // Exactly what was captured, whatever it was: a file's bytes,
            // kept as they are read, or anything else as installing finds it.
            let captured = match &change.after {
                Content::File(hash) => kept(&self.root, &change.path)?.as_ref() == Some(hash),
                after => current(&self.root, &change.path)?.as_ref() == Some(after),
            };
            if !captured {
                return Ok(Some(Installation::Refused(format!(
                    "the workspace no longer holds what was captured at `{}`",
                    change.path
                ))));
            }
            let unchanged = match (&change.before, current(root, &change.path)?) {
                (Content::File(hash), Some(Content::File(_))) => {
                    kept(root, &change.path)?.as_ref() == Some(hash)
                }
                (before, found) => found.as_ref() == Some(before),
            };
            if !unchanged {
                drifted.push(change.path.clone());
            }
        }
        objects.sync()?;
        Ok((!drifted.is_empty()).then_some(Installation::Drifted(drifted)))
    }
}

/// Copies the regular file at `path` in `from` to the same path in `to`,
/// failing unless its bytes hash to `hash`.
fn copy_file(from: &Path, to: &Path, path: &str, hash: &str) -> Result<()> {
    let Entry::File(file) = entry(from, path)? else {
        bail!("`{path}` changed while it was copied");
    };
    let permissions = file.metadata()?.permissions();
    let dest = within(to, path);
    fs::create_dir_all(dest.parent().context("a path has a parent")?)?;
    let mut copy = File::create_new(&dest)?;
    ensure!(
        objects::copy(file, &mut copy)? == hash,
        "`{path}` changed while it was copied"
    );
    copy.set_permissions(permissions)?;
    Ok(())
}

/// Writes recovery object `hash` at `path` in `to`, failing unless its
/// bytes hash to `hash`, with the permissions of the regular file at `path`
/// in `from`, if there is one.
fn recover_file(from: &Path, objects: &Objects, to: &Path, path: &str, hash: &str) -> Result<()> {
    let dest = within(to, path);
    fs::create_dir_all(dest.parent().context("a path has a parent")?)?;
    let mut copy = File::create_new(&dest)?;
    objects.copy_to(hash, &mut copy)?;
    if let Entry::File(file) = entry(from, path)? {
        copy.set_permissions(file.metadata()?.permissions())?;
    }
    Ok(())
}

/// The target of the symlink at `path` in `root`, reached through real
/// directories only.
fn read_link(root: &Path, path: &str) -> Result<PathBuf> {
    let dir = parent(root, path, false)?.context("the symlink vanished")?;
    Ok(fs::read_link(dir.join(super::name(path)))?)
}

/// `path` within `root`, one literal name at a time.
fn within(root: &Path, path: &str) -> PathBuf {
    let mut full = root.to_path_buf();
    full.extend(path.split('/'));
    full
}

/// Whether the symlink at `path`, whose target is `target`, resolves within
/// the repository's copy and outside agentctl's and Git's state, passing
/// through none of the repository's other symlinks (`links`) on the way, so
/// that the path a relative target spells out is where it leads.
fn confined(path: &str, target: &Path, links: &HashSet<&str>) -> bool {
    let mut at: Vec<&str> = path.split('/').collect();
    at.pop();
    let parts: Vec<Component> = target.components().collect();
    for (i, part) in parts.iter().enumerate() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if at.pop().is_none() {
                    return false;
                }
            }
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return false;
                };
                at.push(name);
                let reached = at.join("/");
                if reserved(&reached) || (i + 1 < parts.len() && links.contains(reached.as_str())) {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The entry at `path` in `root` as installing would find it; `None` when
/// an ancestor is a symlink, which installing never follows.
fn current(root: &Path, path: &str) -> Result<Option<Content>> {
    let mut dir = root.to_path_buf();
    let ancestors = path.rsplit_once('/').map_or("", |(ancestors, _)| ancestors);
    for part in ancestors.split('/').filter(|part| !part.is_empty()) {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_symlink() => return Ok(None),
            Ok(meta) if meta.is_dir() => {}
            // Nothing lies beneath a file or a missing directory.
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    identify(root, path).map(Some)
}

/// Makes `path` in `root` hold `content`: a recovery object's bytes, or
/// nothing.
fn write(root: &Path, objects: &Objects, path: &str, content: &Content) -> Result<()> {
    match content {
        Content::File(hash) => write_object(root, objects, path, hash),
        Content::Absent => remove(root, path),
        _ => bail!("agentctl installs no symlink or directory at `{path}`"),
    }
    .with_context(|| format!("writing `{path}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlinks_are_reproduced_only_within_the_copy() {
        let links: HashSet<&str> = ["src/up", "src/lib"].into_iter().collect();
        for (path, target, expected) in [
            ("src/link.rs", "a.rs", true),
            ("src/link.rs", "./a.rs", true),
            ("src/deep/link.rs", "../a.rs", true),
            ("link", "src", true),
            ("src/link", "..", true),
            ("src/link.rs", "../../outside.rs", false),
            ("link.rs", "../outside.rs", false),
            ("src/link.rs", "/etc/passwd", false),
            ("src/state", "../.agentctl/state.db", false),
            ("src/state", "../.AgentCtl", false),
            ("src/git", "../.git/config", false),
            ("src/git", "vendor/.git", false),
            // Through another symlink, the spelled-out path misleads.
            ("link.rs", "src/up/../../x", false),
            ("link.rs", "src/lib/a.rs", false),
            // Merely naming one is where it leads.
            ("link", "src/lib", true),
        ] {
            assert_eq!(
                confined(path, Path::new(target), &links),
                expected,
                "{path} -> {target}"
            );
        }
    }
}
