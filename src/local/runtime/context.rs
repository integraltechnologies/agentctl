//! Stage 2 planner-mediated context relay.
//!
//! The planner is the authority over what a worker initially sees. An executor's
//! base context is materialized only from planner-authored references: the
//! TaskPacket's graph entities, explicit File read scopes and File write targets
//! inside the read envelope, the contract's memory references, and the task's
//! verification requirements. A Directory read scope is an *authorization
//! envelope* for later requests, never an instruction to inject files.
//!
//! A worker that lacks context returns a typed [`ContextRequest`] instead of
//! exploring. [`resolve`] answers it deterministically from the current ontology
//! snapshot and captured source (no model), inside the envelope and within
//! machine-owned budgets, producing a hash-bound [`ContextDelta`]. Requests that
//! need anything outside the envelope are persisted for planner decision; they
//! are never granted automatically. Every grant is issued to a *fresh* provider
//! job as base context plus accumulated deltas.
use super::*;
use crate::local::graph::{
    AssociationBasis, Entity, EntityKind, GraphGeneration, RelationKind, ResolutionRule,
};
use crate::local::memory::{MemoryId, MemoryKind, MemoryStatus, Validity};
use manifest::{Authority, IssuedItem, SuppliedKind, SuppliedSource};

pub const ISSUED_CONTEXT_VERSION: &str = "agentctl-issued-context-1";
pub const DELTA_VERSION: &str = "agentctl-context-delta-1";
pub const RESOLUTION_VERSION: &str = "agentctl-context-resolution-1";
pub const DECISION_VERSION: &str = "agentctl-context-decision-1";

/// Base-context bounds (per planner reference and in total).
const DEFINITION_BYTES: usize = 6 * 1024;
const DEFINITION_LINES: usize = 160;
const FILE_BYTES: usize = 16 * 1024;
const MEMORY_CHARS: usize = 4096;
const BASE_TEXT_BYTES: usize = 64 * 1024;
/// Resolved relation stubs shown per direction for a planner-selected entity.
const RELATED_STUBS: usize = 8;
/// Resolver bounds (per request item).
const RELATION_RESULTS: usize = 32;
const TEST_RESULTS: usize = 8;
const TEST_EXCERPT_BYTES: usize = 4 * 1024;
const NEIGHBORHOOD_NODES: usize = 32;
const NAME_CANDIDATES: usize = 16;
const DELTA_DEFINITION_BYTES: usize = 8 * 1024;
const DELTA_DEFINITION_LINES: usize = 200;
const EDGE_SCAN: usize = 64;
/// Scope additions one planner decision may approve.
pub const MAX_SCOPE_ADDITIONS: usize = 8;

// ---------------------------------------------------------------------------
// Issued material.
// ---------------------------------------------------------------------------

/// Identity and structural facts of an ontology entity (no source text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityFacts {
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub signature: String,
    pub visibility: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content_hash: String,
}
impl EntityFacts {
    fn of(e: &Entity) -> Self {
        Self {
            id: e.id.clone(),
            kind: e.kind,
            name: e.name.clone(),
            qualified_name: e.qualified_name.clone(),
            path: e.provenance.path.clone(),
            signature: e.signature.chars().take(512).collect(),
            visibility: e.visibility.clone(),
            start_line: e.range.start_line,
            end_line: e.range.end_line,
            content_hash: e.provenance.content_hash.clone(),
        }
    }
}

/// An exact, bounded slice of captured source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceText {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub truncated: bool,
}

/// A resolved structural neighbor, by identity only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedEntity {
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub content_hash: String,
    pub relation: RelationKind,
    pub resolution: Option<ResolutionRule>,
}

/// A planner-selected graph entity: its facts, its bounded definition, and the
/// in-envelope entities one resolved relation away (identity only; their
/// source is available through a context request). `withheld` counts related
/// entities not shown (outside the envelope or past the stub bound).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedSymbol {
    pub entity: EntityFacts,
    pub definition: Option<SourceText>,
    pub callers: Vec<RelatedEntity>,
    pub callees: Vec<RelatedEntity>,
    pub withheld: usize,
}

/// A planner-named file: an explicit File read scope or a File write target
/// inside the read envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedFile {
    pub path: String,
    pub authority: Authority,
    /// False for a write target that does not exist yet or is not source.
    pub present: bool,
    pub content_hash: Option<String>,
    pub bytes: Option<u64>,
    pub text: Option<String>,
    pub binary: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedMemory {
    pub id: String,
    pub trust: MemoryTrustClass,
    pub kind: MemoryKind,
    pub validity: Validity,
    pub content: String,
    pub content_truncated: bool,
}

/// A task verification requirement dereferenced to its canonical commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedCheck {
    pub requirement: String,
    pub description: String,
    pub commands: Vec<CommandSpec>,
}

/// The executor's base context: planner references, dereferenced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedContext {
    pub version: String,
    pub graph_generation: Option<GraphGeneration>,
    pub source_hash: String,
    pub symbols: Vec<IssuedSymbol>,
    pub files: Vec<IssuedFile>,
    pub memory: Vec<IssuedMemory>,
    pub checks: Vec<IssuedCheck>,
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// Deltas, resolutions, ledgers and planner decisions.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestItem {
    pub entity: EntityFacts,
    pub basis: AssociationBasis,
    pub definition: Option<SourceText>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationFact {
    pub source: GraphEntityId,
    pub target: GraphEntityId,
    pub kind: RelationKind,
    pub resolution: Option<ResolutionRule>,
}

/// One resolved answer; `request_index` is the position of the request item it
/// answers. Typed facts and exact source only: no prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum DeltaItem {
    Symbol {
        request_index: usize,
        entity: EntityFacts,
        definition: Option<SourceText>,
    },
    Relations {
        request_index: usize,
        subject: GraphEntityId,
        relation: RelationDirection,
        entities: Vec<RelatedEntity>,
        truncated: bool,
    },
    Tests {
        request_index: usize,
        subject: GraphEntityId,
        tests: Vec<TestItem>,
        truncated: bool,
    },
    Neighborhood {
        request_index: usize,
        subject: GraphEntityId,
        depth: u8,
        entities: Vec<EntityFacts>,
        relations: Vec<RelationFact>,
        truncated: bool,
    },
    FileRange {
        request_index: usize,
        path: String,
        content_hash: String,
        start_line: u32,
        end_line: u32,
        text: String,
    },
    Memory {
        request_index: usize,
        memory: IssuedMemory,
    },
}
impl DeltaItem {
    /// The entity, path or memory ID this item is about (for the manifest).
    fn reference(&self) -> String {
        match self {
            Self::Symbol { entity, .. } => entity.id.as_str().into(),
            Self::Relations { subject, .. }
            | Self::Tests { subject, .. }
            | Self::Neighborhood { subject, .. } => subject.as_str().into(),
            Self::FileRange { path, .. } => path.clone(),
            Self::Memory { memory, .. } => memory.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Grant {
    /// Resolved entirely inside the planner-authored envelope.
    Automatic,
    /// Released by an explicit planner decision (bound by its artifact hash).
    PlannerApproved {
        decision_hash: String,
        scope_additions: Vec<ScopePath>,
    },
}

/// Approved additional context for one worker subject and round, bound to the
/// request, ontology generation and source state it was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDelta {
    pub version: String,
    pub delta_id: String,
    pub plan_id: PlanId,
    pub task_id: Option<TaskId>,
    pub role: AgentRole,
    /// The job whose request this delta answers.
    pub parent_job_id: JobId,
    pub request_hash: String,
    /// The round of the fresh job this delta is issued to (1-based).
    pub round: u32,
    pub graph_generation: Option<GraphGeneration>,
    pub source_hash: String,
    pub grant: Grant,
    pub items: Vec<DeltaItem>,
    /// Compact JSON bytes of `items`.
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ItemOutcome {
    Granted,
    OutsideEnvelope,
    NotFound,
    Ambiguous,
    Stale,
    NotSource,
    Binary,
    OutOfRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemResolution {
    pub index: usize,
    pub outcome: ItemOutcome,
    pub bytes: usize,
    /// Paths the answer touches; for OUTSIDE_ENVELOPE, the paths outside it.
    pub paths: Vec<String>,
    /// In-envelope candidates of an ambiguous name (bounded).
    pub candidates: Vec<GraphEntityId>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Granted,
    Escalate,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub request_max_bytes: usize,
    pub round_limit: usize,
    pub task_remaining: usize,
    pub rounds_used: u32,
    pub max_rounds: u32,
    pub escalations_used: u32,
    pub max_escalations: u32,
}

/// The persisted, machine-readable outcome of resolving one request. It is
/// never shown to the requesting worker; it is what a planner reviews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextResolution {
    pub version: String,
    pub subject: String,
    pub request_hash: String,
    pub graph_generation: Option<GraphGeneration>,
    pub source_hash: String,
    pub envelope: Vec<ScopePath>,
    pub items: Vec<ItemResolution>,
    pub verdict: Verdict,
    pub code: Option<String>,
    pub bytes: usize,
    pub budget: Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LedgerState {
    Open,
    NeedsPlannerContextApproval,
    Denied,
    PlannerDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RoundOutcome {
    Granted,
    Escalated,
    Denied,
    Approved,
    PlannerDenied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaRef {
    pub delta_id: String,
    pub artifact: ArtifactRef,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundRecord {
    /// Round of the requesting job (0 is the base issue).
    pub round: u32,
    pub job_id: JobId,
    pub request: ArtifactRef,
    pub resolution: ArtifactRef,
    pub outcome: RoundOutcome,
    pub delta: Option<DeltaRef>,
    pub decision: Option<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseRef {
    pub artifact: ArtifactRef,
    pub graph_generation: Option<GraphGeneration>,
    pub source_hash: String,
}

/// Durable relay state of one worker subject (an executor task, a packet
/// verification, or the integration verification), kept in the run record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextLedger {
    pub role: AgentRole,
    pub task_id: Option<TaskId>,
    pub state: LedgerState,
    pub base: Option<BaseRef>,
    /// Read-scope additions a planner explicitly approved for this subject.
    pub approved_scope: Vec<ScopePath>,
    pub rounds: Vec<RoundRecord>,
    pub granted_bytes: usize,
    pub escalations: u32,
}
impl ContextLedger {
    pub fn new(role: AgentRole, task_id: Option<TaskId>) -> Self {
        Self {
            role,
            task_id,
            state: LedgerState::Open,
            base: None,
            approved_scope: vec![],
            rounds: vec![],
            granted_bytes: 0,
            escalations: 0,
        }
    }
    /// Deltas already granted, in round order.
    pub fn deltas(&self) -> impl Iterator<Item = &DeltaRef> {
        self.rounds.iter().filter_map(|r| r.delta.as_ref())
    }
    /// Granted rounds, which is also the round number of the next fresh job.
    pub fn rounds_used(&self) -> u32 {
        self.deltas().count() as u32
    }
}

/// Ledger key of a worker subject.
pub fn subject_key(role: AgentRole, task: Option<&TaskId>) -> String {
    match (role, task) {
        (AgentRole::Executor, Some(t)) => format!("executor:{}", t.as_str()),
        (AgentRole::Verifier, Some(t)) => format!("verifier:{}", t.as_str()),
        (AgentRole::Verifier, None) => "integration".into(),
        (role, task) => format!("{role:?}:{}", task.map(TaskId::as_str).unwrap_or("-")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionKind {
    Approve,
    Deny,
}

/// A planner's explicit answer to an escalated executor request. It is accepted
/// only through the runtime's decision entry point, never from provider output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDecision {
    pub version: String,
    pub plan_id: PlanId,
    pub task_id: TaskId,
    /// Hash of the escalated ContextRequest artifact being decided.
    pub request_hash: String,
    pub decision: DecisionKind,
    /// For APPROVE: read-scope paths added to this task's relay envelope.
    pub read_scope_additions: Vec<ScopePath>,
    pub reason: String,
    pub actor: String,
}
impl ContextDecision {
    pub fn check(&self) -> Result<()> {
        require(
            self.version == DECISION_VERSION,
            format!("context decision version must be {DECISION_VERSION}"),
        )?;
        planning::validate_text(&self.reason, 1024, "decision reason")?;
        planning::validate_text(&self.actor, 128, "decision actor")?;
        match self.decision {
            DecisionKind::Approve => require(
                (1..=MAX_SCOPE_ADDITIONS).contains(&self.read_scope_additions.len()),
                "an approval adds 1–8 read-scope paths",
            ),
            DecisionKind::Deny => require(
                self.read_scope_additions.is_empty(),
                "a denial cannot add read scope",
            ),
        }?;
        for scope in &self.read_scope_additions {
            crate::validation::repo_path(scope.path())?;
        }
        Ok(())
    }
}

/// The paths a subject may be issued context from.
#[derive(Debug, Clone, Default)]
pub(super) struct Envelope {
    pub scopes: Vec<ScopePath>,
    /// Memory already authorized by the planner (contract memory_refs).
    pub memory: Vec<MemoryId>,
}
impl Envelope {
    pub fn permits(&self, path: &str) -> bool {
        self.scopes.iter().any(|s| permits(s, path))
    }
}

// ---------------------------------------------------------------------------
// Source helpers.
// ---------------------------------------------------------------------------

fn line_count(text: &str) -> usize {
    text.matches('\n').count() + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

/// Exact, char-boundary-safe excerpt of `[start, end)` bounded by bytes/lines.
fn excerpt(
    text: &str,
    start: usize,
    end_byte: usize,
    start_line: usize,
    max_bytes: usize,
    max_lines: usize,
) -> Result<SourceText> {
    require(
        start <= end_byte
            && end_byte <= text.len()
            && text.is_char_boundary(start)
            && text.is_char_boundary(end_byte),
        "invalid indexed source range",
    )?;
    let mut end = end_byte.min(start + max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if max_lines > 0
        && let Some((offset, _)) = text[start..end].match_indices('\n').nth(max_lines - 1)
    {
        end = start + offset + 1;
    }
    let slice = &text[start..end];
    Ok(SourceText {
        start_line,
        end_line: start_line + line_count(slice).max(1) - 1,
        text: slice.into(),
        truncated: end < end_byte,
    })
}

/// The captured UTF-8 text of `path`, only if its content is exactly what the
/// ontology entity was derived from. `None` means the entity is stale.
fn entity_text(
    source: &SourceSnapshot,
    artifacts: &Artifacts,
    entity: &Entity,
) -> Result<Option<String>> {
    let Some(file) = source.files.get(&entity.provenance.path) else {
        return Ok(None);
    };
    if file.content.hash != entity.provenance.content_hash {
        return Ok(None);
    }
    Ok(String::from_utf8(artifacts.get(&file.content)?).ok())
}

fn definition(
    source: &SourceSnapshot,
    artifacts: &Artifacts,
    entity: &Entity,
    max_bytes: usize,
    max_lines: usize,
) -> Result<Option<SourceText>> {
    let Some(text) = entity_text(source, artifacts, entity)? else {
        return Ok(None);
    };
    excerpt(
        &text,
        entity.range.start_byte,
        entity.range.end_byte,
        entity.range.start_line,
        max_bytes,
        max_lines,
    )
    .map(Some)
}

fn related(e: &Entity, edge: &graph::Edge) -> RelatedEntity {
    RelatedEntity {
        id: e.id.clone(),
        kind: e.kind,
        name: e.name.clone(),
        qualified_name: e.qualified_name.clone(),
        path: e.provenance.path.clone(),
        content_hash: e.provenance.content_hash.clone(),
        relation: edge.kind,
        resolution: edge.resolution,
    }
}

/// The resolved structural neighbors of `id` in one direction, deduplicated,
/// in deterministic edge order: `(entity, edge)`.
fn neighbors(
    graph: &graph::GraphQuery<'_>,
    id: &GraphEntityId,
    incoming: bool,
    limit: usize,
) -> Result<(Vec<(Entity, graph::Edge)>, bool)> {
    let edges = graph.resolved_edges(id, incoming, limit + 1)?;
    let truncated = edges.len() > limit;
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for edge in edges.into_iter().take(limit) {
        let other = if incoming {
            edge.source.clone()
        } else {
            match &edge.target {
                Some(t) => t.clone(),
                None => continue,
            }
        };
        if &other == id || !seen.insert(other.clone()) {
            continue;
        }
        if let Some(e) = graph.entity(&other)? {
            out.push((e, edge));
        }
    }
    Ok((out, truncated))
}

fn issued_memory(entry: &crate::local::memory::MemoryEntry, validity: Validity) -> IssuedMemory {
    IssuedMemory {
        id: entry.id.as_str().into(),
        trust: entry.provenance.trust_class,
        kind: entry.kind,
        validity,
        content: entry.content.chars().take(MEMORY_CHARS).collect(),
        content_truncated: entry.content.chars().count() > MEMORY_CHARS,
    }
}

fn drift(what: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("SOURCE_DRIFT: {what}; replan required"))
}

// ---------------------------------------------------------------------------
// Planner-authored base context.
// ---------------------------------------------------------------------------

/// Materializes an executor's base context from planner references only. No
/// objective-derived search runs, and a Directory scope contributes no files.
pub(super) fn base_executor(
    store: &Store,
    info: &RepositoryInfo,
    artifacts: &Artifacts,
    source: &SourceSnapshot,
    task: &planning::TaskInspection,
    envelope: &Envelope,
) -> Result<IssuedContext> {
    let graph = store.graph_for_issue(&info.root).map_err(drift)?;
    let packet = &task.packet;
    // Source text shared by every planner reference, in planner order.
    let mut allowance = BASE_TEXT_BYTES;
    let mut truncated = false;
    let mut symbols = vec![];
    for id in &packet.graph_entities {
        let entity = graph.entity(id)?.ok_or_else(|| {
            drift(format_args!(
                "planner-selected graph entity {} is absent from the current ontology",
                id.as_str()
            ))
        })?;
        require(
            envelope.permits(&entity.provenance.path),
            "planner-selected graph entity lies outside the task read scope",
        )?;
        let budget = allowance.min(DEFINITION_BYTES);
        let definition = if budget == 0 {
            truncated = true;
            None
        } else {
            let d = definition(source, artifacts, &entity, budget, DEFINITION_LINES)?.ok_or_else(
                || {
                    drift(format_args!(
                        "graph entity {} no longer matches captured source",
                        id.as_str()
                    ))
                },
            )?;
            allowance -= d.text.len();
            truncated |= d.truncated;
            Some(d)
        };
        let mut withheld = 0;
        let mut sides = [vec![], vec![]];
        for (side, incoming) in [(0, true), (1, false)] {
            let (found, cut) = neighbors(&graph, id, incoming, EDGE_SCAN)?;
            truncated |= cut;
            for (e, edge) in found {
                if envelope.permits(&e.provenance.path) && sides[side].len() < RELATED_STUBS {
                    sides[side].push(related(&e, &edge));
                } else {
                    withheld += 1;
                }
            }
        }
        let [callers, callees] = sides;
        symbols.push(IssuedSymbol {
            entity: EntityFacts::of(&entity),
            definition,
            callers,
            callees,
            withheld,
        });
    }
    let mut named: Vec<(String, Authority)> = vec![];
    for scope in &packet.read_scope {
        if let ScopePath::File { path } = scope {
            named.push((path.clone(), Authority::PlannerReadFile));
        }
    }
    for scope in &packet.write_scope {
        if let ScopePath::File { path } = scope
            && envelope.permits(path)
            && !named.iter().any(|(p, _)| p == path)
        {
            named.push((path.clone(), Authority::PlannerWriteTarget));
        }
    }
    let mut files = vec![];
    for (path, authority) in named {
        let mut file = IssuedFile {
            path: path.clone(),
            authority,
            present: false,
            content_hash: None,
            bytes: None,
            text: None,
            binary: false,
            truncated: false,
        };
        if let Some(state) = source.files.get(&path) {
            file.present = true;
            file.content_hash = Some(state.content.hash.clone());
            file.bytes = Some(state.content.bytes);
            match String::from_utf8(artifacts.get(&state.content)?) {
                Ok(text) => {
                    let mut end = text.len().min(FILE_BYTES).min(allowance);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    allowance -= end;
                    file.truncated = end < text.len();
                    truncated |= file.truncated;
                    file.text = Some(text[..end].into());
                }
                Err(_) => file.binary = true,
            }
        }
        files.push(file);
    }
    let mut memory = vec![];
    for id in &task.contract.memory_refs {
        let (entry, status, validity) =
            crate::local::memory::issued_reference(&store.connection, info, id)?.ok_or_else(
                || {
                    drift(format_args!(
                        "planner-referenced memory {} is missing",
                        id.as_str()
                    ))
                },
            )?;
        require(
            status == MemoryStatus::Active && validity != Validity::Stale,
            format!(
                "SOURCE_DRIFT: planner-referenced memory {} is no longer active and valid; replan required",
                id.as_str()
            ),
        )?;
        let mut issued = issued_memory(&entry, validity);
        if issued.content.len() > allowance {
            let mut end = allowance;
            while !issued.content.is_char_boundary(end) {
                end -= 1;
            }
            issued.content.truncate(end);
            issued.content_truncated = true;
        }
        allowance -= issued.content.len();
        truncated |= issued.content_truncated;
        memory.push(issued);
    }
    let policy = ProjectConfig::load(&info.root)?;
    let mut checks = vec![];
    for name in &packet.verification.requirement_refs {
        let definition = policy.verification.get(name).ok_or_else(|| {
            Error::Invalid(format!("verification requirement {name} is not declared"))
        })?;
        checks.push(IssuedCheck {
            requirement: name.clone(),
            description: definition.description.clone(),
            commands: definition
                .command_refs
                .iter()
                .filter_map(|c| policy.commands.get(c).cloned())
                .collect(),
        });
    }
    Ok(IssuedContext {
        version: ISSUED_CONTEXT_VERSION.into(),
        graph_generation: graph.generation().cloned(),
        source_hash: source_hash(source)?,
        symbols,
        files,
        memory,
        checks,
        truncated,
    })
}

pub(super) fn source_hash(source: &SourceSnapshot) -> Result<String> {
    Ok(source
        .source_ref()?
        .worktree_diff_hash
        .expect("source_ref always hashes the snapshot"))
}

// ---------------------------------------------------------------------------
// Manifest inventory.
// ---------------------------------------------------------------------------

fn bytes_of(value: &impl Serialize) -> Result<usize> {
    Ok(serde_json::to_vec(value)?.len())
}

/// Adds what a base context and its deltas supply to a manifest inventory:
/// every item traced to its authority with its exact serialized bytes.
pub(super) fn inventory(
    inventory: &mut manifest::ContextInventory,
    base: Option<&IssuedContext>,
    deltas: &[(ContextDelta, ArtifactRef)],
    round: u32,
) -> Result<()> {
    inventory.round = Some(round);
    if let Some(base) = base {
        inventory.graph_generation = base.graph_generation.clone();
        inventory.graph_version = Some(graph::INDEX_VERSION.into());
        inventory.truncated |= base.truncated;
        for s in &base.symbols {
            inventory.issued.push(IssuedItem {
                authority: Authority::PlannerGraphEntity,
                reference: s.entity.id.as_str().into(),
                delta_id: None,
                bytes: bytes_of(s)?,
            });
            inventory.graph_entities.push(s.entity.id.clone());
            if let Some(d) = &s.definition {
                inventory.paths.push(excerpt_source(&s.entity, d));
            }
            for r in s.callers.iter().chain(&s.callees) {
                inventory.graph_entities.push(r.id.clone());
                inventory.paths.push(facts_source(&r.path, &r.content_hash));
            }
        }
        for f in &base.files {
            inventory.issued.push(IssuedItem {
                authority: f.authority,
                reference: f.path.clone(),
                delta_id: None,
                bytes: bytes_of(f)?,
            });
            if f.text.is_some() {
                inventory.paths.push(SuppliedSource {
                    path: f.path.clone(),
                    kind: SuppliedKind::File,
                    start_line: None,
                    end_line: None,
                    content_hash: f.content_hash.clone(),
                    truncated: f.truncated,
                });
            }
        }
        for m in &base.memory {
            inventory.issued.push(IssuedItem {
                authority: Authority::PlannerMemoryRef,
                reference: m.id.clone(),
                delta_id: None,
                bytes: bytes_of(m)?,
            });
            inventory.memory.push(m.id.clone());
        }
        for c in &base.checks {
            inventory.issued.push(IssuedItem {
                authority: Authority::PlannerVerificationRef,
                reference: c.requirement.clone(),
                delta_id: None,
                bytes: bytes_of(c)?,
            });
        }
    }
    for (delta, artifact) in deltas {
        inventory.deltas.push(manifest::DeltaManifest {
            delta_id: delta.delta_id.clone(),
            hash: artifact.hash.clone(),
            bytes: delta.bytes,
            round: delta.round,
            parent_job_id: delta.parent_job_id.clone(),
            planner_approved: matches!(delta.grant, Grant::PlannerApproved { .. }),
        });
        if inventory.graph_generation.is_none() {
            inventory.graph_generation = delta.graph_generation.clone();
            inventory.graph_version = Some(graph::INDEX_VERSION.into());
        }
        for item in &delta.items {
            inventory.issued.push(IssuedItem {
                authority: Authority::ContextDelta,
                reference: item.reference(),
                delta_id: Some(delta.delta_id.clone()),
                bytes: bytes_of(item)?,
            });
            match item {
                DeltaItem::Symbol {
                    entity, definition, ..
                } => {
                    inventory.graph_entities.push(entity.id.clone());
                    if let Some(d) = definition {
                        inventory.paths.push(excerpt_source(entity, d));
                        inventory.truncated |= d.truncated;
                    }
                }
                DeltaItem::Relations {
                    entities,
                    truncated,
                    ..
                } => {
                    inventory.truncated |= truncated;
                    for r in entities {
                        inventory.graph_entities.push(r.id.clone());
                        inventory.paths.push(facts_source(&r.path, &r.content_hash));
                    }
                }
                DeltaItem::Tests {
                    tests, truncated, ..
                } => {
                    inventory.truncated |= truncated;
                    for t in tests {
                        inventory.graph_entities.push(t.entity.id.clone());
                        match &t.definition {
                            Some(d) => inventory.paths.push(excerpt_source(&t.entity, d)),
                            None => inventory
                                .paths
                                .push(facts_source(&t.entity.path, &t.entity.content_hash)),
                        }
                    }
                }
                DeltaItem::Neighborhood {
                    entities,
                    truncated,
                    ..
                } => {
                    inventory.truncated |= truncated;
                    for e in entities {
                        inventory.graph_entities.push(e.id.clone());
                        inventory.paths.push(facts_source(&e.path, &e.content_hash));
                    }
                }
                DeltaItem::FileRange {
                    path,
                    content_hash,
                    start_line,
                    end_line,
                    ..
                } => inventory.paths.push(SuppliedSource {
                    path: path.clone(),
                    kind: SuppliedKind::Excerpt,
                    start_line: Some(*start_line as usize),
                    end_line: Some(*end_line as usize),
                    content_hash: Some(content_hash.clone()),
                    truncated: false,
                }),
                DeltaItem::Memory { memory, .. } => inventory.memory.push(memory.id.clone()),
            }
        }
    }
    Ok(())
}

fn excerpt_source(entity: &EntityFacts, d: &SourceText) -> SuppliedSource {
    SuppliedSource {
        path: entity.path.clone(),
        kind: SuppliedKind::Excerpt,
        start_line: Some(d.start_line),
        end_line: Some(d.end_line),
        content_hash: Some(entity.content_hash.clone()),
        truncated: d.truncated,
    }
}

fn facts_source(path: &str, hash: &str) -> SuppliedSource {
    SuppliedSource {
        path: path.into(),
        kind: SuppliedKind::GraphFacts,
        start_line: None,
        end_line: None,
        content_hash: Some(hash.into()),
        truncated: false,
    }
}

// ---------------------------------------------------------------------------
// Read-only reporting.
// ---------------------------------------------------------------------------

/// The relay state of one run for `agentctl run context`: every subject's
/// budgets and rounds, and for a pending escalation the request, its
/// resolution and a decision template a planner can fill in. Read-only: it
/// launches nothing and changes nothing.
pub fn report(
    store: &Store,
    artifacts: &Artifacts,
    root: &Path,
    plan: &PlanId,
    limits: &ContextConfig,
) -> Result<serde_json::Value> {
    let info = graph::checked_workspace(store, root)?;
    let run = load_run(store, &info, plan)?
        .ok_or_else(|| Error::Invalid("runtime plan not found in this workspace".into()))?;
    let mut subjects = vec![];
    for (key, ledger) in &run.context {
        let max_rounds = if ledger.role == AgentRole::Executor {
            limits.max_rounds
        } else {
            limits.verifier_max_rounds
        };
        let mut rounds = vec![];
        let mut pending = None;
        for record in &ledger.rounds {
            let request: ContextRequest = artifacts.decode(&record.request)?;
            let resolution: ContextResolution = artifacts.decode(&record.resolution)?;
            if record.outcome == RoundOutcome::Escalated
                && ledger.state == LedgerState::NeedsPlannerContextApproval
            {
                let outside: BTreeSet<&String> = resolution
                    .items
                    .iter()
                    .filter(|i| i.outcome == ItemOutcome::OutsideEnvelope)
                    .flat_map(|i| &i.paths)
                    .collect();
                let additions: Vec<serde_json::Value> = outside
                    .iter()
                    .map(|p| serde_json::json!({"kind":"FILE","path":p}))
                    .collect();
                pending = Some(serde_json::json!({
                    "request_hash": record.request.hash,
                    "outside_paths": outside,
                    "decision_template": {
                        "version": DECISION_VERSION,
                        "plan_id": plan,
                        "task_id": ledger.task_id,
                        "request_hash": record.request.hash,
                        "decision": "APPROVE",
                        "read_scope_additions": additions,
                        "reason": "<why this expansion is the planner's intent>",
                        "actor": "<planner or operator>",
                    },
                }));
            }
            rounds.push(serde_json::json!({
                "round": record.round,
                "job_id": record.job_id,
                "outcome": record.outcome,
                "request": {"reason": request.reason, "items": request.items, "max_bytes": request.max_bytes, "hash": record.request.hash},
                "resolution": resolution,
                "delta": record.delta,
                "decision": record.decision,
            }));
        }
        subjects.push(serde_json::json!({
            "subject": key,
            "role": ledger.role,
            "task_id": ledger.task_id,
            "state": ledger.state,
            "rounds_used": ledger.rounds_used(),
            "max_rounds": max_rounds,
            "granted_bytes": ledger.granted_bytes,
            "task_bytes_remaining": (limits.max_task_bytes as usize).saturating_sub(ledger.granted_bytes),
            "escalations": ledger.escalations,
            "approved_scope": ledger.approved_scope,
            "rounds": rounds,
            "pending_decision": pending,
        }));
    }
    Ok(serde_json::json!({
        "plan_id": plan,
        "run_state": run.state,
        "reason": run.reason,
        "limits": limits,
        "subjects": subjects,
        "decide": "agentctl run context decide <plan-id> <decision.json>",
    }))
}

// ---------------------------------------------------------------------------
// Deterministic resolution.
// ---------------------------------------------------------------------------

struct Resolver<'a, 'q> {
    graph: &'a graph::GraphQuery<'q>,
    connection: &'a rusqlite::Connection,
    info: &'a RepositoryInfo,
    artifacts: &'a Artifacts,
    source: &'a SourceSnapshot,
    envelope: &'a Envelope,
}

type Answer = (ItemResolution, Option<DeltaItem>);

fn unresolved(index: usize, outcome: ItemOutcome, paths: Vec<String>, detail: &str) -> Answer {
    (
        ItemResolution {
            index,
            outcome,
            bytes: 0,
            paths,
            candidates: vec![],
            detail: Some(detail.into()),
        },
        None,
    )
}

impl Resolver<'_, '_> {
    /// Grants `answer` iff every path it touches lies inside the envelope;
    /// otherwise reports exactly the outside paths for planner review.
    fn finish(&self, index: usize, answer: DeltaItem, touched: Vec<String>) -> Result<Answer> {
        let touched: BTreeSet<String> = touched.into_iter().collect();
        let outside: Vec<String> = touched
            .iter()
            .filter(|p| !self.envelope.permits(p))
            .cloned()
            .collect();
        Ok((
            ItemResolution {
                index,
                outcome: if outside.is_empty() {
                    ItemOutcome::Granted
                } else {
                    ItemOutcome::OutsideEnvelope
                },
                bytes: bytes_of(&answer)?,
                paths: if outside.is_empty() {
                    touched.into_iter().collect()
                } else {
                    outside
                },
                candidates: vec![],
                detail: None,
            },
            Some(answer),
        ))
    }

    fn symbol(&self, index: usize, entity: Entity) -> Result<Answer> {
        let Some(definition) = definition(
            self.source,
            self.artifacts,
            &entity,
            DELTA_DEFINITION_BYTES,
            DELTA_DEFINITION_LINES,
        )?
        else {
            return Ok(unresolved(
                index,
                ItemOutcome::Stale,
                vec![entity.provenance.path.clone()],
                "entity no longer matches captured source",
            ));
        };
        let path = entity.provenance.path.clone();
        self.finish(
            index,
            DeltaItem::Symbol {
                request_index: index,
                entity: EntityFacts::of(&entity),
                definition: Some(definition),
            },
            vec![path],
        )
    }

    fn item(&self, index: usize, item: &ContextRequestItem) -> Result<Answer> {
        let subject = |id: &GraphEntityId| self.graph.entity(id);
        match item {
            ContextRequestItem::SymbolDefinition { entity_id } => match subject(entity_id)? {
                Some(e) => self.symbol(index, e),
                None => Ok(unresolved(
                    index,
                    ItemOutcome::NotFound,
                    vec![],
                    "entity is absent from the current ontology",
                )),
            },
            ContextRequestItem::SymbolByName { name } => {
                let mut found = self.graph.exact_matches(name, NAME_CANDIDATES)?;
                match found.len() {
                    0 => Ok(unresolved(
                        index,
                        ItemOutcome::NotFound,
                        vec![],
                        "no exact name, qualified-name or ID match",
                    )),
                    1 => self.symbol(index, found.remove(0)),
                    n => {
                        let (mut answer, _) = unresolved(
                            index,
                            ItemOutcome::Ambiguous,
                            vec![],
                            &format!("{n} exact matches; request one by entity_id"),
                        );
                        answer.candidates = found
                            .iter()
                            .filter(|e| self.envelope.permits(&e.provenance.path))
                            .map(|e| e.id.clone())
                            .take(8)
                            .collect();
                        Ok((answer, None))
                    }
                }
            }
            ContextRequestItem::SymbolRelations {
                entity_id,
                relation,
            } => {
                let Some(e) = subject(entity_id)? else {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::NotFound,
                        vec![],
                        "entity is absent from the current ontology",
                    ));
                };
                let incoming = *relation == RelationDirection::Callers;
                let (found, truncated) =
                    neighbors(self.graph, entity_id, incoming, RELATION_RESULTS)?;
                let mut touched = vec![e.provenance.path.clone()];
                touched.extend(found.iter().map(|(o, _)| o.provenance.path.clone()));
                self.finish(
                    index,
                    DeltaItem::Relations {
                        request_index: index,
                        subject: entity_id.clone(),
                        relation: *relation,
                        entities: found.iter().map(|(o, edge)| related(o, edge)).collect(),
                        truncated,
                    },
                    touched,
                )
            }
            ContextRequestItem::RelatedTests { entity_id } => {
                let Some(e) = subject(entity_id)? else {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::NotFound,
                        vec![],
                        "entity is absent from the current ontology",
                    ));
                };
                let found = self.graph.associated_tests(&e, TEST_RESULTS + 1)?;
                let truncated = found.len() > TEST_RESULTS;
                let mut touched = vec![e.provenance.path.clone()];
                let mut tests = vec![];
                for (test, basis) in found.into_iter().take(TEST_RESULTS) {
                    let Some(definition) =
                        definition(self.source, self.artifacts, &test, TEST_EXCERPT_BYTES, 120)?
                    else {
                        return Ok(unresolved(
                            index,
                            ItemOutcome::Stale,
                            vec![test.provenance.path.clone()],
                            "related test no longer matches captured source",
                        ));
                    };
                    touched.push(test.provenance.path.clone());
                    tests.push(TestItem {
                        entity: EntityFacts::of(&test),
                        basis,
                        definition: Some(definition),
                    });
                }
                self.finish(
                    index,
                    DeltaItem::Tests {
                        request_index: index,
                        subject: entity_id.clone(),
                        tests,
                        truncated,
                    },
                    touched,
                )
            }
            ContextRequestItem::Neighborhood { entity_id, depth } => {
                let Some(e) = subject(entity_id)? else {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::NotFound,
                        vec![],
                        "entity is absent from the current ontology",
                    ));
                };
                let mut nodes: BTreeMap<GraphEntityId, Entity> = BTreeMap::new();
                let mut order = vec![e.id.clone()];
                nodes.insert(e.id.clone(), e);
                let mut relations: BTreeMap<String, RelationFact> = BTreeMap::new();
                let mut frontier = vec![entity_id.clone()];
                let mut truncated = false;
                for _ in 0..*depth {
                    let mut next = vec![];
                    for id in &frontier {
                        for incoming in [true, false] {
                            let (found, cut) = neighbors(self.graph, id, incoming, EDGE_SCAN)?;
                            truncated |= cut;
                            for (other, edge) in found {
                                if !nodes.contains_key(&other.id) {
                                    if nodes.len() >= NEIGHBORHOOD_NODES {
                                        truncated = true;
                                        continue;
                                    }
                                    order.push(other.id.clone());
                                    next.push(other.id.clone());
                                    nodes.insert(other.id.clone(), other.clone());
                                }
                                let (source, target) = if incoming {
                                    (other.id.clone(), id.clone())
                                } else {
                                    (id.clone(), other.id.clone())
                                };
                                relations.insert(
                                    edge.id.clone(),
                                    RelationFact {
                                        source,
                                        target,
                                        kind: edge.kind,
                                        resolution: edge.resolution,
                                    },
                                );
                            }
                        }
                    }
                    frontier = next;
                }
                let touched = nodes.values().map(|n| n.provenance.path.clone()).collect();
                self.finish(
                    index,
                    DeltaItem::Neighborhood {
                        request_index: index,
                        subject: entity_id.clone(),
                        depth: *depth,
                        entities: order.iter().map(|id| EntityFacts::of(&nodes[id])).collect(),
                        relations: relations
                            .into_values()
                            .filter(|r| {
                                nodes.contains_key(&r.source) && nodes.contains_key(&r.target)
                            })
                            .collect(),
                        truncated,
                    },
                    touched,
                )
            }
            ContextRequestItem::FileRange {
                path,
                start_line,
                end_line,
            } => {
                let Some(file) = self.source.files.get(path) else {
                    let (outcome, detail) = if self.source.ignored.contains_key(path) {
                        (
                            ItemOutcome::NotSource,
                            "ignored files are observed by metadata only",
                        )
                    } else {
                        (
                            ItemOutcome::NotFound,
                            "no captured source file at this path",
                        )
                    };
                    return Ok(unresolved(index, outcome, vec![path.clone()], detail));
                };
                let Ok(text) = String::from_utf8(self.artifacts.get(&file.content)?) else {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::Binary,
                        vec![path.clone()],
                        "binary content is not issued as text",
                    ));
                };
                let lines: Vec<&str> = text.split_inclusive('\n').collect();
                if *start_line as usize > lines.len() {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::OutOfRange,
                        vec![path.clone()],
                        &format!("file has {} lines", lines.len()),
                    ));
                }
                let end = (*end_line as usize).min(lines.len());
                self.finish(
                    index,
                    DeltaItem::FileRange {
                        request_index: index,
                        path: path.clone(),
                        content_hash: file.content.hash.clone(),
                        start_line: *start_line,
                        end_line: end as u32,
                        text: lines[*start_line as usize - 1..end].concat(),
                    },
                    vec![path.clone()],
                )
            }
            ContextRequestItem::Memory { memory_id } => {
                let found = match MemoryId::new(memory_id.clone()) {
                    Ok(id) => {
                        crate::local::memory::issued_reference(self.connection, self.info, &id)?
                            .map(|found| (id, found))
                    }
                    Err(_) => None,
                };
                let Some((id, (entry, status, validity))) = found else {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::NotFound,
                        vec![],
                        "memory is absent from this workspace",
                    ));
                };
                if status != MemoryStatus::Active || validity == Validity::Stale {
                    return Ok(unresolved(
                        index,
                        ItemOutcome::Stale,
                        vec![],
                        "memory is inactive or stale",
                    ));
                }
                let answer = DeltaItem::Memory {
                    request_index: index,
                    memory: issued_memory(&entry, validity),
                };
                if self.envelope.memory.contains(&id) {
                    return self.finish(index, answer, vec![]);
                }
                if entry.provenance.trust_class == MemoryTrustClass::AgentNote {
                    return Ok((
                        ItemResolution {
                            index,
                            outcome: ItemOutcome::OutsideEnvelope,
                            bytes: bytes_of(&answer)?,
                            paths: vec![],
                            candidates: vec![],
                            detail: Some("agent notes require explicit planner selection".into()),
                        },
                        Some(answer),
                    ));
                }
                let mut touched = vec![];
                for link in &entry.links {
                    match link {
                        crate::local::memory::MemoryLink::File { path } => {
                            touched.push(path.clone())
                        }
                        crate::local::memory::MemoryLink::Graph { id } => {
                            if let Some(e) = self.graph.entity(id)? {
                                touched.push(e.provenance.path);
                            }
                        }
                        _ => {}
                    }
                }
                if touched.is_empty() {
                    return Ok((
                        ItemResolution {
                            index,
                            outcome: ItemOutcome::OutsideEnvelope,
                            bytes: bytes_of(&answer)?,
                            paths: vec![],
                            candidates: vec![],
                            detail: Some(
                                "memory has no repository link inside the envelope".into(),
                            ),
                        },
                        Some(answer),
                    ));
                }
                self.finish(index, answer, touched)
            }
        }
    }
}

/// Resolves one validated request against the current ontology snapshot and
/// captured source, without a model. Items are answered in request order; the
/// verdict is DENIED if any item cannot be resolved or a budget is exceeded,
/// ESCALATE if any answer needs a path outside the envelope, else GRANTED.
/// Only a GRANTED verdict returns delta items.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve(
    store: &Store,
    info: &RepositoryInfo,
    artifacts: &Artifacts,
    source: &SourceSnapshot,
    subject: &str,
    expected_generation: Option<&GraphGeneration>,
    envelope: &Envelope,
    request: &ContextRequest,
    request_hash: &str,
    budget: Budget,
) -> Result<(ContextResolution, Vec<DeltaItem>)> {
    let graph = store.graph_for_issue(&info.root).map_err(drift)?;
    if graph.generation() != expected_generation {
        return Err(drift("ontology generation changed between context rounds"));
    }
    let mut resolution = ContextResolution {
        version: RESOLUTION_VERSION.into(),
        subject: subject.into(),
        request_hash: request_hash.into(),
        graph_generation: graph.generation().cloned(),
        source_hash: source_hash(source)?,
        envelope: envelope.scopes.clone(),
        items: vec![],
        verdict: Verdict::Denied,
        code: None,
        bytes: 0,
        budget: budget.clone(),
    };
    if budget.rounds_used >= budget.max_rounds {
        resolution.code = Some("ROUNDS_EXHAUSTED".into());
        return Ok((resolution, vec![]));
    }
    let resolver = Resolver {
        graph: &graph,
        connection: &store.connection,
        info,
        artifacts,
        source,
        envelope,
    };
    let mut answers = vec![];
    for (index, item) in request.items.iter().enumerate() {
        let (outcome, answer) = resolver.item(index, item)?;
        resolution.items.push(outcome);
        answers.extend(answer);
    }
    resolution.bytes = bytes_of(&answers)?;
    let outcomes = || resolution.items.iter().map(|i| i.outcome);
    let (verdict, code) =
        if outcomes().any(|o| !matches!(o, ItemOutcome::Granted | ItemOutcome::OutsideEnvelope)) {
            (Verdict::Denied, Some("ITEM_UNRESOLVABLE"))
        } else if outcomes().any(|o| o == ItemOutcome::OutsideEnvelope) {
            if budget.escalations_used < budget.max_escalations {
                (Verdict::Escalate, None)
            } else if budget.max_escalations == 0 {
                (Verdict::Denied, Some("ESCALATION_NOT_PERMITTED"))
            } else {
                (Verdict::Denied, Some("ESCALATIONS_EXHAUSTED"))
            }
        } else if resolution.bytes > budget.round_limit {
            (Verdict::Denied, Some("ROUND_BUDGET_EXCEEDED"))
        } else if resolution.bytes > budget.task_remaining {
            (Verdict::Denied, Some("TASK_BUDGET_EXHAUSTED"))
        } else {
            (Verdict::Granted, None)
        };
    resolution.verdict = verdict;
    resolution.code = code.map(str::to_owned);
    if verdict != Verdict::Granted {
        answers.clear();
    }
    Ok((resolution, answers))
}

/// Assembles the hash-bound delta for a GRANTED resolution.
#[allow(clippy::too_many_arguments)]
pub(super) fn delta(
    plan: &PlanId,
    task: Option<&TaskId>,
    role: AgentRole,
    parent: &JobId,
    round: u32,
    resolution: &ContextResolution,
    grant: Grant,
    items: Vec<DeltaItem>,
) -> Result<ContextDelta> {
    require(
        resolution.verdict == Verdict::Granted,
        "only a granted resolution yields a context delta",
    )?;
    let id = planning::hash(&(plan, task, role, parent, &resolution.request_hash, round))?;
    let hex = id.strip_prefix("blake3:").unwrap_or(&id);
    let bytes = bytes_of(&items)?;
    Ok(ContextDelta {
        version: DELTA_VERSION.into(),
        delta_id: format!("delta:{}", &hex[..32]),
        plan_id: plan.clone(),
        task_id: task.cloned(),
        role,
        parent_job_id: parent.clone(),
        request_hash: resolution.request_hash.clone(),
        round,
        graph_generation: resolution.graph_generation.clone(),
        source_hash: resolution.source_hash.clone(),
        grant,
        items,
        bytes,
    })
}
