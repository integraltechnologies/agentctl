//! Stage 9D: DECISION -> CONTROLLED WAKEUP -> PLANNER REQUEST.
//!
//! A wakeup may only originate from a persisted Stage 9C decision whose frozen boundary
//! action is REQUIRE_PLANNER_REVIEW; nothing here reads raw experiment events, WARNING/ERROR
//! health facts, or process output directly. Creating a wakeup means minting exactly the
//! smallest existing planning input the normal planner already consumes (a Stage 4
//! `PlanningRequest`, via `Store::prepare_plan`) and recording the 1:1 binding durably; it
//! never itself launches a provider or spends a model token. Canonical planning authority
//! (routing, provider invocation, PlannerPacket/ExecutionPlan validation, Stage 4+
//! activation/verification) remains entirely in the existing `planning`/`runtime::engine`
//! subsystems - this module only ever calls `prepare_plan`, never `Runtime::plan`.
use super::experiment_decisions::ExperimentDecision;
use super::*;
use experiment::ExperimentRun;

/// A durable 1:1 binding from a fired decision to the planning request that woke the
/// normal planner for it. `context` is the exact bounded `RequestDraft` sent, kept here
/// only for observability so an operator never has to guess what the planner was asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentWakeup {
    pub wakeup_id: String,
    pub experiment_id: ExperimentId,
    pub decision_id: String,
    pub planning_request_id: planning::PlanningRequestId,
    pub context: planning::RequestDraft,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannerInvocationStatus {
    /// The wakeup exists but no planner job has been attempted against it yet.
    NotAttempted,
    /// A planner job for this request is queued or running.
    InProgress,
    /// A planner job for this request produced a validated, imported ExecutionPlan.
    /// Normal Stage 4+ activation/execution/verification authority is unaffected and
    /// still applies in full to that plan.
    Succeeded,
    /// Every planner job attempted for this request has failed or was cancelled, and
    /// none has since succeeded; nothing here retries automatically.
    FailedUnresolved,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExperimentWakeupObservation {
    pub wakeup: ExperimentWakeup,
    /// Derived, not duplicated, from the existing Stage 7 `runtime_jobs` table: every
    /// planner-role job ever issued against this wakeup's planning request.
    pub planner_jobs: Vec<PlannerJobSummary>,
    pub status: PlannerInvocationStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannerJobSummary {
    pub job_id: JobId,
    pub state: RuntimeJobState,
    pub created_at_ms: u64,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentControlSummary {
    pub decision_count: u64,
    pub wakeups_created: u32,
    pub wakeups_budget: u32,
    /// True when continued autonomous escalation is not permitted and a human/operator
    /// needs to look: the wakeup budget is exhausted while a decision still needs one,
    /// or a wakeup's planner invocation failed and nothing has retried it since.
    pub attention_required: bool,
}

/// Bounded, structured, and built only from already-persisted Stage 9B/9C facts - never
/// raw stdout/stderr or an unbounded event dump. `prepare_plan`'s own 16 KiB intent cap
/// and this experiment's own event-summary limits keep it small regardless of how long
/// the experiment has been running.
fn wakeup_context(
    store: &Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    decision: &ExperimentDecision,
    verification_ref: &str,
) -> Result<planning::RequestDraft> {
    let (metric, boundary_tags, comparison, threshold) = match &decision.boundary.condition {
        ExperimentBoundary::MetricThreshold {
            metric,
            tags,
            comparison,
            value,
        } => (metric.clone(), tags.clone(), *comparison, *value),
        other => {
            return Err(Error::Invalid(format!(
                "decision boundary condition {other:?} is not a scalar metric threshold"
            )));
        }
    };
    let comparison = match comparison {
        MetricComparison::LessThan => "<",
        MetricComparison::LessThanOrEqual => "<=",
        MetricComparison::GreaterThan => ">",
        MetricComparison::GreaterThanOrEqual => ">=",
        MetricComparison::Equal => "==",
    };
    let objective = format!(
        "Experiment {} attempt {} tripped decision boundary {} (Stage 9C, deterministic): \
         metric {metric} observed {} {comparison} {threshold}. Review the experiment's current \
         state and decide the next controlled action (continue, adjust, restart, or halt); this \
         request does not itself execute anything.",
        run.experiment_id.as_str(),
        decision.attempt,
        decision.boundary.boundary_id,
        decision.observed_value,
    );
    let mut constraints = vec![
        format!("experiment_id={}", run.experiment_id.as_str()),
        format!("attempt={}", decision.attempt),
        format!("experiment_state={:?}", run.state),
        format!(
            "command={} {}",
            run.command.program,
            run.command.args.join(" ")
        ),
        format!("decision_id={}", decision.decision_id),
        format!(
            "boundary_id={} boundaries_hash={}",
            decision.boundary.boundary_id, decision.boundaries_hash
        ),
        format!(
            "triggering_event_sequence={} metric={metric} boundary_tags={boundary_tags:?} event_tags={:?} observed={} comparison={comparison} threshold={threshold}",
            decision.triggering_event_sequence, decision.event_tags, decision.observed_value
        ),
    ];
    let summary = store.experiment_event_summary(info, &run.experiment_id)?;
    for (name, value) in summary.latest_metrics.iter().take(8) {
        constraints.push(format!("latest_metric {name}={value}"));
    }
    if let Some(checkpoint) = &summary.latest_checkpoint {
        constraints.push(format!("latest_checkpoint_path={checkpoint}"));
    }
    constraints.push(format!(
        "prior_wakeups_used={}/{} for this experiment",
        wakeups_used(store, info, &run.experiment_id)?,
        run.max_planner_wakeups
    ));
    Ok(planning::RequestDraft {
        objective,
        query: Some(metric),
        scope: vec![],
        constraints,
        definition_of_done: vec![
            "Decide whether the experiment continues unchanged, needs a restart/adjustment, \
             or requires an operator halt; produce a plan only if concrete follow-up work is \
             warranted."
                .into(),
        ],
        verification: Some(VerificationRequirements {
            requirement_refs: vec![verification_ref.to_string()],
            evidence_required: false,
        }),
        invariant_refs: vec![],
        provenance: planning::PlanningProvenance {
            actor: "experiment-controller".into(),
            source_refs: vec![
                format!("experiment:{}", run.experiment_id.as_str()),
                format!("decision:{}", decision.decision_id),
            ],
            provider: None,
        },
    })
}

fn wakeups_used(store: &Store, info: &RepositoryInfo, id: &ExperimentId) -> Result<u32> {
    let n: i64 = store.connection.query_row(
        "SELECT count(*) FROM experiment_wakeups WHERE repo_id=?1 AND experiment_id=?2",
        params![info.repository_id.as_str(), id.as_str()],
        |r| r.get(0),
    )?;
    u32::try_from(n).map_err(|e| Error::Invalid(e.to_string()))
}

/// Decisions requiring a planner that have no wakeup yet, oldest first: the only
/// order Stage 9D ever spends wakeup budget in.
fn pending_planner_decisions(
    store: &Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
) -> Result<Vec<ExperimentDecision>> {
    store
        .connection
        .prepare(
            "SELECT record_json FROM experiment_decisions d \
             WHERE d.repo_id=?1 AND d.workspace_id=?2 AND d.experiment_id=?3 AND d.requires_planner=1 \
             AND NOT EXISTS(SELECT 1 FROM experiment_wakeups w WHERE w.decision_id=d.decision_id) \
             ORDER BY d.arrival_sequence ASC",
        )?
        .query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str()
            ],
            |r| r.get::<_, String>(0),
        )?
        .map(|json| Ok(serde_json::from_str(&json?)?))
        .collect()
}

/// Idempotent: creates at most one wakeup per decision (`UNIQUE(decision_id)`), and
/// never more than `run.max_planner_wakeups` wakeups for the experiment even when two
/// reconcilers race for the same last slot (see the atomic INSERT below). Calling
/// `prepare_plan` before that INSERT can commit (a crash, or simply losing the race)
/// leaves one harmless orphaned `PlanningRequest` behind - never a duplicate logical
/// wakeup and never a budget overrun, since the second attempt's insert is either
/// ignored (same decision already has a wakeup) or rejected by the budget check (cap
/// already reached), and the `PlanningRequest` it minted is simply never referenced.
fn create_wakeup(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    decision: &ExperimentDecision,
) -> Result<bool> {
    let Some(verification_ref) = decision.requires_planner() else {
        return Ok(false);
    };
    let context = wakeup_context(store, info, run, decision, verification_ref)?;
    let prepared = store.prepare_plan(
        &info.root,
        context.clone(),
        planning::PlanningLimits::default(),
    )?;
    let hex: String = store
        .connection
        .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    let wakeup = ExperimentWakeup {
        wakeup_id: format!("wakeup:{hex}"),
        experiment_id: run.experiment_id.clone(),
        decision_id: decision.decision_id.clone(),
        planning_request_id: prepared.request.request_id.clone(),
        context,
        created_at_ms: now_ms()?,
    };
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // `BEGIN IMMEDIATE` above acquires SQLite's write lock before this statement runs,
    // serializing against every other writer transaction on this database (this is
    // the only place anything is ever inserted into `experiment_wakeups`). The
    // `count(*)` below is therefore a live subquery evaluated as part of this single
    // atomic statement - never a value read earlier in Rust - so the budget check and
    // the row insertion cannot be torn apart by a concurrent reconciler: this is what
    // actually closes the TOCTOU race (two controllers both observing a stale count
    // and both inserting a wakeup for a different decision).
    let changed = tx.execute(
        "INSERT OR IGNORE INTO experiment_wakeups(wakeup_id,repo_id,workspace_id,experiment_id,decision_id,planning_request_id,created_at_ms,record_json) \
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8 \
         WHERE (SELECT count(*) FROM experiment_wakeups WHERE repo_id=?2 AND experiment_id=?4) < ?9",
        params![
            wakeup.wakeup_id,
            info.repository_id.as_str(),
            info.workspace_id.as_str(),
            run.experiment_id.as_str(),
            wakeup.decision_id,
            wakeup.planning_request_id.as_str(),
            i64::try_from(wakeup.created_at_ms).map_err(|e| Error::Invalid(e.to_string()))?,
            serde_json::to_string(&wakeup)?,
            i64::from(run.max_planner_wakeups),
        ],
    )? == 1;
    if changed {
        store::append(
            &tx,
            &info.repository_id,
            wakeup.created_at_ms,
            &Links::workspace(info.workspace_id.clone()),
            None,
            &JournalEntry::Experiment {
                experiment_id: run.experiment_id.clone(),
                phase: "EXPERIMENT_PLANNER_WAKEUP_CREATED".into(),
                detail: format!(
                    "decision {} -> {}",
                    decision.decision_id,
                    wakeup.planning_request_id.as_str()
                ),
            },
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Stage 9D entry point: turn any not-yet-woken REQUIRE_PLANNER_REVIEW decision into a
/// wakeup, strictly within the experiment's immutable budget. No model/planner output
/// and no experiment event can reach this function or influence the budget; it only ever
/// consumes durable `ExperimentDecision` rows already committed by Stage 9C. Idempotent
/// and safe to call repeatedly (live polling, reopen, explicit reconciliation), and safe
/// to call concurrently from an independent controller/connection: the cap itself is
/// enforced inside `create_wakeup`'s own atomic transaction, not here.
pub(super) fn reconcile(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
) -> Result<()> {
    if run.max_planner_wakeups == 0 {
        return Ok(());
    }
    for decision in pending_planner_decisions(store, info, &run.experiment_id)? {
        // Advisory only, re-read fresh on every iteration: it exists purely to skip
        // the expensive `prepare_plan()` call once the budget is visibly exhausted.
        // It is never the enforcement point and staleness here is harmless - even if
        // a concurrent reconciler consumes the last slot between this read and the
        // `create_wakeup` call below, that call's own atomic budget check still
        // rejects it correctly.
        if wakeups_used(store, info, &run.experiment_id)? >= run.max_planner_wakeups {
            break;
        }
        create_wakeup(store, info, run, &decision)?;
    }
    Ok(())
}

fn planner_status(
    store: &Store,
    request_id: &planning::PlanningRequestId,
    info: &RepositoryInfo,
) -> Result<(Vec<PlannerJobSummary>, PlannerInvocationStatus)> {
    let rows: Vec<String> = store
        .connection
        .prepare(
            "SELECT record_json FROM runtime_jobs WHERE repo_id=?1 AND workspace_id=?2 AND request_id=?3",
        )?
        .query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                request_id.as_str()
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<std::result::Result<_, _>>()?;
    let mut jobs: Vec<RuntimeJob> = rows
        .into_iter()
        .map(|s| Ok(serde_json::from_str(&s)?))
        .collect::<Result<_>>()?;
    jobs.sort_by_key(|j| j.created_at_ms);
    let status = if jobs.iter().any(|j| j.state == RuntimeJobState::Succeeded) {
        PlannerInvocationStatus::Succeeded
    } else if jobs
        .iter()
        .any(|j| matches!(j.state, RuntimeJobState::Queued | RuntimeJobState::Running))
    {
        PlannerInvocationStatus::InProgress
    } else if jobs.is_empty() {
        PlannerInvocationStatus::NotAttempted
    } else {
        PlannerInvocationStatus::FailedUnresolved
    };
    let summaries = jobs
        .iter()
        .map(|j| PlannerJobSummary {
            job_id: j.job_id.clone(),
            state: j.state.clone(),
            created_at_ms: j.created_at_ms,
            failure: j.failure.clone(),
        })
        .collect();
    Ok((summaries, status))
}

/// Info-scoped core of `Store::experiment_wakeups`, reused by `summary` below so
/// neither ever has to re-derive `RepositoryInfo` or (more importantly) go back
/// through `Store::experiment_status` and risk a summary <-> observation cycle.
fn wakeups_for(
    store: &Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
) -> Result<Vec<ExperimentWakeupObservation>> {
    let rows: Vec<String> = store
        .connection
        .prepare(
            "SELECT record_json FROM experiment_wakeups WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 ORDER BY arrival_sequence ASC",
        )?
        .query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str()
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<std::result::Result<_, _>>()?;
    rows.into_iter()
        .map(|json| {
            let wakeup: ExperimentWakeup = serde_json::from_str(&json)?;
            let (planner_jobs, status) = planner_status(store, &wakeup.planning_request_id, info)?;
            Ok(ExperimentWakeupObservation {
                wakeup,
                planner_jobs,
                status,
            })
        })
        .collect()
}

/// Stage 9C decision counts plus Stage 9D wakeup/budget state, including the durable
/// ATTENTION_REQUIRED fact: whether continued autonomous escalation is currently
/// permitted. Called both by `Store::experiment_control_summary` and, with an
/// already-loaded `run`, by `experiment::observation` for `experiment status`/`list`.
pub(super) fn summary(
    store: &Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
) -> Result<ExperimentControlSummary> {
    let decision_count: i64 = store.connection.query_row(
        "SELECT count(*) FROM experiment_decisions WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3",
        params![
            info.repository_id.as_str(),
            info.workspace_id.as_str(),
            run.experiment_id.as_str()
        ],
        |r| r.get(0),
    )?;
    let wakeups = wakeups_for(store, info, &run.experiment_id)?;
    let used = u32::try_from(wakeups.len()).map_err(|e| Error::Invalid(e.to_string()))?;
    let pending = pending_planner_decisions(store, info, &run.experiment_id)?.len();
    // Budget exhaustion is immediate; a still-pending wakeup after the experiment has
    // already gone terminal means wakeup creation itself kept failing (e.g. the code
    // index was never fresh) rather than just "hasn't been polled yet" - that also
    // means an action could not be safely translated into planning semantics, and
    // must surface rather than sit silently in a per-attempt error string.
    let stuck = pending > 0 && (used >= run.max_planner_wakeups || run.state.is_terminal());
    let unresolved_failure = wakeups
        .iter()
        .any(|w| w.status == PlannerInvocationStatus::FailedUnresolved);
    Ok(ExperimentControlSummary {
        decision_count: u64::try_from(decision_count).map_err(|e| Error::Invalid(e.to_string()))?,
        wakeups_created: used,
        wakeups_budget: run.max_planner_wakeups,
        attention_required: stuck || unresolved_failure,
    })
}

impl Store {
    /// Read-only: every wakeup for this experiment, oldest first, with its planner
    /// invocation status derived live from the existing Stage 7 `runtime_jobs` state -
    /// never a separately maintained (and potentially stale) copy of it.
    pub fn experiment_wakeups(
        &self,
        root: &Path,
        id: &ExperimentId,
    ) -> Result<Vec<ExperimentWakeupObservation>> {
        let info = graph::checked_workspace(self, root)?;
        wakeups_for(self, &info, id)
    }
    /// Read-only summary combining Stage 9C decision counts with Stage 9D wakeup/budget
    /// state. See `summary` for the ATTENTION_REQUIRED derivation.
    pub fn experiment_control_summary(
        &self,
        root: &Path,
        id: &ExperimentId,
    ) -> Result<ExperimentControlSummary> {
        let info = graph::checked_workspace(self, root)?;
        let run = experiment::load_experiment(self, &info, id)?
            .ok_or_else(|| Error::Invalid("experiment not found".into()))?;
        summary(self, &info, &run)
    }
}
