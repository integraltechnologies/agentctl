use crate::{
    local::{
        graph::Provenance,
        repository::{RepositoryId, WorkspaceId},
    },
    protocol::*,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MemoryId(String);
impl MemoryId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        GraphEntityId::new(value.clone())?;
        if !value.starts_with("memory:") {
            return Err("memory ID must start with memory:".into());
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for MemoryId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::new(s)
    }
}
impl From<MemoryId> for String {
    fn from(id: MemoryId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryKind {
    Invariant,
    ArchitectureDecision,
    Constraint,
    Finding,
    Observation,
    Limitation,
    TaskConclusion,
    ExperimentConclusion,
    Note,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Rejected,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Origin {
    Human,
    ProjectConfig,
    Task,
    Verification,
    Evidence,
    GraphDerivation,
    Agent,
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum MemoryLink {
    Graph { id: GraphEntityId },
    File { path: String },
    Task { id: TaskId },
    Plan { id: PlanId },
    Job { id: JobId },
    Evidence { id: EvidenceId },
    Invariant { key: String },
    Commit { revision: String },
    Memory { id: MemoryId },
    Tag { value: String },
}
impl MemoryLink {
    pub(super) fn key(&self) -> (&'static str, &str) {
        match self {
            Self::Graph { id } => ("GRAPH", id.as_str()),
            Self::File { path } => ("FILE", path),
            Self::Task { id } => ("TASK", id.as_str()),
            Self::Plan { id } => ("PLAN", id.as_str()),
            Self::Job { id } => ("JOB", id.as_str()),
            Self::Evidence { id } => ("EVIDENCE", id.as_str()),
            Self::Invariant { key } => ("INVARIANT", key),
            Self::Commit { revision } => ("COMMIT", revision),
            Self::Memory { id } => ("MEMORY", id.as_str()),
            Self::Tag { value } => ("TAG", value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDraft {
    pub kind: MemoryKind,
    pub content: String,
    pub workspace_id: Option<WorkspaceId>,
    pub canonical_key: Option<String>,
    pub actor: String,
    pub author_job_id: Option<JobId>,
    pub links: Vec<MemoryLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Derivation {
    pub version: String,
    pub inputs: Vec<Provenance>,
    pub entities: Vec<GraphEntityId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub repository_id: RepositoryId,
    pub workspace_id: Option<WorkspaceId>,
    pub created_in_workspace: WorkspaceId,
    pub kind: MemoryKind,
    pub content: String,
    pub canonical_key: Option<String>,
    pub created_at_ms: u64,
    pub provenance: MemoryProvenance,
    pub origin: Origin,
    pub actor: String,
    pub provider: Option<ProviderMetadata>,
    pub links: Vec<MemoryLink>,
    pub derivation: Option<Derivation>,
    pub observed_source: Option<SourceStateRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Validity {
    Durable,
    Fresh,
    Stale,
    Historical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryView {
    pub entry: MemoryEntry,
    pub status: MemoryStatus,
    pub updated_at_ms: u64,
    pub superseded_by: Option<MemoryId>,
    pub validity: Validity,
    pub validity_detail: String,
    pub unresolved_links: Vec<MemoryLink>,
}

#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: Option<String>,
    pub trust: Option<MemoryTrustClass>,
    pub kind: Option<MemoryKind>,
    pub status: Option<MemoryStatus>,
    pub links: Vec<MemoryLink>,
    pub include_stale: bool,
    pub only_stale: bool,
    pub all_workspaces: bool,
    pub recent: bool,
    pub limit: usize,
}
impl Default for MemoryQuery {
    fn default() -> Self {
        Self {
            text: None,
            trust: None,
            kind: None,
            status: Some(MemoryStatus::Active),
            links: vec![],
            include_stale: false,
            only_stale: false,
            all_workspaces: false,
            recent: false,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProjection {
    pub key: String,
    pub trust: MemoryTrustClass,
    pub kind: MemoryKind,
    pub content: String,
    pub content_truncated: bool,
    pub origin: Origin,
    pub workspace_id: WorkspaceId,
    pub source_path: String,
    pub config_hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryResults {
    pub entries: Vec<MemoryView>,
    pub policy: Vec<PolicyProjection>,
    pub truncated: bool,
    pub checked_candidates: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemoryLimits {
    pub canonical: usize,
    pub facts: usize,
    pub notes: usize,
    pub bytes: usize,
}
impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            canonical: 3,
            facts: 3,
            notes: 1,
            bytes: 4096,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySummary {
    pub id: String,
    pub trust: MemoryTrustClass,
    pub kind: MemoryKind,
    pub content: String,
    pub origin: Origin,
    pub validity: Validity,
    pub workspace_id: Option<WorkspaceId>,
    pub content_truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContext {
    pub items: Vec<MemorySummary>,
    pub limits: MemoryLimits,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct CodeContextWithMemory {
    #[serde(flatten)]
    pub graph: crate::local::graph::ContextPacket,
    pub memory: MemoryContext,
}
