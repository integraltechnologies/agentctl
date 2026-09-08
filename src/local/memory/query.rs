use super::*;
use crate::local::config::ProjectConfig;
use rusqlite::{params_from_iter, types::Value};
use std::collections::{BTreeMap, BTreeSet};

impl Store {
    pub fn memory_show(
        &self,
        start: &Path,
        id: &MemoryId,
        all_workspaces: bool,
    ) -> Result<MemoryView> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let mut view = load(&tx, &info, id)?
            .ok_or_else(|| Error::Invalid("memory not found in this repository".into()))?;
        visible(&info, &view.entry, all_workspaces)?;
        enrich(&tx, &info, &mut view, &mut BTreeMap::new());
        Ok(view)
    }

    pub fn memory_query(&self, start: &Path, query: &MemoryQuery) -> Result<MemoryResults> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        retrieve(&tx, &info, query)
    }

    pub fn memory_for_task(
        &self,
        start: &Path,
        task: &TaskPacket,
        limits: MemoryLimits,
    ) -> Result<MemoryContext> {
        task.validate()?;
        let mut links = vec![MemoryLink::Task {
            id: task.task_id.clone(),
        }];
        links.extend(
            task.graph_entities
                .iter()
                .cloned()
                .map(|id| MemoryLink::Graph { id }),
        );
        links.extend(
            task.invariant_refs
                .iter()
                .cloned()
                .map(|key| MemoryLink::Invariant { key }),
        );
        self.memory_context(start, links, Some(&task.objective), limits)
    }

    pub fn code_context_with_memory(
        &self,
        start: &Path,
        query: &str,
        graph_limits: graph::ContextLimits,
        memory_limits: MemoryLimits,
    ) -> Result<CodeContextWithMemory> {
        let graph = self.graph(start)?.context(query, graph_limits)?;
        let memory = self.memory_for_code(start, &graph, memory_limits)?;
        Ok(CodeContextWithMemory { graph, memory })
    }

    pub fn memory_for_code(
        &self,
        start: &Path,
        context: &graph::ContextPacket,
        limits: MemoryLimits,
    ) -> Result<MemoryContext> {
        let info = graph::checked_workspace(self, start)?;
        require(
            context.freshness.repository_id == info.repository_id
                && context.freshness.workspace_id == info.workspace_id,
            "code context belongs to another repository/workspace",
        )?;
        let mut links = vec![];
        for entity in context
            .primary
            .iter()
            .map(|p| &p.entity)
            .chain(context.tests.iter())
            .chain(context.neighbors.iter())
            .take(32)
        {
            links.push(MemoryLink::Graph {
                id: entity.id.clone(),
            });
            links.push(MemoryLink::File {
                path: entity.provenance.path.clone(),
            });
        }
        let mut memory = self.memory_context(start, links, Some(&context.query), limits)?;
        memory.truncated |=
            context.primary.len() + context.tests.len() + context.neighbors.len() > 32;
        Ok(memory)
    }

    fn memory_context(
        &self,
        start: &Path,
        mut links: Vec<MemoryLink>,
        text: Option<&str>,
        limits: MemoryLimits,
    ) -> Result<MemoryContext> {
        require(
            limits.canonical <= 10
                && limits.facts <= 10
                && limits.notes <= 5
                && (256..=16384).contains(&limits.bytes),
            "memory limits: canonical/facts 0–10, notes 0–5, serialized bytes 256–16384",
        )?;
        let links_truncated = links.len() > 64;
        links.truncate(64);
        let info = graph::checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let mut entries = BTreeMap::new();
        let mut truncated = links_truncated;
        let mut queries = vec![];
        if !links.is_empty() {
            queries.push(MemoryQuery {
                links,
                limit: 100,
                ..MemoryQuery::default()
            });
        }
        let text = text.map(normalize).unwrap_or_default();
        let mut bounded_text = String::new();
        for token in text.split_whitespace().take(32) {
            if bounded_text.len() + token.len() + 1 > 512 {
                break;
            }
            if !bounded_text.is_empty() {
                bounded_text.push(' ');
            }
            bounded_text.push_str(token);
        }
        if !bounded_text.is_empty() {
            queries.push(MemoryQuery {
                text: Some(bounded_text),
                limit: 100,
                ..MemoryQuery::default()
            });
        }
        for query in queries {
            let result = retrieve(&tx, &info, &query)?;
            truncated |= result.truncated;
            for view in result.entries {
                entries.insert(view.entry.id.clone(), view);
            }
        }
        let mut summaries = vec![];
        // Live repository policy has its own source of truth and always remains visible.
        let (policy, policy_truncated) = policy(
            &info,
            &MemoryQuery {
                trust: Some(MemoryTrustClass::Canonical),
                limit: 10,
                ..MemoryQuery::default()
            },
        )?;
        truncated |= policy_truncated;
        for p in policy {
            let content = excerpt(&p.content);
            summaries.push(MemorySummary {
                id: p.key,
                trust: p.trust,
                kind: p.kind,
                content_truncated: p.content_truncated || content != p.content,
                content,
                origin: Origin::ProjectConfig,
                validity: Validity::Durable,
                workspace_id: Some(info.workspace_id.clone()),
            });
        }
        let mut entries: Vec<_> = entries.into_values().collect();
        entries.sort_by(|a, b| {
            priority(a.entry.provenance.trust_class)
                .cmp(&priority(b.entry.provenance.trust_class))
                .then_with(|| b.entry.created_at_ms.cmp(&a.entry.created_at_ms))
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });
        for view in entries {
            let e = view.entry;
            let content = excerpt(&e.content);
            summaries.push(MemorySummary {
                id: e.id.as_str().into(),
                trust: e.provenance.trust_class,
                kind: e.kind,
                content_truncated: content != e.content,
                content,
                origin: e.origin,
                validity: view.validity,
                workspace_id: e.workspace_id,
            });
        }
        let mut counts = [0; 3];
        summaries.sort_by_key(|s| (priority(s.trust), s.origin == Origin::ProjectConfig));
        let quotas = [limits.canonical, limits.facts, limits.notes];
        let mut result = MemoryContext {
            items: vec![],
            limits,
            truncated,
        };
        for summary in summaries {
            let p = priority(summary.trust);
            if counts[p] >= quotas[p] {
                result.truncated = true;
                continue;
            }
            result.items.push(summary);
            if serde_json::to_vec(&result)?.len() > limits.bytes {
                result.items.pop();
                result.truncated = true;
                continue;
            }
            counts[p] += 1;
        }
        Ok(result)
    }
}

fn priority(trust: MemoryTrustClass) -> usize {
    match trust {
        MemoryTrustClass::Canonical => 0,
        MemoryTrustClass::Observed | MemoryTrustClass::Derived => 1,
        MemoryTrustClass::AgentNote => 2,
    }
}
fn excerpt(text: &str) -> String {
    text.chars().take(384).collect()
}

fn retrieve(c: &Connection, info: &RepositoryInfo, q: &MemoryQuery) -> Result<MemoryResults> {
    require(
        (1..=100).contains(&q.limit) && q.links.len() <= 64,
        "memory query limit must be 1–100, with at most 64 linked filters",
    )?;
    let mut sql = "SELECT memory_id FROM memory_entries m WHERE repo_id=?".to_string();
    let mut values = vec![Value::Text(info.repository_id.as_str().into())];
    if !q.all_workspaces {
        sql.push_str(" AND (workspace_id IS NULL OR workspace_id=?)");
        values.push(Value::Text(info.workspace_id.as_str().into()));
    }
    for (column, value) in [
        ("trust", q.trust.map(|v| label(&v)).transpose()?),
        ("kind", q.kind.map(|v| label(&v)).transpose()?),
        ("status", q.status.map(|v| label(&v)).transpose()?),
    ] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {column}=?"));
            values.push(Value::Text(value));
        }
    }
    if let Some(text) = &q.text {
        text_bound(text, 512, "search")?;
        let normalized = normalize(text);
        let tokens: Vec<_> = normalized.split_whitespace().collect();
        require(
            !tokens.is_empty() && tokens.len() <= 32,
            "memory search requires 1–32 lexical tokens",
        )?;
        let expression = tokens
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(" AND ");
        sql.push_str(" AND seq IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?)");
        values.push(Value::Text(expression));
    }
    if !q.links.is_empty() {
        sql.push_str(" AND EXISTS (SELECT 1 FROM memory_links l WHERE l.memory_id=m.memory_id AND l.repo_id=m.repo_id AND (");
        for (i, link) in q.links.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("(l.kind=? AND l.target=?)");
            let (kind, target) = link.key();
            values.push(Value::Text(kind.into()));
            values.push(Value::Text(target.into()));
        }
        sql.push_str("))");
    }
    sql.push_str(
        " ORDER BY CASE trust WHEN 'CANONICAL' THEN 0 WHEN 'AGENT_NOTE' THEN 2 ELSE 1 END,",
    );
    if !q.recent {
        sql.push_str("(instr(lower(json_extract(record_json,'$.content')),lower(?))>0) DESC,");
        values.push(Value::Text(q.text.clone().unwrap_or_default()));
    }
    let candidate_limit = (q.limit * 10).min(1000);
    sql.push_str("created_at_ms DESC,memory_id LIMIT ?");
    values.push(Value::Integer((candidate_limit + 1) as i64));
    let ids: Vec<String> = c
        .prepare(&sql)?
        .query_map(params_from_iter(values), |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut truncated = ids.len() > candidate_limit;
    let mut entries = vec![];
    let mut cache = BTreeMap::new();
    let mut checked = 0;
    for id in ids.into_iter().take(candidate_limit) {
        checked += 1;
        let id = MemoryId::new(id).map_err(Error::Invalid)?;
        let mut view = load(c, info, &id)?
            .ok_or_else(|| Error::Invalid("memory vanished in read snapshot".into()))?;
        enrich(c, info, &mut view, &mut cache);
        if q.only_stale && view.validity != Validity::Stale
            || !q.include_stale && !q.only_stale && view.validity == Validity::Stale
        {
            continue;
        }
        if entries.len() == q.limit {
            truncated = true;
            break;
        }
        entries.push(view);
    }
    let (policy, policy_truncated) = policy(info, q)?;
    Ok(MemoryResults {
        entries,
        policy,
        truncated: truncated || policy_truncated,
        checked_candidates: checked,
    })
}

fn enrich(
    c: &Connection,
    info: &RepositoryInfo,
    view: &mut MemoryView,
    cache: &mut BTreeMap<String, bool>,
) {
    (view.validity, view.validity_detail) = validity_cached(c, info, &view.entry, cache);
    view.unresolved_links = view
        .entry
        .links
        .iter()
        .filter(|link| match link {
            MemoryLink::Graph { .. } => validate_link(c, info, link).is_err(),
            MemoryLink::File { path } => !info.root.join(path).is_file(),
            _ => false,
        })
        .cloned()
        .collect();
}

pub(super) fn validity(
    c: &Connection,
    info: &RepositoryInfo,
    entry: &MemoryEntry,
) -> (Validity, String) {
    validity_cached(c, info, entry, &mut BTreeMap::new())
}
fn validity_cached(
    c: &Connection,
    info: &RepositoryInfo,
    entry: &MemoryEntry,
    cache: &mut BTreeMap<String, bool>,
) -> (Validity, String) {
    match entry.provenance.trust_class {
        MemoryTrustClass::Canonical | MemoryTrustClass::AgentNote => (
            Validity::Durable,
            "Durable decision/note; graph links do not prove or invalidate its content.".into(),
        ),
        MemoryTrustClass::Observed => (
            Validity::Historical,
            "Historical evidence observation; not a claim about current source.".into(),
        ),
        MemoryTrustClass::Derived => {
            let checked = (|| -> Result<bool> {
                let Some(d) = &entry.derivation else { return Ok(false); };
                if d.version != DERIVATION_VERSION || d.inputs.is_empty()
                    || entry.workspace_id.as_ref() != Some(&info.workspace_id) {
                    return Ok(false);
                }
                let index: Option<String> = c.query_row(
                    "SELECT metadata_json FROM graph_indexes WHERE workspace_id=?1",
                    [info.workspace_id.as_str()], |r| r.get(0),
                ).optional()?;
                let Some(index) = index else { return Ok(false); };
                if serde_json::from_str::<graph::IndexMetadata>(&index)?.version != graph::INDEX_VERSION {
                    return Ok(false);
                }
                for input in &d.inputs {
                    let key = serde_json::to_string(input)?;
                    let matches = if let Some(value) = cache.get(&key) { *value } else {
                        let matches = support_matches(c, info, input).unwrap_or(false);
                        cache.insert(key, matches);
                        matches
                    };
                    if !matches { return Ok(false); }
                }
                for id in &d.entities {
                    let count: i64 = c.query_row(
                        "SELECT count(*) FROM graph_entities WHERE workspace_id=?1 AND entity_id=?2",
                        params![info.workspace_id.as_str(), id.as_str()], |r| r.get(0),
                    )?;
                    if count != 1 { return Ok(false); }
                }
                Ok(true)
            })().unwrap_or(false);
            if checked {
                (Validity::Fresh,"Supporting workspace files, graph and derivation versions match (sequential hash observation).".into())
            } else {
                (Validity::Stale,"Supporting source/graph/version is changed, unavailable, excluded, or belongs to another workspace.".into())
            }
        }
    }
}
fn support_matches(
    c: &Connection,
    info: &RepositoryInfo,
    input: &graph::Provenance,
) -> Result<bool> {
    if input.repository_id != info.repository_id
        || input.workspace_id != info.workspace_id
        || input.backend != input.language.backend()
    {
        return Ok(false);
    }
    let row:Option<(Option<String>,String,Option<String>)>=c.query_row("SELECT content_hash,backend,diagnostic FROM indexed_files WHERE workspace_id=?1 AND path=?2",params![info.workspace_id.as_str(),input.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if row
        != Some((
            Some(input.content_hash.clone()),
            input.backend.clone(),
            None,
        ))
    {
        return Ok(false);
    }
    if !graph::files::contains_source(&info.root, &input.path)? {
        return Ok(false);
    }
    Ok(graph::files::read(&info.root, &input.path)?.0 == input.content_hash)
}

fn policy(info: &RepositoryInfo, q: &MemoryQuery) -> Result<(Vec<PolicyProjection>, bool)> {
    if q.only_stale
        || q.trust.is_some_and(|t| t != MemoryTrustClass::Canonical)
        || q.status.is_some_and(|s| s != MemoryStatus::Active)
        || !info.root.join(".agentctl").try_exists()?
    {
        return Ok((vec![], false));
    }
    let config = ProjectConfig::load(&info.root)?;
    let config_hash = graph::content_hash(&serde_json::to_vec(&config)?);
    let mut values = vec![];
    for (key, value) in &config.invariants {
        values.push((
            format!("config:invariants.{key}"),
            MemoryKind::Invariant,
            value.description.clone(),
            Some(key.as_str()),
        ));
    }
    for (key, value) in &config.architecture {
        values.push((
            format!("config:architecture.{key}"),
            MemoryKind::ArchitectureDecision,
            value.description.clone(),
            None,
        ));
    }
    for (key, value) in &config.commands {
        values.push((
            format!("config:commands.{key}"),
            MemoryKind::Constraint,
            serde_json::to_string(value)?,
            None,
        ));
    }
    for (key, value) in &config.verification {
        values.push((
            format!("config:verification.{key}"),
            MemoryKind::Constraint,
            serde_json::to_string(value)?,
            None,
        ));
    }
    for (i, value) in config.protected.iter().enumerate() {
        values.push((
            format!("config:protected.{i}"),
            MemoryKind::Constraint,
            serde_json::to_string(value)?,
            None,
        ));
    }
    let tokens = q.text.as_ref().map(|s| normalize(s)).unwrap_or_default();
    let wanted: BTreeSet<_> = tokens.split_whitespace().collect();
    let mut output = vec![];
    for (key, kind, content, invariant) in values {
        if q.kind.is_some_and(|k| k != kind) {
            continue;
        }
        if !q.links.is_empty()
            && !q.links.iter().any(|l| match l {
                MemoryLink::Invariant { key } => invariant == Some(key.as_str()),
                MemoryLink::File { path } => path == ".agentctl/project.toml",
                _ => false,
            })
        {
            continue;
        }
        let normalized = normalize(&format!("{key} {content}"));
        let available: BTreeSet<_> = normalized.split_whitespace().collect();
        if !wanted.is_subset(&available) {
            continue;
        }
        if output.len() == q.limit.min(10) {
            return Ok((output, true));
        }
        output.push(PolicyProjection {
            key,
            trust: MemoryTrustClass::Canonical,
            kind,
            content: content.chars().take(2048).collect(),
            content_truncated: content.chars().count() > 2048,
            origin: Origin::ProjectConfig,
            workspace_id: info.workspace_id.clone(),
            source_path: ".agentctl/project.toml".into(),
            config_hash: config_hash.clone(),
        });
    }
    Ok((output, false))
}
