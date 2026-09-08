//! The v1 wire format. Fields are strict; incompatible changes require a new version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Required on every top-level document. Unsupported versions fail deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ProtocolVersion {
    #[serde(rename = "1")]
    V1,
}

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(
            #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._:-]*$"))]
            String,
        );

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                Self::try_from(value.into())
            }

            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.len() > 128
                    || !value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                    || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
                {
                    return Err(concat!(stringify!($name), " must be 1–128 ASCII identifier characters").into());
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self { value.0 }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    )+ };
}

ids!(
    TaskId,
    PlanId,
    JobId,
    AgentId,
    VerificationId,
    EvidenceId,
    ExperimentId,
    GraphEntityId
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentRole {
    Planner,
    Executor,
    Verifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Planned,
    Ready,
    Executing,
    AwaitingVerification,
    Verifying,
    Verified,
    Rejected,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationScope {
    Packet,
    Integration,
}

/// The discriminator and ID type cannot disagree about the verification scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "scope",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum VerificationTarget {
    Packet {
        task_id: TaskId,
        executor_job_id: JobId,
    },
    Integration {
        plan_id: PlanId,
        executor_job_ids: Vec<JobId>,
    },
}

impl VerificationTarget {
    pub fn scope(&self) -> VerificationScope {
        match self {
            Self::Packet { .. } => VerificationScope::Packet,
            Self::Integration { .. } => VerificationScope::Integration,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationDecision {
    Pass,
    Reject,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FindingSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryTrustClass {
    Canonical,
    Derived,
    Observed,
    AgentNote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryProvenance {
    pub version: ProtocolVersion,
    pub trust_class: MemoryTrustClass,
    pub source_refs: Vec<String>,
    pub evidence: Vec<EvidenceRef>,
    pub author_job_id: Option<JobId>,
}

/// A normalized repository-relative file or directory subtree. No glob syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ScopePath {
    File { path: String },
    Directory { path: String },
}

impl ScopePath {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path } | Self::Directory { path } => path,
        }
    }
}

/// Requirements are compact references to canonical checks, not embedded scripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationRequirements {
    #[schemars(length(min = 1))]
    pub requirement_refs: Vec<String>,
    pub evidence_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskPacket {
    pub version: ProtocolVersion,
    pub task_id: TaskId,
    pub objective: String,
    pub read_scope: Vec<ScopePath>,
    pub write_scope: Vec<ScopePath>,
    pub graph_entities: Vec<GraphEntityId>,
    pub invariant_refs: Vec<String>,
    pub dependencies: Vec<TaskId>,
    #[schemars(length(min = 1))]
    pub definition_of_done: Vec<String>,
    pub verification: VerificationRequirements,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanPacket {
    pub version: ProtocolVersion,
    pub plan_id: PlanId,
    pub objective: String,
    #[schemars(length(min = 1))]
    pub tasks: Vec<TaskPacket>,
    pub integration_verification: VerificationRequirements,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResultStatus {
    Succeeded,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FailureInfo {
    pub code: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultPacket {
    pub version: ProtocolVersion,
    pub task_id: TaskId,
    pub executor_job_id: JobId,
    pub status: ResultStatus,
    pub changed_paths: Vec<String>,
    pub changed_entities: Vec<GraphEntityId>,
    pub evidence: Vec<EvidenceRef>,
    pub notes: Option<String>,
    pub failure: Option<FailureInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocationRef {
    pub path: Option<String>,
    pub graph_entity: Option<GraphEntityId>,
    /// One-based line, only meaningful when a path is supplied.
    pub line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationFinding {
    pub severity: FindingSeverity,
    pub requirement_refs: Vec<String>,
    pub invariant_refs: Vec<String>,
    pub location: Option<LocationRef>,
    pub problem: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationPacket {
    pub version: ProtocolVersion,
    pub verification_id: VerificationId,
    pub target: VerificationTarget,
    /// A fresh verifier job, distinct from every executor job in the target.
    pub verifier_job_id: JobId,
    pub decision: VerificationDecision,
    pub findings: Vec<VerificationFinding>,
    pub evidence: Vec<EvidenceRef>,
    pub requirement_refs: Vec<String>,
    pub invariant_refs: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceStateRef {
    pub revision: String,
    /// Algorithm-prefixed digest identifying dirty worktree content, if any.
    pub worktree_diff_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActiveTask {
    pub task_id: TaskId,
    pub state: TaskState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResumePhase {
    Planning,
    Executing,
    PacketVerification,
    IntegrationVerification,
    Blocked,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumePacket {
    pub version: ProtocolVersion,
    pub plan_id: PlanId,
    pub active_task: Option<ActiveTask>,
    pub phase: ResumePhase,
    /// Completed means VERIFIED, never merely executor-finished.
    pub completed_tasks: Vec<TaskId>,
    pub pending_tasks: Vec<TaskId>,
    pub latest_verification: Option<VerificationId>,
    pub next_action: String,
    pub source_state: Option<SourceStateRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct EvidenceRef(pub EvidenceId);

/// Structured argv; consumers must not implicitly join this into a shell command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub version: ProtocolVersion,
    pub evidence_id: EvidenceId,
    pub command: Option<CommandSpec>,
    pub source_state: Option<SourceStateRef>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub exit_status: Option<i32>,
    pub stdout_hash: Option<String>,
    pub stderr_hash: Option<String>,
    pub full_log_ref: Option<String>,
    pub summary: String,
}

/// Opaque adapter metadata; roles and protocol semantics never depend on these strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderMetadata {
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentJob {
    pub version: ProtocolVersion,
    pub job_id: JobId,
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub plan_id: PlanId,
    pub task_id: Option<TaskId>,
    pub state: JobState,
    pub provider: Option<ProviderMetadata>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventContext {
    pub agent_id: Option<AgentId>,
    pub plan_id: Option<PlanId>,
    pub task_id: Option<TaskId>,
    /// In v1 a task is a TaskPacket; when both IDs are present they must agree.
    pub packet_id: Option<TaskId>,
    pub job_id: Option<JobId>,
    pub role: Option<AgentRole>,
    pub provider: Option<ProviderMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TokenUsageProvenance {
    Exact,
    Estimated,
    Unknown,
}

/// Per-observation deltas. Optional subcounts may overlap; do not sum them blindly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenUsageEvent {
    pub version: ProtocolVersion,
    pub timestamp_ms: u64,
    pub context: EventContext,
    pub provenance: TokenUsageProvenance,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentEvent {
    pub version: ProtocolVersion,
    pub event_id: String,
    pub timestamp_ms: u64,
    pub context: EventContext,
    pub event: AgentEventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum AgentEventKind {
    AgentStarted,
    AgentFinished {
        state: JobState,
    },
    TaskPacketLoaded {
        task_id: TaskId,
    },
    PlanStepStarted {
        step: String,
    },
    FileRead {
        path: String,
    },
    SymbolRead {
        entity: GraphEntityId,
    },
    FileEdited {
        path: String,
    },
    ToolStarted {
        invocation_id: String,
        tool: String,
    },
    ToolFinished {
        invocation_id: String,
        succeeded: bool,
        evidence: Vec<EvidenceRef>,
    },
    CommandStarted {
        invocation_id: String,
        command: CommandSpec,
    },
    CommandFinished {
        invocation_id: String,
        exit_status: i32,
        evidence: Vec<EvidenceRef>,
    },
    VerificationStarted {
        target: VerificationTarget,
    },
    VerificationCheckStarted {
        requirement_ref: String,
    },
    VerificationFindingCreated {
        finding: VerificationFinding,
    },
    WaitingOnDependency {
        task_ids: Vec<TaskId>,
    },
    ExperimentBoundary {
        boundary: ExperimentEvent,
    },
    TokenUsageObserved {
        usage: TokenUsageEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeSnapshot {
    pub version: ProtocolVersion,
    pub timestamp_ms: u64,
    pub context: EventContext,
    pub phase: String,
    pub current_step: Option<String>,
    pub current_target: Option<LocationRef>,
    pub current_tool: Option<String>,
    pub current_command: Option<CommandSpec>,
    pub last_event_id: Option<String>,
    pub elapsed_ms: u64,
    pub idle_ms: u64,
    pub blocker: Option<String>,
    pub waiting_on: Vec<TaskId>,
    pub current_verification_check: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MetricComparison {
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ExperimentBoundary {
    ProcessExit,
    Crash,
    NoProgress {
        timeout_ms: u64,
    },
    NanMetric {
        metric: String,
    },
    EpochComplete,
    MetricThreshold {
        metric: String,
        comparison: MetricComparison,
        value: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundaryDefinition {
    pub boundary_id: String,
    pub condition: ExperimentBoundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    pub version: ProtocolVersion,
    pub experiment_id: ExperimentId,
    pub command: CommandSpec,
    pub input_refs: Vec<String>,
    pub source_state: Option<SourceStateRef>,
    pub metric_refs: Vec<String>,
    pub output_refs: Vec<String>,
    pub decision_boundaries: Vec<BoundaryDefinition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperimentEvent {
    pub version: ProtocolVersion,
    pub experiment_id: ExperimentId,
    pub job_id: Option<JobId>,
    pub timestamp_ms: u64,
    pub boundary_id: String,
    pub boundary: ExperimentBoundary,
    /// Finite JSON numbers only. NaN is represented by a NAN_METRIC boundary.
    pub metrics: BTreeMap<String, f64>,
    pub evidence: Vec<EvidenceRef>,
    pub summary: String,
}
