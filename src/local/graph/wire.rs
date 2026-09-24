//! Normalized wire form of [`ContextPacket`]. Full provenance (repository,
//! workspace, content hash, language, backend) is identical for every record from
//! one file, so it is written once per file in a source table and each record
//! names only its path. Reads accept this form and the legacy form in which every
//! record carried its own provenance; either way records are rehydrated with
//! their full provenance, so in-memory consumers are unchanged.
use super::*;
use crate::{
    local::repository::{RepositoryId, WorkspaceId},
    protocol::GraphEntityId,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use std::{collections::BTreeMap, result::Result};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceTable {
    repository_id: RepositoryId,
    workspace_id: WorkspaceId,
    files: Vec<SourceFile>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    path: String,
    content_hash: String,
    language: Language,
    backend: String,
}

fn table<'a>(
    provenances: impl Iterator<Item = &'a Provenance>,
) -> Result<Option<SourceTable>, String> {
    let mut owner: Option<(&RepositoryId, &WorkspaceId)> = None;
    let mut files: BTreeMap<&str, &Provenance> = BTreeMap::new();
    for p in provenances {
        match owner {
            None => owner = Some((&p.repository_id, &p.workspace_id)),
            Some((r, w)) if r != &p.repository_id || w != &p.workspace_id => {
                return Err("context records span repositories/workspaces".into());
            }
            Some(_) => {}
        }
        if files.insert(&p.path, p).is_some_and(|prior| prior != p) {
            return Err(format!("inconsistent provenance for {}", p.path));
        }
    }
    Ok(owner.map(|(repository_id, workspace_id)| SourceTable {
        repository_id: repository_id.clone(),
        workspace_id: workspace_id.clone(),
        files: files
            .into_values()
            .map(|p| SourceFile {
                path: p.path.clone(),
                content_hash: p.content_hash.clone(),
                language: p.language,
                backend: p.backend.clone(),
            })
            .collect(),
    }))
}

#[derive(Serialize)]
struct EntityOut<'a> {
    id: &'a GraphEntityId,
    kind: EntityKind,
    name: &'a str,
    qualified_name: &'a str,
    parent: &'a Option<GraphEntityId>,
    range: &'a SourceRange,
    signature: &'a str,
    visibility: &'a Option<String>,
    path: &'a str,
}
impl<'a> From<&'a Entity> for EntityOut<'a> {
    fn from(e: &'a Entity) -> Self {
        Self {
            id: &e.id,
            kind: e.kind,
            name: &e.name,
            qualified_name: &e.qualified_name,
            parent: &e.parent,
            range: &e.range,
            signature: &e.signature,
            visibility: &e.visibility,
            path: &e.provenance.path,
        }
    }
}
#[derive(Serialize)]
struct LocatedOut<'a> {
    entity: EntityOut<'a>,
    score: u32,
    signals: &'a [String],
}
#[derive(Serialize)]
struct EdgeOut<'a> {
    id: &'a str,
    source: &'a GraphEntityId,
    target: &'a Option<GraphEntityId>,
    target_name: &'a str,
    kind: RelationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<ResolutionRule>,
    range: &'a SourceRange,
    path: &'a str,
}
#[derive(Serialize)]
struct PacketOut<'a> {
    version: &'a str,
    query: &'a str,
    generation: &'a Option<GraphGeneration>,
    sources: Option<SourceTable>,
    primary: Vec<LocatedOut<'a>>,
    neighbors: Vec<EntityOut<'a>>,
    relations: Vec<EdgeOut<'a>>,
    tests: Vec<EntityOut<'a>>,
    associations: &'a [TestAssociation],
    unresolved: &'a [UnresolvedSummary],
    coverage: &'a RelationCoverage,
    limits: &'a ContextLimits,
    truncated: bool,
    freshness: &'a IndexStatus,
    meaning: &'a str,
}

impl Serialize for ContextPacket {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let sources = table(
            self.primary
                .iter()
                .map(|p| &p.entity.provenance)
                .chain(self.neighbors.iter().map(|e| &e.provenance))
                .chain(self.tests.iter().map(|e| &e.provenance))
                .chain(self.relations.iter().map(|e| &e.provenance)),
        )
        .map_err(S::Error::custom)?;
        PacketOut {
            version: &self.version,
            query: &self.query,
            generation: &self.generation,
            sources,
            primary: self
                .primary
                .iter()
                .map(|p| LocatedOut {
                    entity: (&p.entity).into(),
                    score: p.score,
                    signals: &p.signals,
                })
                .collect(),
            neighbors: self.neighbors.iter().map(Into::into).collect(),
            relations: self
                .relations
                .iter()
                .map(|e| EdgeOut {
                    id: &e.id,
                    source: &e.source,
                    target: &e.target,
                    target_name: &e.target_name,
                    kind: e.kind,
                    resolution: e.resolution,
                    range: &e.range,
                    path: &e.provenance.path,
                })
                .collect(),
            tests: self.tests.iter().map(Into::into).collect(),
            associations: &self.associations,
            unresolved: &self.unresolved,
            coverage: &self.coverage,
            limits: &self.limits,
            truncated: self.truncated,
            freshness: &self.freshness,
            meaning: &self.meaning,
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
struct EntityIn {
    id: GraphEntityId,
    kind: EntityKind,
    name: String,
    qualified_name: String,
    #[serde(default)]
    key: String,
    parent: Option<GraphEntityId>,
    range: SourceRange,
    signature: String,
    visibility: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    provenance: Option<Provenance>,
}
#[derive(Deserialize)]
struct LocatedIn {
    entity: EntityIn,
    score: u32,
    signals: Vec<String>,
}
#[derive(Deserialize)]
struct EdgeIn {
    id: String,
    source: GraphEntityId,
    target: Option<GraphEntityId>,
    target_name: String,
    kind: RelationKind,
    #[serde(default)]
    resolution: Option<ResolutionRule>,
    #[serde(default)]
    path_hint: Option<String>,
    range: SourceRange,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    provenance: Option<Provenance>,
}
#[derive(Deserialize)]
struct PacketIn {
    version: String,
    query: String,
    #[serde(default)]
    generation: Option<GraphGeneration>,
    #[serde(default)]
    sources: Option<SourceTable>,
    primary: Vec<LocatedIn>,
    neighbors: Vec<EntityIn>,
    relations: Vec<EdgeIn>,
    tests: Vec<EntityIn>,
    #[serde(default)]
    associations: Vec<TestAssociation>,
    #[serde(default)]
    unresolved: Vec<UnresolvedSummary>,
    #[serde(default)]
    coverage: RelationCoverage,
    limits: ContextLimits,
    truncated: bool,
    freshness: IndexStatus,
    meaning: String,
}

fn hydrate(
    sources: Option<&SourceTable>,
    path: Option<String>,
    provenance: Option<Provenance>,
) -> Result<Provenance, String> {
    if let Some(p) = provenance {
        return Ok(p);
    }
    let path = path.ok_or("context record has neither path nor provenance")?;
    let table = sources.ok_or("context record references a missing source table")?;
    let file = table
        .files
        .iter()
        .find(|f| f.path == path)
        .ok_or_else(|| format!("context record references unlisted source {path}"))?;
    Ok(Provenance {
        repository_id: table.repository_id.clone(),
        workspace_id: table.workspace_id.clone(),
        path,
        content_hash: file.content_hash.clone(),
        language: file.language,
        backend: file.backend.clone(),
    })
}
fn entity(sources: Option<&SourceTable>, e: EntityIn) -> Result<Entity, String> {
    Ok(Entity {
        provenance: hydrate(sources, e.path, e.provenance)?,
        id: e.id,
        kind: e.kind,
        name: e.name,
        qualified_name: e.qualified_name,
        key: e.key,
        parent: e.parent,
        range: e.range,
        signature: e.signature,
        visibility: e.visibility,
    })
}

impl<'de> Deserialize<'de> for ContextPacket {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = PacketIn::deserialize(deserializer)?;
        let s = raw.sources.as_ref();
        let entities = |list: Vec<EntityIn>| -> Result<Vec<Entity>, D::Error> {
            list.into_iter()
                .map(|e| entity(s, e).map_err(D::Error::custom))
                .collect()
        };
        Ok(Self {
            primary: raw
                .primary
                .into_iter()
                .map(|p| {
                    Ok(LocatedEntity {
                        entity: entity(s, p.entity).map_err(D::Error::custom)?,
                        score: p.score,
                        signals: p.signals,
                    })
                })
                .collect::<Result<_, D::Error>>()?,
            neighbors: entities(raw.neighbors)?,
            tests: entities(raw.tests)?,
            relations: raw
                .relations
                .into_iter()
                .map(|e| {
                    Ok(Edge {
                        provenance: hydrate(s, e.path, e.provenance).map_err(D::Error::custom)?,
                        id: e.id,
                        source: e.source,
                        target: e.target,
                        target_name: e.target_name,
                        kind: e.kind,
                        resolution: e.resolution,
                        path_hint: e.path_hint,
                        range: e.range,
                    })
                })
                .collect::<Result<_, D::Error>>()?,
            version: raw.version,
            query: raw.query,
            generation: raw.generation,
            associations: raw.associations,
            unresolved: raw.unresolved,
            coverage: raw.coverage,
            limits: raw.limits,
            truncated: raw.truncated,
            freshness: raw.freshness,
            meaning: raw.meaning,
        })
    }
}
