use super::*;
use crate::protocol::GraphEntityId;
use rusqlite::{Transaction, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct QueryResult<T> {
    pub freshness: IndexStatus,
    pub data: T,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchMode {
    Exact,
    Prefix,
    Substring,
}

/// Hash-checked observation at construction time, held in a coherent SQLite snapshot.
/// Public query operations consume the handle so it cannot become a long-lived cache.
pub struct GraphQuery<'a> {
    pub(super) tx: Transaction<'a>,
    pub(super) info: RepositoryInfo,
    pub(super) freshness: IndexStatus,
}

pub fn identifier_tokens(text: &str) -> BTreeSet<String> {
    let chars: Vec<_> = text.chars().collect();
    let mut expanded = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || chars.get(i + 1).is_some_and(|c| c.is_lowercase())
                    && chars[i - 1].is_uppercase())
        {
            expanded.push(' ');
        }
        expanded.extend(c.to_lowercase());
    }
    expanded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn validate_query(query: &str, limit: usize) -> Result<()> {
    require(
        !query.trim().is_empty() && query.len() <= 512,
        "query must contain 1–512 bytes",
    )?;
    require((1..=100).contains(&limit), "query limit must be 1–100")
}

impl GraphQuery<'_> {
    fn result<T>(&self, data: T) -> QueryResult<T> {
        QueryResult {
            freshness: self.freshness.clone(),
            data,
        }
    }

    fn find(&self, name: &str, mode: SearchMode, limit: usize) -> Result<Vec<Entity>> {
        validate_query(name, limit)?;
        let escaped = name
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let (predicate, pattern) = match mode {
            SearchMode::Exact => (
                "(entity_id=?2 OR name=?2 OR qualified_name=?2)",
                name.to_string(),
            ),
            SearchMode::Prefix => (
                "(name LIKE ?2 ESCAPE '\\' OR qualified_name LIKE ?2 ESCAPE '\\')",
                format!("{escaped}%"),
            ),
            SearchMode::Substring => (
                "(name LIKE ?2 ESCAPE '\\' OR qualified_name LIKE ?2 ESCAPE '\\')",
                format!("%{escaped}%"),
            ),
        };
        self.entity_rows(&format!("SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND {predicate} ORDER BY path,qualified_name,entity_id LIMIT ?3"), &pattern, limit)
    }

    fn entity_rows(&self, sql: &str, value: &str, limit: usize) -> Result<Vec<Entity>> {
        self.tx
            .prepare(sql)?
            .query_map(
                params![self.info.workspace_id.as_str(), value, limit],
                |r| r.get::<_, String>(0),
            )?
            .map(|s| Ok(serde_json::from_str(&s?)?))
            .collect()
    }

    fn one(&self, symbol: &str) -> Result<Entity> {
        let mut found = self.find(symbol, SearchMode::Exact, 2)?;
        require(
            found.len() == 1,
            "symbol is absent or ambiguous; use code symbol/code locate and select a qualified name or graph ID",
        )?;
        Ok(found.remove(0))
    }

    pub fn symbols(
        self,
        name: &str,
        mode: SearchMode,
        limit: usize,
    ) -> Result<QueryResult<Vec<Entity>>> {
        Ok(self.result(self.find(name, mode, limit)?))
    }

    pub fn entities_in_file(self, path: &str, limit: usize) -> Result<QueryResult<Vec<Entity>>> {
        validate_query(path, limit)?;
        let rows = self.entity_rows("SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND path=?2 ORDER BY qualified_name,entity_id LIMIT ?3", path, limit)?;
        Ok(self.result(rows))
    }

    fn edges(
        &self,
        id: &GraphEntityId,
        incoming: Option<bool>,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        let predicate = match incoming {
            Some(true) => "target_id=?2",
            Some(false) => "source_id=?2",
            None => "(source_id=?2 OR target_id=?2)",
        };
        let sql = format!(
            "SELECT record_json FROM graph_edges WHERE workspace_id=?1 AND {predicate} AND (?3 IS NULL OR kind=?3) ORDER BY (kind='\"CONTAINS\"'),kind,edge_id LIMIT ?4"
        );
        self.tx
            .prepare(&sql)?
            .query_map(
                params![
                    self.info.workspace_id.as_str(),
                    id.as_str(),
                    kind.map(|k| serde_json::to_string(&k)).transpose()?,
                    limit
                ],
                |r| r.get::<_, String>(0),
            )?
            .map(|s| Ok(serde_json::from_str(&s?)?))
            .collect()
    }

    pub fn relations(
        self,
        symbol: &str,
        incoming: bool,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<QueryResult<Vec<Edge>>> {
        validate_query(symbol, limit)?;
        let entity = self.one(symbol)?;
        Ok(self.result(self.edges(&entity.id, Some(incoming), kind, limit)?))
    }

    fn rank(&self, query: &str, limit: usize) -> Result<Vec<LocatedEntity>> {
        validate_query(query, limit)?;
        let tokens = identifier_tokens(query);
        require(
            !tokens.is_empty() && tokens.len() <= 32,
            "location query needs 1–32 identifier tokens",
        )?;
        let mut top = vec![];
        // Lexical substring/signature scoring is a streaming scan, not an unbounded
        // in-memory graph load. Exact lookup and adjacency use SQLite indexes.
        let mut statement = self.tx.prepare(
            "SELECT record_json FROM graph_entities WHERE workspace_id=?1 ORDER BY entity_id",
        )?;
        let mut rows = statement.query([self.info.workspace_id.as_str()])?;
        while let Some(row) = rows.next()? {
            let entity: Entity = serde_json::from_str(&row.get::<_, String>(0)?)?;
            let mut score = 0;
            let mut signals = vec![];
            if entity.name == query || entity.qualified_name == query || entity.id.as_str() == query
            {
                score += 1000;
                signals.push("exact symbol".into());
            }
            let name = identifier_tokens(&entity.name);
            let path = identifier_tokens(&entity.provenance.path);
            let qualified = identifier_tokens(&entity.qualified_name);
            let signature = identifier_tokens(&entity.signature);
            for token in &tokens {
                if name.contains(token) {
                    score += 100;
                    signals.push(format!("name:{token}"));
                } else if qualified.contains(token) {
                    score += 40;
                    signals.push(format!("container:{token}"));
                } else if path.contains(token) {
                    score += 25;
                    signals.push(format!("path:{token}"));
                } else if signature.contains(token) {
                    score += 5;
                    signals.push(format!("signature:{token}"));
                }
            }
            if score == 0 {
                continue;
            }
            if !matches!(entity.kind, EntityKind::File | EntityKind::Module) {
                score += 1;
            }
            top.push(LocatedEntity {
                entity,
                score,
                signals,
            });
            top.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| a.entity.provenance.path.cmp(&b.entity.provenance.path))
                    .then_with(|| a.entity.qualified_name.cmp(&b.entity.qualified_name))
                    .then_with(|| a.entity.id.cmp(&b.entity.id))
            });
            top.truncate(limit);
        }
        Ok(top)
    }

    pub fn locate(self, query: &str, limit: usize) -> Result<QueryResult<Vec<LocatedEntity>>> {
        Ok(self.result(self.rank(query, limit)?))
    }

    fn neighborhood_data(
        &self,
        primary: &[LocatedEntity],
        limits: ContextLimits,
        incoming_only: bool,
    ) -> Result<(Vec<Entity>, Vec<Edge>, bool)> {
        let mut seen: BTreeSet<_> = primary.iter().map(|p| p.entity.id.clone()).collect();
        let mut frontier = seen.clone();
        let mut neighbors = vec![];
        let mut edges = BTreeMap::new();
        let edge_limit = limits.neighbors * 4;
        let mut truncated = false;
        for _ in 0..limits.depth {
            let mut next = BTreeSet::new();
            for id in frontier {
                let adjacent = self.edges(
                    &id,
                    if incoming_only { Some(true) } else { None },
                    None,
                    edge_limit + 1,
                )?;
                for edge in adjacent {
                    if incoming_only && edge.kind == RelationKind::Contains {
                        continue;
                    }
                    if edges.len() >= edge_limit {
                        truncated = true;
                        break;
                    }
                    let other = if edge.source == id {
                        edge.target.as_ref()
                    } else {
                        Some(&edge.source)
                    };
                    if let Some(other) = other.filter(|other| !seen.contains(*other)) {
                        if neighbors.len() >= limits.neighbors {
                            truncated = true;
                            continue;
                        }
                        let entity = self.one(other.as_str())?;
                        next.insert(other.clone());
                        seen.insert(other.clone());
                        neighbors.push(entity);
                    }
                    edges.insert(edge.id.clone(), edge);
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        neighbors.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((neighbors, edges.into_values().collect(), truncated))
    }

    fn test_entities(&self, primary: &[LocatedEntity], limit: usize) -> Result<Vec<Entity>> {
        let mut tests = BTreeMap::new();
        for p in primary {
            let container = if matches!(
                p.entity.kind,
                EntityKind::File | EntityKind::Module | EntityKind::Type
            ) {
                &p.entity.id
            } else {
                p.entity.parent.as_ref().unwrap_or(&p.entity.id)
            };
            for edge in self.edges(
                container,
                Some(true),
                Some(RelationKind::TestRelatedTo),
                limit,
            )? {
                if tests.len() >= limit {
                    break;
                }
                let test = self.one(edge.source.as_str())?;
                tests.insert(test.id.clone(), test);
            }
        }
        Ok(tests.into_values().collect())
    }

    pub fn related_tests(self, symbol: &str, limit: usize) -> Result<QueryResult<Vec<Entity>>> {
        validate_query(symbol, limit)?;
        let entity = self.one(symbol)?;
        Ok(self.result(self.test_entities(
            &[LocatedEntity {
                entity,
                score: 1000,
                signals: vec!["exact symbol".into()],
            }],
            limit,
        )?))
    }

    pub fn context(self, query: &str, limits: ContextLimits) -> Result<ContextPacket> {
        self.context_data(query, limits, false)
    }
    pub fn impact(self, symbol: &str, limits: ContextLimits) -> Result<ContextPacket> {
        self.context_data(symbol, limits, true)
    }
    pub fn neighborhood(self, symbol: &str, limits: ContextLimits) -> Result<ContextPacket> {
        self.one(symbol)?;
        self.context_data(symbol, limits, false)
    }
    fn context_data(
        self,
        query: &str,
        limits: ContextLimits,
        impact: bool,
    ) -> Result<ContextPacket> {
        require(
            (1..=10).contains(&limits.primary)
                && limits.depth <= 3
                && (1..=100).contains(&limits.neighbors)
                && (1..=20).contains(&limits.tests),
            "context limits: primary 1–10, depth 0–3, neighbors 1–100, tests 1–20",
        )?;
        let mut primary = if impact {
            vec![LocatedEntity {
                entity: self.one(query)?,
                score: 1000,
                signals: vec!["exact symbol".into()],
            }]
        } else {
            self.rank(query, limits.primary + 1)?
        };
        let primary_truncated = primary.len() > limits.primary;
        primary.truncate(limits.primary);
        let (neighbors, relations, truncated) = self.neighborhood_data(&primary, limits, impact)?;
        let mut tests = self.test_entities(&primary, limits.tests + 1)?;
        let truncated = truncated || primary_truncated || tests.len() > limits.tests;
        tests.truncate(limits.tests);
        Ok(ContextPacket { version: INDEX_VERSION.into(), query: query.into(), primary, neighbors, relations, tests, limits,
            truncated, freshness: self.freshness,
            meaning: if impact { "Known structural dependents only; not semantic impact. Tests are lexical-container candidates, not proven coverage." }
                else { "Deterministic graph context; unresolved target names are syntax, not resolved dependencies. Tests are lexical-container candidates." }.into() })
    }
}
