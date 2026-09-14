//! Stage 9C: FACTS -> DETERMINISTIC POLICY -> DECISION.
//!
//! Boundaries are declared once, at experiment creation (`ExperimentRun::decision_boundaries`,
//! frozen and hash-bound), and are evaluated here purely against Stage 9B facts already
//! committed to `experiment_events`. No model/provider call is ever made or required to
//! evaluate a boundary; this module contains no process-control path (it never kills,
//! restarts, or launches anything) and no planner invocation (that begins in
//! `experiment_wakeups`, strictly downstream of a persisted decision).
//!
//! Evaluation is durably incremental: a per-(experiment,attempt) cursor
//! (`experiment_decision_cursors`) tracks how far the append-only `experiment_events`
//! stream has already been scanned, so a live poll or a reconciliation pass after
//! reopen never rescans history that was already handled - only `arrival_sequence >
//! cursor`, fetched in small bounded batches.
use super::*;
use experiment::ExperimentRun;
use std::collections::{BTreeMap, BTreeSet};

/// Bounded batch size for one evaluation transaction: caps memory and lock hold time
/// regardless of how much history an experiment has accumulated. Consistent in spirit
/// with Stage 9B's own `READ_BYTES_PER_POLL`/512-draft ingestion batching.
pub(super) const EVALUATION_BATCH_SIZE: i64 = 512;

/// Only METRIC_THRESHOLD is auto-evaluated: a deliberately small, fixed vocabulary of
/// scalar comparisons over persisted Stage 9B metric facts. The other `ExperimentBoundary`
/// variants remain valid Stage 0 protocol values but are not (yet) decided automatically
/// here; declaring one for Stage 9C evaluation is rejected up front rather than silently
/// never firing.
fn scalar_condition(
    boundary: &BoundaryDefinition,
) -> Result<(&str, &BTreeMap<String, String>, MetricComparison, f64)> {
    match &boundary.condition {
        ExperimentBoundary::MetricThreshold {
            metric,
            tags,
            comparison,
            value,
        } => Ok((metric.as_str(), tags, *comparison, *value)),
        other => Err(Error::Invalid(format!(
            "boundary {}: Stage 9C only auto-evaluates METRIC_THRESHOLD conditions against persisted Stage 9B metric facts; {other:?} is not decided automatically",
            boundary.boundary_id
        ))),
    }
}

/// Boundary tags are a required-match subset selector against the triggering event's
/// own tags, with one fail-closed exception: an EMPTY boundary selector matches ONLY
/// an untagged event. This is deliberate, not an oversight - a legacy boundary with no
/// tags must not silently start matching whichever newly-tagged series happens to
/// share its metric name. If a project starts emitting `loss{phase=train}` and
/// `loss{phase=val}` where it used to emit a single untagged `loss`, an old bare
/// `loss` boundary stays unsatisfied (fail-closed) until it is explicitly given a tag
/// selector, rather than silently firing off whichever series happens to arrive first.
fn tags_match(
    boundary_tags: &BTreeMap<String, String>,
    event_tags: &BTreeMap<String, String>,
) -> bool {
    if boundary_tags.is_empty() {
        event_tags.is_empty()
    } else {
        boundary_tags
            .iter()
            .all(|(k, v)| event_tags.get(k) == Some(v))
    }
}

fn satisfies(comparison: MetricComparison, observed: f64, threshold: f64) -> bool {
    match comparison {
        MetricComparison::GreaterThan => observed > threshold,
        MetricComparison::GreaterThanOrEqual => observed >= threshold,
        MetricComparison::LessThan => observed < threshold,
        MetricComparison::LessThanOrEqual => observed <= threshold,
        MetricComparison::Equal => observed == threshold,
    }
}

/// Called once at experiment creation. Boundaries are immutable for the lifetime of
/// the experiment after this; nothing later may add, remove, or reinterpret one.
pub(super) fn validate_boundaries(
    policy: &ProjectConfig,
    boundaries: &[BoundaryDefinition],
    max_planner_wakeups: u32,
) -> Result<()> {
    require(
        boundaries.len() <= experiment::MAX_DECISION_BOUNDARIES,
        format!(
            "experiment declares more than {} decision boundaries",
            experiment::MAX_DECISION_BOUNDARIES
        ),
    )?;
    let mut ids = BTreeSet::new();
    let mut needs_wakeup_budget = false;
    for boundary in boundaries {
        boundary.validate()?;
        require(
            ids.insert(boundary.boundary_id.clone()),
            format!("duplicate boundary_id {}", boundary.boundary_id),
        )?;
        scalar_condition(boundary)?;
        if let BoundaryAction::RequirePlannerReview { verification_ref } = &boundary.action {
            require(
                policy.verification.contains_key(verification_ref),
                format!(
                    "boundary {}: verification_ref {verification_ref} is not defined in project policy",
                    boundary.boundary_id
                ),
            )?;
            needs_wakeup_budget = true;
        }
    }
    require(
        !needs_wakeup_budget
            || (1..=experiment::MAX_PLANNER_WAKEUPS).contains(&max_planner_wakeups),
        format!(
            "experiment declares a planner-review boundary; max_planner_wakeups must be 1..={}",
            experiment::MAX_PLANNER_WAKEUPS
        ),
    )?;
    Ok(())
}

/// A durable control-plane fact: the frozen boundary that fired, the exact persisted
/// metric event that satisfied it (identity, series tags, and observed value), bound to
/// `boundaries_hash` so a later inspection can prove which declaration produced it even
/// though boundaries can never be redeclared for this experiment. `event_tags` is
/// captured verbatim at firing time rather than re-derived later, so the decision stays
/// independently interpretable even if it were possible to relabel history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentDecision {
    pub decision_id: String,
    pub experiment_id: ExperimentId,
    pub attempt: u32,
    pub boundary: BoundaryDefinition,
    pub boundaries_hash: String,
    pub triggering_event_sequence: i64,
    pub metric_name: String,
    pub event_tags: BTreeMap<String, String>,
    pub observed_value: f64,
    pub decided_at_ms: u64,
}
impl ExperimentDecision {
    pub fn requires_planner(&self) -> Option<&str> {
        match &self.boundary.action {
            BoundaryAction::RequirePlannerReview { verification_ref } => {
                Some(verification_ref.as_str())
            }
            BoundaryAction::RecordOnly => None,
        }
    }
}

/// Boundaries already discharged for this experiment (any attempt). Exactly-once
/// firing is enforced twice over: this in-memory pre-filter avoids redundant work
/// within one evaluation pass, and the `UNIQUE(repo_id,experiment_id,attempt,
/// boundary_id)` constraint makes a duplicate insert impossible even under replay,
/// repeated polling, reopen, or a second controller racing the same reconciliation.
fn already_fired(
    store: &Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
) -> Result<BTreeSet<(u32, String)>> {
    store
        .connection
        .prepare(
            "SELECT attempt,boundary_id FROM experiment_decisions WHERE repo_id=?1 AND experiment_id=?2",
        )?
        .query_map(params![info.repository_id.as_str(), id.as_str()], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<_, _>>()
        .map_err(Error::from)
}

/// The durable evaluation cursor for one (experiment, attempt), bound to the frozen
/// `boundaries_hash` it was computed under. Since boundaries are immutable for an
/// experiment's lifetime this should never actually mismatch; if it ever did, reusing
/// the cursor would silently apply new decision semantics to old, already-scanned
/// history, so a mismatch is treated as a hard internal error rather than reset.
fn load_cursor(
    store: &Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    attempt: u32,
) -> Result<i64> {
    let row: Option<(i64, String)> = store
        .connection
        .query_row(
            "SELECT last_evaluated_arrival_sequence,boundaries_hash FROM experiment_decision_cursors WHERE repo_id=?1 AND experiment_id=?2 AND attempt=?3",
            params![
                info.repository_id.as_str(),
                run.experiment_id.as_str(),
                i64::from(attempt)
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(0),
        Some((sequence, hash)) => {
            require(
                hash == run.boundaries_hash,
                "experiment decision cursor boundaries_hash no longer matches the frozen declaration",
            )?;
            Ok(sequence)
        }
    }
}

struct EventRow {
    arrival_sequence: i64,
    is_metric: bool,
    metric_name: String,
    value: f64,
    tags: BTreeMap<String, String>,
}

/// Up to `EVALUATION_BATCH_SIZE` events strictly after `after`, in ascending arrival
/// order - a bounded index-seek on the existing `experiment_events_by_experiment`
/// index (`repo_id,workspace_id,experiment_id,attempt,arrival_sequence`), never a
/// rescan of already-evaluated history. Non-metric rows still advance the cursor (the
/// cursor tracks position in the whole per-attempt event stream) but never match a
/// boundary.
fn next_batch(
    store: &Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
    attempt: u32,
    after: i64,
) -> Result<Vec<EventRow>> {
    let rows: Vec<(i64, String, String)> = store
        .connection
        .prepare(
            "SELECT arrival_sequence,event_type,event_json FROM experiment_events \
             WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND attempt=?4 AND arrival_sequence>?5 \
             ORDER BY arrival_sequence ASC LIMIT ?6",
        )?
        .query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str(),
                i64::from(attempt),
                after,
                EVALUATION_BATCH_SIZE
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<std::result::Result<_, _>>()?;
    rows.into_iter()
        .map(|(arrival_sequence, event_type, json)| {
            if event_type != "METRIC" {
                return Ok(EventRow {
                    arrival_sequence,
                    is_metric: false,
                    metric_name: String::new(),
                    value: 0.0,
                    tags: BTreeMap::new(),
                });
            }
            let event: ExperimentRuntimeEvent = serde_json::from_str(&json)?;
            Ok(match event.event {
                ExperimentEventData::Metric {
                    name, value, tags, ..
                } => EventRow {
                    arrival_sequence,
                    is_metric: true,
                    metric_name: name,
                    value,
                    tags,
                },
                _ => EventRow {
                    arrival_sequence,
                    is_metric: false,
                    metric_name: String::new(),
                    value: 0.0,
                    tags: BTreeMap::new(),
                },
            })
        })
        .collect()
}

struct Fired {
    boundary: BoundaryDefinition,
    triggering_event_sequence: i64,
    metric_name: String,
    event_tags: BTreeMap<String, String>,
    observed_value: f64,
}

/// Persists every decision newly fired within one batch AND advances the cursor past
/// that batch, in one transaction. This is the crash-safety core of Stage 9C: a crash
/// before commit leaves the cursor exactly where it was (the batch is simply
/// re-evaluated, which is idempotent because of the decision uniqueness constraint), and
/// a crash after commit can never have "advanced the cursor but lost a decision" - the
/// two facts are atomic together, not ordered around each other.
fn persist_batch(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    attempt: u32,
    new_cursor: i64,
    newly_fired: &[Fired],
) -> Result<()> {
    let decided_at_ms = now_ms()?;
    let decided_at_ms_i64 =
        i64::try_from(decided_at_ms).map_err(|e| Error::Invalid(e.to_string()))?;
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    for item in newly_fired {
        let hex: String = tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
        let decision = ExperimentDecision {
            decision_id: format!("decision:{hex}"),
            experiment_id: run.experiment_id.clone(),
            attempt,
            boundary: item.boundary.clone(),
            boundaries_hash: run.boundaries_hash.clone(),
            triggering_event_sequence: item.triggering_event_sequence,
            metric_name: item.metric_name.clone(),
            event_tags: item.event_tags.clone(),
            observed_value: item.observed_value,
            decided_at_ms,
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO experiment_decisions(decision_id,repo_id,workspace_id,experiment_id,attempt,boundary_id,decided_at_ms,requires_planner,record_json) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                decision.decision_id,
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                run.experiment_id.as_str(),
                i64::from(attempt),
                item.boundary.boundary_id,
                decided_at_ms_i64,
                decision.requires_planner().is_some(),
                serde_json::to_string(&decision)?,
            ],
        )? == 1;
        if changed {
            store::append(
                &tx,
                &info.repository_id,
                decided_at_ms,
                &Links::workspace(info.workspace_id.clone()),
                None,
                &JournalEntry::Experiment {
                    experiment_id: run.experiment_id.clone(),
                    phase: "EXPERIMENT_DECISION_FIRED".into(),
                    detail: format!(
                        "boundary {} attempt {attempt}: {} observed {} tags={:?}",
                        item.boundary.boundary_id,
                        item.metric_name,
                        item.observed_value,
                        item.event_tags
                    ),
                },
            )?;
        }
    }
    tx.execute(
        "INSERT INTO experiment_decision_cursors(repo_id,workspace_id,experiment_id,attempt,boundaries_hash,last_evaluated_arrival_sequence,updated_at_ms) \
         VALUES (?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(repo_id,experiment_id,attempt) DO UPDATE SET last_evaluated_arrival_sequence=excluded.last_evaluated_arrival_sequence, updated_at_ms=excluded.updated_at_ms",
        params![
            info.repository_id.as_str(),
            info.workspace_id.as_str(),
            run.experiment_id.as_str(),
            i64::from(attempt),
            run.boundaries_hash,
            new_cursor,
            decided_at_ms_i64,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Drains the cursor for one attempt in bounded batches until it catches up with
/// whatever is currently persisted. `fired` is this attempt's already-discharged
/// boundary set, mutated in place as new decisions fire within this pass.
fn evaluate_attempt(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
    attempt: u32,
    fired: &mut BTreeSet<String>,
) -> Result<()> {
    loop {
        let cursor = load_cursor(store, info, run, attempt)?;
        let batch = next_batch(store, info, &run.experiment_id, attempt, cursor)?;
        if batch.is_empty() {
            break;
        }
        let new_cursor = batch.last().expect("checked nonempty").arrival_sequence;
        let mut newly_fired = vec![];
        for row in &batch {
            if !row.is_metric || !row.value.is_finite() {
                continue;
            }
            for boundary in &run.decision_boundaries {
                if fired.contains(&boundary.boundary_id) {
                    continue;
                }
                let Ok((metric, boundary_tags, comparison, threshold)) = scalar_condition(boundary)
                else {
                    continue;
                };
                if metric != row.metric_name || !tags_match(boundary_tags, &row.tags) {
                    continue;
                }
                if satisfies(comparison, row.value, threshold) {
                    newly_fired.push(Fired {
                        boundary: boundary.clone(),
                        triggering_event_sequence: row.arrival_sequence,
                        metric_name: row.metric_name.clone(),
                        event_tags: row.tags.clone(),
                        observed_value: row.value,
                    });
                    fired.insert(boundary.boundary_id.clone());
                }
            }
        }
        let batch_len = batch.len();
        persist_batch(store, info, run, attempt, new_cursor, &newly_fired)?;
        if (batch_len as i64) < EVALUATION_BATCH_SIZE {
            break;
        }
    }
    Ok(())
}

/// Stage 9C entry point: evaluate every declared boundary against every attempt's
/// already-persisted event facts, in a fixed, documented order:
///
/// 1. attempts ascending;
/// 2. within an attempt, events in arrival (commit) order, resumed from a durable
///    per-attempt cursor rather than rescanned from the start;
/// 3. within one event, boundaries in declaration order.
///
/// A boundary fires on the first qualifying event (matching both metric name and tag
/// selector) and never again for that attempt. Idempotent and safe to call repeatedly
/// (live polling, reopen, restart, or a second controller racing the same
/// reconciliation): already-fired boundaries are always skipped, and already-scanned
/// history is never requeried.
pub(super) fn evaluate(
    store: &mut Store,
    info: &RepositoryInfo,
    run: &ExperimentRun,
) -> Result<()> {
    if run.decision_boundaries.is_empty() {
        return Ok(());
    }
    let fired_by_attempt = already_fired(store, info, &run.experiment_id)?;
    for attempt in 1..=run.attempts.len() as u32 {
        let mut fired: BTreeSet<String> = fired_by_attempt
            .iter()
            .filter(|(a, _)| *a == attempt)
            .map(|(_, boundary_id)| boundary_id.clone())
            .collect();
        evaluate_attempt(store, info, run, attempt, &mut fired)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExperimentBoundarySummary {
    pub decision_boundaries: Vec<BoundaryDefinition>,
    pub boundaries_hash: String,
    pub max_planner_wakeups: u32,
}

impl Store {
    /// Read-only: the declarations bound to this experiment at creation.
    pub fn experiment_boundaries(
        &self,
        root: &Path,
        id: &ExperimentId,
    ) -> Result<ExperimentBoundarySummary> {
        let run = self
            .experiment_status(root, id)?
            .ok_or_else(|| Error::Invalid("experiment not found".into()))?
            .run;
        Ok(ExperimentBoundarySummary {
            decision_boundaries: run.decision_boundaries,
            boundaries_hash: run.boundaries_hash,
            max_planner_wakeups: run.max_planner_wakeups,
        })
    }
    /// Read-only: fired decisions, oldest first. Never mutates state; a decision row
    /// exists only because Stage 9C's write path (`evaluate`, gated by the same
    /// controller authorization as every other experiment table) already inserted it.
    pub fn experiment_decisions(
        &self,
        root: &Path,
        id: &ExperimentId,
    ) -> Result<Vec<ExperimentDecision>> {
        let info = graph::checked_workspace(self, root)?;
        self.connection
            .prepare(
                "SELECT record_json FROM experiment_decisions WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 ORDER BY arrival_sequence ASC",
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
}
