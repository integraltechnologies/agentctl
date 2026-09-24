//! Deterministic description of the agentctl-controlled context issued to a
//! provider job: identities, the ontology generation, exact byte accounting by
//! category, and the repository paths/ranges, graph entities, memory and
//! invariants supplied. A manifest describes context and never copies it: no
//! source text, diff content, memory content or invariant text is recorded.
use super::*;
use serde_json::Value;

pub const MANIFEST_VERSION: &str = "agentctl-context-manifest-1";

/// JSON objects whose members are accounted as separate categories (dotted
/// paths relative to the job input); every other value is one leaf category.
const EXPANDED: &[&str] = &[
    "artifact",
    "artifact.context",
    "artifact.planner_packet",
    "artifact.planner_packet.request",
    "artifact.planner_packet.context",
    "artifact.planner_packet.context.graph",
    "artifact.graph",
    "artifact.diff",
];
const PACKET_EXPANDED: &[&str] = &[
    "planner_packet",
    "planner_packet.request",
    "planner_packet.context",
    "planner_packet.context.graph",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextManifest {
    pub version: String,
    pub role: AgentRole,
    pub job_id: Option<JobId>,
    pub plan_id: Option<PlanId>,
    pub task_id: Option<TaskId>,
    pub request_id: Option<planning::PlanningRequestId>,
    pub graph_version: Option<String>,
    pub graph_generation: Option<graph::GraphGeneration>,
    pub bytes: ContextBytes,
    pub paths: Vec<SuppliedSource>,
    pub graph_entities: Vec<GraphEntityId>,
    pub memory: Vec<String>,
    pub invariants: Vec<String>,
    /// Every approved ContextDelta issued to this job, in round order (empty in
    /// manifests recorded before the relay).
    #[serde(default)]
    pub context_deltas: Vec<DeltaManifest>,
    pub truncated: bool,
    pub bindings: ManifestBindings,
    /// The job's context round: 0 is the base issue, n the n-th fresh re-issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_round: Option<u32>,
    /// Each issued repository item traced to the authority that selected it,
    /// with its exact serialized bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issued: Vec<IssuedItem>,
    /// Repository read visibility the job was launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_visibility: Option<ContextVisibility>,
}

/// Why an item is in a worker's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Authority {
    /// A TaskPacket graph entity.
    PlannerGraphEntity,
    /// An explicit File read scope.
    PlannerReadFile,
    /// A File write target inside the read envelope.
    PlannerWriteTarget,
    /// A contract memory reference.
    PlannerMemoryRef,
    /// A task verification requirement.
    PlannerVerificationRef,
    /// An approved ContextDelta item.
    ContextDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedItem {
    pub authority: Authority,
    /// Entity ID, repository path, memory ID or requirement name.
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_id: Option<String>,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaManifest {
    pub delta_id: String,
    /// Content hash of the persisted delta artifact.
    pub hash: String,
    pub bytes: usize,
    pub round: u32,
    pub parent_job_id: JobId,
    pub planner_approved: bool,
}

/// Exact accounting of the bytes agentctl itself sends. `total` is the compiled
/// provider input; `categories` partitions it exactly: `instructions` (role
/// instructions and profile), one entry per context field, and `framing` (JSON
/// keys and punctuation). Provider-side system prompts, tool definitions and
/// tokenization are not observable by agentctl and are never estimated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBytes {
    pub total: usize,
    pub instructions: usize,
    pub context: usize,
    pub categories: Vec<CategoryBytes>,
    pub provider_hidden: HiddenContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryBytes {
    pub category: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HiddenContext {
    NotObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SuppliedKind {
    /// A bounded source range (planner excerpts).
    Excerpt,
    /// File content, possibly truncated (executor task context).
    File,
    /// Before/after content of a captured change (verifier diff).
    Diff,
    /// Graph facts derived from the file; its content is not supplied.
    GraphFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppliedSource {
    pub path: String,
    pub kind: SuppliedKind,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub content_hash: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestBindings {
    pub prompt_hash: Option<String>,
    pub context_hash: String,
    pub source_hash: String,
}

/// What a context builder intentionally supplied, gathered from typed values
/// at the point of construction rather than inferred from serialized JSON.
#[derive(Debug, Clone, Default)]
pub struct ContextInventory {
    pub paths: Vec<SuppliedSource>,
    pub graph_entities: Vec<GraphEntityId>,
    pub memory: Vec<String>,
    pub invariants: Vec<String>,
    pub graph_version: Option<String>,
    pub graph_generation: Option<graph::GraphGeneration>,
    pub truncated: bool,
    pub issued: Vec<IssuedItem>,
    pub deltas: Vec<DeltaManifest>,
    pub round: Option<u32>,
    pub visibility: Option<ContextVisibility>,
    /// Executor write scope, used only to confine writes under issued visibility.
    pub write_scope: Vec<ScopePath>,
}

impl ContextInventory {
    pub fn graph(&mut self, g: &graph::ContextPacket) {
        self.graph_entities.extend(g.entity_ids());
        self.graph_version = Some(g.version.clone());
        self.graph_generation = g.generation.clone();
        self.truncated |= g.truncated;
    }

    pub fn memory(&mut self, m: &memory::MemoryContext) {
        self.memory.extend(m.items.iter().map(|i| i.id.clone()));
        self.truncated |= m.truncated;
    }

    pub fn planner(p: &planning::PlannerPacket) -> Self {
        let mut inventory = Self::default();
        inventory.graph(&p.context.graph);
        inventory.memory(&p.context.memory);
        inventory.graph_version = Some(p.request.source.graph_version.clone());
        inventory.graph_generation = p.request.source.graph_generation.clone();
        inventory.invariants = p.context.invariants.keys().cloned().collect();
        inventory.truncated |= p.context.truncated;
        inventory
            .paths
            .extend(p.context.excerpts.iter().map(|e| SuppliedSource {
                path: e.provenance.path.clone(),
                kind: SuppliedKind::Excerpt,
                start_line: Some(e.start_line),
                end_line: Some(e.start_line + e.text.lines().count().max(1) - 1),
                content_hash: Some(e.provenance.content_hash.clone()),
                truncated: e.truncated,
            }));
        inventory
            .paths
            .extend(p.request.source.support.iter().map(|s| SuppliedSource {
                path: s.path.clone(),
                kind: SuppliedKind::GraphFacts,
                start_line: None,
                end_line: None,
                content_hash: Some(s.content_hash.clone()),
                truncated: false,
            }));
        inventory
    }

    fn finish(mut self) -> Self {
        self.paths.sort();
        self.paths.dedup();
        self.graph_entities.sort();
        self.graph_entities.dedup();
        self.memory.sort();
        self.memory.dedup();
        self.invariants.sort();
        self.invariants.dedup();
        self
    }
}

/// Adds one category per leaf of `value` (expanding the listed objects) and
/// returns the leaves' total bytes. Compact JSON of a value is identical as a
/// standalone document and as a member of its parent, so leaves are exact.
fn account(
    value: &Value,
    path: &str,
    expanded: &[&str],
    out: &mut Vec<CategoryBytes>,
) -> Result<usize> {
    if let Value::Object(members) = value
        && (path.is_empty() || expanded.contains(&path))
    {
        let mut sum = 0;
        for (key, member) in members {
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            sum += account(member, &child, expanded, out)?;
        }
        return Ok(sum);
    }
    let bytes = serde_json::to_vec(value)?.len();
    out.push(CategoryBytes {
        category: path.into(),
        bytes,
    });
    Ok(bytes)
}

fn accounting(
    context: &Value,
    root: &str,
    expanded: &[&str],
    instructions: usize,
) -> Result<ContextBytes> {
    let mut categories = vec![];
    let leaves = account(context, root, expanded, &mut categories)?;
    let context_bytes = serde_json::to_vec(context)?.len();
    categories.push(CategoryBytes {
        category: "framing".into(),
        bytes: context_bytes - leaves,
    });
    if instructions > 0 {
        categories.push(CategoryBytes {
            category: "instructions".into(),
            bytes: instructions,
        });
    }
    categories.sort_by(|a, b| a.category.cmp(&b.category));
    Ok(ContextBytes {
        total: context_bytes + instructions,
        instructions,
        context: context_bytes,
        categories,
        provider_hidden: HiddenContext::NotObserved,
    })
}

/// The manifest of one issued job, bound to its compiled prompt. Fails closed if
/// the accounting cannot reproduce the compiled prompt's byte counts.
pub fn for_job(
    input: &provider::JobInput,
    request: Option<&planning::PlanningRequestId>,
    compiled: &prompt::PromptProvenance,
    inventory: ContextInventory,
) -> Result<ContextManifest> {
    let context = serde_json::to_value(input)?;
    let instructions = compiled.instruction_bytes.unwrap_or_default();
    let bytes = accounting(&context, "", EXPANDED, instructions)?;
    require(
        compiled.context_bytes == Some(bytes.context) && compiled.bytes == bytes.total,
        "context manifest accounting diverged from the compiled prompt",
    )?;
    let inventory = inventory.finish();
    Ok(ContextManifest {
        version: MANIFEST_VERSION.into(),
        role: input.role,
        job_id: Some(input.job_id.clone()),
        plan_id: input.plan_id.clone(),
        task_id: input.task_id.clone(),
        request_id: request.cloned(),
        graph_version: inventory.graph_version,
        graph_generation: inventory.graph_generation,
        bytes,
        paths: inventory.paths,
        graph_entities: inventory.graph_entities,
        memory: inventory.memory,
        invariants: inventory.invariants,
        context_deltas: inventory.deltas,
        truncated: inventory.truncated || compiled.context_truncated.unwrap_or(false),
        bindings: ManifestBindings {
            prompt_hash: Some(compiled.prompt_hash.clone()),
            context_hash: compiled.context_hash.clone(),
            source_hash: compiled.source_hash.clone(),
        },
        context_round: inventory.round,
        issued: inventory.issued,
        context_visibility: inventory.visibility,
    })
}

/// The manifest of a frozen PlannerPacket itself (no job, no instructions):
/// what any planner job issued from it now receives as `planner_packet`. For a
/// packet prepared by this version `bytes.total` equals its `serialized_bytes`;
/// a packet prepared before the normalized graph encoding re-serializes smaller.
pub fn for_packet(p: &planning::PlannerPacket) -> Result<ContextManifest> {
    let value = serde_json::to_value(p)?;
    let mut bytes = accounting(&value, "", &[], 0)?;
    bytes.categories.clear();
    let leaves = account(
        &value,
        "planner_packet",
        PACKET_EXPANDED,
        &mut bytes.categories,
    )?;
    bytes.categories.push(CategoryBytes {
        category: "framing".into(),
        bytes: bytes.context - leaves,
    });
    bytes.categories.sort_by(|a, b| a.category.cmp(&b.category));
    let inventory = ContextInventory::planner(p).finish();
    Ok(ContextManifest {
        version: MANIFEST_VERSION.into(),
        role: AgentRole::Planner,
        job_id: None,
        plan_id: None,
        task_id: None,
        request_id: Some(p.request.request_id.clone()),
        graph_version: inventory.graph_version,
        graph_generation: inventory.graph_generation,
        bytes,
        paths: inventory.paths,
        graph_entities: inventory.graph_entities,
        memory: inventory.memory,
        invariants: inventory.invariants,
        context_deltas: vec![],
        truncated: inventory.truncated,
        bindings: ManifestBindings {
            prompt_hash: None,
            context_hash: planning::hash(p)?,
            source_hash: planning::hash(&p.request.source)?,
        },
        context_round: None,
        issued: vec![],
        context_visibility: None,
    })
}
