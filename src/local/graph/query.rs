use super::*;
use crate::protocol::GraphEntityId;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Debug, Serialize)]
pub struct QueryResult<T> {
    pub freshness: IndexStatus,
    pub data: T,
    /// Only set by relation queries (`callers`, `refs`). Omitted elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<UnresolvedRelations>,
}

/// What a relation query could not prove. `data` carries only edges whose
/// target was resolved to an entity, so an empty `data` is not evidence that
/// nothing calls or references the symbol: syntactic call/reference sites that
/// name it but were never resolved are reported here instead of being dropped
/// silently. Never a dependency claim — an unresolved site may name something
/// else entirely.
#[derive(Debug, Serialize)]
pub struct UnresolvedRelations {
    /// Unresolved call/reference/implements sites naming this symbol.
    pub sites: usize,
    /// Bounded list of files holding them.
    pub paths: Vec<String>,
    pub meaning: &'static str,
}

const UNRESOLVED_MEANING: &str = "Syntactic sites naming this symbol whose target was never resolved to an entity. They are open questions, not callers: absence of a resolved relation is not proof that none exists.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Ordered camelCase/snake_case/punctuation-split, lowercased identifier tokens.
fn token_list(text: &str) -> Vec<String> {
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

fn is_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// The self type an `impl` header implements for: `impl<'a> Runtime<'a>` →
/// `Runtime`, `impl fmt::Display for Money` → `Money`.
fn impl_self_type(header: &str) -> String {
    let mut rest = header.trim_start_matches("impl").trim_start();
    if rest.starts_with('<') {
        let mut depth = 0;
        for (i, c) in rest.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        rest = rest[i + 1..].trim_start();
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    let target = rest.rsplit_once(" for ").map_or(rest, |(_, t)| t);
    let target = target.split('<').next().unwrap_or(target);
    target
        .trim_start_matches(['&', ' '])
        .trim_start_matches("mut ")
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .trim()
        .to_string()
}

/// Code symbols a natural-language request names explicitly: paths
/// (`Money::from_cents`, `crate::words::shortest`), snake_case and CamelCase
/// identifiers. Plain words are left to lexical ranking. Bounded to 16.
pub fn symbol_mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
        let token = raw.trim_matches(':');
        let path = token.split("::").filter(|s| !s.is_empty()).count() >= 2;
        let snake = token.contains('_') && token.len() >= 4;
        let camel = token.len() >= 4
            && token
                .chars()
                .zip(token.chars().skip(1))
                .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
        if (path || snake || camel)
            && token
                .split("::")
                .all(|s| s.is_empty() || is_path_segment(s))
            && !out.iter().any(|t| t == token)
        {
            out.push(token.to_string());
            if out.len() == 16 {
                break;
            }
        }
    }
    out
}

pub fn identifier_tokens(text: &str) -> BTreeSet<String> {
    token_list(text).into_iter().collect()
}

/// English function words. They carry no repository meaning, yet they are also
/// identifier fragments (`as_str`, `to_string`, `is_empty`), so an objective's
/// prose would otherwise match large parts of any codebase. Sorted.
const STOPWORDS: &[&str] = &[
    "about", "above", "after", "again", "all", "also", "an", "and", "any", "are", "as", "at", "be",
    "because", "been", "before", "being", "both", "but", "by", "can", "could", "did", "do", "does",
    "doing", "during", "each", "either", "for", "from", "had", "has", "have", "having", "how",
    "if", "in", "into", "is", "it", "its", "itself", "just", "may", "might", "more", "most",
    "must", "no", "nor", "not", "of", "off", "on", "once", "only", "or", "other", "our", "ours",
    "out", "over", "own", "same", "shall", "should", "so", "some", "such", "than", "that", "the",
    "their", "theirs", "them", "then", "there", "these", "they", "this", "those", "through", "to",
    "too", "under", "until", "up", "upon", "very", "via", "was", "we", "were", "what", "when",
    "where", "whether", "which", "while", "who", "whom", "why", "will", "with", "within",
    "without", "would", "yet", "you", "your",
];

fn stopword(token: &str) -> bool {
    token.len() < 2 || STOPWORDS.binary_search(&token).is_ok()
}

/// A small deterministic suffix stemmer applied identically to queries and
/// entities, so `ignored`/`ignores`/`ignore` and `capture`/`captured` meet.
fn stem(token: &str) -> String {
    if !token.is_ascii() || token.len() <= 3 || token.bytes().any(|b| b.is_ascii_digit()) {
        return token.to_string();
    }
    let mut t = token.to_string();
    if t.ends_with("ies") && t.len() > 4 {
        t.truncate(t.len() - 3);
        t.push('y');
    } else if t.ends_with('s') && !t.ends_with("ss") && !t.ends_with("us") && !t.ends_with("is") {
        t.pop();
    }
    let mut stripped = false;
    if t.ends_with("ing") && t.len() >= 7 {
        t.truncate(t.len() - 3);
        stripped = true;
    } else if t.ends_with("ed") && !t.ends_with("eed") && t.len() >= 5 {
        t.truncate(t.len() - 2);
        stripped = true;
    }
    let bytes = t.as_bytes();
    let n = bytes.len();
    if stripped && n >= 4 && bytes[n - 1] == bytes[n - 2] && !b"aeioulsz".contains(&bytes[n - 1]) {
        t.pop();
    }
    if t.len() > 3 && t.ends_with('e') {
        t.pop();
    }
    t
}

fn stems(text: &str) -> BTreeSet<String> {
    token_list(text).iter().map(|t| stem(t)).collect()
}

/// Deterministic lexical query for a natural-language objective: identifier
/// tokens in order, without stopwords or repeats, bounded to 32 tokens and 512
/// bytes. Falls back to the raw tokens if nothing else remains.
pub fn objective_query(objective: &str) -> String {
    let tokens = token_list(objective);
    let meaningful: Vec<&String> = tokens.iter().filter(|t| !stopword(t)).collect();
    let chosen: Vec<&String> = if meaningful.is_empty() {
        tokens.iter().collect()
    } else {
        meaningful
    };
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for token in chosen {
        if !seen.insert(token.as_str()) {
            continue;
        }
        if seen.len() > 32 || out.len() + token.len() + 1 > 512 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}

/// Test code by kind, or by the directory/file/module conventions shared by the
/// supported languages (`tests/`, `test_*.py`, `*.test.ts`, Rust `mod tests`).
pub(super) fn is_test_side(kind: EntityKind, qualified: &str, path: &str) -> bool {
    if kind == EntityKind::Test {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let mut directories = path.split('/');
    directories.next_back();
    directories.any(|s| matches!(s, "tests" | "test" | "__tests__" | "spec" | "specs"))
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file == "conftest.py"
        || qualified.split("::").any(|s| s == "tests" || s == "test")
}

/// floor(1024 · log2(x)) for x ≥ 1, integer-only so ranking is bit-identical
/// on every platform.
fn log2_fixed(x: u64) -> u64 {
    let whole = 63 - u64::from(x.leading_zeros());
    let mut y: u128 = (u128::from(x) << 32) >> whole;
    let mut fraction = 0;
    for bit in (0..10).rev() {
        y = (y * y) >> 32;
        if y >= 2 << 32 {
            y >>= 1;
            fraction |= 1 << bit;
        }
    }
    whole * 1024 + fraction
}

/// 1024 · (1 + log2((N + 1) / (df + 1))): rare terms dominate, ubiquitous ones
/// (`src`, `runtime` in a runtime crate) still count, but barely.
fn idf(total: u64, df: u64) -> u64 {
    1024 + log2_fixed(((total + 1) << 16) / (df + 1)).saturating_sub(16 * 1024)
}

const FIELD_NAME: u64 = 1000;
const FIELD_CONTAINER: u64 = 400;
const FIELD_PATH: u64 = 350;
const FIELD_SIGNATURE: u64 = 150;
const EXACT: u64 = 1 << 30;
/// Candidates kept per lane: enough for selection, structural seeds and `locate`.
const LANE: usize = 64;
const SEEDS: usize = 12;
const FANOUT: usize = 32;
const HELPERS: usize = 4;
const SUMMARY_NAMES: usize = 6;
const SUMMARY_SCAN: usize = 128;

fn kind_weight(kind: EntityKind) -> u64 {
    match kind {
        EntityKind::Function | EntityKind::Method | EntityKind::Test => 1000,
        EntityKind::Type | EntityKind::Enum | EntityKind::Trait => 950,
        EntityKind::Constant => 650,
        EntityKind::Module | EntityKind::Other => 500,
        EntityKind::File => 200,
    }
}

fn kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Calls => "CALLS",
        RelationKind::Contains => "CONTAINS",
        RelationKind::DependsOn => "DEPENDS_ON",
        RelationKind::Implements => "IMPLEMENTS",
        RelationKind::Imports => "IMPORTS",
        RelationKind::References => "REFERENCES",
        RelationKind::TestRelatedTo => "TEST_RELATED_TO",
    }
}

fn validate_query(query: &str, limit: usize) -> Result<()> {
    require(
        !query.trim().is_empty() && query.len() <= 512,
        "query must contain 1–512 bytes",
    )?;
    require((1..=100).contains(&limit), "query limit must be 1–100")
}

fn check_limits(limits: ContextLimits) -> Result<()> {
    require(
        (1..=10).contains(&limits.primary)
            && limits.depth <= 3
            && (1..=100).contains(&limits.neighbors)
            && (1..=20).contains(&limits.tests),
        "context limits: primary 1–10, depth 0–3, neighbors 1–100, tests 1–20",
    )
}

/// Stemmed query terms; stopwords are dropped unless nothing else remains. The
/// 512-byte query bound already bounds raw tokens; at most 32 terms score.
fn terms(query: &str) -> Result<Vec<String>> {
    let raw = identifier_tokens(query);
    let kept: BTreeSet<String> = raw
        .iter()
        .filter(|t| !stopword(t))
        .map(|t| stem(t))
        .collect();
    let terms = if kept.is_empty() {
        raw.iter().map(|t| stem(t)).collect::<BTreeSet<_>>()
    } else {
        kept
    };
    require(
        !terms.is_empty() && terms.len() <= 32,
        "location query needs 1–32 identifier tokens (excluding stopwords)",
    )?;
    Ok(terms.into_iter().collect())
}

#[derive(Debug, Clone)]
struct Candidate {
    id: GraphEntityId,
    kind: EntityKind,
    name: String,
    qualified: String,
    path: String,
    /// Lexical evidence alone (before kind weighting and structural credit).
    lexical: u64,
    score: u64,
    exact: bool,
    signals: Vec<String>,
}
impl Candidate {
    fn of(e: &Entity) -> Self {
        Self {
            id: e.id.clone(),
            kind: e.kind,
            name: e.name.clone(),
            qualified: e.qualified_name.clone(),
            path: e.provenance.path.clone(),
            lexical: 0,
            score: 0,
            exact: false,
            signals: vec![],
        }
    }
    fn test_side(&self) -> bool {
        is_test_side(self.kind, &self.qualified, &self.path)
    }
    fn located(&self, entity: Entity) -> LocatedEntity {
        LocatedEntity {
            entity,
            score: u32::try_from(self.score).unwrap_or(u32::MAX),
            signals: self.signals.clone(),
        }
    }
}
fn order(a: &Candidate, b: &Candidate) -> Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.qualified.cmp(&b.qualified))
        .then_with(|| a.id.cmp(&b.id))
}

/// Best-first bounded buffer; memory stays O(keep) during a full scan.
struct Lane {
    keep: usize,
    items: Vec<Candidate>,
}
impl Lane {
    fn new(keep: usize) -> Self {
        Self {
            keep,
            items: vec![],
        }
    }
    fn push(&mut self, candidate: Candidate) {
        self.items.push(candidate);
        if self.items.len() >= 2 * self.keep {
            self.items.sort_by(order);
            self.items.truncate(self.keep);
        }
    }
    fn finish(mut self) -> Vec<Candidate> {
        self.items.sort_by(order);
        self.items.truncate(self.keep);
        self.items
    }
}

struct Ranked {
    implementation: Vec<Candidate>,
    tests: Vec<Candidate>,
}

/// Best-first picks with at most `cap` per file, then best-first fill of any
/// remaining slots, so one file cannot take every slot while others qualify.
fn diverse(
    pool: &[Candidate],
    limit: usize,
    cap: usize,
    taken: &mut BTreeSet<GraphEntityId>,
) -> Vec<Candidate> {
    let mut chosen = vec![];
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    for c in pool {
        if chosen.len() == limit {
            break;
        }
        let used = per_file.entry(&c.path).or_default();
        if taken.contains(&c.id) || *used >= cap {
            continue;
        }
        *used += 1;
        taken.insert(c.id.clone());
        chosen.push(c.clone());
    }
    for c in pool {
        if chosen.len() == limit {
            break;
        }
        if taken.insert(c.id.clone()) {
            chosen.push(c.clone());
        }
    }
    chosen
}

struct Row {
    id: String,
    kind: EntityKind,
    name: String,
    qualified: String,
    path: String,
    signature: String,
}

type Cache = BTreeMap<GraphEntityId, Option<Entity>>;

impl GraphQuery<'_> {
    fn result<T>(&self, data: T) -> QueryResult<T> {
        QueryResult {
            freshness: self.freshness.clone(),
            data,
            unresolved: None,
        }
    }

    fn find(&self, name: &str, mode: SearchMode, limit: usize) -> Result<Vec<Entity>> {
        validate_query(name, limit)?;
        // Several whitespace-separated terms: each must appear, in any order,
        // in the name or qualified name (case-insensitive; `_` counts as a
        // space). The first term narrows in SQL, the rest filter here.
        let terms: Vec<String> = name
            .split_whitespace()
            .map(|t| t.to_lowercase().replace('_', " "))
            .collect();
        if mode == SearchMode::Substring && terms.len() > 1 {
            let words: Vec<&str> = name.split_whitespace().collect();
            let mut out = vec![];
            for e in self.find(words[0], SearchMode::Substring, 100)? {
                let hay = format!("{} {}", e.name, e.qualified_name)
                    .to_lowercase()
                    .replace('_', " ");
                if terms.iter().all(|t| hay.contains(t.as_str())) {
                    out.push(e);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            return Ok(out);
        }
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
        let mut found = self.exact_matches(symbol, 2)?;
        require(
            found.len() == 1,
            "symbol is absent or ambiguous; use code symbol/code locate and select a qualified name or graph ID",
        )?;
        Ok(found.remove(0))
    }

    fn load(&self, cache: &mut Cache, id: &GraphEntityId) -> Result<Option<Entity>> {
        if let Some(hit) = cache.get(id) {
            return Ok(hit.clone());
        }
        let json: Option<String> = self
            .tx
            .query_row(
                "SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND entity_id=?2",
                params![self.info.workspace_id.as_str(), id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let entity: Option<Entity> = json.map(|s| serde_json::from_str(&s)).transpose()?;
        cache.insert(id.clone(), entity.clone());
        Ok(entity)
    }

    /// The ontology generation this snapshot observed.
    pub(crate) fn generation(&self) -> Option<&GraphGeneration> {
        self.freshness.generation()
    }

    /// One entity by ID from this snapshot (context-relay dereference).
    pub(crate) fn entity(&self, id: &GraphEntityId) -> Result<Option<Entity>> {
        self.load(&mut Cache::new(), id)
    }

    /// Exact ID, name or qualified-name matches, at most `limit`. Several
    /// matches are reported, never resolved by guessing.
    pub fn exact_matches(&self, name: &str, limit: usize) -> Result<Vec<Entity>> {
        let exact = self.find(name, SearchMode::Exact, limit)?;
        if !exact.is_empty() {
            return Ok(exact);
        }
        self.source_path_matches(name, limit)
    }

    /// Entities a source-language path names: `crate::words::shortest`,
    /// `textstats::words::shortest`, `words::shortest` or
    /// `ClaudeAdapter::launch` for canonical `src::words::shortest` /
    /// `…::provider::impl ProviderAdapter for ClaudeAdapter::launch`.
    ///
    /// Deterministic and conservative. An entity's source path is its file
    /// module path (without `src`, `lib`, `main`, `mod` or `__init__`), then
    /// its enclosing items, with an `impl` block reduced to its self type. A
    /// `crate::` path must equal it exactly; otherwise the first non-empty
    /// tier wins: equal, equal after a leading crate name, or ending with a
    /// requested path of at least two segments. Every entity of the winning
    /// tier is returned; callers treat more than one as ambiguous.
    pub(crate) fn source_path_matches(&self, name: &str, limit: usize) -> Result<Vec<Entity>> {
        let trimmed = name.trim().trim_end_matches("()");
        let mut segments: Vec<&str> = trimmed.split("::").map(str::trim).collect();
        // `crate::…` is an absolute path from the crate root; `self::…` names
        // an unknown current module and is treated as relative.
        let absolute = segments.first() == Some(&"crate");
        segments.retain(|s| !matches!(*s, "crate" | "self"));
        let Some(last) = segments.last().copied() else {
            return Ok(vec![]);
        };
        if (!absolute && segments.len() < 2) || segments.iter().any(|s| !is_path_segment(s)) {
            return Ok(vec![]);
        }
        let mut cache = Cache::new();
        let mut paths = vec![];
        for candidate in self.find(last, SearchMode::Exact, 64)? {
            let path = self.source_path(&mut cache, &candidate)?;
            paths.push((candidate, path));
        }
        // Tiers, most specific first; the first non-empty tier is the answer,
        // so a longer, exact path is never shadowed by a looser reading.
        let exact = |p: &[String]| p.iter().map(String::as_str).eq(segments.iter().copied());
        let prefixed = |p: &[String]| {
            !absolute
                && p.iter()
                    .map(String::as_str)
                    .eq(segments[1..].iter().copied())
        };
        let suffix = |p: &[String]| {
            !absolute
                && p.len() > segments.len()
                && p[p.len() - segments.len()..]
                    .iter()
                    .map(String::as_str)
                    .eq(segments.iter().copied())
        };
        for tier in [&exact as &dyn Fn(&[String]) -> bool, &prefixed, &suffix] {
            let found: Vec<Entity> = paths
                .iter()
                .filter(|(_, p)| tier(p))
                .map(|(e, _)| e.clone())
                .take(limit)
                .collect();
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Ok(vec![])
    }

    /// See [`Self::source_path_matches`].
    fn source_path(&self, cache: &mut Cache, entity: &Entity) -> Result<Vec<String>> {
        let stem = entity
            .provenance
            .path
            .rsplit_once('.')
            .map_or(entity.provenance.path.as_str(), |(stem, _)| stem);
        let mut module: Vec<String> = stem.split('/').map(str::to_owned).collect();
        if module.first().is_some_and(|s| s == "src") && module.len() > 1 {
            module.remove(0);
        }
        if module
            .last()
            .is_some_and(|s| matches!(s.as_str(), "lib" | "main" | "mod" | "__init__"))
        {
            module.pop();
        }
        let mut items = vec![];
        let mut current = Some(entity.clone());
        while let Some(e) = current {
            if matches!(e.kind, EntityKind::File | EntityKind::Module)
                && e.qualified_name == stem.replace('/', "::")
            {
                break;
            }
            items.push(if e.name.starts_with("impl") {
                impl_self_type(&e.name)
            } else {
                e.name.clone()
            });
            current = match &e.parent {
                Some(parent) => self.load(cache, parent)?,
                None => None,
            };
        }
        items.reverse();
        module.extend(items);
        Ok(module)
    }

    /// Resolved structural relations (no containment/test-container links) of
    /// an entity in one direction, in deterministic edge order.
    pub(crate) fn resolved_edges(
        &self,
        id: &GraphEntityId,
        incoming: bool,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        self.resolved(id, incoming, limit)
    }

    /// The `FILE` entity of an indexed path, if the path is indexed.
    pub(super) fn file_entity(&self, path: &str) -> Result<Option<Entity>> {
        Ok(self
            .entity_rows("SELECT record_json FROM graph_entities WHERE workspace_id=?1 AND path=?2 AND kind='\"FILE\"' ORDER BY entity_id LIMIT ?3", path, 1)?
            .pop())
    }

    /// Unresolved syntactic relations elsewhere in the workspace whose target
    /// name is, or ends with, `name`. A name match is never a dependency; this
    /// count exists so impact analysis can state the open question explicitly.
    pub(super) fn unresolved_references(
        &self,
        name: &str,
        paths: usize,
    ) -> Result<(usize, Vec<String>)> {
        if name.is_empty() {
            return Ok((0, vec![]));
        }
        let escaped = name
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let rows: Vec<(String, i64)> = self
            .tx
            .prepare(
                "SELECT e.path,COUNT(*) FROM graph_edges e WHERE e.workspace_id=?1 AND e.target_id IS NULL \
                 AND e.kind IN ('\"CALLS\"','\"REFERENCES\"','\"IMPLEMENTS\"') \
                 AND (json_extract(e.record_json,'$.target_name')=?2 \
                      OR json_extract(e.record_json,'$.target_name') LIKE ?3 ESCAPE '\\' \
                      OR json_extract(e.record_json,'$.target_name') LIKE ?4 ESCAPE '\\') \
                 AND NOT EXISTS (SELECT 1 FROM graph_resolutions r WHERE r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id) \
                 GROUP BY e.path ORDER BY e.path",
            )?
            .query_map(
                params![
                    self.info.workspace_id.as_str(),
                    name,
                    format!("%::{escaped}"),
                    format!("%.{escaped}")
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<std::result::Result<_, _>>()?;
        let count = rows.iter().map(|(_, n)| *n as usize).sum();
        Ok((
            count,
            rows.into_iter().map(|(p, _)| p).take(paths).collect(),
        ))
    }

    /// Tests associated with `target` by the graph's test-association rules.
    pub(crate) fn associated_tests(
        &self,
        target: &Entity,
        limit: usize,
    ) -> Result<Vec<(Entity, AssociationBasis)>> {
        self.test_entities(target, limit)
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

    /// Runs an edge query whose rows are `(record_json, derived target, rule)`;
    /// a derived target (workspace-level resolution) fills the syntactic edge.
    fn edge_rows(
        &self,
        sql: &str,
        id: &GraphEntityId,
        kind: Option<&str>,
        limit: usize,
        out: &mut BTreeMap<String, Edge>,
    ) -> Result<()> {
        let mut statement = self.tx.prepare(sql)?;
        let rows = statement.query_map(
            params![self.info.workspace_id.as_str(), id.as_str(), kind, limit],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )?;
        for row in rows {
            let (json, target, rule) = row?;
            let mut edge: Edge = serde_json::from_str(&json)?;
            if let Some(target) = target {
                edge.target = Some(GraphEntityId::new(target).map_err(Error::Invalid)?);
                edge.resolution = Some(serde_json::from_value(serde_json::Value::String(
                    rule.unwrap_or_default(),
                ))?);
            }
            out.insert(edge.id.clone(), edge);
        }
        Ok(())
    }

    fn sorted(edges: BTreeMap<String, Edge>, limit: usize) -> Vec<Edge> {
        let mut edges: Vec<Edge> = edges.into_values().collect();
        edges.sort_by(|a, b| {
            (a.kind == RelationKind::Contains)
                .cmp(&(b.kind == RelationKind::Contains))
                .then_with(|| kind_name(a.kind).cmp(kind_name(b.kind)))
                .then_with(|| a.id.cmp(&b.id))
        });
        edges.truncate(limit);
        edges
    }

    /// Syntactic relations of an entity, with workspace-level resolutions applied.
    fn edges(
        &self,
        id: &GraphEntityId,
        incoming: Option<bool>,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        let kind = kind.map(|k| serde_json::to_string(&k)).transpose()?;
        let kind = kind.as_deref();
        let mut out = BTreeMap::new();
        if incoming != Some(true) {
            self.edge_rows("SELECT e.record_json,r.target_id,r.rule FROM graph_edges e LEFT JOIN graph_resolutions r ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id WHERE e.workspace_id=?1 AND e.source_id=?2 AND (?3 IS NULL OR e.kind=?3) ORDER BY (e.kind='\"CONTAINS\"'),e.kind,e.edge_id LIMIT ?4", id, kind, limit, &mut out)?;
        }
        if incoming != Some(false) {
            self.edge_rows("SELECT record_json,NULL,NULL FROM graph_edges WHERE workspace_id=?1 AND target_id=?2 AND (?3 IS NULL OR kind=?3) ORDER BY (kind='\"CONTAINS\"'),kind,edge_id LIMIT ?4", id, kind, limit, &mut out)?;
            self.edge_rows("SELECT e.record_json,r.target_id,r.rule FROM graph_resolutions r JOIN graph_edges e ON e.workspace_id=r.workspace_id AND e.edge_id=r.edge_id WHERE r.workspace_id=?1 AND r.target_id=?2 AND (?3 IS NULL OR r.kind=?3) ORDER BY r.kind,r.edge_id LIMIT ?4", id, kind, limit, &mut out)?;
        }
        Ok(Self::sorted(out, limit))
    }

    /// Resolved structural relations (not containment or test-container links)
    /// in one direction. Filtering happens in SQL, so unresolved call sites
    /// cannot crowd resolved relations out of the bound.
    fn resolved(&self, id: &GraphEntityId, incoming: bool, limit: usize) -> Result<Vec<Edge>> {
        let mut out = BTreeMap::new();
        if incoming {
            self.edge_rows("SELECT record_json,NULL,NULL FROM graph_edges WHERE workspace_id=?1 AND target_id=?2 AND (?3 IS NULL OR kind=?3) AND kind NOT IN ('\"CONTAINS\"','\"TEST_RELATED_TO\"') ORDER BY kind,edge_id LIMIT ?4", id, None, limit, &mut out)?;
            self.edge_rows("SELECT e.record_json,r.target_id,r.rule FROM graph_resolutions r JOIN graph_edges e ON e.workspace_id=r.workspace_id AND e.edge_id=r.edge_id WHERE r.workspace_id=?1 AND r.target_id=?2 AND (?3 IS NULL OR r.kind=?3) ORDER BY r.kind,r.edge_id LIMIT ?4", id, None, limit, &mut out)?;
        } else {
            self.edge_rows("SELECT e.record_json,r.target_id,r.rule FROM graph_edges e LEFT JOIN graph_resolutions r ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id WHERE e.workspace_id=?1 AND e.source_id=?2 AND (?3 IS NULL OR e.kind=?3) AND (e.target_id IS NOT NULL OR r.target_id IS NOT NULL) AND e.kind NOT IN ('\"CONTAINS\"','\"TEST_RELATED_TO\"') ORDER BY e.kind,e.edge_id LIMIT ?4", id, None, limit, &mut out)?;
        }
        Ok(Self::sorted(out, limit))
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
        let edges = self.edges(&entity.id, Some(incoming), kind, limit)?;
        // An empty answer must not read as "nothing references this". Incoming
        // relation queries report the unresolved sites naming the symbol so the
        // caller can tell "none" from "not proven".
        let mut result = self.result(edges);
        if incoming {
            let (sites, paths) =
                self.unresolved_references(&super::impact::short_name(&entity.qualified_name), 8)?;
            if sites > 0 {
                result.unresolved = Some(UnresolvedRelations {
                    sites,
                    paths,
                    meaning: UNRESOLVED_MEANING,
                });
            }
        }
        Ok(result)
    }

    fn scan(&self, mut visit: impl FnMut(Row) -> Result<()>) -> Result<()> {
        let mut statement = self.tx.prepare(
            "SELECT entity_id,kind,name,qualified_name,path,json_extract(record_json,'$.signature') FROM graph_entities WHERE workspace_id=?1 ORDER BY entity_id",
        )?;
        let mut rows = statement.query([self.info.workspace_id.as_str()])?;
        while let Some(r) = rows.next()? {
            visit(Row {
                id: r.get(0)?,
                kind: serde_json::from_str(&r.get::<_, String>(1)?)?,
                name: r.get(2)?,
                qualified: r.get(3)?,
                path: r.get(4)?,
                signature: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            })?;
        }
        Ok(())
    }

    /// Two streaming scans: term document frequencies and per-file term coverage,
    /// then IDF-weighted scoring into bounded implementation and test lanes.
    /// Memory is O(lanes + indexed files); time is O(entities) per query (a
    /// known full scan, not a graph load).
    fn rank(
        &self,
        query: &str,
        keep: usize,
        within: &dyn Fn(&str) -> bool,
        pinned: &BTreeSet<GraphEntityId>,
    ) -> Result<Ranked> {
        let terms = terms(query)?;
        let mut path_tokens: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        // Distinct query terms a file's own declarations (names and signatures,
        // not path-derived file/module names) mention, as a bit mask.
        let mut coverage: BTreeMap<String, u32> = BTreeMap::new();
        let mut df = vec![0u64; terms.len()];
        let mut total = 0u64;
        self.scan(|row| {
            total += 1;
            let path = path_tokens
                .entry(row.path.clone())
                .or_insert_with(|| stems(&row.path));
            let qualified = stems(&row.qualified);
            let signature = stems(&row.signature);
            let own = !matches!(row.kind, EntityKind::File | EntityKind::Module);
            let name = if own {
                stems(&row.name)
            } else {
                BTreeSet::new()
            };
            let mask = coverage.entry(row.path.clone()).or_default();
            for (i, term) in terms.iter().enumerate() {
                if qualified.contains(term) || path.contains(term) || signature.contains(term) {
                    df[i] += 1;
                }
                if own && (name.contains(term) || signature.contains(term)) {
                    *mask |= 1 << i;
                }
            }
            Ok(())
        })?;
        let weights: Vec<u64> = df.iter().map(|&d| idf(total, d)).collect();
        // A file whose declarations cover more of the query's IDF mass is more
        // likely the implementation area: its entities are scaled by up to 4x, so
        // a single rare-word coincidence elsewhere cannot outrank it.
        let mass: u64 = weights
            .iter()
            .zip(&df)
            .filter(|&(_, &d)| d > 0)
            .map(|(w, _)| w)
            .sum::<u64>()
            .max(1);
        let multiplier: BTreeMap<String, u64> = coverage
            .into_iter()
            .map(|(path, mask)| {
                let covered: u64 = (0..terms.len())
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| weights[i])
                    .sum();
                (path, 1000 + 3000 * covered / mass)
            })
            .collect();
        let mut implementation = Lane::new(keep);
        let mut tests = Lane::new(keep);
        self.scan(|row| {
            // Term statistics stay workspace-wide; only candidates are scoped.
            if !within(&row.path) {
                return Ok(());
            }
            let name = stems(&row.name);
            let qualified = stems(&row.qualified);
            let path = &path_tokens[&row.path];
            let signature = stems(&row.signature);
            let (mut lexical, mut name_weight, mut name_hits) = (0u64, 0u64, 0u64);
            let mut matched = 0u64;
            let mut signals = vec![];
            for (i, term) in terms.iter().enumerate() {
                let (weight, field) = if name.contains(term) {
                    (FIELD_NAME, "name")
                } else if qualified.contains(term) {
                    (FIELD_CONTAINER, "container")
                } else if path.contains(term) {
                    (FIELD_PATH, "path")
                } else if signature.contains(term) {
                    (FIELD_SIGNATURE, "signature")
                } else {
                    continue;
                };
                lexical += weights[i] * weight / 1000;
                matched += 1;
                if field == "name" {
                    name_weight += weights[i];
                    name_hits += 1;
                }
                if signals.len() < 8 {
                    signals.push(format!("{field}:{term}"));
                }
            }
            // Precise names (most of the name is query terms) beat long names that
            // merely contain many query words.
            lexical += name_weight * name_hits / (2 * name.len().max(1) as u64);
            // Coordination: evidence for more distinct query terms outweighs one
            // rare-word coincidence; neutral (x1) for single-term matches.
            lexical = lexical * (1 + matched) / 2;
            let exact = row.name == query
                || row.qualified == query
                || row.id == query
                || pinned.iter().any(|p| p.as_str() == row.id);
            if lexical == 0 && !exact {
                return Ok(());
            }
            let scale = multiplier.get(&row.path).copied().unwrap_or(1000);
            lexical = lexical * scale / 1000;
            if scale > 1000 && signals.len() < 8 {
                signals.push(format!("file-coverage:{}%", (scale - 1000) / 30));
            }
            if exact {
                signals.insert(0, "exact symbol".into());
                signals.truncate(8);
            }
            let candidate = Candidate {
                id: GraphEntityId::new(row.id).map_err(Error::Invalid)?,
                kind: row.kind,
                name: row.name,
                qualified: row.qualified,
                path: row.path,
                lexical,
                score: lexical * kind_weight(row.kind) / 1000 + if exact { EXACT } else { 0 },
                exact,
                signals,
            };
            if candidate.test_side() {
                tests.push(candidate);
            } else {
                implementation.push(candidate);
            }
            Ok(())
        })?;
        Ok(Ranked {
            implementation: implementation.finish(),
            tests: tests.finish(),
        })
    }

    pub fn locate(self, query: &str, limit: usize) -> Result<QueryResult<Vec<LocatedEntity>>> {
        validate_query(query, limit)?;
        let ranked = self.rank(query, LANE.max(limit), &|_| true, &BTreeSet::new())?;
        let mut all: Vec<Candidate> = ranked
            .implementation
            .into_iter()
            .chain(ranked.tests)
            .collect();
        all.sort_by(order);
        all.truncate(limit);
        let mut cache = Cache::new();
        let mut located = vec![];
        for c in &all {
            if let Some(entity) = self.load(&mut cache, &c.id)? {
                located.push(c.located(entity));
            }
        }
        Ok(self.result(located))
    }

    /// Credits implementation entities that the strongest lexical implementation
    /// matches resolve to, so the code relevant code actually calls can outrank
    /// name coincidences. Test seeds deliberately give no credit: what tests call
    /// is dominated by shared fixture setup, not the behavior under test.
    fn boost(
        &self,
        ranked: &Ranked,
        implementation: &mut BTreeMap<GraphEntityId, Candidate>,
        cache: &mut Cache,
        within: &dyn Fn(&str) -> bool,
    ) -> Result<()> {
        for seed in ranked.implementation.iter().take(SEEDS) {
            let out = self.resolved(&seed.id, false, FANOUT)?;
            let share = seed.lexical / (2 + out.len() as u64 / 4);
            if share == 0 {
                continue;
            }
            for edge in out {
                let Some(target) = edge.target else { continue };
                let Some(t) = self.load(cache, &target)? else {
                    continue;
                };
                if is_test_side(t.kind, &t.qualified_name, &t.provenance.path)
                    || !within(&t.provenance.path)
                {
                    continue;
                }
                let c = implementation
                    .entry(t.id.clone())
                    .or_insert_with(|| Candidate::of(&t));
                c.score += share;
                let signal = format!(
                    "structural:{}",
                    seed.name.chars().take(48).collect::<String>()
                );
                if c.signals.len() < 8 && !c.signals.contains(&signal) {
                    c.signals.push(signal);
                }
            }
        }
        Ok(())
    }

    /// Bounded digest of an entity's unresolved syntactic relations.
    fn unresolved(&self, id: &GraphEntityId) -> Result<(Vec<UnresolvedSummary>, bool)> {
        let rows: Vec<(String, String)> = self.tx.prepare("SELECT e.kind,json_extract(e.record_json,'$.target_name') FROM graph_edges e WHERE e.workspace_id=?1 AND e.source_id=?2 AND e.target_id IS NULL AND e.kind IN ('\"CALLS\"','\"REFERENCES\"','\"IMPLEMENTS\"') AND NOT EXISTS (SELECT 1 FROM graph_resolutions r WHERE r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id) ORDER BY e.kind,e.edge_id LIMIT ?3")?
            .query_map(params![self.info.workspace_id.as_str(), id.as_str(), SUMMARY_SCAN + 1], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let capped = rows.len() > SUMMARY_SCAN;
        let mut grouped: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for (kind, name) in rows.into_iter().take(SUMMARY_SCAN) {
            *grouped
                .entry(kind)
                .or_default()
                .entry(parser::compact(&name, 64))
                .or_default() += 1;
        }
        let mut summaries = vec![];
        for (kind, names) in grouped {
            let count = names.values().sum();
            let mut names: Vec<(String, usize)> = names.into_iter().collect();
            names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            summaries.push(UnresolvedSummary {
                entity: id.clone(),
                kind: serde_json::from_str(&kind)?,
                count,
                names: names
                    .into_iter()
                    .take(SUMMARY_NAMES)
                    .map(|(name, _)| name)
                    .collect(),
            });
        }
        Ok((summaries, capped))
    }

    /// Tests with a resolved relation to `target`, then tests sharing its
    /// lexical container, strongest basis first.
    fn test_entities(
        &self,
        target: &Entity,
        limit: usize,
    ) -> Result<Vec<(Entity, AssociationBasis)>> {
        let mut cache = Cache::new();
        let mut found: BTreeMap<GraphEntityId, (Entity, AssociationBasis)> = BTreeMap::new();
        for edge in self.resolved(&target.id, true, limit * 4)? {
            if let Some(t) = self.load(&mut cache, &edge.source)?
                && t.kind == EntityKind::Test
            {
                found
                    .entry(t.id.clone())
                    .or_insert((t, AssociationBasis::Calls));
            }
        }
        let container = if matches!(
            target.kind,
            EntityKind::File | EntityKind::Module | EntityKind::Type
        ) {
            &target.id
        } else {
            target.parent.as_ref().unwrap_or(&target.id)
        };
        for edge in self.edges(
            container,
            Some(true),
            Some(RelationKind::TestRelatedTo),
            limit,
        )? {
            if let Some(t) = self.load(&mut cache, &edge.source)? {
                found
                    .entry(t.id.clone())
                    .or_insert((t, AssociationBasis::Container));
            }
        }
        let mut list: Vec<_> = found.into_values().collect();
        list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.id.cmp(&b.0.id)));
        list.truncate(limit);
        Ok(list)
    }

    pub fn related_tests(self, symbol: &str, limit: usize) -> Result<QueryResult<Vec<Entity>>> {
        validate_query(symbol, limit)?;
        let entity = self.one(symbol)?;
        let tests = self.test_entities(&entity, limit)?;
        Ok(self.result(tests.into_iter().map(|(e, _)| e).collect()))
    }

    pub fn context(self, query: &str, limits: ContextLimits) -> Result<ContextPacket> {
        check_limits(limits)?;
        self.context_data(query, limits, &|_| true, &BTreeSet::new())
    }
    /// Context selected only among entities whose file `within` accepts (a
    /// task or request scope), so a narrow scope is filled with its own best
    /// matches instead of workspace-wide winners that scoping would discard.
    pub fn context_within(
        self,
        query: &str,
        limits: ContextLimits,
        within: &dyn Fn(&str) -> bool,
    ) -> Result<ContextPacket> {
        check_limits(limits)?;
        self.context_data(query, limits, within, &BTreeSet::new())
    }
    /// [`Self::context_within`] with entities the request named explicitly
    /// (already resolved to exactly one entity each) ranked as exact symbols,
    /// so a named implementation is never outranked by a lexically busier one.
    pub fn context_pinned(
        self,
        query: &str,
        limits: ContextLimits,
        within: &dyn Fn(&str) -> bool,
        pinned: &BTreeSet<GraphEntityId>,
    ) -> Result<ContextPacket> {
        check_limits(limits)?;
        self.context_data(query, limits, within, pinned)
    }
    pub fn impact(self, symbol: &str, limits: ContextLimits) -> Result<ContextPacket> {
        check_limits(limits)?;
        self.impact_data(symbol, limits)
    }
    pub fn neighborhood(self, symbol: &str, limits: ContextLimits) -> Result<ContextPacket> {
        check_limits(limits)?;
        self.one(symbol)?;
        self.context_data(symbol, limits, &|_| true, &BTreeSet::new())
    }

    fn packet(
        &self,
        query: &str,
        limits: ContextLimits,
        truncated: bool,
        meaning: &str,
    ) -> ContextPacket {
        ContextPacket {
            version: INDEX_VERSION.into(),
            query: query.into(),
            generation: self.freshness.generation().cloned(),
            primary: vec![],
            neighbors: vec![],
            relations: vec![],
            tests: vec![],
            associations: vec![],
            unresolved: vec![],
            coverage: RelationCoverage {
                semantic: self.freshness.semantic().is_some(),
                ..RelationCoverage::default()
            },
            limits,
            truncated,
            freshness: self.freshness.clone(),
            meaning: meaning.into(),
        }
    }

    fn context_data(
        self,
        query: &str,
        limits: ContextLimits,
        within: &dyn Fn(&str) -> bool,
        pinned: &BTreeSet<GraphEntityId>,
    ) -> Result<ContextPacket> {
        let ranked = self.rank(query, LANE.max(limits.primary * 4), within, pinned)?;
        let mut cache = Cache::new();
        let mut truncated = false;
        let mut implementation: BTreeMap<GraphEntityId, Candidate> = ranked
            .implementation
            .iter()
            .map(|c| (c.id.clone(), c.clone()))
            .collect();
        self.boost(&ranked, &mut implementation, &mut cache, within)?;

        // Primary: exact symbols first, then implementation entities scoring at
        // least half the best, with one slot reserved for another file whenever
        // one qualifies. Tests fill primary only when no implementation entity
        // matches at all; weaker implementation matches remain neighbor material.
        let mut taken = BTreeSet::new();
        let mut exact: Vec<Candidate> = ranked
            .implementation
            .iter()
            .chain(&ranked.tests)
            .filter(|c| c.exact)
            .cloned()
            .collect();
        exact.sort_by(order);
        let mut chosen: Vec<Candidate> = exact.into_iter().take(limits.primary).collect();
        taken.extend(chosen.iter().map(|c| c.id.clone()));
        let mut pool: Vec<Candidate> = implementation
            .values()
            .filter(|c| !c.exact)
            .cloned()
            .collect();
        pool.sort_by(order);
        let best = pool.first().map_or(0, |c| c.score);
        pool.retain(|c| c.score.saturating_mul(2) >= best && c.score > 0);
        if pool.is_empty() && chosen.is_empty() {
            pool = ranked
                .tests
                .iter()
                .filter(|c| c.kind == EntityKind::Test)
                .cloned()
                .collect();
        }
        let room = limits.primary - chosen.len();
        let cap = limits.primary.saturating_sub(1).max(1);
        chosen.extend(diverse(&pool, room, cap, &mut taken));
        truncated |= pool.iter().any(|c| !taken.contains(&c.id));
        let mut packet = self.packet(
            query,
            limits,
            false,
            "Deterministic graph context. Primary entities favor implementation over tests and span files. Relations are resolved syntactically (resolution names the rule); unresolved call/reference sites are only summarized by name and are not dependencies. Tests are candidates with a stated association basis, not proven coverage.",
        );
        for c in &chosen {
            let entity = self
                .load(&mut cache, &c.id)?
                .ok_or_else(|| Error::Invalid("ranked entity disappeared from snapshot".into()))?;
            packet.primary.push(c.located(entity));
        }

        // Neighbors: implementation entities one resolved relation away (per
        // depth), weighted by the referring entity's score and the relation kind
        // (callees first), plus their own relevance, and spread across files.
        // Tests reached this way become structural associations.
        let mut selected: BTreeSet<GraphEntityId> = taken.clone();
        let mut edges: BTreeMap<String, Edge> = BTreeMap::new();
        let mut links: BTreeMap<GraphEntityId, (AssociationBasis, GraphEntityId)> = BTreeMap::new();
        let edge_limit = limits.neighbors * 4;
        let mut frontier: Vec<(GraphEntityId, u64)> =
            chosen.iter().map(|c| (c.id.clone(), c.score)).collect();
        for depth in 0..limits.depth {
            let mut scores: BTreeMap<GraphEntityId, u64> = BTreeMap::new();
            for (id, weight) in &frontier {
                for incoming in [false, true] {
                    let adjacent = self.resolved(id, incoming, edge_limit + 1)?;
                    truncated |= adjacent.len() > edge_limit;
                    let mut helpers = 0;
                    for edge in adjacent.into_iter().take(edge_limit) {
                        let other = if incoming {
                            edge.source.clone()
                        } else {
                            edge.target.clone().expect("resolved relation")
                        };
                        if &other == id {
                            continue;
                        }
                        let Some(o) = self.load(&mut cache, &other)? else {
                            continue;
                        };
                        if !within(&o.provenance.path) {
                            continue;
                        }
                        if is_test_side(o.kind, &o.qualified_name, &o.provenance.path) {
                            if depth > 0 || !incoming {
                                continue;
                            }
                            if o.kind == EntityKind::Test {
                                links
                                    .entry(other)
                                    .or_insert((AssociationBasis::Calls, id.clone()));
                                edges.insert(edge.id.clone(), edge);
                            } else if helpers < HELPERS {
                                helpers += 1;
                                for caller in self.resolved(&other, true, FANOUT)? {
                                    if self.load(&mut cache, &caller.source)?.is_some_and(|t| {
                                        t.kind == EntityKind::Test && within(&t.provenance.path)
                                    }) {
                                        links.entry(caller.source.clone()).or_insert((
                                            AssociationBasis::CallsViaHelper,
                                            id.clone(),
                                        ));
                                    }
                                }
                            }
                            continue;
                        }
                        edges.insert(edge.id.clone(), edge.clone());
                        if selected.contains(&other) {
                            continue;
                        }
                        let bond = match (edge.kind, incoming) {
                            (RelationKind::Calls, false) => 4,
                            (RelationKind::Calls, true) | (RelationKind::References, _) => 3,
                            _ => 2,
                        };
                        let relevance = implementation.get(&other).map_or(0, |c| c.score);
                        *scores.entry(other).or_default() += weight * bond / 4 + relevance / 4;
                    }
                }
            }
            let mut found: Vec<Candidate> = scores
                .into_iter()
                .filter_map(|(id, score)| {
                    cache[&id].as_ref().map(|e| Candidate {
                        score,
                        ..Candidate::of(e)
                    })
                })
                .collect();
            found.sort_by(order);
            let room = limits.neighbors - packet.neighbors.len();
            truncated |= found.len() > room;
            let mut considered = selected.clone();
            let picked = diverse(&found, room, limits.neighbors.div_ceil(2), &mut considered);
            frontier.clear();
            for c in picked {
                selected.insert(c.id.clone());
                frontier.push((c.id.clone(), c.score / 4));
                packet
                    .neighbors
                    .push(cache[&c.id].clone().expect("loaded neighbor"));
            }
            if frontier.is_empty() || packet.neighbors.len() == limits.neighbors {
                break;
            }
        }

        // Tests: structural links first, then shared container, then lexical
        // relevance; diversified across files.
        let floor = ranked
            .tests
            .iter()
            .map(|c| c.lexical)
            .max()
            .unwrap_or(0)
            .max(1024);
        let mut tests: BTreeMap<
            GraphEntityId,
            (Candidate, AssociationBasis, Option<GraphEntityId>),
        > = BTreeMap::new();
        for c in ranked.tests.iter().filter(|c| c.kind == EntityKind::Test) {
            tests.insert(c.id.clone(), (c.clone(), AssociationBasis::Lexical, None));
        }
        let link =
            |tests: &mut BTreeMap<_, (Candidate, AssociationBasis, Option<GraphEntityId>)>,
             entity: &Entity,
             basis: AssociationBasis,
             target: &GraphEntityId| {
                let bonus = match basis {
                    AssociationBasis::Calls => floor / 2,
                    AssociationBasis::CallsViaHelper => floor / 3,
                    AssociationBasis::Container => floor / 8,
                    AssociationBasis::Lexical => 0,
                };
                let entry = tests
                    .entry(entity.id.clone())
                    .or_insert_with(|| (Candidate::of(entity), AssociationBasis::Lexical, None));
                entry.0.score += bonus;
                if basis < entry.1 {
                    entry.1 = basis;
                    entry.2 = Some(target.clone());
                }
            };
        for (test, (basis, target)) in &links {
            if let Some(entity) = self.load(&mut cache, test)? {
                link(&mut tests, &entity, *basis, target);
            }
        }
        for p in &packet.primary {
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
                limits.tests * 2,
            )? {
                if let Some(entity) = self.load(&mut cache, &edge.source)?
                    && entity.kind == EntityKind::Test
                    && within(&entity.provenance.path)
                {
                    link(
                        &mut tests,
                        &entity,
                        AssociationBasis::Container,
                        &p.entity.id,
                    );
                }
            }
        }
        let mut pool: Vec<Candidate> = tests
            .values()
            .filter(|(c, ..)| !selected.contains(&c.id))
            .map(|(c, ..)| c.clone())
            .collect();
        pool.sort_by(order);
        let mut tests_taken = selected.clone();
        let picked = diverse(
            &pool,
            limits.tests,
            limits.tests.div_ceil(2),
            &mut tests_taken,
        );
        truncated |= pool.len() > picked.len();
        for c in picked {
            let (_, basis, target) = &tests[&c.id];
            packet.associations.push(TestAssociation {
                test: c.id.clone(),
                target: target.clone(),
                basis: *basis,
            });
            let entity = self
                .load(&mut cache, &c.id)?
                .ok_or_else(|| Error::Invalid("test entity disappeared from snapshot".into()))?;
            packet.tests.push(entity);
        }

        // Relations: resolved relations among the selected entities only.
        let included = packet.entity_ids();
        let mut relations: Vec<Edge> = edges
            .into_values()
            .filter(|e| {
                included.contains(&e.source)
                    && e.target.as_ref().is_some_and(|t| included.contains(t))
            })
            .collect();
        relations.sort_by(|a, b| {
            kind_name(a.kind)
                .cmp(kind_name(b.kind))
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.id.cmp(&b.id))
        });
        truncated |= relations.len() > edge_limit;
        relations.truncate(edge_limit);
        packet.relations = relations;

        let summarized: Vec<GraphEntityId> = packet
            .primary
            .iter()
            .map(|p| &p.entity)
            .chain(&packet.neighbors)
            .map(|e| e.id.clone())
            .collect();
        for id in summarized {
            let (summaries, capped) = self.unresolved(&id)?;
            truncated |= capped;
            // Counted here, before any byte shedding downstream can drop the
            // detailed records: `coverage` is what stops a starved packet from
            // reading as "these entities have no further relations".
            packet.coverage.unresolved_sites += summaries.iter().map(|s| s.count).sum::<usize>();
            if summaries.iter().any(|s| s.count > 0) {
                packet.coverage.unresolved_entities += 1;
            }
            packet.unresolved.extend(summaries);
        }
        packet.truncated = truncated;
        Ok(packet)
    }

    /// Known structural dependents: incoming resolved relations, breadth-first.
    fn impact_data(self, symbol: &str, limits: ContextLimits) -> Result<ContextPacket> {
        let entity = self.one(symbol)?;
        let mut cache = Cache::new();
        let mut seen = BTreeSet::from([entity.id.clone()]);
        let mut frontier = vec![entity.id.clone()];
        let mut neighbors = vec![];
        let mut edges = BTreeMap::new();
        let edge_limit = limits.neighbors * 4;
        let mut truncated = false;
        for _ in 0..limits.depth {
            let mut next = vec![];
            for id in &frontier {
                for edge in self.edges(id, Some(true), None, edge_limit + 1)? {
                    if edge.kind == RelationKind::Contains {
                        continue;
                    }
                    if edges.len() >= edge_limit {
                        truncated = true;
                        break;
                    }
                    let other = edge.source.clone();
                    if !seen.contains(&other) {
                        if neighbors.len() >= limits.neighbors {
                            truncated = true;
                            continue;
                        }
                        let dependent = self.load(&mut cache, &other)?.ok_or_else(|| {
                            Error::Invalid("relation source missing from snapshot".into())
                        })?;
                        next.push(other.clone());
                        seen.insert(other);
                        neighbors.push(dependent);
                    }
                    edges.insert(edge.id.clone(), edge);
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        neighbors.sort_by(|a: &Entity, b| a.id.cmp(&b.id));
        let mut tests = self.test_entities(&entity, limits.tests + 1)?;
        truncated |= tests.len() > limits.tests;
        tests.truncate(limits.tests);
        let mut packet = self.packet(
            symbol,
            limits,
            truncated,
            "Known structural dependents only; not semantic impact. Tests are structural or lexical-container candidates, not proven coverage.",
        );
        for (test, basis) in tests {
            packet.associations.push(TestAssociation {
                test: test.id.clone(),
                target: Some(entity.id.clone()),
                basis,
            });
            packet.tests.push(test);
        }
        packet.primary = vec![LocatedEntity {
            entity,
            score: 1000,
            signals: vec!["exact symbol".into()],
        }];
        packet.neighbors = neighbors;
        packet.relations = edges.into_values().collect();
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_are_sorted_for_binary_search() {
        assert!(STOPWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn stemming_joins_inflections_symmetrically() {
        for group in [
            &["ignore", "ignored", "ignores", "ignoring"][..],
            &["capture", "captured", "captures", "capturing"],
            &["workspace", "workspaces"],
            &["exceed", "exceeds", "exceeded"],
            &["plan", "planning"],
            &["process", "processes", "processed", "processing"],
            &["match", "matches"],
            &["entry", "entries"],
        ] {
            let stems: BTreeSet<_> = group.iter().map(|t| stem(t)).collect();
            assert_eq!(stems.len(), 1, "{group:?} → {stems:?}");
        }
        for (word, kept) in [("string", "string"), ("status", "status"), ("git", "git")] {
            assert_eq!(stem(word), kept);
        }
    }

    #[test]
    fn fixed_point_log2_is_exact_on_powers_and_monotonic() {
        for k in 0..40 {
            assert_eq!(log2_fixed(1 << k), k * 1024);
        }
        let mut previous = 0;
        for x in 1..5000 {
            let v = log2_fixed(x);
            assert!(v >= previous);
            previous = v;
        }
        assert!(idf(1000, 1) > idf(1000, 100));
        assert_eq!(idf(1000, 1000), 1024);
    }

    #[test]
    fn objective_queries_drop_function_words_and_repeats() {
        assert_eq!(
            objective_query("Respect the Git ignore rules, such as the ignore of target/"),
            "respect git ignore rules target"
        );
        assert_eq!(objective_query("to be or not"), "to be or not");
    }
}
