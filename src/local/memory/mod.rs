//! Provider-neutral, auditable engineering knowledge. Stored text is data, not instructions.
pub(crate) mod cli;
mod model;
mod query;
use super::{
    Error, Result, graph, now_ms,
    repository::RepositoryInfo,
    require,
    store::{JournalEntry, Links, Store, append},
};
use crate::{Validate, protocol::*};
pub use model::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;

pub const DERIVATION_VERSION: &str = "graph-memory-1";

impl Store {
    /// Explicit trust selection is required. DERIVED/OBSERVED use mechanical constructors.
    pub fn add_memory(
        &mut self,
        start: &Path,
        draft: MemoryDraft,
        trust: MemoryTrustClass,
        supersedes: Option<&MemoryId>,
    ) -> Result<MemoryEntry> {
        require(
            matches!(
                trust,
                MemoryTrustClass::Canonical | MemoryTrustClass::AgentNote
            ),
            "free-form memory must be explicit CANONICAL or AGENT_NOTE; use derive/observe for mechanical facts",
        )?;
        let info = graph::checked_workspace(self, start)?;
        let mut entry = entry_from_draft(self, &info, draft, trust)?;
        entry.origin = if trust == MemoryTrustClass::Canonical {
            Origin::Human
        } else {
            Origin::Agent
        };
        self.publish_memory(&info, entry, supersedes, None)
    }

    pub fn derive_memory(&mut self, start: &Path, symbol: &str) -> Result<MemoryEntry> {
        let info = graph::checked_workspace(self, start)?;
        let found = self
            .graph(start)?
            .symbols(symbol, graph::SearchMode::Exact, 2)?
            .data;
        require(
            found.len() == 1,
            "derived memory requires one unambiguous graph symbol/ID",
        )?;
        let entity = &found[0];
        let relations = self
            .graph(start)?
            .relations(entity.id.as_str(), false, None, 12)?
            .data;
        let content = format!(
            "Graph {:?} {} in {}. Signature: {}. First {} syntactic outgoing relations (target spelling only, not resolved dependency claims): {}",
            entity.kind,
            entity.qualified_name,
            entity.provenance.path,
            entity.signature,
            relations.len(),
            relations
                .iter()
                .map(|r| format!("{:?} -> {}", r.kind, r.target_name))
                .collect::<Vec<_>>()
                .join("; ")
        );
        let draft = MemoryDraft {
            kind: MemoryKind::Finding,
            content,
            workspace_id: Some(info.workspace_id.clone()),
            canonical_key: None,
            actor: DERIVATION_VERSION.into(),
            author_job_id: None,
            links: vec![
                MemoryLink::Graph {
                    id: entity.id.clone(),
                },
                MemoryLink::File {
                    path: entity.provenance.path.clone(),
                },
            ],
        };
        let mut entry = entry_from_draft(self, &info, draft, MemoryTrustClass::Derived)?;
        entry.origin = Origin::GraphDerivation;
        entry.derivation = Some(Derivation {
            version: DERIVATION_VERSION.into(),
            inputs: vec![entity.provenance.clone()],
            entities: vec![entity.id.clone()],
        });
        self.publish_memory(&info, entry, None, None)
    }

    pub fn observe_evidence(&mut self, start: &Path, id: &EvidenceId) -> Result<MemoryEntry> {
        let info = graph::checked_workspace(self, start)?;
        let evidence = self.evidence(&info.repository_id, id)?.ok_or_else(|| {
            Error::Invalid("evidence is not registered in this repository".into())
        })?;
        let location = self.evidence_workspace(&info.repository_id, id)?;
        require(
            location.as_ref().is_none_or(|w| w == &info.workspace_id),
            "observe evidence in its recorded workspace",
        )?;
        let source = evidence.source_state.clone();
        let content = format!(
            "Evidence {} observed at {} ms: exit status {:?}; source {:?}; recorded summary: {}",
            id.as_str(),
            evidence.finished_at_ms.unwrap_or(evidence.started_at_ms),
            evidence.exit_status,
            source,
            evidence.summary
        );
        let draft = MemoryDraft {
            kind: MemoryKind::Observation,
            content,
            workspace_id: location,
            canonical_key: None,
            actor: "evidence-observation-1".into(),
            author_job_id: None,
            links: vec![MemoryLink::Evidence { id: id.clone() }],
        };
        let mut entry = entry_from_draft(self, &info, draft, MemoryTrustClass::Observed)?;
        entry.origin = Origin::Evidence;
        entry.observed_source = source;
        self.publish_memory(&info, entry, None, None)
    }

    pub fn promote_memory(
        &mut self,
        start: &Path,
        id: &MemoryId,
        actor: &str,
    ) -> Result<MemoryEntry> {
        let info = graph::checked_workspace(self, start)?;
        let old = self.memory_show(start, id, false)?;
        require(
            old.status == MemoryStatus::Active
                && old.entry.provenance.trust_class != MemoryTrustClass::Canonical,
            "promotion requires active noncanonical memory",
        )?;
        require(
            old.validity != Validity::Stale,
            "stale derived memory cannot be promoted; refresh/rederive first",
        )?;
        text_bound(actor, 128, "actor")?;
        let mut entry = old.entry;
        let original = entry.id.clone();
        entry.id = fresh_id(&self.connection)?;
        entry.created_in_workspace = info.workspace_id.clone();
        entry.created_at_ms = now_ms()?;
        entry.provenance.trust_class = MemoryTrustClass::Canonical;
        entry.provenance.source_refs.push(format!(
            "explicit-promotion:{} by {actor}",
            original.as_str()
        ));
        entry.links.push(MemoryLink::Memory {
            id: original.clone(),
        });
        // Keep original origin, actor, author, provider and derivation/evidence context.
        self.publish_memory(&info, entry, None, Some((original, actor.to_string())))
    }

    pub fn supersede_memory(
        &mut self,
        start: &Path,
        old: &MemoryId,
        new: &MemoryId,
        actor: &str,
    ) -> Result<()> {
        let info = graph::checked_workspace(self, start)?;
        text_bound(actor, 128, "actor")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = load(&tx, &info, old)?
            .ok_or_else(|| Error::Invalid("original memory not found".into()))?;
        let replacement = load(&tx, &info, new)?
            .ok_or_else(|| Error::Invalid("replacement memory not found".into()))?;
        require(
            replacement.status == MemoryStatus::Active,
            "replacement must be active",
        )?;
        check_supersession(&info, &original, &replacement.entry)?;
        supersede(&tx, &info, old, new, actor)?;
        tx.commit()?;
        Ok(())
    }

    pub fn reject_memory(&mut self, start: &Path, id: &MemoryId, actor: &str) -> Result<()> {
        let info = graph::checked_workspace(self, start)?;
        text_bound(actor, 128, "actor")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = load(&tx, &info, id)?.ok_or_else(|| Error::Invalid("memory not found".into()))?;
        visible(&info, &old.entry, false)?;
        require(
            old.status == MemoryStatus::Active,
            "only active memory can be rejected",
        )?;
        tx.execute(
            "UPDATE memory_entries SET status='REJECTED',updated_at_ms=?2 WHERE memory_id=?1",
            params![id.as_str(), now_ms()?],
        )?;
        audit(
            &tx,
            &info,
            old.entry.workspace_id.clone(),
            &JournalEntry::MemoryRejected {
                memory_id: id.clone(),
                actor: actor.into(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    fn publish_memory(
        &mut self,
        info: &RepositoryInfo,
        mut entry: MemoryEntry,
        supersedes: Option<&MemoryId>,
        promotion: Option<(MemoryId, String)>,
    ) -> Result<MemoryEntry> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((original, _)) = &promotion {
            let old = load(&tx, info, original)?
                .ok_or_else(|| Error::Invalid("promotion source is missing".into()))?;
            require(
                old.status == MemoryStatus::Active
                    && old.entry.provenance.trust_class != MemoryTrustClass::Canonical,
                "promotion source changed status; retry explicitly",
            )?;
            require(
                query::validity(&tx, info, &old.entry).0 != Validity::Stale,
                "promotion source became stale",
            )?;
        }
        if let Some(id) = supersedes {
            let original = load(&tx, info, id)?
                .ok_or_else(|| Error::Invalid("superseded memory not found".into()))?;
            check_supersession(info, &original, &entry)?;
            entry.links.push(MemoryLink::Memory { id: id.clone() });
            supersede(&tx, info, id, &entry.id, &entry.actor)?;
        }
        validate_entry(&tx, info, &entry, promotion.is_some())?;
        if entry.provenance.trust_class == MemoryTrustClass::Derived {
            require(
                query::validity(&tx, info, &entry).0 == Validity::Fresh,
                "graph changed before memory publication; rederive",
            )?;
        }
        let dedupe = graph::content_hash(
            serde_json::to_string(&(
                entry.workspace_id.clone(),
                entry.provenance.trust_class,
                entry.kind,
                entry
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                &entry.derivation,
                &entry.observed_source,
            ))?
            .as_bytes(),
        );
        tx.execute("INSERT INTO memory_entries(memory_id,repo_id,workspace_id,trust,kind,status,canonical_key,dedupe_key,created_at_ms,updated_at_ms,record_json) VALUES (?1,?2,?3,?4,?5,'ACTIVE',?6,?7,?8,?8,?9)",params![
            entry.id.as_str(),info.repository_id.as_str(),entry.workspace_id.as_ref().map(|w|w.as_str()),label(&entry.provenance.trust_class)?,label(&entry.kind)?,entry.canonical_key,dedupe,entry.created_at_ms,serde_json::to_string(&entry)?])?;
        let rowid = tx.last_insert_rowid();
        let mut tokens = normalize(&entry.content);
        for link in &entry.links {
            let (kind, target) = link.key();
            tx.execute(
                "INSERT OR IGNORE INTO memory_links VALUES (?1,?2,?3,?4)",
                params![entry.id.as_str(), info.repository_id.as_str(), kind, target],
            )?;
            if kind == "TAG" {
                tokens.push(' ');
                tokens.push_str(&normalize(target));
            }
        }
        tx.execute(
            "INSERT INTO memory_fts(rowid,tokens) VALUES (?1,?2)",
            params![rowid, tokens],
        )?;
        let action = if let Some((original, actor)) = promotion {
            JournalEntry::MemoryPromoted {
                original,
                canonical: entry.id.clone(),
                actor,
            }
        } else {
            JournalEntry::MemoryCreated {
                memory_id: entry.id.clone(),
                trust: entry.provenance.trust_class,
                actor: entry.actor.clone(),
            }
        };
        audit(&tx, info, entry.workspace_id.clone(), &action)?;
        tx.commit()?;
        Ok(entry)
    }
}

fn fresh_id(connection: &Connection) -> Result<MemoryId> {
    let id: String =
        connection.query_row("SELECT 'memory:' || lower(hex(randomblob(16)))", [], |r| {
            r.get(0)
        })?;
    MemoryId::new(id).map_err(Error::Invalid)
}
fn entry_from_draft(
    store: &Store,
    info: &RepositoryInfo,
    mut draft: MemoryDraft,
    trust: MemoryTrustClass,
) -> Result<MemoryEntry> {
    let mut provider = None;
    if let Some(id) = &draft.author_job_id {
        let job = store.job(&info.repository_id, id)?.ok_or_else(|| {
            Error::Invalid("author job is not registered in this repository".into())
        })?;
        provider = job.provider;
        draft.links.push(MemoryLink::Job { id: id.clone() });
        draft.links.push(MemoryLink::Plan { id: job.plan_id });
        if let Some(id) = job.task_id {
            draft.links.push(MemoryLink::Task { id });
        }
    }
    let evidence = draft
        .links
        .iter()
        .filter_map(|l| {
            if let MemoryLink::Evidence { id } = l {
                Some(EvidenceRef(id.clone()))
            } else {
                None
            }
        })
        .collect();
    Ok(MemoryEntry {
        id: fresh_id(&store.connection)?,
        repository_id: info.repository_id.clone(),
        workspace_id: draft.workspace_id,
        created_in_workspace: info.workspace_id.clone(),
        kind: draft.kind,
        content: draft.content,
        canonical_key: draft.canonical_key,
        created_at_ms: now_ms()?,
        provenance: MemoryProvenance {
            version: ProtocolVersion::V1,
            trust_class: trust,
            source_refs: vec![format!(
                "local-action:{} in {}",
                draft.actor,
                info.workspace_id.as_str()
            )],
            evidence,
            author_job_id: draft.author_job_id,
        },
        origin: Origin::Human,
        actor: draft.actor,
        provider,
        links: draft.links,
        derivation: None,
        observed_source: None,
    })
}
fn validate_entry(
    connection: &Connection,
    info: &RepositoryInfo,
    entry: &MemoryEntry,
    promotion: bool,
) -> Result<()> {
    entry.provenance.validate()?;
    text_bound(&entry.actor, 128, "actor")?;
    text_bound(&entry.content, 8192, "memory content")?;
    require(entry.links.len() <= 64, "memory has more than 64 links")?;
    visible(info, entry, false)?;
    if let Some(key) = &entry.canonical_key {
        require(
            entry.provenance.trust_class == MemoryTrustClass::Canonical
                && entry.workspace_id.is_none(),
            "canonical keys require repository-scoped CANONICAL memory",
        )?;
        GraphEntityId::new(key.clone()).map_err(Error::Invalid)?;
        require(
            !key.starts_with("config:"),
            "config: keys belong to live project policy; edit project.toml",
        )?;
    }
    if !promotion {
        for link in &entry.links {
            validate_link(connection, info, link)?;
        }
    } else {
        // Promotion preserves historical graph/file links even after source layout moves.
        for link in &entry.links {
            if !matches!(link, MemoryLink::Graph { .. } | MemoryLink::File { .. }) {
                validate_link(connection, info, link)?;
            }
        }
    }
    require(
        serde_json::to_vec(entry)?.len() <= 64 * 1024,
        "memory metadata exceeds 64 KiB",
    )
}
fn validate_link(c: &Connection, info: &RepositoryInfo, link: &MemoryLink) -> Result<()> {
    let (kind, target) = link.key();
    let table = match kind {
        "TASK" => Some(("tasks", "task_id")),
        "PLAN" => Some(("plans", "plan_id")),
        "JOB" => Some(("jobs", "job_id")),
        "EVIDENCE" => Some(("evidence", "evidence_id")),
        "MEMORY" => Some(("memory_entries", "memory_id")),
        _ => None,
    };
    if let Some((table, column)) = table {
        let count: i64 = c.query_row(
            &format!("SELECT count(*) FROM {table} WHERE repo_id=?1 AND {column}=?2"),
            params![info.repository_id.as_str(), target],
            |r| r.get(0),
        )?;
        return require(
            count == 1,
            format!("{kind} link does not exist in this repository: {target}"),
        );
    }
    match link {
        MemoryLink::Graph { id } => {
            let count: i64 = c.query_row(
                "SELECT count(*) FROM graph_entities WHERE workspace_id=?1 AND entity_id=?2",
                params![info.workspace_id.as_str(), id.as_str()],
                |r| r.get(0),
            )?;
            require(
                count == 1,
                "graph link is absent from the current workspace index",
            )
        }
        MemoryLink::File { path } => crate::validation::repo_path(path).map_err(Error::from),
        MemoryLink::Invariant { key } | MemoryLink::Tag { value: key } => {
            GraphEntityId::new(key.clone())
                .map(|_| ())
                .map_err(Error::Invalid)
        }
        MemoryLink::Commit { revision } => require(
            [40, 64].contains(&revision.len()) && revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "commit link must be a full 40/64-digit Git object ID",
        ),
        _ => Ok(()),
    }
}
fn check_supersession(info: &RepositoryInfo, old: &MemoryView, new: &MemoryEntry) -> Result<()> {
    visible(info, &old.entry, false)?;
    visible(info, new, false)?;
    require(
        old.status == MemoryStatus::Active && old.entry.id != new.id,
        "supersession requires distinct active memories",
    )?;
    require(
        old.entry.workspace_id == new.workspace_id
            && old.entry.kind == new.kind
            && old.entry.canonical_key == new.canonical_key,
        "supersession requires the same ownership scope, kind, and canonical key",
    )?;
    require(
        old.entry.provenance.trust_class == new.provenance.trust_class,
        "supersession cannot change trust; use explicit promotion",
    )
}
fn supersede(
    c: &Connection,
    info: &RepositoryInfo,
    old: &MemoryId,
    new: &MemoryId,
    actor: &str,
) -> Result<()> {
    let workspace = load(c, info, old)?
        .ok_or_else(|| Error::Invalid("superseded memory missing".into()))?
        .entry
        .workspace_id;
    c.execute("UPDATE memory_entries SET status='SUPERSEDED',superseded_by=?2,updated_at_ms=?3 WHERE memory_id=?1",params![old.as_str(),new.as_str(),now_ms()?])?;
    audit(
        c,
        info,
        workspace,
        &JournalEntry::MemorySuperseded {
            original: old.clone(),
            replacement: new.clone(),
            actor: actor.into(),
        },
    )
}
fn audit(
    c: &Connection,
    info: &RepositoryInfo,
    workspace: Option<super::repository::WorkspaceId>,
    event: &JournalEntry,
) -> Result<()> {
    let links = workspace.map_or_else(Links::default, Links::workspace);
    append(c, &info.repository_id, now_ms()?, &links, None, event)?;
    Ok(())
}
fn visible(info: &RepositoryInfo, entry: &MemoryEntry, all: bool) -> Result<()> {
    require(
        entry.repository_id == info.repository_id
            && (all
                || entry
                    .workspace_id
                    .as_ref()
                    .is_none_or(|w| w == &info.workspace_id)),
        "memory belongs to another repository/workspace",
    )
}
fn load(c: &Connection, info: &RepositoryInfo, id: &MemoryId) -> Result<Option<MemoryView>> {
    let row:Option<(String,String,u64,Option<String>)>=c.query_row("SELECT record_json,status,updated_at_ms,superseded_by FROM memory_entries WHERE repo_id=?1 AND memory_id=?2",params![info.repository_id.as_str(),id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    row.map(|(json, status, updated_at_ms, superseded_by)| {
        Ok(MemoryView {
            entry: serde_json::from_str(&json)?,
            status: parse_label(&status)?,
            updated_at_ms,
            superseded_by: superseded_by
                .map(MemoryId::new)
                .transpose()
                .map_err(Error::Invalid)?,
            validity: Validity::Durable,
            validity_detail: String::new(),
            unresolved_links: vec![],
        })
    })
    .transpose()
}
fn text_bound(text: &str, max: usize, name: &str) -> Result<()> {
    require(
        !text.trim().is_empty() && text.len() <= max && !text.contains('\0'),
        format!("{name} must contain 1–{max} bytes and no NUL"),
    )
}
pub(super) fn label(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .ok_or_else(|| Error::Invalid("expected enum label".into()))?
        .to_string())
}
pub(super) fn parse_label<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.into(),
    ))?)
}
fn normalize(text: &str) -> String {
    let chars: Vec<_> = text.chars().collect();
    let mut normalized = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || chars.get(i + 1).is_some_and(|n| n.is_lowercase())
                    && chars[i - 1].is_uppercase())
        {
            normalized.push(' ');
        }
        if c.is_alphanumeric() {
            normalized.extend(c.to_lowercase());
        } else {
            normalized.push(' ');
        }
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}
