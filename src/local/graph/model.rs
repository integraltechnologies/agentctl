use crate::{
    local::repository::{RepositoryId, RepositorySourceState, WorkspaceId},
    protocol::GraphEntityId,
};
use serde::{Deserialize, Serialize};

/// Bump for any extraction, resolution, identity, or discovery-policy change.
pub const INDEX_VERSION: &str = "agentctl-graph-1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Language {
    pub fn for_path(path: &str) -> Option<Self> {
        match path.rsplit('.').next()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            _ => None,
        }
    }
    pub fn backend(self) -> String {
        let grammar = match self {
            Self::Rust => "rust-0.24.2",
            Self::Python => "python-0.25.0",
            Self::TypeScript => "typescript-0.23.2",
            Self::Tsx => "tsx-0.23.2",
            Self::JavaScript => "javascript-0.25.0",
        };
        format!("{INDEX_VERSION}/tree-sitter-0.25.10/{grammar}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntityKind {
    File,
    Module,
    Function,
    Method,
    Type,
    Enum,
    Trait,
    Constant,
    Test,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RelationKind {
    Contains,
    Imports,
    Calls,
    References,
    Implements,
    TestRelatedTo,
    DependsOn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub content_hash: String,
    pub language: Language,
    pub backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub parent: Option<GraphEntityId>,
    pub range: SourceRange,
    pub signature: String,
    pub visibility: Option<String>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source: GraphEntityId,
    /// None means a syntactic relation, NOT a resolved entity dependency.
    pub target: Option<GraphEntityId>,
    pub target_name: String,
    pub kind: RelationKind,
    pub range: SourceRange,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub path: String,
    pub content_hash: Option<String>,
    pub backend: String,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub discovered: usize,
    pub indexed: usize,
    pub reused: usize,
    pub changed: usize,
    pub deleted: usize,
    pub failed: usize,
    pub entities: usize,
    pub edges: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub version: String,
    pub indexed_at_ms: u64,
    pub source: RepositorySourceState,
    pub stats: IndexStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    pub index_version: String,
    pub backends: std::collections::BTreeMap<String, usize>,
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub current_source: RepositorySourceState,
    pub index: Option<IndexMetadata>,
    pub indexed_files: usize,
    pub stale_files: Vec<String>,
    pub failed_files: Vec<IndexedFile>,
    pub stale_file_count: usize,
    pub failed_file_count: usize,
    pub diagnostics_truncated: bool,
    pub entities: usize,
    pub edges: usize,
    /// Hash-checked observation, not an atomic filesystem snapshot or evidence binding.
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocatedEntity {
    pub entity: Entity,
    pub score: u32,
    pub signals: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ContextLimits {
    pub primary: usize,
    pub depth: usize,
    pub neighbors: usize,
    pub tests: usize,
}
impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            primary: 5,
            depth: 1,
            neighbors: 20,
            tests: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPacket {
    pub version: String,
    pub query: String,
    pub primary: Vec<LocatedEntity>,
    pub neighbors: Vec<Entity>,
    pub relations: Vec<Edge>,
    pub tests: Vec<Entity>,
    pub limits: ContextLimits,
    pub truncated: bool,
    pub freshness: IndexStatus,
    pub meaning: String,
}

pub fn content_hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

pub(super) fn stable_id(parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for part in parts {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("graph:{}", h.finalize().to_hex())
}
