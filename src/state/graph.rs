//! CodeGraph persistence and queries. Every query reads one snapshot and
//! checks the facts it gives against the accepted source in that snapshot.

use std::collections::{BTreeSet, HashSet, VecDeque};

use anyhow::{Result, bail, ensure};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{Store, event};
use crate::graph::{
    Bounds, Contribution, Direction, Entity, EntityId, Evidence, External, Freshness, Hop,
    Location, Node, Relation, Relations, Span, Traversal,
};

/// A source's current graph contribution.
struct Indexed {
    id: i64,
    hash: String,
    language: String,
}

impl Store {
    /// Replaces the graph of a validated contribution's source, provided the
    /// content it and every source it was resolved against were derived
    /// from is still accepted.
    pub(crate) fn replace_graph(&mut self, c: &Contribution) -> Result<()> {
        self.write(|tx| {
            expect_accepted(tx, &c.path, &c.hash)?;
            for (path, hash) in &c.resolved_against {
                expect_accepted(tx, path, hash)?;
            }
            tx.execute("DELETE FROM graph_sources WHERE path = ?1", [&c.path])?;
            tx.execute(
                "INSERT INTO graph_sources (path, hash, language) VALUES (?1, ?2, ?3)",
                params![c.path, c.hash, c.language],
            )?;
            let source = tx.last_insert_rowid();
            let mut entity = tx.prepare(
                "INSERT INTO graph_entities (source_id, kind, symbol, span_start, span_end)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in &c.entities {
                let (start, end) = offsets(e.span)?;
                entity.execute(params![source, e.kind, e.symbol, start, end])?;
            }
            let mut relation = tx.prepare(
                "INSERT INTO graph_relations (source_id, kind, evidence,
                   from_path, from_kind, from_symbol, from_namespace, from_ecosystem,
                   to_path, to_kind, to_symbol, to_namespace, to_ecosystem, foreign_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            let mut site = tx.prepare(
                "INSERT INTO graph_sites (relation_id, span_start, span_end) VALUES (?1, ?2, ?3)",
            )?;
            for r in &c.relations {
                let foreign = [&r.from, &r.to].into_iter().find_map(|node| match node {
                    Node::Entity(id) if id.path != c.path => Some(&c.resolved_against[&id.path]),
                    _ => None,
                });
                let (fp, fk, fs, fn_, fe) = columns(&r.from);
                let (tp, tk, ts, tn, te) = columns(&r.to);
                relation.execute(params![
                    source, r.kind, r.evidence, fp, fk, fs, fn_, fe, tp, tk, ts, tn, te, foreign
                ])?;
                let id = tx.last_insert_rowid();
                for &span in &r.sites {
                    let (start, end) = offsets(span)?;
                    site.execute(params![id, start, end])?;
                }
            }
            event(tx, "graph.indexed", None, None, None, &c.path)
        })
    }

    /// Removes the graph of a source accepted as absent, returning whether
    /// it had one. Other sources' relations to its entities become stale.
    pub fn remove_absent_graph(&mut self, path: &str) -> Result<bool> {
        self.write(|tx| {
            match accepted(tx, path)? {
                None => bail!("`{path}` has no accepted state"),
                Some(Some(_)) => bail!("`{path}` has accepted content; replace its graph instead"),
                Some(None) => {}
            }
            let removed = tx.execute("DELETE FROM graph_sources WHERE path = ?1", [path])? > 0;
            if removed {
                event(tx, "graph.removed", None, None, None, path)?;
            }
            Ok(removed)
        })
    }

    /// Whether a tracked source's graph is current.
    pub fn graph_status(&self, path: &str) -> Result<Freshness<()>> {
        status(&self.conn, path)
    }

    /// The entity `id` names, if its source's current graph defines one.
    pub fn entity(&self, id: &EntityId) -> Result<Freshness<Option<Entity>>> {
        let tx = self.conn.unchecked_transaction()?;
        current(source(&tx, &id.path)?, |src| {
            tx.query_row(
                "SELECT span_start, span_end FROM graph_entities
                 WHERE source_id = ?1 AND kind = ?2 AND symbol = ?3",
                params![src.id, id.kind, id.symbol],
                |r| span(r, 0),
            )
            .optional()
            .map(|span| {
                span.map(|span| entity(&id.path, &src, id.kind.clone(), id.symbol.clone(), span))
            })
            .map_err(Into::into)
        })
    }

    /// Every entity a source's current graph defines, in source order.
    pub fn entities(&self, path: &str) -> Result<Freshness<Vec<Entity>>> {
        let tx = self.conn.unchecked_transaction()?;
        current(source(&tx, path)?, |src| {
            let rows = tx
                .prepare(
                    "SELECT kind, symbol, span_start, span_end FROM graph_entities
                     WHERE source_id = ?1 ORDER BY span_start, span_end, kind, symbol",
                )?
                .query_map([src.id], |r| Ok((r.get(0)?, r.get(1)?, span(r, 2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(kind, symbol, span)| entity(path, &src, kind, symbol, span))
                .collect())
        })
    }

    /// The direct relations from (`Outgoing`) or to (`Incoming`) a node. An
    /// entity's relations are known only while its source's graph is
    /// current; an external symbol's are always known.
    pub fn relations(&self, node: &Node, direction: Direction) -> Result<Freshness<Relations>> {
        let tx = self.conn.unchecked_transaction()?;
        current(expandable(&tx, node)?, |()| relations(&tx, node, direction))
    }

    /// Follows current direct relations from `start` within `bounds`.
    pub fn traverse(
        &self,
        start: &Node,
        direction: Direction,
        bounds: &Bounds,
    ) -> Result<Traversal> {
        let tx = self.conn.unchecked_transaction()?;
        let mut walk = Traversal::default();
        let mut stale = BTreeSet::new();
        let mut reached = HashSet::from([start.clone()]);
        let mut queue = VecDeque::from([(start.clone(), 0)]);
        'walk: while let Some((node, depth)) = queue.pop_front() {
            if depth == bounds.depth {
                continue;
            }
            let found = match expandable(&tx, &node)? {
                Freshness::Current(()) => relations(&tx, &node, direction)?,
                status => {
                    if let Node::Entity(id) = node {
                        walk.unexpanded.push((id, status));
                    }
                    continue;
                }
            };
            stale.extend(found.stale);
            for relation in found.current {
                if !bounds.kinds.is_empty() && !bounds.kinds.contains(&relation.kind) {
                    continue;
                }
                if walk.hops.len() == bounds.limit {
                    walk.truncated = true;
                    break 'walk;
                }
                let next = match direction {
                    Direction::Outgoing => &relation.to,
                    Direction::Incoming => &relation.from,
                };
                if reached.insert(next.clone()) {
                    queue.push_back((next.clone(), depth + 1));
                }
                walk.hops.push(Hop {
                    depth: depth + 1,
                    relation,
                });
            }
        }
        walk.stale = stale.into_iter().collect();
        Ok(walk)
    }
}

/// The accepted state of a tracked path: `Some(None)` for accepted absence.
fn accepted(conn: &Connection, path: &str) -> Result<Option<Option<String>>> {
    Ok(conn
        .query_row(
            "SELECT hash FROM accepted_sources WHERE path = ?1",
            [path],
            |r| r.get(0),
        )
        .optional()?)
}

/// Refuses graph input derived from anything but `path`'s accepted content.
fn expect_accepted(conn: &Connection, path: &str, hash: &str) -> Result<()> {
    match accepted(conn, path)? {
        None => bail!("`{path}` has no accepted state"),
        Some(None) => bail!("`{path}` is accepted as absent"),
        Some(Some(accepted)) => ensure!(
            accepted == hash,
            "graph input for `{path}` is stale: derived from {hash}, but {accepted} is accepted"
        ),
    }
    Ok(())
}

/// A tracked source's graph contribution, if current.
fn source(conn: &Connection, path: &str) -> Result<Freshness<Indexed>> {
    let row: Option<(Option<String>, Option<Indexed>)> = conn
        .query_row(
            "SELECT a.hash, s.id, s.hash, s.language FROM accepted_sources a
             LEFT JOIN graph_sources s ON s.path = a.path WHERE a.path = ?1",
            [path],
            |r| {
                let indexed = match r.get::<_, Option<i64>>(1)? {
                    Some(id) => Some(Indexed {
                        id,
                        hash: r.get(2)?,
                        language: r.get(3)?,
                    }),
                    None => None,
                };
                Ok((r.get(0)?, indexed))
            },
        )
        .optional()?;
    let Some((accepted, indexed)) = row else {
        bail!("`{path}` has no accepted state");
    };
    Ok(match (accepted, indexed) {
        (Some(accepted), Some(indexed)) if accepted == indexed.hash => Freshness::Current(indexed),
        (_, Some(_)) => Freshness::Stale,
        (Some(_), None) => Freshness::Unindexed,
        (None, None) => Freshness::Absent,
    })
}

fn status(conn: &Connection, path: &str) -> Result<Freshness<()>> {
    current(source(conn, path)?, |_| Ok(()))
}

/// Whether a node's relations can be known: those of an entity only while
/// its source's graph is current.
fn expandable(conn: &Connection, node: &Node) -> Result<Freshness<()>> {
    match node {
        Node::Entity(id) => status(conn, &id.path),
        Node::External(_) => Ok(Freshness::Current(())),
    }
}

/// Applies `then` to current facts, keeping any other freshness.
fn current<T, U>(facts: Freshness<T>, then: impl FnOnce(T) -> Result<U>) -> Result<Freshness<U>> {
    Ok(match facts {
        Freshness::Current(facts) => Freshness::Current(then(facts)?),
        Freshness::Stale => Freshness::Stale,
        Freshness::Unindexed => Freshness::Unindexed,
        Freshness::Absent => Freshness::Absent,
    })
}

/// The relations at `node`, split by whether they are current: asserted by
/// a source whose graph is current and, if they name another source's
/// entity, resolved against that source's accepted content.
fn relations(conn: &Connection, node: &Node, direction: Direction) -> Result<Relations> {
    let side = match direction {
        Direction::Outgoing => "from",
        Direction::Incoming => "to",
    };
    let sql = format!(
        "SELECT r.id, s.path, s.hash, r.kind, r.evidence,
           r.from_path, r.from_kind, r.from_symbol, r.from_namespace, r.from_ecosystem,
           r.to_path, r.to_kind, r.to_symbol, r.to_namespace, r.to_ecosystem,
           s.hash IS a.hash AND (r.foreign_hash IS NULL OR r.foreign_hash IS
             (SELECT f.hash FROM accepted_sources f WHERE f.path =
               CASE WHEN r.from_path <> s.path THEN r.from_path ELSE r.to_path END))
         FROM graph_relations r
         JOIN graph_sources s ON s.id = r.source_id
         JOIN accepted_sources a ON a.path = s.path
         WHERE r.{side}_symbol = ?3 AND r.{side}_path IS ?1 AND r.{side}_kind IS ?2
           AND r.{side}_namespace IS ?4 AND r.{side}_ecosystem IS ?5
         ORDER BY r.id"
    );
    let (path, kind, symbol, namespace, ecosystem) = columns(node);
    let mut sites = conn.prepare(
        "SELECT span_start, span_end FROM graph_sites WHERE relation_id = ?1
         ORDER BY span_start, span_end",
    )?;
    let mut found = Relations {
        current: Vec::new(),
        stale: Vec::new(),
    };
    let mut stale = BTreeSet::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![path, kind, symbol, namespace, ecosystem])?;
    while let Some(r) = rows.next()? {
        let owner: String = r.get(1)?;
        if !r.get::<_, bool>(15)? {
            stale.insert(owner);
            continue;
        }
        let hash: String = r.get(2)?;
        let sites = sites
            .query_map([r.get::<_, i64>(0)?], |s| span(s, 0))?
            .map(|span| {
                Ok(Location {
                    path: owner.clone(),
                    hash: hash.clone(),
                    span: span?,
                })
            })
            .collect::<rusqlite::Result<_>>()?;
        found.current.push(Relation {
            from: node_at(r, 5)?,
            kind: r.get(3)?,
            to: node_at(r, 10)?,
            evidence: r.get(4)?,
            owner,
            sites,
        });
    }
    found.stale = stale.into_iter().collect();
    Ok(found)
}

type Columns<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
);

/// A node as its relation columns: path, kind, symbol, namespace, ecosystem.
fn columns(node: &Node) -> Columns<'_> {
    match node {
        Node::Entity(id) => (Some(&id.path), Some(&id.kind), &id.symbol, None, None),
        Node::External(x) => (
            None,
            None,
            &x.name,
            x.namespace.as_deref(),
            x.ecosystem.as_deref(),
        ),
    }
}

fn node_at(r: &Row, at: usize) -> rusqlite::Result<Node> {
    let symbol = r.get(at + 2)?;
    Ok(match (r.get(at)?, r.get(at + 1)?) {
        (Some(path), Some(kind)) => Node::Entity(EntityId { path, kind, symbol }),
        _ => Node::External(External {
            namespace: r.get(at + 3)?,
            ecosystem: r.get(at + 4)?,
            name: symbol,
        }),
    })
}

fn entity(path: &str, src: &Indexed, kind: String, symbol: String, span: Span) -> Entity {
    Entity {
        id: EntityId {
            path: path.to_owned(),
            kind,
            symbol,
        },
        language: src.language.clone(),
        location: Location {
            path: path.to_owned(),
            hash: src.hash.clone(),
            span,
        },
    }
}

fn span(r: &Row, at: usize) -> rusqlite::Result<Span> {
    let offset = |i| {
        let n: i64 = r.get(i)?;
        u64::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, n))
    };
    Ok(Span {
        start: offset(at)?,
        end: offset(at + 1)?,
    })
}

fn offsets(span: Span) -> Result<(i64, i64)> {
    Ok((span.start.try_into()?, span.end.try_into()?))
}

impl ToSql for Evidence {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.to_string().into())
    }
}

impl FromSql for Evidence {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "proven" => Ok(Self::Proven),
            "inferred" => Ok(Self::Inferred),
            other => Err(FromSqlError::Other(
                format!("unknown evidence `{other}`").into(),
            )),
        }
    }
}

#[cfg(test)]
impl Store {
    pub(crate) fn raw(&self) -> &Connection {
        &self.conn
    }
}
