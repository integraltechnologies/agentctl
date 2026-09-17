//! Immutable ontology snapshots and the semantic delta between two of them.
//!
//! A snapshot is a manifest of per-file fact chunks stored content-addressed in
//! `ontology_blobs`, so unchanged files share storage across generations. A
//! delta is a pure function of two snapshots: no model, similarity measure, or
//! source execution is involved, and identical inputs give identical bytes.
//!
//! Cross-generation identity (see `docs/architecture.md#cross-generation-identity`):
//! an old and a new entity are the same entity only when they have the same
//! Stage-1 entity ID and their `(path, kind, qualified_name)` group holds exactly
//! one declaration in both generations. The ID already binds repository, path,
//! language, kind and lexical qualified name, so a body edit, signature edit or
//! line shift keeps identity, while a rename or a move is a removal plus an
//! addition. Duplicate groups are ordinal-numbered in file order, so an ID in
//! such a group proves nothing about which declaration it names: unless the
//! group's facts are unchanged as a whole, every member is reported removed and
//! added with `DUPLICATE_ORDINAL` identity, and so is every relation touching it.
use super::*;
use crate::{
    local::repository::WorkspaceId,
    protocol::{GraphEntityId, ProtocolVersion},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;

/// Largest single ontology artifact. Larger deltas are recorded as unavailable.
pub(super) const MAX_ARTIFACT_BYTES: usize = 64 << 20;

/// A content-addressed ontology artifact, verified by hash and length on read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsRef {
    pub hash: String,
    pub bytes: u64,
}

pub(super) fn put(c: &Connection, value: &impl Serialize) -> Result<FactsRef> {
    let body = serde_json::to_string(value)?;
    require(
        body.len() <= MAX_ARTIFACT_BYTES,
        "ontology artifact exceeds 64 MiB",
    )?;
    let reference = FactsRef {
        hash: content_hash(body.as_bytes()),
        bytes: body.len() as u64,
    };
    c.execute(
        "INSERT INTO ontology_blobs(hash,bytes,body) VALUES (?1,?2,?3) ON CONFLICT(hash) DO NOTHING",
        params![reference.hash, body.len() as i64, body],
    )?;
    Ok(reference)
}

pub(super) fn get<T: DeserializeOwned>(c: &Connection, reference: &FactsRef) -> Result<T> {
    let row: Option<(i64, String)> = c
        .query_row(
            "SELECT bytes,body FROM ontology_blobs WHERE hash=?1",
            [reference.hash.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (bytes, body) = row.ok_or_else(|| {
        Error::Invalid(format!("ontology artifact {} is missing", reference.hash))
    })?;
    require(
        u64::try_from(bytes).ok() == Some(reference.bytes)
            && body.len() as u64 == reference.bytes
            && content_hash(body.as_bytes()) == reference.hash,
        format!(
            "ontology artifact {} is corrupt (hash/length mismatch)",
            reference.hash
        ),
    )?;
    Ok(serde_json::from_str(&body)?)
}

/// Where a declaration's text begins once the comment, attribute and
/// decorator lines directly above it (no blank line between) are counted as
/// part of it. Never earlier than `floor`, the end of the previous sibling or
/// the start of the container.
fn attached_start(bytes: &[u8], start: usize, floor: usize) -> usize {
    let line_start = |i: usize| {
        bytes[..i]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |n| n + 1)
            .max(floor)
    };
    let mut cursor = line_start(start);
    if !bytes[cursor..start].iter().all(u8::is_ascii_whitespace) {
        return start;
    }
    let mut first = start;
    while cursor > floor {
        let above = line_start(cursor - 1);
        let line = bytes[above..cursor - 1].trim_ascii();
        let trivia = [&b"//"[..], b"#", b"/*", b"*", b"@"]
            .iter()
            .any(|prefix| line.starts_with(prefix));
        if !trivia {
            break;
        }
        first = above
            + bytes[above..]
                .iter()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
        cursor = above;
    }
    first
}

/// BLAKE3 over each entity's own source text: its declaration plus the
/// comment/attribute lines attached above it, with nested declarations (and
/// their attached lines) cut out together with the whitespace around them.
/// An edit is therefore attributed to the innermost declaration containing
/// it, and adding, removing or moving a member does not by itself change its
/// container (the member's own addition or removal reports that). Every other
/// byte is significant: whitespace inside a declaration is not normalized,
/// since some languages give it meaning.
pub(super) fn text_hashes(source: &str, entities: &[Entity]) -> Result<Vec<String>> {
    let bytes = source.as_bytes();
    for e in entities {
        require(
            e.range.start_byte <= e.range.end_byte && e.range.end_byte <= bytes.len(),
            "entity range lies outside its source",
        )?;
    }
    let index: HashMap<&GraphEntityId, usize> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| (&e.id, i))
        .collect();
    let mut children = vec![vec![]; entities.len()];
    for (i, e) in entities.iter().enumerate() {
        if let Some(p) = e.parent.as_ref().and_then(|p| index.get(p)) {
            children[*p].push(i);
        }
    }
    // Attached starts, computed per container in source order.
    let mut starts: Vec<usize> = entities.iter().map(|e| e.range.start_byte).collect();
    for (parent, members) in children.iter_mut().enumerate() {
        members.sort_by_key(|&c| (entities[c].range.start_byte, entities[c].range.end_byte));
        let mut floor = entities[parent].range.start_byte;
        for &c in members.iter() {
            let range = &entities[c].range;
            if range.start_byte >= floor {
                starts[c] = attached_start(bytes, range.start_byte, floor);
            }
            floor = floor.max(range.end_byte);
        }
    }
    Ok(entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let (start, end) = (starts[i], e.range.end_byte);
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"entity-text-v1\0");
            let mut position = start;
            for &c in &children[i] {
                let (s, t) = (starts[c], entities[c].range.end_byte);
                // Only well-nested members are excised; anything else stays inline.
                if s < position || t > end || s > t {
                    continue;
                }
                let kept = bytes[position..s]
                    .iter()
                    .rposition(|b| !b.is_ascii_whitespace())
                    .map_or(position, |k| position + k + 1);
                hasher.update(&bytes[position..kept]);
                position = t + bytes[t..end]
                    .iter()
                    .position(|b| !b.is_ascii_whitespace())
                    .unwrap_or(end - t);
            }
            hasher.update(&bytes[position..end]);
            format!("blake3:{}", hasher.finalize().to_hex())
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityFacts {
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub visibility: Option<String>,
    /// See [`text_hashes`]. Ignored when comparing `FILE` entities, whose
    /// content change is reported per file instead.
    pub text_hash: String,
}

/// One distinct resolved relation. Call-site multiplicity and positions are
/// not part of the fact, so moving or repeating a call is not a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationFact {
    pub source: GraphEntityId,
    pub kind: RelationKind,
    pub target: GraphEntityId,
    pub target_path: String,
    pub rules: Vec<ResolutionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileFacts {
    pub version: ProtocolVersion,
    pub path: String,
    pub content_hash: Option<String>,
    pub backend: String,
    pub diagnostic: Option<String>,
    pub entities: Vec<EntityFacts>,
    /// Resolved relations whose source is declared in this file.
    pub relations: Vec<RelationFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFile {
    pub path: String,
    pub content_hash: Option<String>,
    pub facts: FactsRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub version: ProtocolVersion,
    pub index_version: String,
    pub workspace_id: WorkspaceId,
    pub generation: GraphGeneration,
    pub files: Vec<SnapshotFile>,
    pub entities: usize,
    pub relations: usize,
}

type RelationKey = (GraphEntityId, RelationKind, GraphEntityId);

fn enum_text<T: DeserializeOwned>(text: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        text.to_owned(),
    ))?)
}

/// Materializes the live graph tables as an immutable snapshot. Must run in
/// the transaction that published `generation`.
pub(super) fn snapshot(
    c: &Connection,
    info: &RepositoryInfo,
    generation: &GraphGeneration,
) -> Result<(FactsRef, SnapshotManifest)> {
    let workspace = info.workspace_id.as_str();
    let mut entities: BTreeMap<String, Vec<EntityFacts>> = BTreeMap::new();
    let mut statement = c.prepare(
        "SELECT record_json,text_hash FROM graph_entities WHERE workspace_id=?1 ORDER BY path,entity_id",
    )?;
    let mut rows = statement.query([workspace])?;
    while let Some(row) = rows.next()? {
        let entity: Entity = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let text_hash: String = row.get::<_, Option<String>>(1)?.ok_or_else(|| {
            Error::Invalid(
                "indexed facts predate ontology snapshots; run agentctl repo index".into(),
            )
        })?;
        let path = entity.provenance.path;
        entities.entry(path.clone()).or_default().push(EntityFacts {
            id: entity.id,
            kind: entity.kind,
            name: entity.name,
            qualified_name: entity.qualified_name,
            key: entity.key,
            path,
            start_line: entity.range.start_line,
            end_line: entity.range.end_line,
            signature: entity.signature,
            visibility: entity.visibility,
            text_hash,
        });
    }
    let mut relations: BTreeMap<String, BTreeMap<RelationKey, (String, BTreeSet<ResolutionRule>)>> =
        BTreeMap::new();
    let mut statement = c.prepare(
        "SELECT e.path,e.source_id,e.kind,t.entity_id,t.path,COALESCE(r.rule,json_extract(e.record_json,'$.resolution')) \
         FROM graph_edges e LEFT JOIN graph_resolutions r ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id \
         JOIN graph_entities t ON t.workspace_id=e.workspace_id AND t.entity_id=COALESCE(e.target_id,r.target_id) \
         WHERE e.workspace_id=?1 AND e.kind NOT IN ('\"CONTAINS\"','\"TEST_RELATED_TO\"')",
    )?;
    let mut rows = statement.query([workspace])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let key = (
            GraphEntityId::new(row.get::<_, String>(1)?).map_err(Error::Invalid)?,
            serde_json::from_str::<RelationKind>(&row.get::<_, String>(2)?)?,
            GraphEntityId::new(row.get::<_, String>(3)?).map_err(Error::Invalid)?,
        );
        let entry = relations
            .entry(path)
            .or_default()
            .entry(key)
            .or_insert_with(|| (String::new(), BTreeSet::new()));
        entry.0 = row.get(4)?;
        if let Some(rule) = row.get::<_, Option<String>>(5)? {
            entry.1.insert(enum_text(&rule)?);
        }
    }
    let mut manifest = SnapshotManifest {
        version: ProtocolVersion::V1,
        index_version: INDEX_VERSION.into(),
        workspace_id: info.workspace_id.clone(),
        generation: generation.clone(),
        files: vec![],
        entities: 0,
        relations: 0,
    };
    for file in super::stored_files(c, info)?.into_values() {
        let entities = entities.remove(&file.path).unwrap_or_default();
        let relations: Vec<_> = relations
            .remove(&file.path)
            .unwrap_or_default()
            .into_iter()
            .map(
                |((source, kind, target), (target_path, rules))| RelationFact {
                    source,
                    kind,
                    target,
                    target_path,
                    rules: rules.into_iter().collect(),
                },
            )
            .collect();
        manifest.entities += entities.len();
        manifest.relations += relations.len();
        let facts = put(
            c,
            &FileFacts {
                version: ProtocolVersion::V1,
                path: file.path.clone(),
                content_hash: file.content_hash.clone(),
                backend: file.backend,
                diagnostic: file.diagnostic,
                entities,
                relations,
            },
        )?;
        manifest.files.push(SnapshotFile {
            path: file.path,
            content_hash: file.content_hash,
            facts,
        });
    }
    require(
        entities.is_empty() && relations.is_empty(),
        "graph facts reference files that are not indexed",
    )?;
    Ok((put(c, &manifest)?, manifest))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Change {
    Added,
    Removed,
    Modified,
}

/// Why an entry's identity across the two generations is (or is not) known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IdentityBasis {
    /// Same entity ID, and a single declaration of that path, kind and
    /// qualified name in both generations.
    Unique,
    /// The declaration shares its path, kind and qualified name with another,
    /// so its ordinal-based ID does not prove which declaration it names.
    DuplicateOrdinal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntityField {
    /// The declared signature text.
    Signature,
    Visibility,
    /// Any byte of the declaration's own source text (nested declarations
    /// excluded), signature included.
    Text,
    /// The normalized lexical key used for path resolution.
    Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContentChange {
    Added,
    Removed,
    Modified,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityChange {
    pub change: Change,
    pub identity: IdentityBasis,
    pub id: GraphEntityId,
    pub kind: EntityKind,
    pub path: String,
    pub qualified_name: String,
    /// The semantic facts that differ (MODIFIED only). Line ranges alone are
    /// never a change; they are carried in `before`/`after`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EntityField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<EntityFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<EntityFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationChange {
    pub change: Change,
    pub identity: IdentityBasis,
    pub source: GraphEntityId,
    pub kind: RelationKind,
    pub target: GraphEntityId,
    pub source_path: String,
    pub target_path: String,
    pub rules: Vec<ResolutionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub content: ContentChange,
    pub before: Option<String>,
    pub after: Option<String>,
    /// Entity changes declared in this file.
    pub entities: usize,
    /// Relation changes whose source is declared in this file.
    pub relations: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaSummary {
    /// Files whose content or facts differ.
    pub files: usize,
    /// Files with at least one entity or relation change.
    pub semantic_files: usize,
    pub entities_added: usize,
    pub entities_removed: usize,
    pub entities_modified: usize,
    pub relations_added: usize,
    pub relations_removed: usize,
    /// Entries reported as removal plus addition because identity is unproven.
    pub unproven_identity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationPoint {
    pub generation_id: String,
    pub generation: GraphGeneration,
    pub snapshot: FactsRef,
}

/// The typed difference between two ontology generations of one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticDelta {
    pub version: ProtocolVersion,
    pub workspace_id: WorkspaceId,
    pub index_version: String,
    pub from: GenerationPoint,
    pub to: GenerationPoint,
    pub summary: DeltaSummary,
    pub files: Vec<FileChange>,
    pub entities: Vec<EntityChange>,
    pub relations: Vec<RelationChange>,
}

impl SemanticDelta {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.relations.is_empty() && self.files.is_empty()
    }

    /// A read-only projection: entries matching `change` and (literal) `path`,
    /// at most `limit` per section. The summary still describes the whole delta.
    pub fn select(&self, change: Option<Change>, path: Option<&str>, limit: usize) -> Self {
        let mut out = self.clone();
        let keep = |c: Change, paths: &[&str]| {
            change.is_none_or(|w| w == c) && path.is_none_or(|p| paths.contains(&p))
        };
        out.entities.retain(|e| keep(e.change, &[&e.path]));
        out.relations
            .retain(|r| keep(r.change, &[&r.source_path, &r.target_path]));
        out.files.retain(|f| {
            path.is_none_or(|p| p == f.path)
                && change.is_none_or(|w| {
                    self.entities
                        .iter()
                        .any(|e| e.change == w && e.path == f.path)
                        || self
                            .relations
                            .iter()
                            .any(|r| r.change == w && r.source_path == f.path)
                })
        });
        out.entities.truncate(limit);
        out.relations.truncate(limit);
        out.files.truncate(limit);
        out
    }
}

fn same_facts(a: &EntityFacts, b: &EntityFacts) -> Vec<EntityField> {
    let mut fields = vec![];
    if a.signature != b.signature {
        fields.push(EntityField::Signature);
    }
    if a.visibility != b.visibility {
        fields.push(EntityField::Visibility);
    }
    if a.kind != EntityKind::File && a.text_hash != b.text_hash {
        fields.push(EntityField::Text);
    }
    if a.key != b.key {
        fields.push(EntityField::Key);
    }
    fields
}

type GroupFacts<'a> = Vec<(&'a str, Option<&'a str>, Option<&'a str>, &'a str)>;

/// A duplicate group's comparable facts as a multiset (order-free).
fn group_facts<'a>(side: &[&'a EntityFacts]) -> GroupFacts<'a> {
    let mut facts: Vec<_> = side
        .iter()
        .map(|e| {
            let text = (e.kind != EntityKind::File).then_some(e.text_hash.as_str());
            (
                e.signature.as_str(),
                e.visibility.as_deref(),
                text,
                e.key.as_str(),
            )
        })
        .collect();
    facts.sort_unstable();
    facts
}

type Group<'a> = BTreeMap<(String, &'a str), (Vec<&'a EntityFacts>, Vec<&'a EntityFacts>)>;

fn load(c: &Connection, file: Option<&SnapshotFile>) -> Result<Option<FileFacts>> {
    file.map(|f| {
        let facts: FileFacts = get(c, &f.facts)?;
        require(
            facts.path == f.path && facts.content_hash == f.content_hash,
            "ontology snapshot chunk does not match its manifest",
        )?;
        Ok(facts)
    })
    .transpose()
}

fn relation_change(
    change: Change,
    identity: IdentityBasis,
    path: &str,
    r: &RelationFact,
) -> RelationChange {
    RelationChange {
        change,
        identity,
        source: r.source.clone(),
        kind: r.kind,
        target: r.target.clone(),
        source_path: path.to_owned(),
        target_path: r.target_path.clone(),
        rules: r.rules.clone(),
    }
}

pub(super) fn diff(
    c: &Connection,
    from: (&GenerationPoint, &SnapshotManifest),
    to: (&GenerationPoint, &SnapshotManifest),
) -> Result<SemanticDelta> {
    let (old, new) = (from.1, to.1);
    require(
        old.workspace_id == new.workspace_id,
        "a semantic delta compares generations of one workspace",
    )?;
    require(
        old.index_version == new.index_version,
        format!(
            "generations were indexed by different graph versions ({} vs {}); their identities are not comparable",
            old.index_version, new.index_version
        ),
    )?;
    let before: BTreeMap<&str, &SnapshotFile> =
        old.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let after: BTreeMap<&str, &SnapshotFile> =
        new.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let paths: BTreeSet<&str> = before.keys().chain(after.keys()).copied().collect();
    let mut changed = vec![];
    let mut unchanged = vec![];
    for path in paths {
        let (a, b) = (before.get(path).copied(), after.get(path).copied());
        match (a, b) {
            (Some(x), Some(y)) if x.facts == y.facts => unchanged.push(y),
            _ => changed.push((path, a, b, load(c, a)?, load(c, b)?)),
        }
    }
    let mut entities = vec![];
    let mut unproven: BTreeSet<GraphEntityId> = BTreeSet::new();
    for (_, _, _, a, b) in &changed {
        let mut groups: Group<'_> = BTreeMap::new();
        for (side, facts) in [(0, a), (1, b)] {
            for e in facts.iter().flat_map(|f| &f.entities) {
                let slot = groups
                    .entry((format!("{:?}", e.kind), e.qualified_name.as_str()))
                    .or_default();
                if side == 0 { &mut slot.0 } else { &mut slot.1 }.push(e);
            }
        }
        for (old, new) in groups.values() {
            let change = |change, identity, e: &EntityFacts| EntityChange {
                change,
                identity,
                id: e.id.clone(),
                kind: e.kind,
                path: e.path.clone(),
                qualified_name: e.qualified_name.clone(),
                fields: vec![],
                before: (change == Change::Removed).then(|| e.clone()),
                after: (change == Change::Added).then(|| e.clone()),
            };
            if old.len() <= 1 && new.len() <= 1 {
                match (old.first(), new.first()) {
                    (Some(x), Some(y)) if x.id == y.id => {
                        let fields = same_facts(x, y);
                        if !fields.is_empty() {
                            entities.push(EntityChange {
                                fields,
                                before: Some((*x).clone()),
                                after: Some((*y).clone()),
                                ..change(Change::Modified, IdentityBasis::Unique, y)
                            });
                        }
                    }
                    (x, y) => {
                        entities
                            .extend(x.map(|e| change(Change::Removed, IdentityBasis::Unique, e)));
                        entities.extend(y.map(|e| change(Change::Added, IdentityBasis::Unique, e)));
                    }
                }
                continue;
            }
            // Duplicates: only the group as a whole can be compared.
            if old.len() == new.len() && group_facts(old) == group_facts(new) {
                continue;
            }
            for e in old {
                unproven.insert(e.id.clone());
                entities.push(change(Change::Removed, IdentityBasis::DuplicateOrdinal, e));
            }
            for e in new {
                unproven.insert(e.id.clone());
                entities.push(change(Change::Added, IdentityBasis::DuplicateOrdinal, e));
            }
        }
    }
    let touches = |r: &RelationFact| unproven.contains(&r.source) || unproven.contains(&r.target);
    let mut relations = vec![];
    let mut relation_diff = |path: &str, a: Option<&FileFacts>, b: Option<&FileFacts>| {
        let key = |r: &RelationFact| (r.source.clone(), r.kind, r.target.clone());
        let old: BTreeMap<_, _> = a
            .iter()
            .flat_map(|f| &f.relations)
            .map(|r| (key(r), r))
            .collect();
        let new: BTreeMap<_, _> = b
            .iter()
            .flat_map(|f| &f.relations)
            .map(|r| (key(r), r))
            .collect();
        let identity = |r: &RelationFact| {
            if touches(r) {
                IdentityBasis::DuplicateOrdinal
            } else {
                IdentityBasis::Unique
            }
        };
        for (k, r) in &old {
            match new.get(k) {
                Some(n) if touches(r) || touches(n) => {
                    relations.push(relation_change(
                        Change::Removed,
                        IdentityBasis::DuplicateOrdinal,
                        path,
                        r,
                    ));
                    relations.push(relation_change(
                        Change::Added,
                        IdentityBasis::DuplicateOrdinal,
                        path,
                        n,
                    ));
                }
                Some(_) => {}
                None => relations.push(relation_change(Change::Removed, identity(r), path, r)),
            }
        }
        for (k, r) in &new {
            if !old.contains_key(k) {
                relations.push(relation_change(Change::Added, identity(r), path, r));
            }
        }
    };
    for (path, _, _, a, b) in &changed {
        relation_diff(path, a.as_ref(), b.as_ref());
    }
    let mut touched_unchanged = vec![];
    if !unproven.is_empty() {
        for file in &unchanged {
            let facts = load(c, Some(file))?.expect("loaded");
            if facts.relations.iter().any(touches) {
                relation_diff(&file.path, Some(&facts), Some(&facts));
                touched_unchanged.push(*file);
            }
        }
    }
    entities.sort_by(|x, y| {
        (
            &x.path,
            &x.qualified_name,
            format!("{:?}", x.kind),
            x.change,
            &x.id,
        )
            .cmp(&(
                &y.path,
                &y.qualified_name,
                format!("{:?}", y.kind),
                y.change,
                &y.id,
            ))
    });
    relations.sort_by(|x, y| {
        (
            &x.source_path,
            &x.source,
            format!("{:?}", x.kind),
            &x.target,
            x.change,
        )
            .cmp(&(
                &y.source_path,
                &y.source,
                format!("{:?}", y.kind),
                &y.target,
                y.change,
            ))
    });
    let mut files = vec![];
    let file_change = |path: &str, a: Option<&SnapshotFile>, b: Option<&SnapshotFile>| FileChange {
        path: path.to_owned(),
        content: match (a, b) {
            (None, _) => ContentChange::Added,
            (_, None) => ContentChange::Removed,
            (Some(x), Some(y)) if x.content_hash != y.content_hash => ContentChange::Modified,
            _ => ContentChange::Unchanged,
        },
        before: a.and_then(|f| f.content_hash.clone()),
        after: b.and_then(|f| f.content_hash.clone()),
        entities: entities.iter().filter(|e| e.path == path).count(),
        relations: relations.iter().filter(|r| r.source_path == path).count(),
    };
    for (path, a, b, _, _) in &changed {
        files.push(file_change(path, *a, *b));
    }
    for file in touched_unchanged {
        files.push(file_change(
            &file.path,
            before.get(file.path.as_str()).copied(),
            Some(file),
        ));
    }
    files.sort_by(|x, y| x.path.cmp(&y.path));
    let count = |c: Change| entities.iter().filter(|e| e.change == c).count();
    let links = |c: Change| relations.iter().filter(|r| r.change == c).count();
    let summary = DeltaSummary {
        files: files.len(),
        semantic_files: files
            .iter()
            .filter(|f| f.entities + f.relations > 0)
            .count(),
        entities_added: count(Change::Added),
        entities_removed: count(Change::Removed),
        entities_modified: count(Change::Modified),
        relations_added: links(Change::Added),
        relations_removed: links(Change::Removed),
        unproven_identity: entities
            .iter()
            .filter(|e| e.identity == IdentityBasis::DuplicateOrdinal)
            .count()
            + relations
                .iter()
                .filter(|r| r.identity == IdentityBasis::DuplicateOrdinal)
                .count(),
    };
    Ok(SemanticDelta {
        version: ProtocolVersion::V1,
        workspace_id: new.workspace_id.clone(),
        index_version: new.index_version.clone(),
        from: from.0.clone(),
        to: to.0.clone(),
        summary,
        files,
        entities,
        relations,
    })
}
