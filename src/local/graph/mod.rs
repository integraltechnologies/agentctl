//! Content-bound, workspace-specific repository intelligence. No source execution.
pub(crate) mod cli;
mod delta;
pub mod files;
mod footprint;
mod impact;
mod lifecycle;
mod model;
mod parser;
mod query;
mod resolve;
mod wire;
pub use delta::{
    Change, ContentChange, DeltaSummary, EntityChange, EntityFacts, EntityField, FactsRef,
    FileChange, GenerationPoint, IdentityBasis, RelationChange, SemanticDelta,
};
pub use footprint::{
    FootprintLimits, FootprintOutlook, FootprintRequest, FootprintSummary, ReviewEvidence,
    ReviewSignal, ReviewSignalKind, StructuralEntity, StructuralFile, StructuralFootprint,
    StructuralRelation, StructuralRole, StructuralRoleBasis, Surface, VerificationLink,
};
pub(crate) use impact::proposed as proposed_impact;
pub use impact::{
    BoundaryReason, ImpactAuthority, ImpactBasis, ImpactBoundary, ImpactClass, ImpactEdge,
    ImpactItem, ImpactLimits, ImpactOrigin, ImpactOutlook, ImpactReport, ImpactRequest, ImpactSeed,
    ImpactStep, ImpactSummary,
};
pub use lifecycle::{
    DecisionReason, DeltaStatus, GenerationDecision, GenerationOrigin, GenerationState,
    OntologyGeneration, OntologyStatus,
};
pub(crate) use lifecycle::{accept_for_plan, close_for_plan, require_accepted, require_issuable};
pub use model::*;
pub use query::{GraphQuery, QueryResult, SearchMode, objective_query};

use super::{
    Error, Result, now_ms,
    repository::RepositoryInfo,
    require,
    store::{JournalEntry, Links, Store, append},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

impl Store {
    /// An explicit observation of the workspace (`agentctl repo index`). The
    /// result becomes accepted ontology truth only through the lifecycle rules
    /// in `lifecycle.rs`; otherwise it is recorded as a candidate.
    pub fn index_repository(&mut self, start: &Path) -> Result<IndexStats> {
        self.index_observed(start, &GenerationOrigin::External)
    }

    /// File derivations, index metadata, the generation's lifecycle record and
    /// the aggregate journal event commit together.
    pub(crate) fn index_observed(
        &mut self,
        start: &Path,
        origin: &GenerationOrigin,
    ) -> Result<IndexStats> {
        let started = Instant::now();
        let info = checked_workspace(self, start)?;
        let candidates = files::discover(&info.root)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = stored_files(&tx, &info)?;
        let old_metadata = metadata(&tx, &info)?;
        // Facts indexed before ontology snapshots carry no text hashes; their
        // files are re-derived so every generation can be snapshotted.
        let unhashed: BTreeSet<String> = tx
            .prepare("SELECT DISTINCT path FROM graph_entities WHERE workspace_id=?1 AND text_hash IS NULL")?
            .query_map([info.workspace_id.as_str()], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut stats = IndexStats {
            discovered: candidates.len(),
            indexed: 0,
            reused: 0,
            changed: 0,
            deleted: 0,
            failed: 0,
            entities: 0,
            edges: 0,
            duration_ms: 0,
            resolved: 0,
            generation: None,
        };
        let previous = old_metadata.as_ref().and_then(|m| m.generation.clone());
        let initial = IndexMetadata {
            version: INDEX_VERSION.into(),
            indexed_at_ms: now_ms()?,
            source: info.source.clone(),
            stats: stats.clone(),
            generation: previous.clone(),
        };
        tx.execute("INSERT INTO graph_indexes(workspace_id,repo_id,metadata_json) VALUES (?1,?2,?3) ON CONFLICT(workspace_id) DO UPDATE SET metadata_json=excluded.metadata_json",
            params![info.workspace_id.as_str(), info.repository_id.as_str(), serde_json::to_string(&initial)?])?;
        let paths: BTreeSet<_> = candidates.iter().map(String::as_str).collect();
        for path in old.keys().filter(|p| !paths.contains(p.as_str())) {
            tx.execute(
                "DELETE FROM indexed_files WHERE workspace_id=?1 AND path=?2",
                params![info.workspace_id.as_str(), path],
            )?;
            stats.deleted += 1;
        }
        let mut observations = BTreeMap::new();
        for path in &candidates {
            let language = Language::for_path(path).expect("discovered supported language");
            let observed = files::read(&info.root, path);
            let hash = observed.as_ref().ok().map(|(hash, _)| hash.clone());
            observations.insert(path.clone(), hash.clone());
            let backend = language.backend();
            let previous = old.get(path);
            if observed.is_ok()
                && old_metadata
                    .as_ref()
                    .is_some_and(|m| m.version == INDEX_VERSION)
                && previous.is_some_and(|f| {
                    f.content_hash == hash && f.backend == backend && f.diagnostic.is_none()
                })
                && !unhashed.contains(path)
            {
                stats.reused += 1;
                continue;
            }
            stats.indexed += 1;
            stats.changed += usize::from(previous.is_some());
            let derivation = observed.and_then(|(hash, source)| {
                let derivation = parser::extract(
                    &source,
                    Provenance {
                        repository_id: info.repository_id.clone(),
                        workspace_id: info.workspace_id.clone(),
                        path: path.clone(),
                        content_hash: hash,
                        language,
                        backend: backend.clone(),
                    },
                )?;
                let texts = delta::text_hashes(&source, &derivation.entities)?;
                Ok((derivation, texts))
            });
            let diagnostic = derivation
                .as_ref()
                .err()
                .map(|e| parser::compact(&e.to_string(), 240));
            stats.failed += usize::from(diagnostic.is_some());
            // DELETE cascades to ALL facts supported by this file, including failed parses.
            tx.execute(
                "DELETE FROM indexed_files WHERE workspace_id=?1 AND path=?2",
                params![info.workspace_id.as_str(), path],
            )?;
            tx.execute(
                "INSERT INTO indexed_files VALUES (?1,?2,?3,?4,?5)",
                params![info.workspace_id.as_str(), path, hash, backend, diagnostic],
            )?;
            if let Ok((derivation, texts)) = derivation {
                for (entity, text) in derivation.entities.into_iter().zip(texts) {
                    tx.execute(
                        "INSERT INTO graph_entities(workspace_id,entity_id,path,name,qualified_name,kind,record_json,text_hash) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![
                            info.workspace_id.as_str(),
                            entity.id.as_str(),
                            path,
                            entity.name,
                            entity.qualified_name,
                            serde_json::to_string(&entity.kind)?,
                            serde_json::to_string(&entity)?,
                            text
                        ],
                    )?;
                }
                for edge in derivation.edges {
                    tx.execute(
                        "INSERT INTO graph_edges(workspace_id,edge_id,path,source_id,target_id,kind,record_json,path_hint) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![
                            info.workspace_id.as_str(),
                            edge.id,
                            path,
                            edge.source.as_str(),
                            edge.target
                                .as_ref()
                                .map(crate::protocol::GraphEntityId::as_str),
                            serde_json::to_string(&edge.kind)?,
                            serde_json::to_string(&edge)?,
                            edge.path_hint
                        ],
                    )?;
                }
            }
        }
        // Do not publish a knowingly mixed observation if discovery/content changed mid-run.
        require(
            files::discover(&info.root)? == candidates,
            "files changed during indexing; transaction rolled back, retry",
        )?;
        for (path, hash) in observations {
            require(
                files::read(&info.root, &path).ok().map(|(h, _)| h) == hash,
                format!("{path} changed during indexing; transaction rolled back, retry"),
            )?;
        }
        let after = RepositoryInfo::discover(&info.root)?;
        require(
            after.workspace_id == info.workspace_id
                && after.repository_id == info.repository_id
                && after.source.head_commit == info.source.head_commit,
            "Git source identity changed during indexing; transaction rolled back, retry",
        )?;
        // The generation advances only when the indexed facts actually change.
        // Workspace resolution is rebuilt whenever any file was re-derived, even
        // with identical content: re-deriving a file cascades away its rows.
        let fingerprint = fingerprint(&tx, &info)?;
        let changed = previous
            .as_ref()
            .is_none_or(|g| g.fingerprint != fingerprint);
        if changed || stats.indexed > 0 || stats.deleted > 0 {
            resolve::rebuild(&tx, info.workspace_id.as_str())?;
        }
        let generation = match previous {
            Some(g) if !changed => g,
            prior => GraphGeneration {
                sequence: prior.map_or(1, |g| g.sequence + 1),
                fingerprint,
            },
        };
        (stats.entities, stats.edges) = counts(&tx, &info)?;
        stats.resolved = tx.query_row(
            "SELECT count(*) FROM graph_resolutions WHERE workspace_id=?1",
            [info.workspace_id.as_str()],
            |r| r.get(0),
        )?;
        stats.generation = Some(generation.clone());
        stats.duration_ms = started.elapsed().as_millis() as u64;
        let metadata = IndexMetadata {
            version: INDEX_VERSION.into(),
            indexed_at_ms: now_ms()?,
            source: after.source,
            stats: stats.clone(),
            generation: Some(generation.clone()),
        };
        tx.execute(
            "UPDATE graph_indexes SET metadata_json=?2 WHERE workspace_id=?1",
            params![
                info.workspace_id.as_str(),
                serde_json::to_string(&metadata)?
            ],
        )?;
        append(
            &tx,
            &info.repository_id,
            metadata.indexed_at_ms,
            &Links::workspace(info.workspace_id.clone()),
            None,
            &JournalEntry::IndexCompleted {
                stats: stats.clone(),
            },
        )?;
        lifecycle::observe(&tx, &info, origin, &generation, &stats, &metadata.source)?;
        tx.commit()?;
        Ok(stats)
    }

    pub fn index_status(&self, start: &Path) -> Result<IndexStatus> {
        let info = checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        status(&tx, &info)
    }

    /// A single-use, hash-checked SQLite read snapshot. Build a new query for each request.
    pub fn graph(&self, start: &Path) -> Result<GraphQuery<'_>> {
        self.graph_snapshot(start, true)
    }

    /// A read snapshot for dereferencing planner references and context
    /// requests, which tolerates an index that is stale relative to the
    /// worktree: a verifier is issued context while the executor's captured
    /// edits are deliberately not yet indexed (only accepted work refreshes the
    /// ontology). Staleness cannot produce a wrong answer, because every issued
    /// fact stays bound to the content hash it was derived from and is
    /// rechecked against the captured source before it is issued; a row that no
    /// longer matches fails closed instead.
    pub(crate) fn graph_for_issue(&self, start: &Path) -> Result<GraphQuery<'_>> {
        self.graph_snapshot(start, false)
    }

    fn graph_snapshot(&self, start: &Path, fresh: bool) -> Result<GraphQuery<'_>> {
        let info = checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let mut freshness = status(&tx, &info)?;
        require(
            freshness.index.is_some(),
            "workspace has no code index; run agentctl repo index",
        )?;
        require(
            !fresh || freshness.stale_files.is_empty(),
            format!(
                "code index is stale ({} paths/version changes); run agentctl repo index or repo index --status",
                freshness.stale_files.len()
            ),
        )?;
        freshness.diagnostics_truncated = freshness.failed_files.len() > 10;
        freshness.failed_files.truncate(10);
        Ok(GraphQuery {
            tx,
            info,
            freshness,
        })
    }
}

pub(super) fn checked_workspace(store: &Store, start: &Path) -> Result<RepositoryInfo> {
    let info = RepositoryInfo::discover(start)?;
    let registered = store.workspace(&info.workspace_id)?.ok_or_else(|| {
        Error::Invalid("workspace is not registered; run agentctl repo init".into())
    })?;
    let repository = store
        .repository(&info.repository_id)?
        .ok_or_else(|| Error::Invalid("repository is not registered".into()))?;
    require(
        registered.info.repository_id == info.repository_id
            && registered.info.root == info.root
            && registered.info.git_directory_identity == info.git_directory_identity
            && repository
                .common_directory_identity
                .as_ref()
                .is_none_or(|id| Some(id) == info.common_directory_identity.as_ref()),
        "workspace registration no longer matches Git metadata; inspect agentctl repo status",
    )?;
    Ok(info)
}

fn stored_files(
    connection: &Connection,
    info: &RepositoryInfo,
) -> Result<BTreeMap<String, IndexedFile>> {
    connection.prepare("SELECT path,content_hash,backend,diagnostic FROM indexed_files WHERE workspace_id=?1 ORDER BY path")?
        .query_map([info.workspace_id.as_str()], |r| Ok(IndexedFile { path: r.get(0)?, content_hash: r.get(1)?, backend: r.get(2)?, diagnostic: r.get(3)? }))?
        .map(|r| r.map(|f| (f.path.clone(), f)).map_err(Error::from)).collect()
}

/// Re-derives one workspace's cross-file resolutions from its stored facts
/// (used by the index pass and by the schema migration that introduced them).
pub(crate) fn rebuild_resolutions(connection: &Connection, workspace: &str) -> Result<()> {
    resolve::rebuild(connection, workspace)
}

/// The persisted generation of the workspace's index, without rehashing sources.
pub(crate) fn generation(
    connection: &Connection,
    info: &RepositoryInfo,
) -> Result<Option<GraphGeneration>> {
    Ok(metadata(connection, info)?.and_then(|m| m.generation))
}

fn metadata(connection: &Connection, info: &RepositoryInfo) -> Result<Option<IndexMetadata>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT metadata_json FROM graph_indexes WHERE workspace_id=?1",
            [info.workspace_id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    json.map(|s| serde_json::from_str(&s).map_err(Error::from))
        .transpose()
}

/// Content-derived identity of the indexed facts: the index version plus every
/// indexed file's path, hash, backend and diagnostic, in path order.
fn fingerprint(connection: &Connection, info: &RepositoryInfo) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut part = |s: &str| {
        hasher.update(&(s.len() as u64).to_le_bytes());
        hasher.update(s.as_bytes());
    };
    part("graph-generation-v1");
    part(INDEX_VERSION);
    for file in stored_files(connection, info)?.values() {
        part(&file.path);
        part(file.content_hash.as_deref().unwrap_or("\0unreadable"));
        part(&file.backend);
        part(file.diagnostic.as_deref().unwrap_or(""));
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn counts(connection: &Connection, info: &RepositoryInfo) -> Result<(usize, usize)> {
    let count = |table: &str| -> Result<usize> {
        Ok(connection.query_row(
            &format!("SELECT count(*) FROM {table} WHERE workspace_id=?1"),
            [info.workspace_id.as_str()],
            |r| r.get(0),
        )?)
    };
    Ok((count("graph_entities")?, count("graph_edges")?))
}

pub(super) fn status(connection: &Connection, info: &RepositoryInfo) -> Result<IndexStatus> {
    let index = metadata(connection, info)?;
    let stored = stored_files(connection, info)?;
    let mut stale = BTreeSet::new();
    let paths = files::discover(&info.root)?;
    let current: BTreeSet<_> = paths.iter().collect();
    for path in &paths {
        let hash = files::read(&info.root, path).ok().map(|(h, _)| h);
        if stored.get(path).is_none_or(|f| {
            f.content_hash != hash || f.backend != Language::for_path(path).unwrap().backend()
        }) {
            stale.insert(path.clone());
        }
    }
    for path in stored.keys().filter(|p| !current.contains(p)) {
        stale.insert(path.clone());
    }
    if index.as_ref().is_some_and(|m| m.version != INDEX_VERSION) {
        stale.insert("<index-version>".into());
    }
    let failed_files: Vec<_> = stored
        .values()
        .filter(|f| f.diagnostic.is_some())
        .cloned()
        .collect();
    let (entities, edges) = counts(connection, info)?;
    let fresh = index.is_some() && stale.is_empty() && failed_files.is_empty();
    let mut backends = BTreeMap::new();
    for file in stored.values() {
        *backends.entry(file.backend.clone()).or_default() += 1;
    }
    Ok(IndexStatus {
        index_version: INDEX_VERSION.into(),
        backends,
        repository_id: info.repository_id.clone(),
        workspace_id: info.workspace_id.clone(),
        current_source: info.source.clone(),
        index,
        indexed_files: stored.len(),
        stale_file_count: stale.len(),
        failed_file_count: failed_files.len(),
        diagnostics_truncated: false,
        stale_files: stale.into_iter().collect(),
        failed_files,
        entities,
        edges,
        fresh,
    })
}
