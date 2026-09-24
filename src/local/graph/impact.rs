//! Semantic impact analysis over the accepted ontology, seeded by a semantic
//! delta or by a planner's prospective edit.
//!
//! The core invariant is that no impact claim exists without a machine
//! inspectable evidence chain through already-observed repository facts: every
//! [`ImpactItem`] carries the ordered [`ImpactStep`]s that reached it, and each
//! step names a resolved relation, a containment link, or a graph test
//! association. Lexical similarity, name coincidence and graph proximity never
//! create an item; where the ontology cannot prove connectivity the traversal
//! stops and records an [`ImpactBoundary`] instead of inventing an edge.
//!
//! Traversal is a bounded breadth-first walk of *incoming* resolved relations.
//! Distance alone is never relevance: a hop beyond the first is taken only when
//! the ontology proves the previous node re-exposes the change (it implements
//! it, or its own declared visibility is exported and the seed changed a
//! contract). Everything else terminates with `UNDETERMINED_PROPAGATION`.
use super::*;
use crate::protocol::{GraphEntityId, ProtocolVersion, ScopePath};
use serde::{Deserialize, Serialize};

/// Why an entity may be affected. The taxonomy is deliberately small: each
/// variant corresponds to a distinct kind of evidence the ontology can prove.
/// Uncertainty is not a class; it is an [`ImpactBoundary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImpactClass {
    /// One resolved relation away from a changed entity.
    DirectDependency,
    /// Reached through a node the ontology proves re-exposes the change (an
    /// implementation, an import, or an exported declaration).
    ContractExposure,
    /// A test associated with a changed or affected entity by the graph's
    /// association rules. Candidate coverage, never proven coverage.
    VerificationRelevance,
    /// The container whose composition changed because a declaration it owns
    /// was added or removed.
    ContainmentOwnership,
}

/// One evidence hop. Direction is always "affected entity <- source of truth".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ImpactEdge {
    /// `entity` has this resolved relation to `from`.
    Relation {
        kind: RelationKind,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rules: Vec<ResolutionRule>,
        /// Present when the relation existed only in the compared-from
        /// generation, i.e. the evidence is a removed relation in the delta.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        removed: bool,
    },
    /// `entity` lexically contains `from`.
    Containment,
    /// `entity` is a test associated with `from`.
    TestAssociation { basis: AssociationBasis },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactStep {
    pub from: GraphEntityId,
    pub entity: GraphEntityId,
    #[serde(flatten)]
    pub edge: ImpactEdge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactItem {
    pub entity: GraphEntityId,
    pub kind: EntityKind,
    pub path: String,
    pub qualified_name: String,
    pub class: ImpactClass,
    /// Evidence hops from the seed; equals `evidence.len()`.
    pub distance: usize,
    pub seed: GraphEntityId,
    /// Ordered chain seed -> ... -> entity. Never empty.
    pub evidence: Vec<ImpactStep>,
}

/// An explicitly represented limit of what the ontology can prove. A boundary
/// is not a claim that something is affected; it is a claim that the question
/// is open.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "reason",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum BoundaryReason {
    /// The seed shares its path, kind and qualified name with another
    /// declaration, so its ordinal-based ID proves nothing. Not traversed.
    UnprovenIdentity,
    /// Unresolved syntactic relations elsewhere name this symbol. They are
    /// candidates only: a name is not a dependency.
    UnresolvedReferences { count: usize, paths: Vec<String> },
    /// This entity has dependents, but the ontology does not prove that the
    /// change propagates through it, so traversal stopped here.
    UndeterminedPropagation,
    /// Propagation was provable but the depth bound was reached.
    DepthLimit,
    /// More dependents than the fan-out bound; the rest were not examined.
    FanoutLimit { dependents: usize },
    /// A relation endpoint is absent from the analyzed generation.
    EntityAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImpactBoundary {
    pub entity: GraphEntityId,
    pub path: String,
    pub qualified_name: String,
    #[serde(flatten)]
    pub reason: BoundaryReason,
}

/// Why an entity is a seed, and whether the change it describes is a contract
/// change (one whose consequences the ontology can follow past the first hop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "origin",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum ImpactOrigin {
    /// An observed entity change in a semantic delta.
    EntityChange {
        change: Change,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<EntityField>,
        identity: IdentityBasis,
    },
    /// An observed resolved-relation change; the relation's source entity is
    /// the seed, because its own behavior is what changed.
    RelationChange {
        change: Change,
        kind: RelationKind,
        identity: IdentityBasis,
    },
    /// An entity a planner may edit, or a symbol named on the command line. No
    /// edit has been observed, so it is analyzed as a contract change.
    ProposedChange,
}

impl ImpactOrigin {
    /// Whether the change can be followed past a direct dependent. A body-only
    /// edit changes behavior for its callers; that their own callers are
    /// affected is not provable, so it is reported as a boundary instead.
    fn contract(&self) -> bool {
        match self {
            Self::ProposedChange | Self::RelationChange { .. } => true,
            Self::EntityChange { change, fields, .. } => {
                *change != Change::Modified || fields.iter().any(|f| *f != EntityField::Text)
            }
        }
    }
    fn identity(&self) -> IdentityBasis {
        match self {
            Self::EntityChange { identity, .. } | Self::RelationChange { identity, .. } => {
                *identity
            }
            Self::ProposedChange => IdentityBasis::Unique,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactSeed {
    pub entity: GraphEntityId,
    pub kind: EntityKind,
    pub path: String,
    pub qualified_name: String,
    pub origin: ImpactOrigin,
    /// The seed exists in the compared-from generation only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// Traversal was refused because the seed's identity is unproven.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

/// What the report was computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "basis",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum ImpactBasis {
    /// The recorded difference between two ontology generations.
    ObservedDelta { from: String, to: String },
    /// Named entities analyzed as a prospective contract change.
    ProposedChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactLimits {
    /// Maximum evidence hops from a seed.
    pub depth: usize,
    pub seeds: usize,
    pub items: usize,
    pub tests: usize,
    pub boundaries: usize,
    /// Dependents examined per node.
    pub fanout: usize,
}

impl Default for ImpactLimits {
    fn default() -> Self {
        Self {
            depth: 2,
            seeds: 32,
            items: 64,
            tests: 12,
            boundaries: 32,
            fanout: 64,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactSummary {
    pub seeds: usize,
    /// Seeds not traversed because their identity is unproven.
    pub seeds_skipped: usize,
    pub seeds_omitted: usize,
    pub items: usize,
    pub items_omitted: usize,
    pub direct: usize,
    pub contract: usize,
    pub verification: usize,
    pub containment: usize,
    /// Distinct files named by the reported items.
    pub files: usize,
    pub boundaries: usize,
    pub boundaries_omitted: usize,
    pub max_distance: usize,
    /// Items whose file is not the file of any seed: what file-local reasoning
    /// would have missed.
    pub cross_file: usize,
}

/// A bounded, evidence-backed statement of what a change could affect. It is
/// derived, never stored: the ontology generation it names is its only binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactReport {
    pub version: ProtocolVersion,
    pub index_version: String,
    pub workspace_id: crate::local::repository::WorkspaceId,
    /// The generation every fact in this report was read from.
    pub generation: GraphGeneration,
    pub basis: ImpactBasis,
    pub limits: ImpactLimits,
    pub seeds: Vec<ImpactSeed>,
    pub items: Vec<ImpactItem>,
    pub boundaries: Vec<ImpactBoundary>,
    pub summary: ImpactSummary,
    pub meaning: String,
}

const MEANING: &str = "Evidence-backed impact candidates only: every item is reached by resolved relations, containment or graph test association from a changed entity. Absence of an item is not proof of no impact; open questions are listed as boundaries. This report grants no read or write authority.";

/// Impact never widens authority. The field exists so a consumer cannot read a
/// report as permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImpactAuthority {
    /// Advisory: scope changes remain a deliberate planner decision.
    AdvisoryOnly,
}

/// An impact report read against a declared scope (a planning request's intent
/// scope, or a plan's write scope). Used by planning to see consequences
/// outside its intended neighborhood, and by a reviewer to see what an observed
/// change touched that the plan never declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactOutlook {
    pub report: ImpactReport,
    pub scope: Vec<ScopePath>,
    /// Files of reported items that the declared scope does not cover.
    pub outside_scope: Vec<String>,
    pub outside_scope_items: usize,
    pub authority: ImpactAuthority,
}

impl ImpactOutlook {
    /// `covered` answers whether a repository-relative path lies in `scope`.
    /// An empty scope declares nothing, so nothing is reported as outside it.
    pub fn new(
        report: ImpactReport,
        scope: Vec<ScopePath>,
        covered: &dyn Fn(&str) -> bool,
    ) -> Self {
        let mut outside = BTreeSet::new();
        let mut items = 0;
        if !scope.is_empty() {
            for item in &report.items {
                if !covered(&item.path) {
                    outside.insert(item.path.clone());
                    items += 1;
                }
            }
        }
        Self {
            report,
            scope,
            outside_scope: outside.into_iter().collect(),
            outside_scope_items: items,
            authority: ImpactAuthority::AdvisoryOnly,
        }
    }
}

// ---------------------------------------------------------------------------
// Seeds.
// ---------------------------------------------------------------------------

/// A seed plus the dependents a removed entity had in the compared-from
/// generation, which are the only evidence a removal leaves behind.
struct SeedInput {
    seed: ImpactSeed,
    former: Vec<(GraphEntityId, RelationKind, Vec<ResolutionRule>)>,
}

/// Derives seeds from an observed delta: every changed entity, plus the source
/// of every changed resolved relation that is not already a changed entity.
fn delta_seeds(delta: &SemanticDelta) -> Vec<SeedInput> {
    let mut seeds: BTreeMap<GraphEntityId, SeedInput> = BTreeMap::new();
    for change in &delta.entities {
        let input = SeedInput {
            seed: ImpactSeed {
                entity: change.id.clone(),
                kind: change.kind,
                path: change.path.clone(),
                qualified_name: change.qualified_name.clone(),
                origin: ImpactOrigin::EntityChange {
                    change: change.change,
                    fields: change.fields.clone(),
                    identity: change.identity,
                },
                removed: change.change == Change::Removed,
                skipped: false,
            },
            former: vec![],
        };
        // An entity reported both removed and added (unproven identity) keeps
        // the removal, which is the conservative reading.
        match seeds.entry(change.id.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(input);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if input.seed.removed {
                    slot.get_mut().seed = input.seed;
                }
            }
        }
    }
    for relation in &delta.relations {
        // A relation change whose target is itself a changed entity is a
        // consequence of that change, not an independent one: seeding its
        // source would hide the caller inside the seed set instead of
        // reporting it as impact.
        if seeds.contains_key(&relation.target) {
            if relation.change == Change::Removed
                && let Some(target) = seeds.get_mut(&relation.target)
                && target.seed.removed
            {
                target.former.push((
                    relation.source.clone(),
                    relation.kind,
                    relation.rules.clone(),
                ));
            }
            continue;
        }
        seeds
            .entry(relation.source.clone())
            .or_insert_with(|| SeedInput {
                seed: ImpactSeed {
                    entity: relation.source.clone(),
                    kind: EntityKind::Other,
                    path: relation.source_path.clone(),
                    qualified_name: relation.source.as_str().into(),
                    origin: ImpactOrigin::RelationChange {
                        change: relation.change,
                        kind: relation.kind,
                        identity: relation.identity,
                    },
                    removed: false,
                    skipped: false,
                },
                former: vec![],
            });
    }
    for input in seeds.values_mut() {
        input.former.sort();
        input.former.dedup();
    }
    seeds.into_values().collect()
}

// ---------------------------------------------------------------------------
// Traversal.
// ---------------------------------------------------------------------------

/// A node the walk may continue from.
struct Reached {
    id: GraphEntityId,
    distance: usize,
    /// The ontology proves the change reaches this node's own contract, so its
    /// dependents may be examined.
    contract: bool,
    seed: GraphEntityId,
    evidence: Vec<ImpactStep>,
}

/// Whether a declaration is provably visible outside its own file. Unknown
/// visibility (every language but Rust today) is never read as exported.
fn exported(entity: &Entity) -> bool {
    entity
        .visibility
        .as_deref()
        .is_some_and(|v| v.starts_with("pub"))
}

fn check(limits: ImpactLimits) -> Result<()> {
    require(
        (1..=4).contains(&limits.depth)
            && (1..=200).contains(&limits.seeds)
            && (1..=500).contains(&limits.items)
            && limits.tests <= 100
            && limits.boundaries <= 200
            && (1..=512).contains(&limits.fanout),
        "impact limits: depth 1–4, seeds 1–200, items 1–500, tests 0–100, boundaries 0–200, fanout 1–512",
    )
}

struct Walk<'a> {
    query: &'a GraphQuery<'a>,
    limits: ImpactLimits,
    items: BTreeMap<GraphEntityId, ImpactItem>,
    visited: BTreeSet<GraphEntityId>,
    boundaries: BTreeSet<ImpactBoundary>,
    items_omitted: usize,
    boundaries_omitted: usize,
}

impl Walk<'_> {
    fn boundary(&mut self, entity: &GraphEntityId, path: &str, name: &str, reason: BoundaryReason) {
        let record = ImpactBoundary {
            entity: entity.clone(),
            path: path.into(),
            qualified_name: name.into(),
            reason,
        };
        if self.boundaries.contains(&record) {
            return;
        }
        if self.boundaries.len() >= self.limits.boundaries {
            self.boundaries_omitted += 1;
            return;
        }
        self.boundaries.insert(record);
    }

    /// Distinct resolved dependents of `id`, deterministically ordered.
    fn dependents(
        &mut self,
        id: &GraphEntityId,
        path: &str,
        name: &str,
    ) -> Result<Vec<(GraphEntityId, RelationKind, Vec<ResolutionRule>)>> {
        let edges = self
            .query
            .resolved_edges(id, true, self.limits.fanout + 1)?;
        let mut found: BTreeMap<(GraphEntityId, RelationKind), BTreeSet<ResolutionRule>> =
            BTreeMap::new();
        for edge in &edges {
            let entry = found.entry((edge.source.clone(), edge.kind)).or_default();
            if let Some(rule) = edge.resolution {
                entry.insert(rule);
            }
        }
        if edges.len() > self.limits.fanout {
            self.boundary(
                id,
                path,
                name,
                BoundaryReason::FanoutLimit {
                    dependents: edges.len(),
                },
            );
        }
        Ok(found
            .into_iter()
            .map(|((source, kind), rules)| (source, kind, rules.into_iter().collect()))
            .collect())
    }

    fn has_dependents(&self, id: &GraphEntityId) -> Result<bool> {
        Ok(!self.query.resolved_edges(id, true, 1)?.is_empty())
    }

    /// Records one reached entity and decides whether the walk continues
    /// through it. Returns the continuation node, if any.
    fn reach(
        &mut self,
        from: &Reached,
        dependent: (GraphEntityId, RelationKind, Vec<ResolutionRule>),
        removed: bool,
    ) -> Result<Option<Reached>> {
        let (id, kind, rules) = dependent;
        if self.visited.contains(&id) {
            return Ok(None);
        }
        let Some(entity) = self.query.entity(&id)? else {
            self.boundary(&id, "", id.as_str(), BoundaryReason::EntityAbsent);
            self.visited.insert(id);
            return Ok(None);
        };
        self.visited.insert(id.clone());
        if self.items.len() >= self.limits.items {
            self.items_omitted += 1;
            return Ok(None);
        }
        let distance = from.distance + 1;
        let mut evidence = from.evidence.clone();
        evidence.push(ImpactStep {
            from: from.id.clone(),
            entity: id.clone(),
            edge: ImpactEdge::Relation {
                kind,
                rules,
                removed,
            },
        });
        self.items.insert(
            id.clone(),
            ImpactItem {
                entity: id.clone(),
                kind: entity.kind,
                path: entity.provenance.path.clone(),
                qualified_name: entity.qualified_name.clone(),
                class: if entity.kind == EntityKind::Test {
                    ImpactClass::VerificationRelevance
                } else if distance == 1 {
                    ImpactClass::DirectDependency
                } else {
                    ImpactClass::ContractExposure
                },
                distance,
                seed: from.seed.clone(),
                evidence: evidence.clone(),
            },
        );
        // Past the first hop, only proven re-exposure continues: implementing
        // the changed declaration, or being an exported declaration whose own
        // contract the change reached. A resolved IMPORTS relation is a
        // direct dependency, not a re-exposure: syntax does not tell a
        // `pub use` re-export from a private import, so it stops here.
        let reexposes = matches!(kind, RelationKind::Implements);
        let carries =
            entity.kind != EntityKind::Test && (reexposes || (from.contract && exported(&entity)));
        let budget = distance < self.limits.depth;
        if carries && budget {
            return Ok(Some(Reached {
                id,
                distance,
                contract: from.contract,
                seed: from.seed.clone(),
                evidence,
            }));
        }
        if self.has_dependents(&id)? {
            self.boundary(
                &id,
                &entity.provenance.path,
                &entity.qualified_name,
                if carries {
                    BoundaryReason::DepthLimit
                } else {
                    BoundaryReason::UndeterminedPropagation
                },
            );
        }
        Ok(None)
    }
}

/// The analysis itself. `query` must be a snapshot of the generation `basis`
/// names; callers bind that before calling.
fn analyze(
    query: &GraphQuery<'_>,
    basis: ImpactBasis,
    mut inputs: Vec<SeedInput>,
    limits: ImpactLimits,
) -> Result<ImpactReport> {
    check(limits)?;
    let generation = query
        .generation()
        .cloned()
        .ok_or_else(|| Error::Invalid("workspace has no indexed generation".into()))?;
    inputs.sort_by(|a, b| a.seed.entity.cmp(&b.seed.entity));
    let seeds_omitted = inputs.len().saturating_sub(limits.seeds);
    inputs.truncate(limits.seeds);
    let mut walk = Walk {
        query,
        limits,
        items: BTreeMap::new(),
        visited: inputs.iter().map(|i| i.seed.entity.clone()).collect(),
        boundaries: BTreeSet::new(),
        items_omitted: 0,
        boundaries_omitted: 0,
    };
    let seed_paths: BTreeSet<String> = inputs.iter().map(|i| i.seed.path.clone()).collect();
    let mut seeds = vec![];
    let mut frontier = vec![];
    for input in inputs {
        let mut seed = input.seed;
        let node = Reached {
            id: seed.entity.clone(),
            distance: 0,
            contract: seed.origin.contract(),
            seed: seed.entity.clone(),
            evidence: vec![],
        };
        // A name is not a dependency: unresolved relations naming this symbol
        // are recorded as an open question, never as impact.
        let (count, paths) = query.unresolved_references(&short_name(&seed.qualified_name), 8)?;
        if count > 0 {
            walk.boundary(
                &seed.entity,
                &seed.path,
                &seed.qualified_name,
                BoundaryReason::UnresolvedReferences { count, paths },
            );
        }
        if seed.origin.identity() != IdentityBasis::Unique {
            seed.skipped = true;
            walk.boundary(
                &seed.entity,
                &seed.path,
                &seed.qualified_name,
                BoundaryReason::UnprovenIdentity,
            );
            seeds.push(seed);
            continue;
        }
        if seed.removed {
            // A removed entity is gone from this generation; the delta's removed
            // relations are the only evidence of what depended on it.
            for dependent in input.former.clone() {
                if let Some(next) = walk.reach(&node, dependent, true)? {
                    frontier.push(next);
                }
            }
        } else {
            for dependent in walk.dependents(&seed.entity, &seed.path, &seed.qualified_name)? {
                if let Some(next) = walk.reach(&node, dependent, false)? {
                    frontier.push(next);
                }
            }
        }
        // Adding or removing a declaration changes its container's composition.
        if matches!(
            seed.origin,
            ImpactOrigin::EntityChange {
                change: Change::Added | Change::Removed,
                ..
            }
        ) {
            container(query, &mut walk, &seed)?;
        }
        seeds.push(seed);
    }
    for _ in 1..limits.depth {
        let mut next = vec![];
        frontier.sort_by(|a: &Reached, b| a.id.cmp(&b.id));
        for node in &frontier {
            let (path, name) = walk
                .items
                .get(&node.id)
                .map(|i| (i.path.clone(), i.qualified_name.clone()))
                .unwrap_or_default();
            for dependent in walk.dependents(&node.id, &path, &name)? {
                if let Some(reached) = walk.reach(node, dependent, false)? {
                    next.push(reached);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    tests(query, &mut walk, &seeds)?;
    let mut items: Vec<ImpactItem> = walk.items.into_values().collect();
    items.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| a.class.cmp(&b.class))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.entity.cmp(&b.entity))
    });
    let mut summary = ImpactSummary {
        seeds: seeds.len(),
        seeds_skipped: seeds.iter().filter(|s| s.skipped).count(),
        seeds_omitted,
        items: items.len(),
        items_omitted: walk.items_omitted,
        boundaries: walk.boundaries.len(),
        boundaries_omitted: walk.boundaries_omitted,
        ..Default::default()
    };
    let mut files = BTreeSet::new();
    for item in &items {
        match item.class {
            ImpactClass::DirectDependency => summary.direct += 1,
            ImpactClass::ContractExposure => summary.contract += 1,
            ImpactClass::VerificationRelevance => summary.verification += 1,
            ImpactClass::ContainmentOwnership => summary.containment += 1,
        }
        summary.max_distance = summary.max_distance.max(item.distance);
        summary.cross_file += usize::from(!seed_paths.contains(&item.path));
        files.insert(item.path.clone());
    }
    summary.files = files.len();
    Ok(ImpactReport {
        version: ProtocolVersion::V1,
        index_version: INDEX_VERSION.into(),
        workspace_id: query.info.workspace_id.clone(),
        generation,
        basis,
        limits,
        seeds,
        items,
        boundaries: walk.boundaries.into_iter().collect(),
        summary,
        meaning: MEANING.into(),
    })
}

/// Last path segment of a qualified name. Shared with relation queries,
/// which report unresolved sites by the same short name.
pub(super) fn short_name(qualified: &str) -> String {
    qualified
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(qualified)
        .to_string()
}

/// Records the container of an added or removed declaration: its lexical parent
/// when the declaration is still indexed, otherwise the file that owns it.
fn container(query: &GraphQuery<'_>, walk: &mut Walk<'_>, seed: &ImpactSeed) -> Result<()> {
    let parent = match query.entity(&seed.entity)? {
        Some(entity) => match entity.parent {
            Some(parent) => query.entity(&parent)?,
            None => None,
        },
        None => query.file_entity(&seed.path)?,
    };
    let Some(parent) = parent else { return Ok(()) };
    if walk.visited.contains(&parent.id) {
        return Ok(());
    }
    if walk.items.len() >= walk.limits.items {
        walk.items_omitted += 1;
        return Ok(());
    }
    walk.visited.insert(parent.id.clone());
    walk.items.insert(
        parent.id.clone(),
        ImpactItem {
            entity: parent.id.clone(),
            kind: parent.kind,
            path: parent.provenance.path.clone(),
            qualified_name: parent.qualified_name.clone(),
            class: ImpactClass::ContainmentOwnership,
            distance: 1,
            seed: seed.entity.clone(),
            evidence: vec![ImpactStep {
                from: seed.entity.clone(),
                entity: parent.id.clone(),
                edge: ImpactEdge::Containment,
            }],
        },
    );
    Ok(())
}

/// Tests associated with the seeds, then with the entities already reported,
/// strongest association first and bounded as a whole.
fn tests(query: &GraphQuery<'_>, walk: &mut Walk<'_>, seeds: &[ImpactSeed]) -> Result<()> {
    if walk.limits.tests == 0 {
        return Ok(());
    }
    let mut targets: Vec<(GraphEntityId, GraphEntityId, Vec<ImpactStep>)> = vec![];
    for seed in seeds.iter().filter(|s| !s.skipped) {
        targets.push((seed.entity.clone(), seed.entity.clone(), vec![]));
    }
    let existing: Vec<ImpactItem> = walk.items.values().cloned().collect();
    for item in existing {
        targets.push((
            item.entity.clone(),
            item.seed.clone(),
            item.evidence.clone(),
        ));
    }
    let mut added = 0;
    for (target, seed, evidence) in targets {
        if added >= walk.limits.tests {
            break;
        }
        let Some(entity) = query.entity(&target)? else {
            continue;
        };
        for (test, basis) in query.associated_tests(&entity, walk.limits.tests)? {
            if added >= walk.limits.tests {
                break;
            }
            if walk.visited.contains(&test.id) {
                continue;
            }
            if walk.items.len() >= walk.limits.items {
                walk.items_omitted += 1;
                continue;
            }
            walk.visited.insert(test.id.clone());
            let mut chain = evidence.clone();
            chain.push(ImpactStep {
                from: target.clone(),
                entity: test.id.clone(),
                edge: ImpactEdge::TestAssociation { basis },
            });
            walk.items.insert(
                test.id.clone(),
                ImpactItem {
                    entity: test.id.clone(),
                    kind: test.kind,
                    path: test.provenance.path.clone(),
                    qualified_name: test.qualified_name.clone(),
                    class: ImpactClass::VerificationRelevance,
                    distance: chain.len(),
                    seed: seed.clone(),
                    evidence: chain,
                },
            );
            added += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Store surface.
// ---------------------------------------------------------------------------

/// What to analyze. Every variant binds the analysis to one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImpactRequest {
    /// The delta recorded when this generation was observed.
    Generation(String),
    /// A delta derived between two recorded generations.
    Diff { from: String, to: String },
    /// Prospective edits to named symbols in the indexed generation.
    Symbols(Vec<String>),
}

impl Store {
    /// Impact of an observed or proposed change, read from the indexed
    /// generation. A delta is analyzable only while the generation it describes
    /// is the one the graph tables materialize, so every evidence chain is read
    /// from the same facts the delta was derived from.
    pub fn ontology_impact(
        &self,
        start: &Path,
        request: &ImpactRequest,
        limits: ImpactLimits,
    ) -> Result<ImpactReport> {
        check(limits)?;
        // The delta is resolved before the read snapshot opens: deriving one
        // between two named generations needs a transaction of its own.
        let delta = match request {
            ImpactRequest::Symbols(_) => None,
            ImpactRequest::Generation(id) => Some(self.ontology_delta(start, id)?),
            ImpactRequest::Diff { from, to } => Some(self.ontology_diff(start, from, to)?),
        };
        let query = self.graph(start)?;
        let live = query
            .generation()
            .cloned()
            .ok_or_else(|| Error::Invalid("workspace has no indexed generation".into()))?;
        let (basis, seeds) = match request {
            ImpactRequest::Symbols(names) => {
                require(!names.is_empty(), "impact needs at least one symbol")?;
                let mut seeds = vec![];
                for name in names {
                    let mut found = query.exact_matches(name, 2)?;
                    require(
                        found.len() == 1,
                        format!(
                            "symbol {name} is absent or ambiguous; name a qualified name or graph ID"
                        ),
                    )?;
                    let entity = found.remove(0);
                    seeds.push(SeedInput {
                        seed: ImpactSeed {
                            entity: entity.id,
                            kind: entity.kind,
                            path: entity.provenance.path,
                            qualified_name: entity.qualified_name,
                            origin: ImpactOrigin::ProposedChange,
                            removed: false,
                            skipped: false,
                        },
                        former: vec![],
                    });
                }
                (ImpactBasis::ProposedChange, seeds)
            }
            _ => {
                let delta = delta.expect("a delta for every non-symbol request");
                require(
                    delta.to.generation == live && delta.index_version == INDEX_VERSION,
                    format!(
                        "impact analysis reads the generation the delta describes; {} is not the indexed generation (sequence {}). Run agentctl repo index or analyze the indexed generation.",
                        delta.to.generation_id, live.sequence
                    ),
                )?;
                (
                    ImpactBasis::ObservedDelta {
                        from: delta.from.generation_id.clone(),
                        to: delta.to.generation_id.clone(),
                    },
                    delta_seeds(&delta),
                )
            }
        };
        analyze(&query, basis, seeds, limits)
    }

    /// The same report read against a plan's declared write scope, so a
    /// reviewer can see which observed consequences the plan never declared.
    /// Advisory: it decides nothing and grants nothing.
    pub fn plan_impact(
        &self,
        start: &Path,
        plan: &crate::protocol::PlanId,
        request: &ImpactRequest,
        limits: ImpactLimits,
    ) -> Result<ImpactOutlook> {
        let view = self.execution_plan(start, plan)?;
        let mut scope: Vec<ScopePath> = view
            .plan
            .packet
            .tasks
            .iter()
            .flat_map(|t| t.write_scope.iter().cloned())
            .collect();
        scope.sort_by(|a, b| a.path().cmp(b.path()));
        scope.dedup();
        let report = self.ontology_impact(start, request, limits)?;
        let covered = scope.clone();
        Ok(ImpactOutlook::new(report, scope, &|path| {
            covered
                .iter()
                .any(|s| crate::local::planning::covers(s, path))
        }))
    }
}

/// Seeds for a planner's prospective edit to entities it already selected.
fn proposed_seeds(entities: &[Entity]) -> Vec<SeedInput> {
    entities
        .iter()
        .map(|entity| SeedInput {
            seed: ImpactSeed {
                entity: entity.id.clone(),
                kind: entity.kind,
                path: entity.provenance.path.clone(),
                qualified_name: entity.qualified_name.clone(),
                origin: ImpactOrigin::ProposedChange,
                removed: false,
                skipped: false,
            },
            former: vec![],
        })
        .collect()
}

/// Planning entry point: the prospective impact of editing `entities`, read
/// from the accepted generation `query` snapshots.
pub(crate) fn proposed(
    query: &GraphQuery<'_>,
    entities: &[Entity],
    limits: ImpactLimits,
) -> Result<ImpactReport> {
    analyze(
        query,
        ImpactBasis::ProposedChange,
        proposed_seeds(entities),
        limits,
    )
}

#[cfg(test)]
mod tests {
    //! Seeding is a pure function of a delta, so its rule table is tested
    //! directly: a repository fixture cannot reach every combination.
    use super::*;

    fn id(name: &str) -> GraphEntityId {
        GraphEntityId::new(format!("graph:{name}")).unwrap()
    }

    fn entity(change: Change, fields: Vec<EntityField>, name: &str) -> EntityChange {
        EntityChange {
            change,
            identity: IdentityBasis::Unique,
            id: id(name),
            kind: EntityKind::Function,
            path: format!("src/{name}.rs"),
            qualified_name: name.into(),
            fields,
            before: None,
            after: None,
        }
    }

    fn relation(change: Change, source: &str, target: &str) -> RelationChange {
        RelationChange {
            change,
            identity: IdentityBasis::Unique,
            source: id(source),
            kind: RelationKind::Calls,
            target: id(target),
            source_path: format!("src/{source}.rs"),
            target_path: format!("src/{target}.rs"),
            rules: vec![ResolutionRule::QualifiedPath],
        }
    }

    fn delta(entities: Vec<EntityChange>, relations: Vec<RelationChange>) -> SemanticDelta {
        let point = |name: &str| GenerationPoint {
            generation_id: name.into(),
            generation: GraphGeneration {
                sequence: 1,
                fingerprint: "blake3:0".into(),
            },
            snapshot: FactsRef {
                hash: "blake3:0".into(),
                bytes: 0,
            },
        };
        SemanticDelta {
            version: ProtocolVersion::V1,
            workspace_id: crate::local::repository::WorkspaceId::for_git_directory(Path::new("/w")),
            index_version: INDEX_VERSION.into(),
            from: point("a"),
            to: point("b"),
            summary: DeltaSummary::default(),
            files: vec![],
            entities,
            relations,
        }
    }

    #[test]
    fn a_body_edit_is_not_a_contract_change_but_every_other_entity_change_is() {
        let text = ImpactOrigin::EntityChange {
            change: Change::Modified,
            fields: vec![EntityField::Text],
            identity: IdentityBasis::Unique,
        };
        assert!(!text.contract());
        for fields in [
            vec![EntityField::Signature],
            vec![EntityField::Visibility],
            vec![EntityField::Key],
            vec![EntityField::Text, EntityField::Signature],
        ] {
            assert!(
                ImpactOrigin::EntityChange {
                    change: Change::Modified,
                    fields,
                    identity: IdentityBasis::Unique,
                }
                .contract()
            );
        }
        for change in [Change::Added, Change::Removed] {
            assert!(
                ImpactOrigin::EntityChange {
                    change,
                    fields: vec![],
                    identity: IdentityBasis::Unique,
                }
                .contract()
            );
        }
        assert!(ImpactOrigin::ProposedChange.contract());
    }

    #[test]
    fn a_removed_entitys_former_dependents_become_its_only_traversal_evidence() {
        let seeds = delta_seeds(&delta(
            vec![entity(Change::Removed, vec![], "gone")],
            vec![relation(Change::Removed, "caller", "gone")],
        ));
        assert_eq!(seeds.len(), 1, "the caller is impact, not a change");
        assert!(seeds[0].seed.removed);
        assert_eq!(
            seeds[0].former,
            vec![(
                id("caller"),
                RelationKind::Calls,
                vec![ResolutionRule::QualifiedPath]
            )]
        );
    }

    #[test]
    fn a_relation_change_explained_by_a_changed_endpoint_creates_no_extra_seed() {
        let seeds = delta_seeds(&delta(
            vec![entity(Change::Added, vec![], "fresh")],
            vec![relation(Change::Added, "caller", "fresh")],
        ));
        assert_eq!(
            seeds.iter().map(|s| &s.seed.entity).collect::<Vec<_>>(),
            vec![&id("fresh")]
        );
    }

    #[test]
    fn a_relation_change_with_two_unchanged_endpoints_seeds_its_source() {
        // The ontology says a fact about `caller` changed; nothing else in the
        // delta explains it, so `caller` is the changed thing.
        let seeds = delta_seeds(&delta(
            vec![],
            vec![relation(Change::Removed, "caller", "callee")],
        ));
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].seed.entity, id("caller"));
        assert!(matches!(
            seeds[0].seed.origin,
            ImpactOrigin::RelationChange {
                change: Change::Removed,
                ..
            }
        ));
        assert!(seeds[0].seed.origin.contract());
    }

    #[test]
    fn an_entity_change_always_wins_over_a_relation_change_for_the_same_entity() {
        let seeds = delta_seeds(&delta(
            vec![entity(Change::Modified, vec![EntityField::Text], "caller")],
            vec![relation(Change::Added, "caller", "callee")],
        ));
        assert_eq!(seeds.len(), 1);
        assert!(matches!(
            seeds[0].seed.origin,
            ImpactOrigin::EntityChange { .. }
        ));
        assert!(
            !seeds[0].seed.origin.contract(),
            "a body edit stays a body edit"
        );
    }
}
