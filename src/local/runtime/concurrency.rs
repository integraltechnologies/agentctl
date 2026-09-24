//! Deterministic task compatibility for concurrent execution. This is derived evidence, not a
//! second dependency graph or a heuristic score: an absent proof fails closed.

use super::*;
use crate::local::graph::{self, ImpactLimits, ImpactRequest};
use std::{fs, process::Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Compatibility {
    Compatible,
    Conflict,
    DependencyBlocked,
    SourceIncompatible,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "reason",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum CompatibilityReason {
    DisjointDeclaredScopes,
    OntologyProvesNoInterference {
        generation: graph::GraphGeneration,
    },
    DagDependency {
        predecessor: TaskId,
        successor: TaskId,
    },
    WriteWriteOverlap {
        left: String,
        right: String,
    },
    WriteReadInterference {
        writer: TaskId,
        write: String,
        reader: TaskId,
        read: String,
    },
    SharedEntity {
        entity: GraphEntityId,
    },
    SemanticInterference {
        source: GraphEntityId,
        affected: GraphEntityId,
        path: String,
    },
    SemanticBoundary {
        task: TaskId,
        detail: String,
    },
    MissingSemanticAuthority {
        task: TaskId,
    },
    DifferentSource {
        left: String,
        right: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityDecision {
    pub left: TaskId,
    pub right: TaskId,
    pub decision: Compatibility,
    pub reasons: Vec<CompatibilityReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IsolatedWorkspace {
    pub(super) root: String,
    pub(super) physical_index_hash: String,
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "--no-pager", "-C"])
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    require(
        output.status.success(),
        format!(
            "Git worktree operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    )
}

fn write_state(
    root: &Path,
    path: &str,
    state: &source::FileState,
    artifacts: &Artifacts,
    create: bool,
) -> Result<()> {
    crate::validation::repo_path(path)?;
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = artifacts.get(&state.content)?;
    if create {
        use std::io::Write;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(state.mode);
        }
        options.open(&target)?.write_all(&bytes)?;
    } else {
        fs::write(&target, bytes)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(state.mode))?;
    }
    Ok(())
}

/// Rewrites `root` until it is byte-for-byte the recorded snapshot. Used both
/// to seed an isolated worktree and to discard a refused result.
pub(super) fn materialize(
    root: &Path,
    expected: &SourceSnapshot,
    artifacts: &Artifacts,
) -> Result<()> {
    require(
        expected.ignored.is_empty(),
        "UNKNOWN: isolated execution cannot reproduce ignored-file metadata safely",
    )?;
    let actual = source::capture_bound(
        root,
        artifacts,
        &expected.repository_id,
        &expected.workspace_id,
    )?;
    require(
        actual.head == expected.head && actual.exclude_hash == expected.exclude_hash,
        "SOURCE_INCOMPATIBLE: isolated worktree Git baseline differs",
    )?;
    for path in actual
        .files
        .keys()
        .filter(|path| !expected.files.contains_key(*path))
    {
        fs::remove_file(root.join(path))?;
    }
    for (path, state) in &expected.files {
        if actual.files.get(path) != Some(state) {
            write_state(root, path, state, artifacts, false)?;
        }
    }
    let mut observed = source::capture_bound(
        root,
        artifacts,
        &expected.repository_id,
        &expected.workspace_id,
    )?;
    observed.index_hash = expected.index_hash.clone();
    require(
        observed == *expected,
        "SOURCE_INCOMPATIBLE: isolated worktree could not reproduce issued baseline",
    )
}

pub(super) fn create_workspace(
    store: &Store,
    paths: &paths::MachinePaths,
    info: &RepositoryInfo,
    plan: &PlanId,
    task: &TaskId,
    baseline: &SourceSnapshot,
    artifacts: &Artifacts,
) -> Result<(IsolatedWorkspace, RepositoryInfo)> {
    let nonce: String =
        store
            .connection
            .query_row("SELECT lower(hex(randomblob(8)))", [], |row| row.get(0))?;
    let parent = plan_worktrees(paths, info, plan);
    paths::ensure_directory(&parent)?;
    let root = parent.join(format!("{}-{nonce}", task.as_str().replace(':', "-")));
    let root_text = root
        .to_str()
        .ok_or_else(|| Error::Invalid("worktree path is not UTF-8".into()))?;
    let head = baseline.head.as_deref().ok_or_else(|| {
        Error::Invalid("concurrent execution requires a committed baseline".into())
    })?;
    git(
        &info.root,
        &["worktree", "add", "--detach", root_text, head],
    )?;
    let result = (|| {
        materialize(&root, baseline, artifacts)?;
        let physical = RepositoryInfo::discover(&root)?;
        let mut bound = physical.clone();
        bound.repository_id = info.repository_id.clone();
        bound.workspace_id = info.workspace_id.clone();
        bound.source.repository_id = info.repository_id.clone();
        bound.source.workspace_id = info.workspace_id.clone();
        let branch_snapshot =
            source::capture_bound(&root, artifacts, &info.repository_id, &info.workspace_id)?;
        let workspace = IsolatedWorkspace {
            root: root_text.into(),
            physical_index_hash: branch_snapshot.index_hash,
        };
        Ok((workspace, bound))
    })();
    if result.is_err() {
        let _ = git(&info.root, &["worktree", "remove", "--force", root_text]);
    }
    result
}

pub(super) fn remove_workspace(info: &RepositoryInfo, root: &str) -> Result<()> {
    git(&info.root, &["worktree", "remove", "--force", root])
}

/// Managed worktrees for one plan, under the machine worktree root. The
/// workspace segment keeps linked worktrees of different checkouts apart even
/// when two plans share an ID across repositories.
pub(super) fn plan_worktrees(
    paths: &paths::MachinePaths,
    info: &RepositoryInfo,
    plan: &PlanId,
) -> PathBuf {
    paths
        .worktree_root
        .join(info.workspace_id.as_str())
        .join(plan.as_str().replace(':', "-"))
}

/// Drops every managed worktree of this workspace that durable runtime state
/// does not still refer to, then prunes Git's own registrations. The caller
/// holds the exclusive workspace lease, so anything outside `keep` is abandoned
/// by definition: a branch whose controller died, or a plan that already ended.
/// Failures are reported but never abort a run; a leftover directory must not
/// be able to block recovery.
pub(super) fn prune_workspaces(
    paths: &paths::MachinePaths,
    info: &RepositoryInfo,
    keep: &BTreeSet<String>,
) -> Vec<String> {
    let mut removed = vec![];
    let root = paths.worktree_root.join(info.workspace_id.as_str());
    let plans = match fs::read_dir(&root) {
        Ok(plans) => plans,
        Err(_) => return removed,
    };
    for plan in plans.flatten() {
        let branches = match fs::read_dir(plan.path()) {
            Ok(branches) => branches,
            Err(_) => continue,
        };
        for branch in branches.flatten() {
            let path = branch.path();
            let Some(text) = path.to_str() else { continue };
            if keep.contains(text) {
                continue;
            }
            let _ = remove_workspace(info, text);
            let _ = fs::remove_dir_all(&path);
            removed.push(text.to_owned());
        }
        let _ = fs::remove_dir(plan.path());
    }
    let _ = git(&info.root, &["worktree", "prune"]);
    removed
}

pub(super) fn live_generation_id(store: &Store, info: &RepositoryInfo) -> Result<String> {
    let generation = graph::generation(&store.connection, info)?.ok_or_else(|| {
        Error::Invalid("concurrency requires an indexed ontology generation".into())
    })?;
    store.connection.query_row(
        "SELECT generation_id FROM ontology_generations WHERE workspace_id=?1 AND sequence=?2 AND fingerprint=?3 ORDER BY ordinal DESC LIMIT 1",
        params![info.workspace_id.as_str(), generation.sequence as i64, generation.fingerprint],
        |row| row.get(0),
    ).map_err(Error::from)
}

/// Revalidate a branch against semantic changes accepted into the plan since
/// its executor context was issued. Any open boundary is uncertainty, not
/// independence, and therefore blocks reconciliation.
pub(super) fn revalidate_semantic(
    store: &Store,
    info: &RepositoryInfo,
    task: &TaskPacket,
    issued_generation: &str,
) -> Result<()> {
    let current = live_generation_id(store, info)?;
    if current == issued_generation {
        return Ok(());
    }
    let report = store.ontology_impact(
        &info.root,
        &ImpactRequest::Diff {
            from: issued_generation.into(),
            to: current,
        },
        ImpactLimits {
            depth: 4,
            seeds: 200,
            items: 500,
            tests: 100,
            boundaries: 200,
            fanout: 512,
        },
    )?;
    require(
        report.boundaries.is_empty()
            && report.summary.items_omitted == 0
            && report.summary.boundaries_omitted == 0,
        "UNKNOWN: semantic changes since branch issue have unresolved/truncated impact",
    )?;
    let ids: BTreeSet<_> = task.graph_entities.iter().collect();
    require(
        !report.seeds.iter().any(|seed| {
            ids.contains(&seed.entity)
                || task
                    .read_scope
                    .iter()
                    .chain(&task.write_scope)
                    .any(|scope| source::permits(scope, &seed.path))
        }) && !report.items.iter().any(|item| {
            ids.contains(&item.entity)
                || task
                    .read_scope
                    .iter()
                    .chain(&task.write_scope)
                    .any(|scope| source::permits(scope, &item.path))
        }),
        "SEMANTIC_INTERFERENCE: verified sibling changed a dependency of this branch",
    )
}

fn scope_unchanged(before: &SourceSnapshot, current: &SourceSnapshot, scope: &ScopePath) -> bool {
    let paths: BTreeSet<_> = before
        .files
        .keys()
        .chain(current.files.keys())
        .filter(|path| source::permits(scope, path))
        .collect();
    paths
        .into_iter()
        .all(|path| before.files.get(path) == current.files.get(path))
}

/// Validate one captured branch and construct the complete, content-addressed
/// publication intent without mutating canonical source.
pub(super) fn reconciliation_intent(
    task: &TaskPacket,
    pending: &PendingTask,
    current: &SourceSnapshot,
    artifacts: &Artifacts,
) -> Result<ReconciliationIntent> {
    require(
        pending
            .compatibility
            .iter()
            .all(|decision| decision.decision == Compatibility::Compatible),
        "UNKNOWN: branch lacks positive compatibility evidence",
    )?;
    let diff: CapturedDiff = artifacts.decode(&pending.diff)?;
    let before: SourceSnapshot = artifacts.decode(&diff.before)?;
    require(
        before.repository_id == current.repository_id
            && before.workspace_id == current.workspace_id
            && before.head == current.head
            && before.index_hash == current.index_hash
            && before.exclude_hash == current.exclude_hash,
        "STALE_SOURCE_CONFLICT: branch and canonical Git baselines differ",
    )?;
    for scope in task.read_scope.iter().chain(&task.write_scope) {
        require(
            scope_unchanged(&before, current, scope),
            format!(
                "RECONCILIATION_CONFLICT: issued scope {} changed while branch executed",
                scope.path()
            ),
        )?;
    }
    for change in &diff.changes {
        require(
            change.before_ignored.is_none() && change.after_ignored.is_none(),
            "RECONCILIATION_CONFLICT: ignored-file content is intentionally unavailable",
        )?;
        require(
            current.files.get(&change.path) == change.before.as_ref(),
            format!(
                "RECONCILIATION_CONFLICT: {} no longer matches the branch baseline",
                change.path
            ),
        )?;
    }
    let mut intended = current.clone();
    for change in &diff.changes {
        match &change.after {
            Some(state) => {
                intended.files.insert(change.path.clone(), state.clone());
            }
            None => {
                intended.files.remove(&change.path);
            }
        }
    }
    intended.dirty = intended.files != before.files || current.dirty;
    Ok(ReconciliationIntent {
        task_id: pending.task_id.clone(),
        executor: pending.executor.clone(),
        diff: pending.diff.clone(),
        expected_source: artifacts.json(current)?,
        intended_source: artifacts.json(&intended)?,
        paths: diff
            .changes
            .into_iter()
            .map(|change| ReconciliationPath {
                path: change.path,
                before: change.before,
                after: change.after,
            })
            .collect(),
    })
}

/// Recover or complete a durable publication intent. The initial classification
/// proves that the whole workspace is a recoverable 0/N..N/N state. Each path
/// is then captured again immediately before its destructive operation and is
/// changed only from its exact expected-before identity; intended-after is
/// idempotently skipped and every other identity is refused. The result of each
/// operation is captured before advancing. The compare and write/delete syscalls
/// are necessarily separate (there is no filesystem CAS), but no earlier batch
/// classification is used as destructive authority.
pub(super) fn publish_reconciliation(
    root: &Path,
    intent: &ReconciliationIntent,
    artifacts: &Artifacts,
) -> Result<SourceSnapshot> {
    publish_reconciliation_with(root, intent, artifacts, |_| Ok(()))
}

fn publish_reconciliation_with(
    root: &Path,
    intent: &ReconciliationIntent,
    artifacts: &Artifacts,
    mut after_write: impl FnMut(usize) -> Result<()>,
) -> Result<SourceSnapshot> {
    let expected: SourceSnapshot = artifacts.decode(&intent.expected_source)?;
    let intended: SourceSnapshot = artifacts.decode(&intent.intended_source)?;
    let actual = source::capture_bound(
        root,
        artifacts,
        &expected.repository_id,
        &expected.workspace_id,
    )?;
    require(
        actual.repository_id == expected.repository_id
            && actual.workspace_id == expected.workspace_id
            && actual.head == expected.head
            && actual.index_hash == expected.index_hash
            && actual.exclude_hash == expected.exclude_hash
            && actual.ignored == expected.ignored,
        "RECONCILIATION_UNRESOLVED: canonical Git/ignored-source identity differs from durable intent",
    )?;
    let affected: BTreeSet<_> = intent.paths.iter().map(|path| path.path.as_str()).collect();
    require(
        actual
            .files
            .keys()
            .chain(expected.files.keys())
            .filter(|path| !affected.contains(path.as_str()))
            .all(|path| actual.files.get(path) == expected.files.get(path)),
        "RECONCILIATION_UNRESOLVED: non-publication source changed while intent was pending",
    )?;
    let mut writes = 0;
    for path in &intent.paths {
        let observed = actual.files.get(&path.path);
        require(
            observed == path.before.as_ref() || observed == path.after.as_ref(),
            format!(
                "RECONCILIATION_UNRESOLVED: {} matches neither expected-before nor intended-after content",
                path.path
            ),
        )?;
    }
    for path in &intent.paths {
        let current = source::capture_bound(
            root,
            artifacts,
            &expected.repository_id,
            &expected.workspace_id,
        )?;
        let observed = current.files.get(&path.path);
        if observed == path.after.as_ref() {
            continue;
        }
        require(
            observed == path.before.as_ref(),
            format!(
                "RECONCILIATION_UNRESOLVED: {} changed after classification and before publication",
                path.path
            ),
        )?;
        match &path.after {
            Some(state) => write_state(root, &path.path, state, artifacts, path.before.is_none())?,
            None => fs::remove_file(root.join(&path.path))?,
        }
        let published = source::capture_bound(
            root,
            artifacts,
            &expected.repository_id,
            &expected.workspace_id,
        )?;
        require(
            published.files.get(&path.path) == path.after.as_ref(),
            format!(
                "RECONCILIATION_UNRESOLVED: {} did not reach intended-after identity",
                path.path
            ),
        )?;
        writes += 1;
        after_write(writes)?;
    }
    let result = source::capture_bound(
        root,
        artifacts,
        &expected.repository_id,
        &expected.workspace_id,
    )?;
    require(
        result == intended,
        "RECONCILIATION_UNRESOLVED: publication did not reach its intended canonical source",
    )?;
    Ok(result)
}

fn overlap(a: &ScopePath, b: &ScopePath) -> bool {
    source::permits(a, b.path()) || source::permits(b, a.path())
}

fn reaches(tasks: &[TaskPacket], from: &TaskId, to: &TaskId) -> bool {
    let mut open = vec![to];
    let mut seen = BTreeSet::new();
    while let Some(id) = open.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(task) = tasks.iter().find(|task| &task.task_id == id) else {
            continue;
        };
        if task
            .dependencies
            .iter()
            .any(|dependency| dependency == from)
        {
            return true;
        }
        open.extend(&task.dependencies);
    }
    false
}

fn entity(
    store: &Store,
    workspace: &WorkspaceId,
    id: &GraphEntityId,
) -> Result<Option<graph::Entity>> {
    let json: Option<String> = store
        .connection
        .query_row(
            "SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND entity_id=?2",
            params![workspace.as_str(), id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|value| serde_json::from_str(&value).map_err(Error::from))
        .transpose()
}

/// Decide whether two tasks may be issued from the same source baseline. Every
/// positive decision contains both scope and accepted-ontology evidence.
pub fn decide(
    store: &Store,
    info: &RepositoryInfo,
    plan: &planning::ExecutionPlan,
    left: &TaskPacket,
    right: &TaskPacket,
) -> Result<CompatibilityDecision> {
    let mut reasons = vec![];
    if plan.metadata.source.observation.repository_id != info.repository_id
        || plan.metadata.source.observation.workspace_id != info.workspace_id
    {
        reasons.push(CompatibilityReason::DifferentSource {
            left: plan
                .metadata
                .source
                .observation
                .workspace_id
                .as_str()
                .into(),
            right: info.workspace_id.as_str().into(),
        });
        return Ok(CompatibilityDecision {
            left: left.task_id.clone(),
            right: right.task_id.clone(),
            decision: Compatibility::SourceIncompatible,
            reasons,
        });
    }
    if reaches(&plan.packet.tasks, &left.task_id, &right.task_id) {
        reasons.push(CompatibilityReason::DagDependency {
            predecessor: left.task_id.clone(),
            successor: right.task_id.clone(),
        });
    }
    if reaches(&plan.packet.tasks, &right.task_id, &left.task_id) {
        reasons.push(CompatibilityReason::DagDependency {
            predecessor: right.task_id.clone(),
            successor: left.task_id.clone(),
        });
    }
    if !reasons.is_empty() {
        return Ok(CompatibilityDecision {
            left: left.task_id.clone(),
            right: right.task_id.clone(),
            decision: Compatibility::DependencyBlocked,
            reasons,
        });
    }
    for a in &left.write_scope {
        for b in &right.write_scope {
            if overlap(a, b) {
                reasons.push(CompatibilityReason::WriteWriteOverlap {
                    left: a.path().into(),
                    right: b.path().into(),
                });
            }
        }
        for b in &right.read_scope {
            if overlap(a, b) {
                reasons.push(CompatibilityReason::WriteReadInterference {
                    writer: left.task_id.clone(),
                    write: a.path().into(),
                    reader: right.task_id.clone(),
                    read: b.path().into(),
                });
            }
        }
    }
    for b in &right.write_scope {
        for a in &left.read_scope {
            if overlap(a, b) {
                reasons.push(CompatibilityReason::WriteReadInterference {
                    writer: right.task_id.clone(),
                    write: b.path().into(),
                    reader: left.task_id.clone(),
                    read: a.path().into(),
                });
            }
        }
    }
    if !reasons.is_empty() {
        return Ok(CompatibilityDecision {
            left: left.task_id.clone(),
            right: right.task_id.clone(),
            decision: Compatibility::Conflict,
            reasons,
        });
    }
    reasons.push(CompatibilityReason::DisjointDeclaredScopes);
    if left.graph_entities.is_empty() || right.graph_entities.is_empty() {
        let task = if left.graph_entities.is_empty() {
            &left.task_id
        } else {
            &right.task_id
        };
        reasons.push(CompatibilityReason::MissingSemanticAuthority { task: task.clone() });
        return Ok(CompatibilityDecision {
            left: left.task_id.clone(),
            right: right.task_id.clone(),
            decision: Compatibility::Unknown,
            reasons,
        });
    }
    let left_ids: BTreeSet<_> = left.graph_entities.iter().cloned().collect();
    let right_ids: BTreeSet<_> = right.graph_entities.iter().cloned().collect();
    if let Some(shared) = left_ids.intersection(&right_ids).next() {
        reasons.push(CompatibilityReason::SharedEntity {
            entity: shared.clone(),
        });
        return Ok(CompatibilityDecision {
            left: left.task_id.clone(),
            right: right.task_id.clone(),
            decision: Compatibility::Conflict,
            reasons,
        });
    }
    let generation = graph::generation(&store.connection, info)?
        .ok_or_else(|| Error::Invalid("concurrency requires an accepted ontology".into()))?;
    for (source_task, source_ids, other, other_ids) in [
        (&left.task_id, &left_ids, right, &right_ids),
        (&right.task_id, &right_ids, left, &left_ids),
    ] {
        let entities: Vec<_> = source_ids
            .iter()
            .map(|id| {
                entity(store, &info.workspace_id, id)?.ok_or_else(|| {
                    Error::Invalid(format!("task graph entity {} disappeared", id.as_str()))
                })
            })
            .collect::<Result<_>>()?;
        let report = graph::proposed_impact(
            &store.graph(&info.root)?,
            &entities,
            ImpactLimits {
                depth: 4,
                seeds: 32,
                items: 500,
                tests: 100,
                boundaries: 200,
                fanout: 512,
            },
        )?;
        if let Some(boundary) = report.boundaries.first() {
            reasons.push(CompatibilityReason::SemanticBoundary {
                task: source_task.clone(),
                detail: format!("{:?}:{}", boundary.reason, boundary.entity.as_str()),
            });
            return Ok(CompatibilityDecision {
                left: left.task_id.clone(),
                right: right.task_id.clone(),
                decision: Compatibility::Unknown,
                reasons,
            });
        }
        if report.summary.items_omitted > 0 || report.summary.boundaries_omitted > 0 {
            reasons.push(CompatibilityReason::SemanticBoundary {
                task: source_task.clone(),
                detail: "bounded impact evidence was truncated".into(),
            });
            return Ok(CompatibilityDecision {
                left: left.task_id.clone(),
                right: right.task_id.clone(),
                decision: Compatibility::Unknown,
                reasons,
            });
        }
        if let Some(item) = report.items.iter().find(|item| {
            other_ids.contains(&item.entity)
                || other
                    .read_scope
                    .iter()
                    .chain(&other.write_scope)
                    .any(|scope| source::permits(scope, &item.path))
        }) {
            reasons.push(CompatibilityReason::SemanticInterference {
                source: item.seed.clone(),
                affected: item.entity.clone(),
                path: item.path.clone(),
            });
            return Ok(CompatibilityDecision {
                left: left.task_id.clone(),
                right: right.task_id.clone(),
                decision: Compatibility::Conflict,
                reasons,
            });
        }
    }
    reasons.push(CompatibilityReason::OntologyProvesNoInterference { generation });
    Ok(CompatibilityDecision {
        left: left.task_id.clone(),
        right: right.task_id.clone(),
        decision: Compatibility::Compatible,
        reasons,
    })
}

impl Store {
    pub fn runtime_compatibility(
        &self,
        root: &Path,
        plan_id: &PlanId,
    ) -> Result<Vec<CompatibilityDecision>> {
        let info = graph::checked_workspace(self, root)?;
        let plan = self.execution_plan(root, plan_id)?.plan;
        let tasks = self.execution_tasks(root, plan_id)?;
        let ready: Vec<_> = tasks
            .iter()
            .filter(|task| task.structurally_ready)
            .collect();
        let mut decisions = vec![];
        for (index, left) in ready.iter().enumerate() {
            for right in &ready[index + 1..] {
                decisions.push(decide(self, &info, &plan, &left.packet, &right.packet)?);
            }
        }
        Ok(decisions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_read_scope_blocks_reconciliation_before_any_write() {
        let root = std::env::temp_dir().join(format!(
            "agentctl-reconcile-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("write.rs"), b"before").unwrap();
        let artifacts = Artifacts::new(&root.join("artifacts")).unwrap();
        let repo = RepositoryId::try_from(format!("repo-{}", "1".repeat(64))).unwrap();
        let workspace = WorkspaceId::try_from(format!("workspace-{}", "2".repeat(64))).unwrap();
        let state = |bytes: &[u8]| source::FileState {
            content: artifacts.put(bytes).unwrap(),
            mode: 0o644,
        };
        let before = SourceSnapshot {
            repository_id: repo.clone(),
            workspace_id: workspace.clone(),
            head: Some("deadbeef".into()),
            dirty: false,
            index_hash: "index".into(),
            files: BTreeMap::from([
                ("read.rs".into(), state(b"old read")),
                ("write.rs".into(), state(b"before")),
            ]),
            ignored: BTreeMap::new(),
            exclude_hash: None,
        };
        let mut current = before.clone();
        current
            .files
            .insert("read.rs".into(), state(b"sibling changed it"));
        let after = SourceSnapshot {
            files: BTreeMap::from([
                ("read.rs".into(), state(b"old read")),
                ("write.rs".into(), state(b"branch result")),
            ]),
            dirty: true,
            ..before.clone()
        };
        let plan = PlanId::new("plan:test").unwrap();
        let task_id = TaskId::new("task:test").unwrap();
        let diff = CapturedDiff {
            plan_id: plan,
            task_id: Some(task_id.clone()),
            executor_job_id: Some(JobId::new("job:test").unwrap()),
            workspace_id: workspace,
            before: artifacts.json(&before).unwrap(),
            after: artifacts.json(&after).unwrap(),
            changes: vec![source::FileChange {
                path: "write.rs".into(),
                before: Some(state(b"before")),
                after: Some(state(b"branch result")),
                before_ignored: None,
                after_ignored: None,
            }],
            scope_violations: vec![],
        };
        let task = TaskPacket {
            version: ProtocolVersion::V1,
            task_id: task_id.clone(),
            objective: "test stale reconcile".into(),
            read_scope: vec![ScopePath::File {
                path: "read.rs".into(),
            }],
            write_scope: vec![ScopePath::File {
                path: "write.rs".into(),
            }],
            graph_entities: vec![],
            invariant_refs: vec![],
            dependencies: vec![],
            definition_of_done: vec!["done".into()],
            verification: VerificationRequirements {
                requirement_refs: vec!["unit".into()],
                evidence_required: true,
            },
        };
        let pending = PendingTask {
            task_id: task_id.clone(),
            executor: JobId::new("job:test").unwrap(),
            before: diff.before.clone(),
            after: diff.after.clone(),
            diff: artifacts.json(&diff).unwrap(),
            evidence: vec![],
            verifier: None,
            proof: None,
            execution_workspace: Some(root.display().to_string()),
            compatibility: vec![CompatibilityDecision {
                left: task_id.clone(),
                right: TaskId::new("task:sibling").unwrap(),
                decision: Compatibility::Compatible,
                reasons: vec![CompatibilityReason::DisjointDeclaredScopes],
            }],
            ontology_generation: Some("generation:test".into()),
            reconciled_from: None,
        };
        let error = reconciliation_intent(&task, &pending, &current, &artifacts).unwrap_err();
        assert!(error.to_string().contains("RECONCILIATION_CONFLICT"));
        assert_eq!(fs::read(root.join("write.rs")).unwrap(), b"before");
        fs::remove_dir_all(root).unwrap();
    }

    struct PublicationFixture {
        root: PathBuf,
        artifacts: Artifacts,
        intent: ReconciliationIntent,
        paths: Vec<(String, Vec<u8>, Vec<u8>)>,
    }

    fn publication_fixture() -> PublicationFixture {
        static NEXT_PUBLICATION: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agentctl-publication-{}-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap(),
            NEXT_PUBLICATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]).unwrap();
        ProjectConfig::initialize(&root).unwrap();
        let paths: Vec<(String, Vec<u8>, Vec<u8>)> = vec![
            ("a.rs".into(), b"a-before".to_vec(), b"a-after".to_vec()),
            ("b.rs".into(), b"b-before".to_vec(), b"b-after".to_vec()),
            ("c.rs".into(), b"c-before".to_vec(), b"c-after".to_vec()),
        ];
        for (path, before, _) in &paths {
            fs::write(root.join(path), before).unwrap();
        }
        git(&root, &["add", "."]).unwrap();
        git(
            &root,
            &[
                "-c",
                "user.name=agentctl",
                "-c",
                "user.email=agentctl@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        )
        .unwrap();
        let info = RepositoryInfo::discover(&root).unwrap();
        let artifacts = Artifacts::new(&root.with_extension("artifacts")).unwrap();
        let expected =
            source::capture_bound(&root, &artifacts, &info.repository_id, &info.workspace_id)
                .unwrap();
        let mut intended = expected.clone();
        intended.dirty = true;
        let reconciliation_paths = paths
            .iter()
            .map(|(path, _, after)| {
                let before = expected.files.get(path).cloned();
                let after = source::FileState {
                    content: artifacts.put(after).unwrap(),
                    mode: before.as_ref().unwrap().mode,
                };
                intended.files.insert(path.clone(), after.clone());
                ReconciliationPath {
                    path: path.clone(),
                    before,
                    after: Some(after),
                }
            })
            .collect();
        let intent = ReconciliationIntent {
            task_id: TaskId::new("task:publish").unwrap(),
            executor: JobId::new("job:publish").unwrap(),
            diff: artifacts.put(b"diff").unwrap(),
            expected_source: artifacts.json(&expected).unwrap(),
            intended_source: artifacts.json(&intended).unwrap(),
            paths: reconciliation_paths,
        };
        PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        }
    }

    fn intent_for_paths(
        root: &Path,
        artifacts: &Artifacts,
        paths: Vec<ReconciliationPath>,
    ) -> ReconciliationIntent {
        let info = RepositoryInfo::discover(root).unwrap();
        let expected =
            source::capture_bound(root, artifacts, &info.repository_id, &info.workspace_id)
                .unwrap();
        let mut intended = expected.clone();
        for path in &paths {
            match &path.after {
                Some(after) => {
                    intended.files.insert(path.path.clone(), after.clone());
                }
                None => {
                    intended.files.remove(&path.path);
                }
            }
        }
        intended.dirty = intended.files != expected.files || expected.dirty;
        ReconciliationIntent {
            task_id: TaskId::new("task:publish").unwrap(),
            executor: JobId::new("job:publish").unwrap(),
            diff: artifacts.put(b"diff").unwrap(),
            expected_source: artifacts.json(&expected).unwrap(),
            intended_source: artifacts.json(&intended).unwrap(),
            paths,
        }
    }

    #[test]
    fn every_publication_prefix_is_identified_and_completed() {
        let PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        } = publication_fixture();
        for prefix in 0..=paths.len() {
            for (index, (path, before, after)) in paths.iter().enumerate() {
                fs::write(root.join(path), if index < prefix { after } else { before }).unwrap();
            }
            let result = publish_reconciliation(&root, &intent, &artifacts).unwrap();
            let intended: SourceSnapshot = artifacts.decode(&intent.intended_source).unwrap();
            assert_eq!(result, intended, "prefix {prefix}/{}", paths.len());
        }
        // This is the N/N-before-completion-checkpoint case: replay is idempotent.
        publish_reconciliation(&root, &intent, &artifacts).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn injected_io_failure_leaves_a_known_prefix_that_resumes() {
        let PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        } = publication_fixture();
        let error = publish_reconciliation_with(&root, &intent, &artifacts, |writes| {
            require(writes != 2, "injected publication I/O failure")
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected publication I/O failure")
        );
        assert_eq!(fs::read(root.join(&paths[0].0)).unwrap(), paths[0].2);
        assert_eq!(fs::read(root.join(&paths[1].0)).unwrap(), paths[1].2);
        assert_eq!(fs::read(root.join(&paths[2].0)).unwrap(), paths[2].1);
        publish_reconciliation(&root, &intent, &artifacts).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_third_party_content_fails_closed_before_any_write() {
        let PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        } = publication_fixture();
        fs::write(root.join(&paths[1].0), b"third-party").unwrap();
        let error = publish_reconciliation(&root, &intent, &artifacts).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither expected-before nor intended-after")
        );
        assert_eq!(fs::read(root.join(&paths[0].0)).unwrap(), paths[0].1);
        assert_eq!(fs::read(root.join(&paths[1].0)).unwrap(), b"third-party");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_changed_after_batch_classification_is_not_overwritten() {
        let PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        } = publication_fixture();
        let attacked = root.join(&paths[1].0);
        let error = publish_reconciliation_with(&root, &intent, &artifacts, |writes| {
            if writes == 1 {
                fs::write(&attacked, b"third-party-after-classification")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed after classification"));
        assert_eq!(fs::read(root.join(&paths[0].0)).unwrap(), paths[0].2);
        assert_eq!(
            fs::read(root.join(&paths[1].0)).unwrap(),
            b"third-party-after-classification"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_appearing_after_classification_is_not_overwritten_by_add() {
        let PublicationFixture {
            root,
            artifacts,
            paths,
            ..
        } = publication_fixture();
        let added = "new.rs";
        let after = source::FileState {
            content: artifacts.put(b"intended-add").unwrap(),
            mode: 0o644,
        };
        let first_before = source::capture(&root, &artifacts)
            .unwrap()
            .files
            .get(&paths[0].0)
            .cloned()
            .unwrap();
        let first_after = source::FileState {
            content: artifacts.put(&paths[0].2).unwrap(),
            mode: first_before.mode,
        };
        let intent = intent_for_paths(
            &root,
            &artifacts,
            vec![
                ReconciliationPath {
                    path: paths[0].0.clone(),
                    before: Some(first_before),
                    after: Some(first_after),
                },
                ReconciliationPath {
                    path: added.into(),
                    before: None,
                    after: Some(after),
                },
            ],
        );
        let target = root.join(added);
        let error = publish_reconciliation_with(&root, &intent, &artifacts, |writes| {
            if writes == 1 {
                fs::write(&target, b"third-party-add")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed after classification"));
        assert_eq!(fs::read(target).unwrap(), b"third-party-add");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_changed_after_classification_is_not_deleted() {
        let PublicationFixture {
            root,
            artifacts,
            paths,
            ..
        } = publication_fixture();
        let snapshot = source::capture(&root, &artifacts).unwrap();
        let first_before = snapshot.files.get(&paths[0].0).cloned().unwrap();
        let first_after = source::FileState {
            content: artifacts.put(&paths[0].2).unwrap(),
            mode: first_before.mode,
        };
        let deleted_before = snapshot.files.get(&paths[1].0).cloned().unwrap();
        let intent = intent_for_paths(
            &root,
            &artifacts,
            vec![
                ReconciliationPath {
                    path: paths[0].0.clone(),
                    before: Some(first_before),
                    after: Some(first_after),
                },
                ReconciliationPath {
                    path: paths[1].0.clone(),
                    before: Some(deleted_before),
                    after: None,
                },
            ],
        );
        let target = root.join(&paths[1].0);
        let error = publish_reconciliation_with(&root, &intent, &artifacts, |writes| {
            if writes == 1 {
                fs::write(&target, b"third-party-delete")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed after classification"));
        assert_eq!(fs::read(target).unwrap(), b"third-party-delete");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_file_transition_is_published_and_verified() {
        let PublicationFixture {
            root,
            artifacts,
            paths,
            ..
        } = publication_fixture();
        let before = source::capture(&root, &artifacts)
            .unwrap()
            .files
            .get(&paths[0].0)
            .cloned()
            .unwrap();
        let after = source::FileState {
            content: artifacts.put(b"").unwrap(),
            mode: before.mode,
        };
        let intent = intent_for_paths(
            &root,
            &artifacts,
            vec![ReconciliationPath {
                path: paths[0].0.clone(),
                before: Some(before),
                after: Some(after),
            }],
        );
        publish_reconciliation(&root, &intent, &artifacts).unwrap();
        assert!(fs::read(root.join(&paths[0].0)).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mode_changed_after_classification_refuses_publication() {
        use std::os::unix::fs::PermissionsExt;

        let PublicationFixture {
            root,
            artifacts,
            intent,
            paths,
        } = publication_fixture();
        let attacked = root.join(&paths[1].0);
        let error = publish_reconciliation_with(&root, &intent, &artifacts, |writes| {
            if writes == 1 {
                fs::set_permissions(&attacked, fs::Permissions::from_mode(0o755))?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed after classification"));
        assert_eq!(
            fs::metadata(attacked).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }
}
