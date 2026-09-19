//! Ontology generation lifecycle.
//!
//! The live graph tables materialize the latest index pass, which is an
//! *observation*. Accepted truth is a separate durable pointer: the single
//! `ACCEPTED` row of a workspace, bound to an immutable snapshot. An index pass
//! whose facts differ from the accepted generation records a `CANDIDATE` with a
//! semantic delta against it; the candidate becomes accepted only through an
//! explicit decision (`agentctl ontology accept`) or, for runtime work, inside
//! the transaction that completes the owning plan after its integration
//! verification passed.
//!
//! ```text
//! observe ─▶ CANDIDATE ─┬─ accept ──────────────▶ ACCEPTED ── replaced ─▶ RETIRED
//!                       ├─ reject ──────────────▶ REJECTED
//!                       └─ superseded/cancelled ▶ ABANDONED
//! ```
//!
//! Two observations are accepted mechanically, because they cannot introduce
//! new truth: the first complete observation of a workspace that has no
//! accepted generation (`BOOTSTRAP`), and a re-observation whose fingerprint
//! and facts equal the accepted generation (`IDENTICAL_TO_ACCEPTED`, e.g. a
//! revert). The latter is still a new row with a new ordinal and sequence.
use super::delta::{
    self, DeltaSummary, FactsRef, GenerationPoint, SemanticDelta, SnapshotManifest,
};
use super::*;
use crate::{
    local::repository::{RepositoryId, RepositorySourceState, WorkspaceId},
    protocol::{PlanId, ProtocolVersion, TaskId},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GenerationState {
    Candidate,
    Accepted,
    Retired,
    Rejected,
    Abandoned,
}

impl GenerationState {
    fn sql(self) -> &'static str {
        match self {
            Self::Candidate => "CANDIDATE",
            Self::Accepted => "ACCEPTED",
            Self::Retired => "RETIRED",
            Self::Rejected => "REJECTED",
            Self::Abandoned => "ABANDONED",
        }
    }
}

/// What produced an observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GenerationOrigin {
    /// An index pass outside the runtime (`agentctl repo index`).
    External,
    /// The runtime's refresh after a task of `plan_id` passed verification.
    Runtime {
        plan_id: PlanId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
    },
}

impl GenerationOrigin {
    fn plan(&self) -> Option<&PlanId> {
        match self {
            Self::External => None,
            Self::Runtime { plan_id, .. } => Some(plan_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionReason {
    /// First complete observation of a workspace with no accepted generation.
    Bootstrap,
    /// Facts identical to the accepted generation, observed again.
    IdenticalToAccepted,
    /// `agentctl ontology accept`.
    ManualAcceptance,
    /// The owning plan's integration verification passed and the plan completed.
    IntegrationVerified,
    /// A later generation was accepted.
    Replaced,
    /// A later observation replaced this undecided candidate.
    Superseded,
    /// `agentctl ontology reject`.
    ManualRejection,
    /// A packet verifier rejected a task of the owning plan.
    VerificationRejected,
    /// The integration verifier rejected the owning plan.
    IntegrationRejected,
    PlanCancelled,
    PlanSuperseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationDecision {
    pub reason: DecisionReason,
    pub at_ms: u64,
    /// The generation that replaced or superseded this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// BLAKE3 of the verification decision that authorized the transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_hash: Option<String>,
    /// The final source-state reference that verification was bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
}

impl GenerationDecision {
    fn new(reason: DecisionReason) -> Result<Self> {
        Ok(Self {
            reason,
            at_ms: now_ms()?,
            by: None,
            note: None,
            plan_id: None,
            task_id: None,
            verification_hash: None,
            source_hash: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeltaStatus {
    /// No accepted generation existed to compare with.
    NoBase,
    Recorded {
        artifact: FactsRef,
        summary: DeltaSummary,
    },
    /// No delta could be derived (different graph versions, or too large).
    Unavailable { reason: String },
}

/// One recorded observation of a workspace's ontology and its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OntologyGeneration {
    pub version: ProtocolVersion,
    pub generation_id: String,
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    /// Lifecycle order within the workspace; unlike the fingerprint, never repeats.
    pub ordinal: u64,
    pub state: GenerationState,
    pub generation: GraphGeneration,
    pub index_version: String,
    pub snapshot: FactsRef,
    pub files: usize,
    pub entities: usize,
    pub relations: usize,
    pub failed_files: usize,
    /// HEAD/dirty observation of the index pass (sequential, not atomic).
    pub source: RepositorySourceState,
    pub origin: GenerationOrigin,
    pub observed_at_ms: u64,
    /// The accepted generation this observation was compared with.
    pub base: Option<String>,
    pub delta: DeltaStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<GenerationDecision>,
    /// Retirement, rejection or abandonment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<GenerationDecision>,
}

impl OntologyGeneration {
    fn point(&self) -> GenerationPoint {
        GenerationPoint {
            generation_id: self.generation_id.clone(),
            generation: self.generation.clone(),
            snapshot: self.snapshot.clone(),
        }
    }
    pub fn plan(&self) -> Option<&PlanId> {
        self.origin.plan()
    }
}

/// Where a workspace's accepted truth and its live observation stand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OntologyStatus {
    pub workspace_id: WorkspaceId,
    /// The generation the live graph tables currently materialize.
    pub live: Option<GraphGeneration>,
    /// True when the live generation is the accepted one; planning requires it.
    pub live_accepted: bool,
    pub accepted: Option<OntologyGeneration>,
    pub candidate: Option<OntologyGeneration>,
    /// The latest record of the live generation, whatever its state.
    pub observed: Option<OntologyGeneration>,
}

fn generation_id(info: &RepositoryInfo, ordinal: u64) -> String {
    let workspace = blake3::hash(info.workspace_id.as_str().as_bytes()).to_hex();
    format!("gen:{}:{ordinal}", &workspace[..16])
}

fn load(
    c: &Connection,
    info: &RepositoryInfo,
    clause: &str,
    arg: &str,
) -> Result<Option<OntologyGeneration>> {
    let row: Option<(String, String)> = c
        .query_row(
            &format!(
                "SELECT state,record_json FROM ontology_generations WHERE workspace_id=?1 AND {clause} ORDER BY ordinal DESC LIMIT 1"
            ),
            params![info.workspace_id.as_str(), arg],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(state, json)| decode(info, &state, &json))
        .transpose()
}

fn decode(info: &RepositoryInfo, state: &str, json: &str) -> Result<OntologyGeneration> {
    let record: OntologyGeneration = serde_json::from_str(json)?;
    require(
        record.state.sql() == state
            && record.workspace_id == info.workspace_id
            && record.repository_id == info.repository_id,
        "ontology generation record disagrees with its row",
    )?;
    Ok(record)
}

pub(crate) fn accepted(
    c: &Connection,
    info: &RepositoryInfo,
) -> Result<Option<OntologyGeneration>> {
    load(c, info, "state=?2", "ACCEPTED")
}

fn candidate(c: &Connection, info: &RepositoryInfo) -> Result<Option<OntologyGeneration>> {
    load(c, info, "state=?2", "CANDIDATE")
}

fn latest(c: &Connection, info: &RepositoryInfo) -> Result<Option<OntologyGeneration>> {
    load(c, info, "?2=?2", "")
}

fn by_id(c: &Connection, info: &RepositoryInfo, id: &str) -> Result<OntologyGeneration> {
    load(c, info, "generation_id=?2", id)?.ok_or_else(|| {
        Error::Invalid(format!(
            "ontology generation {id} is not recorded for this workspace"
        ))
    })
}

fn journal(
    c: &Connection,
    info: &RepositoryInfo,
    record: &OntologyGeneration,
    reason: Option<DecisionReason>,
) -> Result<()> {
    append(
        c,
        &info.repository_id,
        now_ms()?,
        &Links::planning(info.workspace_id.clone(), record.plan().cloned()),
        None,
        &JournalEntry::OntologyGenerationChanged {
            generation_id: record.generation_id.clone(),
            state: record.state,
            reason,
        },
    )?;
    Ok(())
}

fn transition(
    c: &Connection,
    info: &RepositoryInfo,
    record: &mut OntologyGeneration,
    to: GenerationState,
    decision: GenerationDecision,
) -> Result<()> {
    let reason = decision.reason;
    if to == GenerationState::Accepted {
        record.acceptance = Some(decision);
    } else {
        record.closure = Some(decision);
    }
    record.state = to;
    let updated = c.execute(
        "UPDATE ontology_generations SET state=?1,record_json=?2 WHERE generation_id=?3 AND workspace_id=?4",
        params![
            to.sql(),
            serde_json::to_string(record)?,
            record.generation_id,
            info.workspace_id.as_str()
        ],
    )?;
    require(updated == 1, "ontology generation row is missing")?;
    journal(c, info, record, Some(reason))
}

fn compare(
    c: &Connection,
    base: &OntologyGeneration,
    point: &GenerationPoint,
    manifest: &SnapshotManifest,
) -> Result<DeltaStatus> {
    if base.index_version != manifest.index_version {
        return Ok(DeltaStatus::Unavailable {
            reason: format!(
                "the accepted generation was indexed by {}; identities are not comparable across graph versions",
                base.index_version
            ),
        });
    }
    let old: SnapshotManifest = delta::get(c, &base.snapshot)?;
    let delta = delta::diff(c, (&base.point(), &old), (point, manifest))?;
    if serde_json::to_vec(&delta)?.len() > delta::MAX_ARTIFACT_BYTES {
        return Ok(DeltaStatus::Unavailable {
            reason: "semantic delta exceeds 64 MiB".into(),
        });
    }
    Ok(DeltaStatus::Recorded {
        artifact: delta::put(c, &delta)?,
        summary: delta.summary,
    })
}

/// Records the lifecycle consequence of an index pass. Runs inside the index
/// transaction, after the live generation has been published.
pub(super) fn observe(
    c: &Connection,
    info: &RepositoryInfo,
    origin: &GenerationOrigin,
    live: &GraphGeneration,
    stats: &IndexStats,
    source: &RepositorySourceState,
) -> Result<()> {
    let latest = latest(c, info)?;
    let accepted = accepted(c, info)?;
    if let Some(last) = &latest
        && last.generation == *live
    {
        // An explicit observation puts the live state up for a human decision
        // when nobody can otherwise decide it: a closed record, or a runtime
        // candidate (only its plan's integration could accept that). The
        // runtime itself never reopens anything.
        let undecidable = match last.state {
            GenerationState::Rejected | GenerationState::Abandoned => true,
            GenerationState::Candidate => last.plan().is_some(),
            GenerationState::Accepted | GenerationState::Retired => false,
        };
        let reopen = undecidable
            && *origin == GenerationOrigin::External
            && accepted.as_ref().is_none_or(|a| a.generation != *live);
        if !reopen {
            return Ok(());
        }
    }
    let ordinal = latest.as_ref().map_or(1, |l| l.ordinal + 1);
    let id = generation_id(info, ordinal);
    let (snapshot, manifest) = delta::snapshot(c, info, live)?;
    let point = GenerationPoint {
        generation_id: id.clone(),
        generation: live.clone(),
        snapshot: snapshot.clone(),
    };
    let complete = stats.failed == 0;
    let (state, delta, reason) = match &accepted {
        None => (
            if complete && *origin == GenerationOrigin::External {
                GenerationState::Accepted
            } else {
                GenerationState::Candidate
            },
            DeltaStatus::NoBase,
            DecisionReason::Bootstrap,
        ),
        Some(base) => {
            let delta = compare(c, base, &point, &manifest)?;
            let identical = complete
                && base.generation.fingerprint == live.fingerprint
                && matches!(&delta, DeltaStatus::Recorded { summary, .. } if *summary == DeltaSummary::default());
            (
                if identical {
                    GenerationState::Accepted
                } else {
                    GenerationState::Candidate
                },
                delta,
                DecisionReason::IdenticalToAccepted,
            )
        }
    };
    let mut record = OntologyGeneration {
        version: ProtocolVersion::V1,
        generation_id: id.clone(),
        repository_id: info.repository_id.clone(),
        workspace_id: info.workspace_id.clone(),
        ordinal,
        state: GenerationState::Candidate,
        generation: live.clone(),
        index_version: manifest.index_version.clone(),
        snapshot,
        files: manifest.files.len(),
        entities: manifest.entities,
        relations: manifest.relations,
        failed_files: stats.failed,
        source: source.clone(),
        origin: origin.clone(),
        observed_at_ms: now_ms()?,
        base: accepted.as_ref().map(|a| a.generation_id.clone()),
        delta,
        acceptance: None,
        closure: None,
    };
    if let Some(mut open) = candidate(c, info)? {
        let mut decision = GenerationDecision::new(DecisionReason::Superseded)?;
        decision.by = Some(id.clone());
        transition(c, info, &mut open, GenerationState::Abandoned, decision)?;
    }
    if state == GenerationState::Accepted {
        if let Some(mut old) = accepted {
            let mut decision = GenerationDecision::new(DecisionReason::Replaced)?;
            decision.by = Some(id.clone());
            transition(c, info, &mut old, GenerationState::Retired, decision)?;
        }
        let mut decision = GenerationDecision::new(reason)?;
        if let GenerationOrigin::Runtime { plan_id, task_id } = origin {
            decision.plan_id = Some(plan_id.clone());
            decision.task_id = task_id.clone();
        }
        record.state = state;
        record.acceptance = Some(decision);
    }
    c.execute(
        "INSERT INTO ontology_generations(generation_id,repo_id,workspace_id,ordinal,sequence,fingerprint,snapshot,plan_id,state,record_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            id,
            info.repository_id.as_str(),
            info.workspace_id.as_str(),
            ordinal as i64,
            live.sequence as i64,
            live.fingerprint,
            record.snapshot.hash,
            record.plan().map(PlanId::as_str),
            record.state.sql(),
            serde_json::to_string(&record)?
        ],
    )?;
    journal(
        c,
        info,
        &record,
        record.acceptance.as_ref().map(|a| a.reason),
    )
}

/// Makes `record` the accepted generation. The caller has checked that the
/// worktree still matches the live index.
fn promote(
    c: &Connection,
    info: &RepositoryInfo,
    record: &mut OntologyGeneration,
    decision: GenerationDecision,
) -> Result<()> {
    require(
        record.state == GenerationState::Candidate,
        format!(
            "only a CANDIDATE can be accepted; {} is {:?}",
            record.generation_id, record.state
        ),
    )?;
    require(
        generation(c, info)?.as_ref() == Some(&record.generation),
        format!(
            "SOURCE_DRIFT: {} is no longer the indexed generation; run agentctl repo index",
            record.generation_id
        ),
    )?;
    require(
        record.failed_files == 0,
        "a partial observation (file failures) cannot be accepted",
    )?;
    let current = accepted(c, info)?;
    require(
        current.as_ref().map(|a| &a.generation_id) == record.base.as_ref(),
        "the accepted generation changed after this candidate was compared with it; run agentctl repo index to observe it again",
    )?;
    if let Some(mut old) = current {
        let mut retired = GenerationDecision::new(DecisionReason::Replaced)?;
        retired.by = Some(record.generation_id.clone());
        transition(c, info, &mut old, GenerationState::Retired, retired)?;
    }
    transition(c, info, record, GenerationState::Accepted, decision)
}

fn note(text: Option<&str>) -> Result<Option<String>> {
    if let Some(t) = text {
        require(
            !t.trim().is_empty() && t.len() <= 1024 && !t.chars().any(char::is_control),
            "decision reason must be 1–1024 bytes of printable text",
        )?;
    }
    Ok(text.map(str::to_owned))
}

/// Promotes the live candidate in the transaction that completes the plan
/// after its integration verification passed. Returns the accepted generation,
/// or `None` when the plan left the accepted facts unchanged.
pub(crate) fn accept_for_plan(
    c: &Connection,
    info: &RepositoryInfo,
    plan: &PlanId,
    verification_hash: String,
    source_hash: String,
) -> Result<Option<String>> {
    let live = generation(c, info)?
        .ok_or_else(|| Error::Invalid("SOURCE_DRIFT: workspace has no code index".into()))?;
    let accepted = accepted(c, info)?.ok_or_else(|| {
        Error::Invalid("SOURCE_DRIFT: workspace has no accepted ontology generation".into())
    })?;
    if live == accepted.generation {
        return Ok(None);
    }
    // The generation, not the record's label, identifies the facts: the
    // index is rehashed against the worktree below, and the caller has
    // checked that the worktree is exactly the verified state.
    let mut open = plan_candidate(c, info, plan, &live)?.ok_or_else(|| {
        Error::Invalid(
            "SOURCE_DRIFT: the indexed ontology generation is not an open candidate this plan produced (it was rejected, or the source moved); replan required".into(),
        )
    })?;
    require(
        status(c, info)?.fresh,
        "SOURCE_DRIFT: the indexed ontology no longer matches the worktree; replan required",
    )?;
    let mut decision = GenerationDecision::new(DecisionReason::IntegrationVerified)?;
    decision.plan_id = Some(plan.clone());
    decision.verification_hash = Some(verification_hash);
    decision.source_hash = Some(source_hash);
    promote(c, info, &mut open, decision)?;
    Ok(Some(open.generation_id))
}

/// Closes the plan's open candidate, if any, so it can never become accepted.
pub(crate) fn close_for_plan(
    c: &Connection,
    info: &RepositoryInfo,
    plan: &PlanId,
    task: Option<&TaskId>,
    reason: DecisionReason,
) -> Result<()> {
    let state = match reason {
        DecisionReason::VerificationRejected | DecisionReason::IntegrationRejected => {
            GenerationState::Rejected
        }
        DecisionReason::PlanCancelled | DecisionReason::PlanSuperseded => {
            GenerationState::Abandoned
        }
        _ => return Err(Error::Invalid("not a plan closure reason".into())),
    };
    if let Some(mut open) = candidate(c, info)?.filter(|o| o.plan() == Some(plan)) {
        let mut decision = GenerationDecision::new(reason)?;
        decision.plan_id = Some(plan.clone());
        decision.task_id = task.cloned();
        transition(c, info, &mut open, state, decision)?;
    }
    Ok(())
}

/// The open candidate, if it is the live generation and this plan's runtime
/// itself recorded that exact generation (sequence and fingerprint). Its own
/// label may be external: a human re-observing the unchanged state does not
/// change which facts it holds.
pub(crate) fn plan_candidate(
    c: &Connection,
    info: &RepositoryInfo,
    plan: &PlanId,
    live: &GraphGeneration,
) -> Result<Option<OntologyGeneration>> {
    let Some(open) = candidate(c, info)?.filter(|o| o.generation == *live) else {
        return Ok(None);
    };
    let observed = c
        .prepare("SELECT 1 FROM ontology_generations WHERE workspace_id=?1 AND plan_id=?2 AND sequence=?3 AND fingerprint=?4")?
        .exists(params![
            info.workspace_id.as_str(),
            plan.as_str(),
            live.sequence as i64,
            live.fingerprint
        ])?;
    Ok(observed.then_some(open))
}

impl Store {
    /// Returns the live open candidate only when this plan's runtime recorded
    /// the exact generation. This is the same ownership rule used by context
    /// issuance and final promotion, including identical external
    /// re-observations after a crash.
    pub(crate) fn candidate_for_plan(
        &self,
        start: &Path,
        plan: &PlanId,
    ) -> Result<Option<OntologyGeneration>> {
        let info = checked_workspace(self, start)?;
        let Some(live) = generation(&self.connection, &info)? else {
            return Ok(None);
        };
        plan_candidate(&self.connection, &info, plan, &live)
    }
}

/// Context may be issued only from accepted truth or from the plan's own
/// verified candidate, never from an unaccepted external observation.
pub(crate) fn require_issuable(c: &Connection, info: &RepositoryInfo, plan: &PlanId) -> Result<()> {
    let ok = match generation(c, info)? {
        None => false,
        Some(live) => {
            accepted(c, info)?.is_some_and(|a| a.generation == live)
                || plan_candidate(c, info, plan, &live)?.is_some()
        }
    };
    require(
        ok,
        "SOURCE_DRIFT: the indexed ontology generation is neither accepted nor this plan's verified candidate; replan required",
    )
}

/// Planning and runtime adoption bind only to accepted truth.
pub(crate) fn require_accepted(
    c: &Connection,
    info: &RepositoryInfo,
    bound: Option<&GraphGeneration>,
) -> Result<()> {
    let accepted = accepted(c, info)?;
    let live = generation(c, info)?;
    require(
        accepted.as_ref().is_some_and(|a| {
            Some(&a.generation) == bound && Some(&a.generation) == live.as_ref()
        }),
        match &accepted {
            None => "no accepted ontology generation; run agentctl repo index (see agentctl ontology status)".to_string(),
            Some(a) => format!(
                "the indexed ontology is not the accepted generation {} (sequence {}); inspect agentctl ontology status and delta, then accept the candidate or restore the source",
                a.generation_id, a.generation.sequence
            ),
        },
    )
}

impl Store {
    pub fn ontology_status(&self, start: &Path) -> Result<OntologyStatus> {
        let info = checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let live = generation(&tx, &info)?;
        let accepted = accepted(&tx, &info)?;
        let observed = match &live {
            Some(g) => load(&tx, &info, "sequence=?2", &g.sequence.to_string())?
                .filter(|r| r.generation == *g),
            None => None,
        };
        Ok(OntologyStatus {
            workspace_id: info.workspace_id.clone(),
            live_accepted: accepted
                .as_ref()
                .is_some_and(|a| Some(&a.generation) == live.as_ref()),
            live,
            accepted,
            candidate: candidate(&tx, &info)?,
            observed,
        })
    }

    /// Recorded generations, newest first.
    pub fn ontology_generations(
        &self,
        start: &Path,
        limit: usize,
    ) -> Result<Vec<OntologyGeneration>> {
        require(
            (1..=1000).contains(&limit),
            "generation list limit must be 1–1000",
        )?;
        let info = checked_workspace(self, start)?;
        self.connection
            .prepare("SELECT state,record_json FROM ontology_generations WHERE workspace_id=?1 ORDER BY ordinal DESC LIMIT ?2")?
            .query_map(params![info.workspace_id.as_str(), limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (state, json) = row?;
                decode(&info, &state, &json)
            })
            .collect()
    }

    pub fn ontology_generation(&self, start: &Path, id: &str) -> Result<OntologyGeneration> {
        let info = checked_workspace(self, start)?;
        by_id(&self.connection, &info, id)
    }

    /// The delta recorded when `id` was observed (against its base).
    pub fn ontology_delta(&self, start: &Path, id: &str) -> Result<SemanticDelta> {
        let info = checked_workspace(self, start)?;
        let record = by_id(&self.connection, &info, id)?;
        match &record.delta {
            DeltaStatus::Recorded { artifact, .. } => {
                let delta: SemanticDelta = delta::get(&self.connection, artifact)?;
                require(
                    delta.to.generation_id == record.generation_id
                        && Some(&delta.from.generation_id) == record.base.as_ref(),
                    "recorded semantic delta does not belong to its generation",
                )?;
                Ok(delta)
            }
            DeltaStatus::NoBase => Err(Error::Invalid(format!(
                "{id} was observed with no accepted generation to compare with; use --from/--to"
            ))),
            DeltaStatus::Unavailable { reason } => Err(Error::Invalid(format!(
                "{id} has no semantic delta: {reason}"
            ))),
        }
    }

    /// A delta between any two recorded generations, derived on demand.
    pub fn ontology_diff(&self, start: &Path, from: &str, to: &str) -> Result<SemanticDelta> {
        let info = checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let (a, b) = (by_id(&tx, &info, from)?, by_id(&tx, &info, to)?);
        let old: SnapshotManifest = delta::get(&tx, &a.snapshot)?;
        let new: SnapshotManifest = delta::get(&tx, &b.snapshot)?;
        require(
            old.generation == a.generation && new.generation == b.generation,
            "ontology snapshot does not match its generation",
        )?;
        delta::diff(&tx, (&a.point(), &old), (&b.point(), &new))
    }

    /// Deliberately accepts an external candidate. The candidate must still be
    /// the indexed generation, and the worktree must still match it.
    pub fn accept_generation(
        &mut self,
        start: &Path,
        id: &str,
        reason: Option<&str>,
    ) -> Result<OntologyGeneration> {
        let note = note(reason)?;
        let info = checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = by_id(&tx, &info, id)?;
        if record.state == GenerationState::Accepted {
            return Ok(record);
        }
        require(
            record.origin == GenerationOrigin::External,
            "runtime candidates are accepted only by their plan's integration verification",
        )?;
        require(
            record.state == GenerationState::Candidate,
            format!(
                "only an open CANDIDATE can be accepted; {id} is {:?}",
                record.state
            ),
        )?;
        require(
            status(&tx, &info)?.fresh,
            "the worktree no longer matches the indexed candidate; run agentctl repo index and inspect the new candidate",
        )?;
        let mut decision = GenerationDecision::new(DecisionReason::ManualAcceptance)?;
        decision.note = note;
        promote(&tx, &info, &mut record, decision)?;
        tx.commit()?;
        Ok(record)
    }

    /// Deliberately rejects an external candidate. Its evidence is retained.
    pub fn reject_generation(
        &mut self,
        start: &Path,
        id: &str,
        reason: &str,
    ) -> Result<OntologyGeneration> {
        let note = note(Some(reason))?;
        let info = checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = by_id(&tx, &info, id)?;
        if record.state == GenerationState::Rejected {
            return Ok(record);
        }
        require(
            record.origin == GenerationOrigin::External,
            "runtime candidates are closed by their plan's lifecycle (run cancel / run replace)",
        )?;
        require(
            record.state == GenerationState::Candidate,
            format!(
                "only an open CANDIDATE can be rejected; {id} is {:?}",
                record.state
            ),
        )?;
        let mut decision = GenerationDecision::new(DecisionReason::ManualRejection)?;
        decision.note = note;
        transition(&tx, &info, &mut record, GenerationState::Rejected, decision)?;
        tx.commit()?;
        Ok(record)
    }
}
