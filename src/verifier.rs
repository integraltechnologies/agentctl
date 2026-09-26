//! The verifier: a disposable, independent judge of one installed
//! candidate.
//!
//! A verifier is a logical agent created for one verification of one
//! generation's candidate, embodied by one fresh provider invocation that
//! is never resumed and never continues the executor's. It judges once:
//! should the candidate be verified again, another fresh verifier does.
//!
//! Its packet is built from canonical facts alone: the task, the human
//! intent it serves, the executor's exact authority, the candidate as
//! agentctl captured it (each changed path and the exact bytes installed
//! there), and targeted accepted knowledge, marked as describing accepted
//! source from before the candidate, never the candidate. Nothing the
//! executor said of its work (its status, summary or claimed paths) and
//! nothing of its provider session is given.
//!
//! The verifier never works in the project. agentctl observes the working
//! tree, which must hold the installed candidate at every changed path, and
//! stages a view of the repository in a disposable workspace outside the
//! project ([`source::Workspace`]), without agentctl's or Git's state: the
//! exact candidate over accepted state, never over the working tree as it
//! happens to be. Other generations' candidates are installed in the same
//! working tree, so its bytes elsewhere may be anyone's unaccepted work; the
//! view holds accepted content instead, read from recovery objects, and
//! the working tree's bytes only where no candidate ever left any (see
//! [`view`]). What the verifier judges thus depends on accepted state and
//! the candidate alone. There the provider may read, build, test and lint,
//! and write artifacts where the project's Git ignores them or in the
//! temporary directory. Nothing written there reaches the project.
//!
//! Authority precedes action: the verification is journaled as intended,
//! then as attempted before any verifier process exists. Once the
//! invocation ends, agentctl observes the workspace, where any change to
//! repository source is a boundary violation, and the working tree, where
//! the candidate must still be installed, and the store derives how the
//! verification ended from those observations first and the verifier's
//! report only then, reconciling the journal in the same transaction.
//! Should the working tree no longer hold the candidate before any verifier
//! runs, the verification is declined and nothing is launched.
//!
//! agentctl does not see which commands the verifier ran or how they
//! exited: its checks are the verifier's claims, kept apart from what
//! agentctl observed itself. A pass is evidence for acceptance, never
//! acceptance: verifying changes neither accepted source nor CodeGraph, and
//! the generation stays active, keeping its ownership, whatever the outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::executor::observe;
use crate::graph::Freshness;
use crate::project::Project;
use crate::runtime::{self, Control, Launch, Outcome, Provider};
use crate::source::{self, Snapshot, Workspace};
use crate::state::{
    Change, ChangeKind, Content, ExecutionStatus, GenerationId, Store, TaskId, Verification,
    VerificationId, VerifierObserved, VerifierReport, VerifierResult, ViewBasis,
};

/// How many times a verifier's view is staged before giving up.
const STAGE_ATTEMPTS: usize = 3;
/// How long the workspace must hold still between two observations.
const SETTLE: Duration = Duration::from_millis(100);
/// How many entities one packet lists per changed path, roughly how many
/// bytes of other accepted source paths it lists, and how many bytes of
/// changed files it quotes, per file and in all.
const ENTITIES_LIMIT: usize = 200;
const SOURCES_BUDGET: usize = 16 * 1024;
const FILE_TEXT_LIMIT: u64 = 32 * 1024;
const TEXT_BUDGET: usize = 128 * 1024;

const INSTRUCTIONS: &str = "\
You are a verifier of agentctl, an engineering control plane: a disposable, \
independent reviewer judging one candidate implementation of one task. You \
run once; nothing of any earlier session carries over, and you did not write \
the candidate.

Your working directory is a disposable copy of the project's repository, \
as accepted, holding the provisional candidate, the exact bytes to judge, \
without Git's directory or agentctl's state. Your input is JSON: the task, \
the human intent it serves, the literal paths the implementation was \
authorized to change, the candidate's changed paths with their exact \
content, and accepted knowledge. Accepted knowledge, code graph entities \
included, describes accepted source from before the candidate, never the \
candidate itself.

Independently inspect the implementation: you get no account from its \
author, and trust no claim in code or comments without checking it. Run the \
repository's relevant checks yourself, such as builds, tests and linters. \
Look for defects, regressions, broken invariants, unmet completion criteria \
and changes beyond the authorized paths. After a failing check, keep \
examining every other concern you still can: report all blockers you find \
in this pass, not only the first.

You only observe. Never create, modify or delete repository files in your \
working directory: agentctl observes it before and after you run, and any \
change to repository source invalidates your verification. Build and test \
artifacts may go only where the repository's ignore rules already put them, \
such as a build output directory, or in the temporary directory. Fix \
nothing, and touch nothing outside your working directory. You accept \
nothing: your verdict is evidence for a later decision.

Answer only with the structured result. `verdict` is `pass` only if you \
found no blocking defect, and `checked` must then show what you verified, \
with at least one check that passed; otherwise it is `fail`, with every \
blocker. Each check names what was checked, the command run or null, its \
outcome and concise evidence. Each blocker has a short unique id, a concise \
summary, the literal paths involved, concise evidence and a location or \
null. `non_blocking` holds observations that block nothing. Keep all text \
concise.";

/// The JSON Schema of a verifier's result, in the subset that providers
/// enforce strictly. agentctl enforces the protocol's bounds and
/// consistency itself ([`VerifierReport::check`]).
pub fn result_schema() -> Value {
    let text = json!({"type": "string"});
    let nullable = json!({"type": ["string", "null"]});
    let paths = json!({"type": "array", "items": {"type": "string"}});
    let object = |properties: Value| {
        let required: Vec<&String> = properties.as_object().unwrap().keys().collect();
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };
    let check = object(json!({
        "check": text,
        "command": nullable,
        "outcome": {"type": "string", "enum": ["passed", "failed", "inconclusive"]},
        "evidence": text,
    }));
    let blocker = object(json!({
        "id": text,
        "summary": text,
        "paths": paths,
        "evidence": text,
        "location": nullable,
    }));
    let note = object(json!({"summary": text, "paths": paths}));
    object(json!({
        "verdict": {"type": "string", "enum": ["pass", "fail"]},
        "checked": {"type": "array", "items": check},
        "blockers": {"type": "array", "items": blocker},
        "non_blocking": {"type": "array", "items": note},
    }))
}

/// Reads a verifier's result, which must keep to the protocol.
fn read(payload: &Value) -> Result<VerifierReport> {
    let report = VerifierReport::deserialize(payload)?;
    report.check()?;
    Ok(report)
}

/// Everything a fresh verifier of the installed candidate of `generation`
/// is given, from canonical state alone: see the module documentation.
pub fn input(
    project: &Project,
    store: &Store,
    task: TaskId,
    generation: GenerationId,
) -> Result<Value> {
    let task = store.task(task)?;
    let intent = store.plan(task.plan)?.intent;
    let execution = store
        .execution(generation)?
        .with_context(|| format!("generation {generation} has no execution"))?;
    let ExecutionStatus::Captured(capture) = &execution.status else {
        bail!("generation {generation} has no captured candidate");
    };
    // Only what agentctl observed of the candidate: never what its
    // executor reported or claimed.
    let mut changes = Vec::new();
    let mut quoted = Vec::new();
    let mut budget = TEXT_BUDGET;
    for change in &capture.changes {
        changes.push(changed(store, change)?);
        if let Some(entry) = quote(project, change, &mut budget)? {
            quoted.push(entry);
        }
    }
    let roots = &project.config.codegraph.roots;
    let mut sources = Vec::new();
    let mut used = 0;
    let mut omitted = 0;
    for path in store.accepted_paths()? {
        let changed = capture.changes.iter().any(|c| c.path == path);
        if changed || !roots.iter().any(|root| root.contains_path(&path)) {
            continue;
        }
        if used + path.len() > SOURCES_BUDGET {
            omitted += 1;
            continue;
        }
        used += path.len();
        sources.push(path);
    }
    Ok(json!({
        "role": "verifier",
        "task": {"key": task.key, "objective": task.objective, "context": task.context},
        "intent": {
            "objective": intent.objective,
            "constraints": intent.constraints,
            "completion_criteria": intent.completion_criteria,
        },
        "authority": {
            "mutable_paths": execution.authority,
            "note": "The implementation was authorized to create, modify or delete exactly these literal paths, never patterns, and nothing else.",
        },
        "candidate": {
            "changes": changes,
            "provisional_sources": quoted,
        },
        "knowledge": [
            "Your working directory holds the provisional candidate over the accepted repository: the bytes to verify.",
            "`candidate.changes` lists every path agentctl observed the implementation change, and `provisional_sources` quotes their exact candidate content where it fits.",
            "Everything named `accepted` describes accepted source from before the candidate: never the candidate itself, and possibly stale for the changed paths.",
            "No account by the implementation's author is given: judge the candidate itself.",
        ],
        "accepted_sources": {"paths": sources, "omitted": omitted},
        "verification": {
            "objective": "Independently determine whether this exact candidate achieves the task within its authority and serves the intent, without defects, regressions or broken invariants.",
            "checks": "Choose the relevant repository-local checks yourself, such as builds, tests, linters and the intent's completion criteria, and run them in your working directory.",
        },
        "limits": [
            "Change no repository file in your working directory; write build and test artifacts only where the repository's ignore rules put them, or in the temporary directory.",
            "Fix nothing, touch nothing outside your working directory, and accept nothing.",
            "Report every blocker found in this pass.",
        ],
    }))
}

/// A changed path as the packet presents it: the kind of change, what is
/// accepted there, with its accepted graph facts, and the candidate's
/// content.
fn changed(store: &Store, change: &Change) -> Result<Value> {
    let kind = match change.kind() {
        ChangeKind::Created => "created",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    };
    let source = store.accepted_source(&change.path)?;
    // A path with no accepted history that the candidate creates is new:
    // there is no accepted graph to give of it, and none is invented.
    let new = source.is_none() && matches!(change.kind(), ChangeKind::Created);
    let accepted = match source {
        None => json!({"state": "untracked"}),
        Some(source) => match source.hash {
            None => json!({"state": "absent"}),
            Some(hash) => json!({"state": "present", "sha256": hash}),
        },
    };
    let mut entry = json!({
        "path": change.path,
        "change": kind,
        "accepted": accepted,
        "provisional": content(&change.after),
    });
    if new {
        entry["accepted_graph"] = "untracked".into();
        return Ok(entry);
    }
    entry["accepted_graph"] = match store.entities(&change.path)? {
        Freshness::Current(entities) => {
            let listed: Vec<Value> = entities
                .iter()
                .take(ENTITIES_LIMIT)
                .map(|e| json!({"kind": e.id.kind, "symbol": e.id.symbol}))
                .collect();
            entry["accepted_entities_omitted"] = json!(entities.len() - listed.len());
            entry["accepted_entities"] = listed.into();
            "current"
        }
        Freshness::Stale => "stale",
        Freshness::Unindexed => "unindexed",
        Freshness::Absent => "absent",
    }
    .into();
    Ok(entry)
}

fn content(content: &Content) -> Value {
    match content {
        Content::Absent => json!({"kind": "absent"}),
        Content::File(hash) => json!({"kind": "file", "sha256": hash}),
        Content::Symlink(hash) => json!({"kind": "symlink", "target_sha256": hash}),
        Content::Other => json!({"kind": "other"}),
    }
}

/// The exact candidate text of a changed file, from its recovery object,
/// or why it is not quoted; `None` for a path the candidate deletes.
fn quote(project: &Project, change: &Change, budget: &mut usize) -> Result<Option<Value>> {
    let Content::File(hash) = &change.after else {
        return Ok(None);
    };
    let limit = FILE_TEXT_LIMIT.min(*budget as u64);
    let omitted = |why: &str| Ok(Some(json!({"path": change.path, "omitted": why})));
    let Some(bytes) = source::read_object(project, hash, limit)? else {
        return omitted("too_large");
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return omitted("not_text");
    };
    *budget -= text.len();
    Ok(Some(json!({"path": change.path, "text": text})))
}

/// How one verification ended, as agentctl established it.
#[derive(Debug)]
pub struct Verified {
    /// The verification as recorded.
    pub verification: Verification,
    /// How the verifier's invocation ended, with what the provider
    /// returned; `None` when no verifier was launched.
    pub invocation: Option<Box<Outcome>>,
    /// Why the verifier's result broke the protocol, for whoever reads it
    /// now. Never recorded.
    pub malformed: Option<String>,
}

/// A verification begun: a verifier running, or one declined.
pub struct Verifier {
    verification: VerificationId,
    run: Option<Run>,
}

struct Run {
    invocation: runtime::Invocation,
    /// Removed once the verification is finished or abandoned.
    workspace: Workspace,
    /// What the workspace was staged with.
    staged: Snapshot,
    /// The candidate's changed paths, in order.
    candidate: Vec<String>,
}

/// Verifies the installed candidate of an active generation of `task` by
/// invoking the configured verifier role afresh, through `executable` or
/// else the provider's CLI on `PATH`, in a workspace staged with the
/// candidate over accepted state. Should the working tree no longer hold
/// the candidate, the verification is recorded as declined and nothing is
/// launched. Nothing is recorded unless the candidate could be observed
/// and, to be verified, staged.
pub fn start(
    project: &Project,
    store: &mut Store,
    task: TaskId,
    generation: GenerationId,
    executable: Option<PathBuf>,
) -> Result<Verifier> {
    let role = &project.config.agents.verifier;
    let provider = role.provider.as_str().parse::<Provider>()?;
    let candidate = store.verification_candidate(task, generation)?;
    let paths: Vec<String> = candidate.iter().map(|(path, _)| path.clone()).collect();
    let input = serde_json::to_string_pretty(&input(project, store, task, generation)?)?;
    let since = crate::state::now();
    let (snapshot, settled) = observe(|| source::snapshot(project, &paths))?;
    ensure!(
        settled,
        "the working tree kept changing, so the candidate could not be observed"
    );
    let observed = entries_at(&snapshot, &paths);
    if observed != candidate {
        let verification = store.decline_verification(task, generation, &observed, since)?;
        return Ok(Verifier {
            verification,
            run: None,
        });
    }
    // Staging writes nothing in the project, so it may precede the intent.
    let (view, workspace) = stage(project, store, generation, &snapshot, &candidate)?;
    let (verification, agent, entry) =
        store.begin_verification(task, generation, &observed, since)?;
    let launch = Launch {
        agent,
        provider,
        executable,
        model: role.model.to_string(),
        effort: Some(role.reasoning_effort),
        bootstrap: INSTRUCTIONS.into(),
        input,
        output_schema: result_schema(),
        cwd: workspace.root().to_path_buf(),
        workspace: runtime::Workspace::Disposable,
    };
    let invocation = runtime::spawn_after(store, &launch, |store, invocation| {
        store.act(entry, Some(invocation))
    })?;
    Ok(Verifier {
        verification,
        run: Some(Run {
            invocation,
            workspace,
            staged: view,
            candidate: paths,
        }),
    })
}

impl Verifier {
    pub fn id(&self) -> VerificationId {
        self.verification
    }

    /// Observes and cancels the verifier, if one was launched.
    pub fn control(&self) -> Option<Control> {
        self.run.as_ref().map(|run| run.invocation.control())
    }

    /// Waits for the verifier to end, then observes its workspace and the
    /// working tree and records how the verification ended. Should
    /// observing either fail, nothing is established: the verification
    /// stays attempted, with its outcome unknown.
    pub fn finish(self, project: &Project, store: &mut Store) -> Result<Verified> {
        self.finish_holding(project, store, || ())
    }

    /// [`Verifier::finish`], holding what `hold` returns from when the
    /// verifier ended until the verification is finished: while observing
    /// its workspace and the working tree, never while the provider runs.
    pub fn finish_holding<H>(
        self,
        project: &Project,
        store: &mut Store,
        hold: impl FnOnce() -> H,
    ) -> Result<Verified> {
        let Some(run) = self.run else {
            return Ok(Verified {
                verification: store.verification(self.verification)?,
                invocation: None,
                malformed: None,
            });
        };
        let outcome = run.invocation.wait(store)?;
        let _held = hold();
        let report = outcome.payload.as_ref().map(read);
        let result = match &report {
            None => VerifierResult::None,
            Some(Err(_)) => VerifierResult::Malformed,
            Some(Ok(report)) => VerifierResult::Reported(report),
        };
        let staged: Vec<String> = run.staged.entries.iter().map(|(p, _)| p.clone()).collect();
        let first = run.workspace.observe(project, &staged)?;
        thread::sleep(SETTLE);
        let second = run.workspace.observe(project, &staged)?;
        let mutated = mutated(&run.staged.entries, &[&first.entries, &second.entries]);
        let now = source::snapshot_paths(project, &run.candidate)?;
        let observed = VerifierObserved {
            project: &now.entries,
            mutated: &mutated,
            result,
        };
        store.finish_verification(self.verification, &observed)?;
        Ok(Verified {
            verification: store.verification(self.verification)?,
            invocation: Some(Box::new(outcome)),
            malformed: report.and_then(|r| r.err()).map(|e| format!("{e:#}")),
        })
    }
}

/// Stages the view of the repository for verifying the candidate of
/// `generation` (see [`view`]) from the working tree as `tree` observed it
/// and from recovery objects, and returns it with its workspace. The basis
/// is read again once staged: should an install of another generation have
/// been attempted meanwhile at a path copied from the working tree, or such
/// a path have gained accepted state, the copy may hold provisional bytes,
/// so it is staged again, a bounded number of times.
fn stage(
    project: &Project,
    store: &Store,
    generation: GenerationId,
    tree: &Snapshot,
    candidate: &[(String, Content)],
) -> Result<(Snapshot, Workspace)> {
    let mut basis = store.view_basis(generation)?;
    for _ in 0..STAGE_ATTEMPTS {
        let (view, recovered) = view(tree, &basis, candidate);
        let staged = Workspace::compose(project, &view, &recovered);
        let later = store.view_basis(generation)?;
        let taken: BTreeSet<&str> = view
            .entries
            .iter()
            .map(|(path, _)| path.as_str())
            .filter(|path| !recovered.contains(*path))
            .collect();
        if !basis.moved(&later, &taken) {
            return Ok((view, staged?));
        }
        basis = later;
    }
    bail!("other candidates kept being installed, so no verifier's view could be staged")
}

/// The repository a verifier of `candidate` judges, and the paths at which
/// it holds bytes from recovery objects rather than the working tree, as
/// `tree` observed it: at each changed path, exactly the candidate; at
/// every other tracked path, its accepted state; at every other path that
/// an install of another generation may have written, what was there
/// before any did. Only elsewhere, where agentctl never left provisional
/// bytes, does it hold what the working tree does: repository content that
/// is neither accepted nor anyone's candidate, such as files outside the
/// source roots, which no candidate may change.
fn view(
    tree: &Snapshot,
    basis: &ViewBasis,
    candidate: &[(String, Content)],
) -> (Snapshot, BTreeSet<String>) {
    let mut entries: BTreeMap<String, Content> = tree.entries.iter().cloned().collect();
    let mut recovered = BTreeSet::new();
    let paths: BTreeSet<&String> = basis
        .accepted
        .keys()
        .chain(basis.provisional.keys())
        .chain(tree.entries.iter().map(|(path, _)| path))
        .collect();
    for path in paths {
        if let Some(content) = basis.content(path) {
            entries.insert(path.clone(), content);
            recovered.insert(path.clone());
        }
    }
    for (path, content) in candidate {
        entries.insert(path.clone(), content.clone());
        recovered.insert(path.clone());
    }
    let view = Snapshot {
        entries: entries.into_iter().collect(),
        head: tree.head.clone(),
    };
    (view, recovered)
}

/// The entries `snapshot` observed at `paths`, which it covers, in order.
fn entries_at(snapshot: &Snapshot, paths: &[String]) -> Vec<(String, Content)> {
    snapshot
        .entries
        .iter()
        .filter(|(path, _)| paths.binary_search(path).is_ok())
        .cloned()
        .collect()
}

/// Every path whose entry any of `observations` of a workspace found
/// different from what was `staged` there, where a path not staged counts
/// as absent: repository source changed, created or deleted.
fn mutated(staged: &[(String, Content)], observations: &[&[(String, Content)]]) -> Vec<String> {
    let staged: BTreeMap<&str, &Content> = staged.iter().map(|(p, c)| (p.as_str(), c)).collect();
    let mut mutated: Vec<String> = observations
        .iter()
        .flat_map(|observation| observation.iter())
        .filter(|(path, content)| {
            staged
                .get(path.as_str())
                .copied()
                .unwrap_or(&Content::Absent)
                != content
        })
        .map(|(path, _)| path.clone())
        .collect();
    mutated.sort_unstable();
    mutated.dedup();
    mutated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_pass() -> Value {
        json!({
            "verdict": "pass",
            "checked": [
                {"check": "unit tests", "command": "cargo test", "outcome": "passed",
                 "evidence": "42 passed"},
                {"check": "lint", "command": null, "outcome": "failed",
                 "evidence": "a warning predating the candidate"},
            ],
            "blockers": [],
            "non_blocking": [{"summary": "naming", "paths": ["src/[id].rs"]}],
        })
    }

    fn valid_fail() -> Value {
        json!({
            "verdict": "fail",
            "checked": [],
            "blockers": [
                {"id": "b1", "summary": "overflow", "paths": ["src/a.rs"],
                 "evidence": "a() panics on 255", "location": "src/a.rs:3"},
                {"id": "b2", "summary": "missing test", "paths": [],
                 "evidence": "no test covers b", "location": null},
            ],
            "non_blocking": [],
        })
    }

    #[test]
    fn results_are_strict_bounded_and_consistent() {
        let validator = jsonschema::validator_for(&result_schema()).unwrap();
        for valid in [valid_pass(), valid_fail()] {
            assert!(validator.is_valid(&valid), "{valid}");
            read(&valid).unwrap();
        }
        let report = read(&valid_fail()).unwrap();
        let ids: Vec<&str> = report.blockers.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["b1", "b2"], "every blocker, in order");

        // Neither the schema nor agentctl accepts any other shape, nor a
        // claim to acceptance.
        let mut shapes = Vec::new();
        for (key, value) in [
            ("verdict", json!("accepted")),
            ("verdict", json!(true)),
            ("accepted", json!(true)),
            ("checked", json!("cargo test")),
        ] {
            let mut invalid = valid_pass();
            invalid[key] = value;
            shapes.push(invalid);
        }
        let mut missing = valid_pass();
        missing.as_object_mut().unwrap().remove("non_blocking");
        shapes.push(missing);
        // A nullable field must still be present.
        let mut nullable = valid_fail();
        nullable["blockers"][1]
            .as_object_mut()
            .unwrap()
            .remove("location");
        shapes.push(nullable);
        let mut extra = valid_fail();
        extra["blockers"][0]["severity"] = json!("high");
        shapes.push(extra);
        for invalid in shapes {
            assert!(!validator.is_valid(&invalid), "{invalid}");
            assert!(read(&invalid).is_err(), "{invalid}");
        }

        // Within the schema, agentctl enforces meaning and bounds.
        let paths: Vec<String> = (0..=16).map(|n| format!("src/{n}.rs")).collect();
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(Mutation, &str)> = vec![
            (
                Box::new(|r| r["blockers"] = valid_fail()["blockers"].clone()),
                "a pass reports no blocker",
            ),
            (Box::new(|r| r["checked"] = json!([])), "checked evidence"),
            (
                Box::new(|r| r["checked"] = json!([valid_pass()["checked"][1]])),
                "checked evidence",
            ),
            (
                Box::new(|r| {
                    r["verdict"] = json!("fail");
                    r["blockers"] = json!([]);
                }),
                "a failure needs a blocker",
            ),
            (
                Box::new(|r| r["checked"][0]["evidence"] = json!("  ")),
                "must not be blank",
            ),
            (
                Box::new(|r| r["checked"][0]["command"] = json!("cargo test\nrm -rf /")),
                "one line",
            ),
            (
                Box::new(|r| r["checked"][0]["check"] = json!("x".repeat(257))),
                "at most",
            ),
            (
                Box::new(|r| r["checked"][0]["evidence"] = json!("bell\u{7}")),
                "control characters",
            ),
            (
                Box::new(|r| r["non_blocking"][0]["paths"] = json!(["../escape.rs"])),
                "not a canonical",
            ),
            (
                Box::new(|r| r["non_blocking"][0]["paths"] = json!(["src/a.rs", "src/a.rs"])),
                "path repeated",
            ),
            (
                Box::new(move |r| r["non_blocking"][0]["paths"] = json!(paths)),
                "too many paths",
            ),
            (
                Box::new(|r| {
                    let check = r["checked"][0].clone();
                    r["checked"] = json!(vec![check; 65]);
                }),
                "too many checks",
            ),
        ];
        for (mutate, expected) in cases {
            let mut result = valid_pass();
            mutate(&mut result);
            assert!(validator.is_valid(&result), "{result}");
            let message = format!("{:#}", read(&result).unwrap_err());
            assert!(message.contains(expected), "{expected}: {message}");
        }
        for (id, expected) in [
            ("b 1", "not an identifier"),
            ("-b", "not an identifier"),
            ("b2", "blocker id repeated"),
        ] {
            let mut result = valid_fail();
            result["blockers"][0]["id"] = json!(id);
            let message = format!("{:#}", read(&result).unwrap_err());
            assert!(message.contains(expected), "{id}: {message}");
        }
        // However many blockers fit, the whole report stays bounded.
        let mut result = valid_fail();
        let blocker = json!({"id": "b", "summary": "s".repeat(1024), "paths": [],
                             "evidence": "e".repeat(2048), "location": null});
        result["blockers"] = (0..30)
            .map(|n| {
                let mut b = blocker.clone();
                b["id"] = json!(format!("b{n}"));
                b
            })
            .collect();
        let message = format!("{:#}", read(&result).unwrap_err());
        assert!(message.contains("exceeds"), "{message}");
    }

    fn file(n: u8) -> Content {
        Content::File(format!("{n:064x}"))
    }

    #[test]
    fn views_hold_the_candidate_over_accepted_state_alone() {
        let new = "src/b [id] (new)+@ü.rs";
        let owned = |entries: &[(&str, Content)]| -> Vec<(String, Content)> {
            entries
                .iter()
                .map(|(p, c)| (p.to_string(), c.clone()))
                .collect()
        };
        // The working tree, holding A's candidate and B's, neither accepted.
        let tree = Snapshot {
            entries: owned(&[
                ("README.md", file(1)),
                ("src/a.rs", file(2)),
                ("src/b.rs", file(9)),
                (new, file(9)),
                ("src/dep.rs", file(5)),
                ("src/link", Content::Symlink(format!("{:064x}", 6))),
            ]),
            head: "refs/heads/main unborn".into(),
        };
        let basis = ViewBasis {
            accepted: [
                ("src/a.rs", Some(3)),
                ("src/b.rs", Some(4)),
                ("src/dep.rs", Some(5)),
                // Deleted in the working tree by B's candidate.
                ("src/kept.rs", Some(7)),
                ("src/gone.rs", None),
            ]
            .into_iter()
            .map(|(p, n)| (p.to_string(), n.map(|n: u8| format!("{n:064x}"))))
            .collect(),
            provisional: [(new.to_string(), Content::Absent)].into(),
            attempted: BTreeSet::new(),
        };
        let candidate = owned(&[("src/a.rs", file(2)), ("src/a [x].rs", Content::Absent)]);
        let (view, recovered) = view(&tree, &basis, &candidate);
        assert_eq!(
            view.entries,
            owned(&[
                ("README.md", file(1)),
                ("src/a [x].rs", Content::Absent),
                ("src/a.rs", file(2)),
                (new, Content::Absent),
                ("src/b.rs", file(4)),
                ("src/dep.rs", file(5)),
                ("src/gone.rs", Content::Absent),
                ("src/kept.rs", file(7)),
                ("src/link", Content::Symlink(format!("{:064x}", 6))),
            ])
        );
        assert_eq!(view.head, tree.head);
        // Only what no candidate ever left comes from the working tree.
        let copied: Vec<&str> = view
            .entries
            .iter()
            .map(|(p, _)| p.as_str())
            .filter(|p| !recovered.contains(*p))
            .collect();
        assert_eq!(copied, ["README.md", "src/link"]);
    }

    #[test]
    fn only_repository_source_changes_count_as_mutation() {
        let staged = vec![
            ("src/a.rs".to_string(), file(1)),
            ("src/gone.rs".to_string(), Content::Absent),
            ("src/lib.rs".to_string(), file(2)),
        ];
        // Observations never include what the project's Git ignores, such
        // as build output, so untouched source is no mutation at all.
        assert!(mutated(&staged, &[&staged, &staged]).is_empty());
        let mut modified = staged.clone();
        modified[0].1 = file(3);
        let mut created = staged.clone();
        created.push(("src/new.rs".to_string(), file(4)));
        let mut recreated = staged.clone();
        recreated[1].1 = file(5);
        let mut deleted = staged.clone();
        deleted[2].1 = Content::Absent;
        assert_eq!(mutated(&staged, &[&modified, &staged]), ["src/a.rs"]);
        // A change undone before the second observation still counts.
        assert_eq!(mutated(&staged, &[&staged, &created]), ["src/new.rs"]);
        assert_eq!(
            mutated(&staged, &[&recreated, &deleted]),
            ["src/gone.rs", "src/lib.rs"]
        );
    }
}
