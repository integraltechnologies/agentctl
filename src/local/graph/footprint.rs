//! Structural footprint: a bounded, derived view of permanent structural change.
//!
//! The report is a projection of a [`SemanticDelta`]. It does not scan
//! source, persist metrics, infer intent, or score quality. Every fact retains
//! the entity/file/relation identity that caused it, and review signals merely
//! select those same facts for inspection. Unsupported claims (configuration,
//! persistence, semantic duplication, and non-Rust export visibility) are not
//! guessed.
use super::*;
use crate::{
    local::planning::PlanState,
    protocol::{
        EvidenceRef, GraphEntityId, PlanId, ProtocolVersion, ScopePath, VerificationDecision,
        VerificationId,
    },
};
use serde::{Deserialize, Serialize};

const MEANING: &str = "Derived structural facts and review signals, not a quality score or rejection policy. Test classification uses observed test kinds and indexed test-path conventions; public surface is known only where the index records visibility (currently Rust). Configuration, persistence/schema machinery, semantic duplication, unresolved imports, and verification exercise are not inferred. This report grants no context or filesystem authority.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FootprintLimits {
    pub files: usize,
    pub entities: usize,
    pub relations: usize,
    pub signals: usize,
    pub evidence_per_signal: usize,
}

impl Default for FootprintLimits {
    fn default() -> Self {
        Self {
            files: 100,
            entities: 200,
            relations: 200,
            signals: 12,
            evidence_per_signal: 16,
        }
    }
}

fn check(limits: FootprintLimits) -> Result<()> {
    require(
        (1..=500).contains(&limits.files)
            && (1..=1000).contains(&limits.entities)
            && (1..=1000).contains(&limits.relations)
            && (1..=32).contains(&limits.signals)
            && (1..=64).contains(&limits.evidence_per_signal),
        "footprint limits: files 1–500, entities 1–1000, relations 1–1000, signals 1–32, evidence-per-signal 1–64",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FootprintRequest {
    Generation(String),
    Diff { from: String, to: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StructuralRole {
    Production,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StructuralRoleBasis {
    ObservedTestEntity,
    Stage1PathConvention,
    NoTestIndicator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Surface {
    Public,
    Restricted,
    Private,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralFile {
    pub path: String,
    pub change: ContentChange,
    pub role: StructuralRole,
    pub role_basis: StructuralRoleBasis,
    pub entity_changes: usize,
    pub relation_changes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralEntity {
    pub change: Change,
    pub identity: IdentityBasis,
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub path: String,
    pub qualified_name: String,
    pub fields: Vec<EntityField>,
    pub role: StructuralRole,
    pub role_basis: StructuralRoleBasis,
    pub before_surface: Option<Surface>,
    pub after_surface: Option<Surface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralRelation {
    pub change: Change,
    pub identity: IdentityBasis,
    pub source: GraphEntityId,
    pub kind: RelationKind,
    pub target: GraphEntityId,
    pub source_path: String,
    pub target_path: String,
    pub rules: Vec<ResolutionRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FootprintSummary {
    pub files_added: usize,
    pub files_removed: usize,
    pub files_modified: usize,
    pub production_files_added: usize,
    pub production_files_removed: usize,
    pub production_files_touched: usize,
    pub test_files_added: usize,
    pub test_files_removed: usize,
    pub production_entities_added: usize,
    pub production_entities_removed: usize,
    pub production_entities_modified: usize,
    pub test_entities_added: usize,
    pub test_entities_removed: usize,
    pub test_entities_modified: usize,
    pub public_surface_added: usize,
    pub public_surface_removed: usize,
    pub public_surface_expanded: usize,
    pub surface_unknown: usize,
    pub relations_added: usize,
    pub relations_removed: usize,
    pub relation_sources_expanded: usize,
    pub max_added_relations_per_source: usize,
    pub added_declaration_files: usize,
    pub max_added_declarations_in_file: usize,
    pub unproven_identity: usize,
    pub files_reported: usize,
    pub files_omitted: usize,
    pub entities_reported: usize,
    pub entities_omitted: usize,
    pub relations_reported: usize,
    pub relations_omitted: usize,
    pub signals_reported: usize,
    pub signals_omitted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewSignalKind {
    PublicSurfaceGrowth,
    AbstractionSurfaceGrowth,
    FragmentedProductionGrowth,
    UndeclaredStructuralGrowth,
    VerificationEvidencePending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "basis",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum ReviewEvidence {
    File {
        fact: StructuralFile,
    },
    Entity {
        fact: StructuralEntity,
    },
    Relation {
        fact: StructuralRelation,
    },
    OutsideWriteScope {
        path: String,
    },
    Verification {
        plan_id: PlanId,
        state: PlanState,
        verified_tasks: usize,
        total_tasks: usize,
        integration_verification: Option<VerificationId>,
        evidence: Vec<EvidenceRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSignal {
    pub kind: ReviewSignalKind,
    pub meaning: String,
    pub evidence: Vec<ReviewEvidence>,
    pub evidence_omitted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralFootprint {
    pub version: ProtocolVersion,
    pub index_version: String,
    pub workspace_id: crate::local::repository::WorkspaceId,
    pub from: GenerationPoint,
    pub to: GenerationPoint,
    pub limits: FootprintLimits,
    pub summary: FootprintSummary,
    pub files: Vec<StructuralFile>,
    pub entities: Vec<StructuralEntity>,
    pub relations: Vec<StructuralRelation>,
    pub signals: Vec<ReviewSignal>,
    pub meaning: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationLink {
    pub plan_id: PlanId,
    pub state: PlanState,
    pub verified_tasks: usize,
    pub total_tasks: usize,
    pub integration_verification: Option<VerificationId>,
    pub integration_decision: Option<VerificationDecision>,
    pub evidence: Vec<EvidenceRef>,
    pub meaning: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FootprintOutlook {
    pub report: StructuralFootprint,
    pub scope: Vec<ScopePath>,
    pub outside_scope: Vec<String>,
    pub outside_scope_omitted: usize,
    pub verification: VerificationLink,
    pub authority: ImpactAuthority,
}

fn role(kind: EntityKind, qualified: &str, path: &str) -> (StructuralRole, StructuralRoleBasis) {
    if kind == EntityKind::Test {
        (
            StructuralRole::Test,
            StructuralRoleBasis::ObservedTestEntity,
        )
    } else if super::query::is_test_side(kind, qualified, path) {
        (
            StructuralRole::Test,
            StructuralRoleBasis::Stage1PathConvention,
        )
    } else {
        (
            StructuralRole::Production,
            StructuralRoleBasis::NoTestIndicator,
        )
    }
}

fn module_name(path: &str) -> String {
    path.rsplit_once('.')
        .map_or(path, |(stem, _)| stem)
        .replace('/', "::")
}

fn declaration(entity: &StructuralEntity) -> bool {
    entity.kind != EntityKind::File
        && !(entity.kind == EntityKind::Module
            && entity.qualified_name == module_name(&entity.path))
}

fn structural_growth(entity: &StructuralEntity) -> bool {
    declaration(entity)
        && entity.role == StructuralRole::Production
        && entity.identity == IdentityBasis::Unique
        && (entity.change == Change::Added
            || (entity.change == Change::Modified
                && entity.before_surface != Some(Surface::Public)
                && entity.after_surface == Some(Surface::Public)))
}

fn surface(facts: Option<&EntityFacts>) -> Option<Surface> {
    facts.map(
        |f| match (Language::for_path(&f.path), f.visibility.as_deref()) {
            (Some(Language::Rust), Some("pub")) => Surface::Public,
            (Some(Language::Rust), Some(_)) => Surface::Restricted,
            (Some(Language::Rust), None) => Surface::Private,
            _ => Surface::Unknown,
        },
    )
}

fn file_fact(change: &FileChange) -> StructuralFile {
    let (role, role_basis) = role(EntityKind::Other, "", &change.path);
    StructuralFile {
        path: change.path.clone(),
        change: change.content,
        role,
        role_basis,
        entity_changes: change.entities,
        relation_changes: change.relations,
    }
}

fn entity_fact(change: &EntityChange) -> StructuralEntity {
    let (role, role_basis) = role(change.kind, &change.qualified_name, &change.path);
    StructuralEntity {
        change: change.change,
        identity: change.identity,
        id: change.id.clone(),
        kind: change.kind,
        path: change.path.clone(),
        qualified_name: change.qualified_name.clone(),
        fields: change.fields.clone(),
        role,
        role_basis,
        before_surface: surface(change.before.as_ref()),
        after_surface: surface(change.after.as_ref()),
    }
}

fn relation_fact(change: &RelationChange) -> StructuralRelation {
    StructuralRelation {
        change: change.change,
        identity: change.identity,
        source: change.source.clone(),
        kind: change.kind,
        target: change.target.clone(),
        source_path: change.source_path.clone(),
        target_path: change.target_path.clone(),
        rules: change.rules.clone(),
    }
}

fn signal(
    kind: ReviewSignalKind,
    meaning: &str,
    mut evidence: Vec<ReviewEvidence>,
    limit: usize,
) -> ReviewSignal {
    let evidence_omitted = evidence.len().saturating_sub(limit);
    evidence.truncate(limit);
    ReviewSignal {
        kind,
        meaning: meaning.into(),
        evidence,
        evidence_omitted,
    }
}

fn derive(delta: &SemanticDelta, limits: FootprintLimits) -> Result<StructuralFootprint> {
    check(limits)?;
    let all_files: Vec<_> = delta.files.iter().map(file_fact).collect();
    let all_entities: Vec<_> = delta.entities.iter().map(entity_fact).collect();
    let all_relations: Vec<_> = delta.relations.iter().map(relation_fact).collect();
    let declarations: Vec<_> = all_entities.iter().filter(|e| declaration(e)).collect();
    let production: Vec<_> = declarations
        .iter()
        .copied()
        .filter(|e| e.role == StructuralRole::Production)
        .collect();
    let tests: Vec<_> = declarations
        .iter()
        .copied()
        .filter(|e| e.role == StructuralRole::Test)
        .collect();
    let count =
        |items: &[&StructuralEntity], change| items.iter().filter(|e| e.change == change).count();
    let public_added: Vec<_> = production
        .iter()
        .copied()
        .filter(|e| {
            e.change == Change::Added
                && e.identity == IdentityBasis::Unique
                && e.after_surface == Some(Surface::Public)
        })
        .collect();
    let public_removed = production
        .iter()
        .filter(|e| {
            e.change == Change::Removed
                && e.identity == IdentityBasis::Unique
                && e.before_surface == Some(Surface::Public)
        })
        .count();
    let public_expanded: Vec<_> = production
        .iter()
        .copied()
        .filter(|e| {
            e.change == Change::Modified
                && e.identity == IdentityBasis::Unique
                && e.before_surface != Some(Surface::Public)
                && e.after_surface == Some(Surface::Public)
        })
        .collect();
    let abstractions: Vec<_> = production
        .iter()
        .copied()
        .filter(|e| {
            e.change == Change::Added
                && e.identity == IdentityBasis::Unique
                && matches!(
                    e.kind,
                    EntityKind::Module | EntityKind::Type | EntityKind::Enum | EntityKind::Trait
                )
        })
        .collect();
    let mut added_by_path = BTreeMap::<String, usize>::new();
    for entity in production.iter().filter(|e| e.change == Change::Added) {
        *added_by_path.entry(entity.path.clone()).or_default() += 1;
    }
    let added_production_files: Vec<_> = all_files
        .iter()
        .filter(|f| f.role == StructuralRole::Production && f.change == ContentChange::Added)
        .collect();
    let mut relation_fanout = BTreeMap::<GraphEntityId, usize>::new();
    for relation in all_relations.iter().filter(|r| r.change == Change::Added) {
        *relation_fanout.entry(relation.source.clone()).or_default() += 1;
    }
    let mut signals = vec![];
    let mut public = public_added;
    public.extend(public_expanded.iter().copied());
    if !public.is_empty() {
        signals.push(signal(
            ReviewSignalKind::PublicSurfaceGrowth,
            "Observed Rust visibility added or expanded public surface; review whether the capability needs this contract.",
            public.into_iter().cloned().map(|fact| ReviewEvidence::Entity { fact }).collect(),
            limits.evidence_per_signal,
        ));
    }
    if !abstractions.is_empty() {
        signals.push(signal(
            ReviewSignalKind::AbstractionSurfaceGrowth,
            "Observed production module/type/enum/trait additions; this is concrete abstraction surface, not a judgment that it is unnecessary.",
            abstractions.into_iter().cloned().map(|fact| ReviewEvidence::Entity { fact }).collect(),
            limits.evidence_per_signal,
        ));
    }
    if added_production_files.len() > 1 {
        signals.push(signal(
            ReviewSignalKind::FragmentedProductionGrowth,
            "Production growth added more than one file; review whether distributing the new structure is justified.",
            added_production_files
                .iter()
                .map(|fact| ReviewEvidence::File {
                    fact: (*fact).clone(),
                })
                .collect(),
            limits.evidence_per_signal,
        ));
    }
    let mut summary = FootprintSummary {
        files_added: all_files
            .iter()
            .filter(|f| f.change == ContentChange::Added)
            .count(),
        files_removed: all_files
            .iter()
            .filter(|f| f.change == ContentChange::Removed)
            .count(),
        files_modified: all_files
            .iter()
            .filter(|f| f.change == ContentChange::Modified)
            .count(),
        production_files_added: added_production_files.len(),
        production_files_removed: all_files
            .iter()
            .filter(|f| f.role == StructuralRole::Production && f.change == ContentChange::Removed)
            .count(),
        production_files_touched: all_files
            .iter()
            .filter(|f| f.role == StructuralRole::Production)
            .count(),
        test_files_added: all_files
            .iter()
            .filter(|f| f.role == StructuralRole::Test && f.change == ContentChange::Added)
            .count(),
        test_files_removed: all_files
            .iter()
            .filter(|f| f.role == StructuralRole::Test && f.change == ContentChange::Removed)
            .count(),
        production_entities_added: count(&production, Change::Added),
        production_entities_removed: count(&production, Change::Removed),
        production_entities_modified: count(&production, Change::Modified),
        test_entities_added: count(&tests, Change::Added),
        test_entities_removed: count(&tests, Change::Removed),
        test_entities_modified: count(&tests, Change::Modified),
        public_surface_added: production
            .iter()
            .filter(|e| {
                e.change == Change::Added
                    && e.identity == IdentityBasis::Unique
                    && e.after_surface == Some(Surface::Public)
            })
            .count(),
        public_surface_removed: public_removed,
        public_surface_expanded: public_expanded.len(),
        surface_unknown: production
            .iter()
            .filter(|e| {
                matches!(e.change, Change::Added | Change::Modified)
                    && e.after_surface == Some(Surface::Unknown)
            })
            .count(),
        relations_added: all_relations
            .iter()
            .filter(|r| r.change == Change::Added)
            .count(),
        relations_removed: all_relations
            .iter()
            .filter(|r| r.change == Change::Removed)
            .count(),
        relation_sources_expanded: relation_fanout.len(),
        max_added_relations_per_source: relation_fanout.values().copied().max().unwrap_or(0),
        added_declaration_files: added_by_path.len(),
        max_added_declarations_in_file: added_by_path.values().copied().max().unwrap_or(0),
        unproven_identity: delta.summary.unproven_identity,
        files_reported: all_files.len().min(limits.files),
        files_omitted: all_files.len().saturating_sub(limits.files),
        entities_reported: all_entities.len().min(limits.entities),
        entities_omitted: all_entities.len().saturating_sub(limits.entities),
        relations_reported: all_relations.len().min(limits.relations),
        relations_omitted: all_relations.len().saturating_sub(limits.relations),
        signals_reported: signals.len().min(limits.signals),
        signals_omitted: signals.len().saturating_sub(limits.signals),
    };
    let mut files = all_files;
    let mut entities = all_entities;
    let mut relations = all_relations;
    files.truncate(limits.files);
    entities.truncate(limits.entities);
    relations.truncate(limits.relations);
    signals.truncate(limits.signals);
    summary.signals_reported = signals.len();
    Ok(StructuralFootprint {
        version: ProtocolVersion::V1,
        index_version: delta.index_version.clone(),
        workspace_id: delta.workspace_id.clone(),
        from: delta.from.clone(),
        to: delta.to.clone(),
        limits,
        summary,
        files,
        entities,
        relations,
        signals,
        meaning: MEANING.into(),
    })
}

fn requested_delta(
    store: &Store,
    start: &Path,
    request: &FootprintRequest,
) -> Result<SemanticDelta> {
    let delta = match request {
        FootprintRequest::Generation(id) => store.ontology_delta(start, id)?,
        FootprintRequest::Diff { from, to } => store.ontology_diff(start, from, to)?,
    };
    let live = store
        .graph(start)?
        .generation()
        .cloned()
        .ok_or_else(|| Error::Invalid("workspace has no indexed generation".into()))?;
    require(
        delta.to.generation == live && delta.index_version == INDEX_VERSION,
        format!(
            "structural footprint reads the generation the delta describes; {} is not the indexed generation (sequence {}). Run agentctl repo index or analyze the indexed generation.",
            delta.to.generation_id, live.sequence
        ),
    )?;
    Ok(delta)
}

impl Store {
    /// Derives structural footprint from the exact delta named by `request`.
    /// The compared-to generation must still be materialized by the graph, so
    /// an old report can never be silently applied to a newer candidate.
    pub fn ontology_footprint(
        &self,
        start: &Path,
        request: &FootprintRequest,
        limits: FootprintLimits,
    ) -> Result<StructuralFootprint> {
        check(limits)?;
        let delta = requested_delta(self, start, request)?;
        derive(&delta, limits)
    }

    /// Adds plan scope and verification linkage without mutating either. The
    /// proof association is plan-level only; it does not claim per-entity test
    /// coverage.
    pub fn plan_footprint(
        &self,
        start: &Path,
        plan_id: &PlanId,
        request: &FootprintRequest,
        limits: FootprintLimits,
    ) -> Result<FootprintOutlook> {
        let view = self.execution_plan(start, plan_id)?;
        let tasks = self.execution_tasks(start, plan_id)?;
        let mut scope: Vec<ScopePath> = view
            .plan
            .packet
            .tasks
            .iter()
            .flat_map(|t| t.write_scope.iter().cloned())
            .collect();
        scope.sort_by(|a, b| a.path().cmp(b.path()));
        scope.dedup();
        let delta = requested_delta(self, start, request)?;
        let candidate = self
            .candidate_for_plan(start, plan_id)?
            .ok_or_else(|| {
                Error::Invalid(
                    "SOURCE_DRIFT: plan-linked footprint requires the live candidate this plan's runtime produced"
                        .into(),
                )
            })?;
        require(
            delta.to.generation_id == candidate.generation_id
                && candidate.base.as_ref() == Some(&delta.from.generation_id),
            "SOURCE_DRIFT: plan-linked footprint must analyze this plan candidate's own accepted-base delta",
        )?;
        let mut report = derive(&delta, limits)?;
        // Scope review reads the complete delta rather than the bounded
        // presentation, so lowering output limits cannot hide an addition.
        let growth_paths: BTreeSet<String> = delta
            .files
            .iter()
            .map(file_fact)
            .filter(|fact| {
                fact.role == StructuralRole::Production && fact.change == ContentChange::Added
            })
            .map(|fact| fact.path)
            .chain(
                delta
                    .entities
                    .iter()
                    .map(entity_fact)
                    .filter(structural_growth)
                    .map(|fact| fact.path),
            )
            .chain(
                delta
                    .relations
                    .iter()
                    .filter(|relation| relation.change == Change::Added)
                    .filter(|relation| relation.identity == IdentityBasis::Unique)
                    .filter(|relation| {
                        role(EntityKind::Other, "", &relation.source_path).0
                            == StructuralRole::Production
                    })
                    .map(|relation| relation.source_path.clone()),
            )
            .collect();
        let all_outside_scope: Vec<String> = growth_paths
            .into_iter()
            .filter(|path| {
                !scope
                    .iter()
                    .any(|s| crate::local::planning::covers(s, path))
            })
            .collect();
        let proof = view.integration_proof.as_ref();
        let outside_scope_omitted = all_outside_scope.len().saturating_sub(limits.files);
        let outside_scope: Vec<String> = all_outside_scope
            .iter()
            .take(limits.files)
            .cloned()
            .collect();
        let verification = VerificationLink {
            plan_id: plan_id.clone(),
            state: view.state,
            verified_tasks: tasks
                .iter()
                .filter(|t| t.state == crate::protocol::TaskState::Verified)
                .count(),
            total_tasks: tasks.len(),
            integration_verification: proof.map(|p| p.verification_id.clone()),
            integration_decision: proof.map(|p| p.decision),
            evidence: proof.map_or_else(Vec::new, |p| p.evidence.clone()),
            meaning: "Plan-level association only; evidence is bound to the plan's final source, not proof that an individual structural entity was exercised.".into(),
        };
        if !all_outside_scope.is_empty() {
            report.signals.push(signal(
                ReviewSignalKind::UndeclaredStructuralGrowth,
                "Observed production growth lies outside every declared task write scope; the report does not widen that scope.",
                all_outside_scope
                    .iter()
                    .cloned()
                    .map(|path| ReviewEvidence::OutsideWriteScope { path })
                    .collect(),
                limits.evidence_per_signal,
            ));
        }
        let production_relations_added = delta
            .relations
            .iter()
            .filter(|relation| {
                relation.change == Change::Added
                    && relation.identity == IdentityBasis::Unique
                    && role(EntityKind::Other, "", &relation.source_path).0
                        == StructuralRole::Production
            })
            .count();
        let production_entities_grown = delta
            .entities
            .iter()
            .map(entity_fact)
            .filter(structural_growth)
            .count();
        let growth = report.summary.production_files_added
            + production_entities_grown
            + production_relations_added;
        if growth > 0 && proof.is_none() {
            let mut evidence = vec![ReviewEvidence::Verification {
                plan_id: plan_id.clone(),
                state: verification.state,
                verified_tasks: verification.verified_tasks,
                total_tasks: verification.total_tasks,
                integration_verification: None,
                evidence: vec![],
            }];
            evidence.extend(
                delta
                    .files
                    .iter()
                    .map(file_fact)
                    .filter(|fact| {
                        fact.role == StructuralRole::Production
                            && fact.change == ContentChange::Added
                    })
                    .map(|fact| ReviewEvidence::File { fact }),
            );
            evidence.extend(
                delta
                    .entities
                    .iter()
                    .map(entity_fact)
                    .filter(structural_growth)
                    .map(|fact| ReviewEvidence::Entity { fact }),
            );
            evidence.extend(
                delta
                    .relations
                    .iter()
                    .filter(|relation| relation.change == Change::Added)
                    .filter(|relation| relation.identity == IdentityBasis::Unique)
                    .filter(|relation| {
                        role(EntityKind::Other, "", &relation.source_path).0
                            == StructuralRole::Production
                    })
                    .map(relation_fact)
                    .map(|fact| ReviewEvidence::Relation { fact }),
            );
            report.signals.push(signal(
                ReviewSignalKind::VerificationEvidencePending,
                "Observed production growth has no accepted integration proof yet; this is expected during verification and requires inspection, not automatic rejection.",
                evidence,
                limits.evidence_per_signal,
            ));
        }
        if report.signals.len() > limits.signals {
            report.summary.signals_omitted += report.signals.len() - limits.signals;
            report.signals.truncate(limits.signals);
        }
        report.summary.signals_reported = report.signals.len();
        Ok(FootprintOutlook {
            report,
            scope,
            outside_scope,
            outside_scope_omitted,
            verification,
            authority: ImpactAuthority::AdvisoryOnly,
        })
    }
}
