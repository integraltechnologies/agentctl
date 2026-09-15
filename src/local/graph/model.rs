use crate::{
    local::repository::{RepositoryId, RepositorySourceState, WorkspaceId},
    protocol::GraphEntityId,
};
use serde::{Deserialize, Serialize};

/// Bump for any extraction, resolution, identity, or discovery-policy change.
pub const INDEX_VERSION: &str = "agentctl-graph-2";

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
    /// Normalized lexical path used only for deterministic path resolution
    /// (`mod.rs`/`lib.rs`/`__init__` and Rust `impl` segments normalized). It is
    /// not carried in context packets.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    pub parent: Option<GraphEntityId>,
    pub range: SourceRange,
    pub signature: String,
    pub visibility: Option<String>,
    pub provenance: Provenance,
}

/// How a relation's target was determined. Every rule is syntactic and requires
/// exactly one compatible candidate; anything ambiguous stays unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResolutionRule {
    /// A bare or `self::`/`super::` path to the unique declaration visible in the
    /// same file's lexical scope chain, never through a shadowing local binding.
    LexicalScope,
    /// `self.m()`, `this.m()`, `Self::m` or `Type::m` to the unique method of the
    /// enclosing or named type declared in the same file.
    EnclosingType,
    /// A qualified path whose normalized suffix names exactly one compatible
    /// declaration in the workspace. Re-exports and imports are not followed.
    QualifiedPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source: GraphEntityId,
    /// None means a syntactic relation, NOT a resolved entity dependency.
    pub target: Option<GraphEntityId>,
    pub target_name: String,
    pub kind: RelationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionRule>,
    /// Normalized path used by workspace-level resolution; not carried in
    /// context packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hint: Option<String>,
    pub range: SourceRange,
    pub provenance: Provenance,
}

/// The ontology state a consumer reasons against. `fingerprint` is derived only
/// from the index version and every indexed file's path, content hash, backend
/// and diagnostic, so identical sources yield identical fingerprints anywhere.
/// `sequence` is monotonic per workspace and advances only when the fingerprint
/// changes, which orders generations for later stale-generation checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphGeneration {
    pub sequence: u64,
    pub fingerprint: String,
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
    /// Relations resolved by workspace-level qualified-path resolution.
    #[serde(default)]
    pub resolved: usize,
    /// Absent in index events recorded before generations existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GraphGeneration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub version: String,
    pub indexed_at_ms: u64,
    pub source: RepositorySourceState,
    pub stats: IndexStats,
    /// Absent (None) only in metadata written before generations existed.
    #[serde(default)]
    pub generation: Option<GraphGeneration>,
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
impl IndexStatus {
    pub fn generation(&self) -> Option<&GraphGeneration> {
        self.index.as_ref().and_then(|m| m.generation.as_ref())
    }
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

/// Why a test is offered as related context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssociationBasis {
    /// The test has a resolved relation to the target.
    Calls,
    /// The test has a resolved relation to a test helper that resolves to the target.
    CallsViaHelper,
    /// The test shares the target's lexical container.
    Container,
    /// Lexical similarity to the query only; not a structural link.
    Lexical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestAssociation {
    pub test: GraphEntityId,
    pub target: Option<GraphEntityId>,
    pub basis: AssociationBasis,
}

/// Bounded digest of an entity's unresolved syntactic relations: the most
/// frequent distinct target names, not one record per call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedSummary {
    pub entity: GraphEntityId,
    pub kind: RelationKind,
    /// Unresolved relations of this kind observed (bounded by the query).
    pub count: usize,
    pub names: Vec<String>,
}

/// Serialized with one normalized source table instead of per-record provenance
/// (see `wire.rs`); records are rehydrated with their full provenance on read.
#[derive(Debug, Clone)]
pub struct ContextPacket {
    pub version: String,
    pub query: String,
    pub generation: Option<GraphGeneration>,
    pub primary: Vec<LocatedEntity>,
    pub neighbors: Vec<Entity>,
    /// Resolved relations among the selected entities only.
    pub relations: Vec<Edge>,
    pub tests: Vec<Entity>,
    pub associations: Vec<TestAssociation>,
    pub unresolved: Vec<UnresolvedSummary>,
    pub limits: ContextLimits,
    pub truncated: bool,
    pub freshness: IndexStatus,
    pub meaning: String,
}

impl ContextPacket {
    pub fn entity_ids(&self) -> std::collections::BTreeSet<GraphEntityId> {
        self.primary
            .iter()
            .map(|p| &p.entity)
            .chain(&self.neighbors)
            .chain(&self.tests)
            .map(|e| e.id.clone())
            .collect()
    }

    /// Keeps only entities whose file passes `keep`, then drops relations,
    /// associations and summaries whose endpoints are gone. Returns whether
    /// anything was removed.
    pub fn retain_files(&mut self, keep: impl Fn(&str) -> bool) -> bool {
        let before = self.record_count();
        self.primary.retain(|p| keep(&p.entity.provenance.path));
        self.neighbors.retain(|e| keep(&e.provenance.path));
        self.tests.retain(|e| keep(&e.provenance.path));
        self.prune();
        before != self.record_count()
    }

    /// Removes relations, associations and summaries that refer to entities no
    /// longer present, so no record outlives its endpoints.
    pub fn prune(&mut self) -> bool {
        let before = self.record_count();
        let ids = self.entity_ids();
        self.relations.retain(|r| {
            ids.contains(&r.source) && r.target.as_ref().is_some_and(|t| ids.contains(t))
        });
        self.associations
            .retain(|a| ids.contains(&a.test) && a.target.as_ref().is_none_or(|t| ids.contains(t)));
        self.unresolved.retain(|u| ids.contains(&u.entity));
        before != self.record_count()
    }

    fn record_count(&self) -> usize {
        self.primary.len()
            + self.neighbors.len()
            + self.tests.len()
            + self.relations.len()
            + self.associations.len()
            + self.unresolved.len()
    }
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
