//! Workspace-level resolution of Rust qualified paths (`module::f`, `Type::f`,
//! `crate::`/`super::`/`self::` paths into other files). Derived only from the
//! committed extraction facts, rebuilt in the index transaction whenever the graph
//! generation changes, and kept apart from `graph_edges` so re-deriving one file
//! can never cascade away another file's relations.
//!
//! Cost: one pass over hinted unresolved edges plus one indexed name lookup per
//! distinct target name, O(hinted edges + candidate entities) per changing pass.
use super::*;

enum Hint<'a> {
    /// A fully anchored key (`self::`, `super::`, `crate::` paths).
    Absolute(&'a str),
    /// A relative path, resolved by unique key suffix; `root` is the source's crate.
    Relative { root: &'a str, path: &'a str },
}

fn parse(hint: &str) -> Option<Hint<'_>> {
    if let Some(key) = hint.strip_prefix("abs:") {
        return Some(Hint::Absolute(key));
    }
    let (root, path) = hint.strip_prefix("rel:")?.split_once('|')?;
    Some(Hint::Relative { root, path })
}

fn compatible(kind: RelationKind, target: EntityKind) -> bool {
    match kind {
        RelationKind::Calls => matches!(
            target,
            EntityKind::Function | EntityKind::Method | EntityKind::Test
        ),
        RelationKind::References => matches!(
            target,
            EntityKind::Type | EntityKind::Enum | EntityKind::Trait
        ),
        RelationKind::Implements => target == EntityKind::Trait,
        _ => false,
    }
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
    match *hint {
        Hint::Absolute(key) => unique(candidates.iter().copied().filter(|c| c.key == key)),
        Hint::Relative { root, path } => {
            let matching: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|c| names(&c.key, path))
                .collect();
            match matching.len() {
                1 => Some(matching[0]),
                0 => None,
                _ => unique(matching.into_iter().filter(|c| in_crate(&c.key, root))),
            }
        }
    }
}

pub(super) fn rebuild(connection: &Connection, workspace: &str) -> Result<()> {
    connection.execute(
        "DELETE FROM graph_resolutions WHERE workspace_id=?1",
        [workspace],
    )?;
    let mut edges = connection.prepare(
        "SELECT edge_id,source_id,kind,path_hint FROM graph_edges WHERE workspace_id=?1 AND target_id IS NULL AND path_hint IS NOT NULL ORDER BY edge_id",
    )?;
    let mut by_name = connection.prepare(
        "SELECT entity_id,kind,json_extract(record_json,'$.key') FROM graph_entities WHERE workspace_id=?1 AND name=?2 ORDER BY entity_id",
    )?;
    let mut insert = connection.prepare(
        "INSERT INTO graph_resolutions(workspace_id,edge_id,source_id,target_id,kind,rule) VALUES (?1,?2,?3,?4,?5,'QUALIFIED_PATH')",
    )?;
    let mut declarations: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut rows = edges.query([workspace])?;
    while let Some(row) = rows.next()? {
        let (edge_id, source_id, kind_json, hint): (String, String, String, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        let kind: RelationKind = serde_json::from_str(&kind_json)?;
        let Some(parsed) = parse(&hint) else {
            continue;
        };
        let path = match parsed {
            Hint::Absolute(key) => key,
            Hint::Relative { path, .. } => path,
        };
        let name = path.rsplit("::").next().unwrap_or(path);
        if !declarations.contains_key(name) {
            let found = by_name
                .query_map(params![workspace, name], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .map(|r| {
                    let (id, kind, key) = r?;
                    Ok(Candidate {
                        id,
                        kind: serde_json::from_str(&kind)?,
                        key: key.unwrap_or_default(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            declarations.insert(name.to_string(), found);
        }
        let candidates: Vec<&Candidate> = declarations[name]
            .iter()
            .filter(|c| c.id != source_id && compatible(kind, c.kind))
            .collect();
        if let Some(target) = select(&parsed, &candidates) {
            insert.execute(params![workspace, edge_id, source_id, target.id, kind_json])?;
        }
    }
    Ok(())
}
