//! Workspace-level resolution of qualified and imported paths (`module::f`,
//! `Type::f`, `crate::`/`super::`/`self::` paths, and the targets of import
//! statements and of names they bind) into other files. Derived only from the
//! committed extraction facts, rebuilt in the index transaction whenever the graph
//! generation changes, and kept apart from `graph_edges` so re-deriving one file
//! can never cascade away another file's relations.
//!
//! Cost: one pass over the workspace's declarations to index them by key leaf,
//! then one pass over hinted unresolved edges: O(entities + hinted edges ×
//! candidates per leaf) per changing pass, with no per-edge query.
use super::*;

enum Hint<'a> {
    /// A fully anchored key (`self::`, `super::`, `crate::` paths).
    Absolute(&'a str),
    /// A relative path, resolved by unique key suffix; `root` is the source's crate.
    Relative { root: &'a str, path: &'a str },
    /// A Python absolute import path. Python finds its top-level package on
    /// `sys.path`: the repository root when that key exists, otherwise some
    /// source directory, which a unique multi-segment suffix stands in for. A
    /// lone top-level name is never matched by suffix — that would be a
    /// workspace-wide name match, not a path.
    Rooted(&'a str),
}

/// Splits the evidence tag the extractor attached (see `parser::rule_tag`) from
/// the hint itself. An untagged hint is a path the source wrote out.
fn tagged(hint: &str) -> (ResolutionRule, &str) {
    match hint.split_once('|') {
        Some(("import", rest)) => (ResolutionRule::ImportBinding, rest),
        Some(("alias", rest)) => (ResolutionRule::AliasBinding, rest),
        _ => (ResolutionRule::QualifiedPath, hint),
    }
}

fn parse(hint: &str) -> Option<Hint<'_>> {
    if let Some(key) = hint.strip_prefix("abs:") {
        return Some(Hint::Absolute(key));
    }
    if let Some(path) = hint.strip_prefix("py:") {
        return Some(Hint::Rooted(path));
    }
    let (root, path) = hint.strip_prefix("rel:")?.split_once('|')?;
    Some(Hint::Relative { root, path })
}

fn compatible(kind: RelationKind, target: EntityKind, language: Option<Language>) -> bool {
    match kind {
        // Calling a class is how Python constructs one, so a call may name a
        // type there. No other language in the set does this.
        RelationKind::Calls if language == Some(Language::Python) => matches!(
            target,
            EntityKind::Function | EntityKind::Method | EntityKind::Test | EntityKind::Type
        ),
        RelationKind::Calls => matches!(
            target,
            EntityKind::Function | EntityKind::Method | EntityKind::Test
        ),
        RelationKind::References => matches!(
            target,
            EntityKind::Type | EntityKind::Enum | EntityKind::Trait
        ),
        RelationKind::Implements => target == EntityKind::Trait,
        // What a path can import: a module or a named item. Never a file, an
        // impl block (which shares its type's key) or a method (not importable).
        RelationKind::Imports => importable(target),
        _ => false,
    }
}

pub(super) fn importable(target: EntityKind) -> bool {
    matches!(
        target,
        EntityKind::Module
            | EntityKind::Function
            | EntityKind::Type
            | EntityKind::Enum
            | EntityKind::Trait
            | EntityKind::Constant
            | EntityKind::Test
    )
}

/// `key` ends with `path` on a `::` segment boundary.
fn names(key: &str, path: &str) -> bool {
    key == path
        || key
            .strip_suffix(path)
            .is_some_and(|prefix| prefix.ends_with("::"))
}

fn in_crate(key: &str, root: &str) -> bool {
    key == root
        || key
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with("::"))
}

struct Candidate {
    id: String,
    kind: EntityKind,
    key: String,
    language: Option<Language>,
    /// The module a whole source file is, as opposed to a `mod x;` item that
    /// declares it from its parent. Both carry the module's key.
    file_module: bool,
}

fn unique<'c>(mut matches: impl Iterator<Item = &'c Candidate>) -> Option<&'c Candidate> {
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Exactly one candidate, or nothing. A relative path that matches several
/// declarations resolves only if exactly one lies in the source's own crate.
///
/// A path with no full match is left unresolved. agentctl has no verified
/// crate-identity metadata (no Cargo.toml/workspace parsing), so an unmatched
/// leading segment — e.g. `ext` in `ext::helpers::run` — cannot be assumed to
/// be a droppable crate name: it may equally be an unresolved external crate,
/// an unresolved re-export, or a typo. Dropping it and resolving whatever
/// suffix happens to be unique elsewhere in the workspace would fabricate a
/// structural relation with no evidence that the target is what the path
/// actually refers to; uniqueness after discarding information the resolver
/// does not understand is not proof of identity, so we abstain instead.
fn select<'c>(hint: &Hint<'_>, candidates: &[&'c Candidate]) -> Option<&'c Candidate> {
    let matching: Vec<_> = match *hint {
        Hint::Absolute(key) => candidates
            .iter()
            .copied()
            .filter(|c| c.key == key)
            .collect(),
        Hint::Rooted(path) => {
            let exact: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|c| c.key == path)
                .collect();
            if !exact.is_empty() || !path.contains("::") {
                exact
            } else {
                candidates
                    .iter()
                    .copied()
                    .filter(|c| names(&c.key, path))
                    .collect()
            }
        }
        Hint::Relative { root, path } => {
            let matching: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|c| names(&c.key, path))
                .collect();
            if matching.len() > 1 {
                matching
                    .into_iter()
                    .filter(|c| in_crate(&c.key, root))
                    .collect()
            } else {
                matching
            }
        }
    };
    match matching.as_slice() {
        [one] => Some(one),
        _ => one_module(&matching),
    }
}

/// `mod shapes;` in `lib.rs` and the file `shapes.rs` are one module observed
/// twice, under one key: the declaration and the body. When every match is
/// that module, the body is the target. Anything else stays ambiguous.
fn one_module<'c>(matching: &[&'c Candidate]) -> Option<&'c Candidate> {
    let key = &matching.first()?.key;
    if !matching
        .iter()
        .all(|c| c.kind == EntityKind::Module && &c.key == key)
    {
        return None;
    }
    unique(matching.iter().copied().filter(|c| c.file_module))
}

pub(super) fn rebuild(connection: &Connection, workspace: &str) -> Result<()> {
    connection.execute(
        "DELETE FROM graph_resolutions WHERE workspace_id=?1",
        [workspace],
    )?;
    // Every declaration, indexed by the last segment of its key. A key's leaf
    // is its name for items, and the module name for a file module (whose
    // entity *name* is its path), so one index serves calls and imports alike.
    let mut declarations: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut entities = connection.prepare(
        "SELECT entity_id,kind,json_extract(record_json,'$.key'),json_extract(record_json,'$.provenance.language'), \
                (SELECT p.kind FROM graph_entities p WHERE p.workspace_id=e.workspace_id \
                   AND p.entity_id=json_extract(e.record_json,'$.parent'))='\"FILE\"' \
         FROM graph_entities e WHERE workspace_id=?1 ORDER BY entity_id",
    )?;
    let mut rows = entities.query([workspace])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let candidate = Candidate {
            id: row.get(0)?,
            kind: serde_json::from_str(&row.get::<_, String>(1)?)?,
            language: row
                .get::<_, Option<String>>(3)?
                .and_then(|l| serde_json::from_str(&format!("\"{l}\"")).ok()),
            key,
            file_module: row.get::<_, Option<bool>>(4)?.unwrap_or(false),
        };
        let leaf = candidate.key.rsplit("::").next().unwrap_or_default();
        declarations
            .entry(leaf.to_string())
            .or_default()
            .push(candidate);
    }
    let mut edges = connection.prepare(
        "SELECT edge_id,source_id,kind,path_hint FROM graph_edges WHERE workspace_id=?1 AND target_id IS NULL AND path_hint IS NOT NULL ORDER BY edge_id",
    )?;
    let mut insert = connection.prepare(
        "INSERT INTO graph_resolutions(workspace_id,edge_id,source_id,target_id,kind,rule) VALUES (?1,?2,?3,?4,?5,?6)",
    )?;
    let mut rows = edges.query([workspace])?;
    while let Some(row) = rows.next()? {
        let (edge_id, source_id, kind_json, hint): (String, String, String, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        let kind: RelationKind = serde_json::from_str(&kind_json)?;
        let (rule, hint) = tagged(&hint);
        let Some(parsed) = parse(hint) else {
            continue;
        };
        let path = match parsed {
            Hint::Absolute(key) | Hint::Rooted(key) => key,
            Hint::Relative { path, .. } => path,
        };
        let name = path.rsplit("::").next().unwrap_or(path);
        let candidates: Vec<&Candidate> = declarations
            .get(name)
            .into_iter()
            .flatten()
            .filter(|c| c.id != source_id && compatible(kind, c.kind, c.language))
            .collect();
        if let Some(target) = select(&parsed, &candidates) {
            let rule = serde_json::to_value(rule)?;
            insert.execute(params![
                workspace,
                edge_id,
                source_id,
                target.id,
                kind_json,
                rule.as_str().unwrap_or("QUALIFIED_PATH")
            ])?;
        }
    }
    Ok(())
}
