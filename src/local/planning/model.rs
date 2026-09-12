use crate::{
    local::{
        config::ProjectConfig,
        graph,
        memory::{MemoryContext, MemoryId, MemoryLimits},
        repository::RepositorySourceState,
    },
    protocol::*,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PlanningRequestId(String);
impl PlanningRequestId {
    pub fn new(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();
        GraphEntityId::new(s.clone())?;
        if !s.starts_with("request:") {
            return Err("planning request ID must start with request:".into());
        }
        Ok(Self(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for PlanningRequestId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::new(s)
    }
}
impl From<PlanningRequestId> for String {
    fn from(id: PlanningRequestId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningProvenance {
    pub actor: String,
    pub source_refs: Vec<String>,
    pub provider: Option<ProviderMetadata>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SourceGuarantee {
    SequentialObservationNotExactDiff,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningSource {
    pub observation: RepositorySourceState,
    pub policy_hash: String,
    pub graph_version: String,
    pub support: Vec<graph::Provenance>,
    pub guarantee: SourceGuarantee,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDraft {
    pub objective: String,
    /// Optional narrower lexical query; never interpreted as a prompt.
    pub query: Option<String>,
    pub scope: Vec<ScopePath>,
    pub constraints: Vec<String>,
    pub definition_of_done: Vec<String>,
    pub verification: Option<VerificationRequirements>,
    pub invariant_refs: Vec<String>,
    pub provenance: PlanningProvenance,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningRequest {
    pub version: ProtocolVersion,
    pub request_id: PlanningRequestId,
    pub intent: RequestDraft,
    pub source: PlanningSource,
    pub created_at_ms: u64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningLimits {
    pub graph: graph::ContextLimits,
    pub memory: MemoryLimits,
    pub files: usize,
    pub excerpt_bytes: usize,
    pub excerpt_lines: usize,
    pub bytes: usize,
}
impl Default for PlanningLimits {
    fn default() -> Self {
        Self {
            graph: graph::ContextLimits {
                primary: 4,
                depth: 1,
                neighbors: 8,
                tests: 4,
            },
            memory: MemoryLimits {
                canonical: 4,
                facts: 3,
                notes: 0,
                bytes: 4096,
            },
            files: 8,
            excerpt_bytes: 768,
            excerpt_lines: 20,
            bytes: 32768,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceExcerpt {
    pub provenance: graph::Provenance,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub text: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningContext {
    pub graph: graph::ContextPacket,
    pub memory: MemoryContext,
    /// Frozen, validated policy input; not a competing mutable policy store.
    pub policy: ProjectConfig,
    pub invariants: std::collections::BTreeMap<String, String>,
    pub excerpts: Vec<SourceExcerpt>,
    pub limits: PlanningLimits,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerPacket {
    pub artifact: PlanningArtifactKind,
    pub request: PlanningRequest,
    pub context: PlanningContext,
    /// Compact UTF-8 JSON bytes of this entire envelope, including this field.
    pub serialized_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanningArtifactKind {
    FrozenPlanningInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerifierInput {
    PacketDiffAndEvidence,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationContract {
    pub task_id: TaskId,
    /// Binds objective, scope, invariants, done criteria and checks without duplicating them.
    pub task_packet_hash: String,
    pub independent_verifier: bool,
    pub input: VerifierInput,
    pub memory_refs: Vec<MemoryId>,
    pub exclusions: Vec<ScopePath>,
    pub non_goals: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationVerificationContract {
    pub plan_id: PlanId,
    pub plan_packet_hash: String,
    pub independent_verifier: bool,
    pub require_all_task_verifications: bool,
    pub require_final_diff_and_evidence: bool,
    pub expectations: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplanReference {
    pub previous_plan_id: PlanId,
    pub reason: String,
    /// Historical references only: never copies VERIFIED state into new tasks.
    pub previously_verified_tasks: Vec<TaskId>,
    pub replaced_tasks: Vec<TaskId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanMetadata {
    pub version: ProtocolVersion,
    pub request_id: PlanningRequestId,
    pub source: PlanningSource,
    pub created_at_ms: u64,
    pub provenance: PlanningProvenance,
    pub contracts: Vec<VerificationContract>,
    pub integration: IntegrationVerificationContract,
    pub replan: Option<ReplanReference>,
}
/// Stage 4 envelope. The canonical Stage 0 PlanPacket remains the only task definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlan {
    pub metadata: PlanMetadata,
    pub packet: PlanPacket,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanState {
    Validated,
    Active,
    Complete,
    Superseded,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlanView {
    pub plan: ExecutionPlan,
    pub state: PlanState,
    pub updated_at_ms: u64,
    pub superseded_by: Option<PlanId>,
    pub integration_proof: Option<VerificationPacket>,
    pub final_source: Option<SourceStateRef>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TaskInspection {
    pub planning_request_id: PlanningRequestId,
    pub source: PlanningSource,
    pub constraints: Vec<String>,
    pub invariants: std::collections::BTreeMap<String, String>,
    pub packet: TaskPacket,
    pub contract: VerificationContract,
    pub state: TaskState,
    pub structurally_ready: bool,
    pub reasons: Vec<String>,
    pub packet_bytes: usize,
    pub contract_bytes: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct PlanSummary {
    pub plan_id: PlanId,
    pub state: PlanState,
    pub workspace_id: crate::local::repository::WorkspaceId,
    pub objective: String,
}
