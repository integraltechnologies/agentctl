//! CodeGraph: persistent, language-neutral facts about accepted source.
//!
//! A language frontend derives facts from one source's accepted content and
//! submits them as a [`Contribution`]: the entities the source defines and
//! the direct relations it asserts, located by byte ranges of that content.
//! [`replace`] validates a contribution and atomically makes it the source's
//! graph, only if the content it was derived from is still accepted.
//!
//! Facts are bound to accepted content, never to working-tree bytes. A
//! source's facts are current while the content they were derived from is
//! still accepted, and stale once other content, or absence, is. `Store`
//! queries give facts only as current and say explicitly when they cannot.
//!
//! A contribution owns its entities, relations and their sites; replacing or
//! removing it replaces or removes them all. Relations name entities by
//! identity, never by storage row, so no source's replacement leaves another
//! source's relations dangling. A relation naming an entity of another
//! source was resolved against that source's accepted content, so it is
//! current only while both that content and its own source's content are
//! still accepted. Changing either makes it stale until its own source is
//! indexed again; nothing is re-indexed transitively.
//!
//! Only direct relations are stored. Impact is derived by traversal, each
//! hop keeping the relation it followed.

pub mod rust;

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use anyhow::{Context, Result, ensure};

use crate::project::Project;
use crate::source;
use crate::state::{Store, check_hash, check_path};

/// An entity's logical identity: the source defining it, a kind, and a
/// symbol its frontend makes unique among entities of that kind in the
/// source, such as a qualified name. It survives edits that keep these, but
/// not arbitrary refactors.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId {
    pub path: String,
    pub kind: String,
    pub symbol: String,
}

/// A symbol a relation names without resolving it to a project entity: one
/// outside the project, or one its frontend knows only by how it is written.
/// Its source is never indexed. Equal values are one node, so a frontend
/// qualifies a name it did not resolve by where it was written, keeping
/// unrelated uses of the same text apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct External {
    /// Such as a package registry or language, if known.
    pub ecosystem: Option<String>,
    /// The package, module or namespace holding it, if known.
    pub namespace: Option<String>,
    pub name: String,
}

/// An end of a relation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Node {
    Entity(EntityId),
    External(External),
}

/// How strongly a frontend established a relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evidence {
    Proven,
    Inferred,
}

/// A half-open byte range of a source's accepted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

/// A byte range of exactly the accepted content `hash` of `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub hash: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub id: EntityId,
    pub language: String,
    pub location: Location,
}

/// A current direct relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub from: Node,
    pub kind: String,
    pub to: Node,
    pub evidence: Evidence,
    /// The source whose contribution asserts it.
    pub owner: String,
    /// Where the owner's content evidences it.
    pub sites: Vec<Location>,
}

/// The direct relations at a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relations {
    pub current: Vec<Relation>,
    /// Sources asserting relations at the node that are no longer current;
    /// what those say now is unknown until they are indexed again.
    pub stale: Vec<String>,
}

/// Graph facts of a source, given only while they are current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness<T> {
    /// Derived from the source's accepted content.
    Current(T),
    /// Derived from content that is no longer accepted.
    Stale,
    /// The source has accepted content but no graph.
    Unindexed,
    /// The source is accepted as absent and has no graph.
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
}

/// Limits of a traversal.
#[derive(Debug, Clone)]
pub struct Bounds {
    /// The most hops between the start and any node reached.
    pub depth: u32,
    /// Relation kinds to follow; all when empty.
    pub kinds: Vec<String>,
    /// The most hops to return.
    pub limit: usize,
}

/// A relation followed `depth` hops from the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub depth: u32,
    pub relation: Relation,
}

/// The current relations reachable from a node, breadth first. Every hop
/// starts at the start node or at a node an earlier hop reached, so each
/// reached node's path from the start can be read back hop by hop. Each node
/// is expanded once; a hop back to a reached node shows a cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Traversal {
    pub hops: Vec<Hop>,
    /// Reached entities whose relations were not followed because their
    /// source's graph is not current, and why.
    pub unexpanded: Vec<(EntityId, Freshness<()>)>,
    /// Sources asserting relations at expanded nodes that are no longer
    /// current.
    pub stale: Vec<String>,
    /// Whether `Bounds::limit` cut the traversal short.
    pub truncated: bool,
}

/// The graph a frontend derived from the accepted content `hash` of `path`.
#[derive(Debug, Clone)]
pub struct Contribution {
    pub path: String,
    pub hash: String,
    pub language: String,
    pub entities: Vec<EntityDef>,
    pub relations: Vec<RelationDef>,
    /// The accepted content hash of each other source whose entities the
    /// relations name, as the frontend resolved them.
    pub resolved_against: BTreeMap<String, String>,
}

/// An entity the contributing source defines.
#[derive(Debug, Clone)]
pub struct EntityDef {
    pub kind: String,
    pub symbol: String,
    pub span: Span,
}

/// A direct relation the contributing source asserts. At least one end must
/// be an entity it defines.
#[derive(Debug, Clone)]
pub struct RelationDef {
    pub from: Node,
    pub kind: String,
    pub to: Node,
    pub evidence: Evidence,
    pub sites: Vec<Span>,
}

/// Makes `contribution` the graph of its source, replacing any previous one
/// as a whole, provided its content is still accepted, as is the content of
/// every source it was resolved against. Otherwise nothing changes.
pub fn replace(project: &Project, store: &mut Store, contribution: &Contribution) -> Result<()> {
    let c = contribution;
    let check = || {
        check_path(&c.path)?;
        check_hash(&c.hash)?;
        validate(c, source::content_len(project, &c.hash)?)
    };
    check().with_context(|| format!("invalid graph contribution for `{}`", c.path))?;
    store.replace_graph(c)
}

/// Checks everything about a contribution that does not depend on canonical
/// state, given the length of its content.
fn validate(c: &Contribution, len: u64) -> Result<()> {
    token(&c.language, "language")?;
    for (path, hash) in &c.resolved_against {
        check_path(path)?;
        check_hash(hash)?;
        ensure!(
            *path != c.path,
            "a source cannot be resolved against itself"
        );
    }
    let mut defined = HashSet::new();
    for e in &c.entities {
        token(&e.kind, "entity kind")?;
        text(&e.symbol, "symbol")?;
        span(e.span, len)?;
        ensure!(
            defined.insert((e.kind.as_str(), e.symbol.as_str())),
            "{} `{}` is defined twice",
            e.kind,
            e.symbol
        );
    }
    let mut resolved = HashSet::new();
    let mut asserted = HashSet::new();
    for r in &c.relations {
        let relation = format!("relation {} --{}--> {}", r.from, r.kind, r.to);
        token(&r.kind, "relation kind").context(relation.clone())?;
        let mut local = false;
        for node in [&r.from, &r.to] {
            match node {
                Node::Entity(id) if id.path == c.path => {
                    ensure!(
                        defined.contains(&(id.kind.as_str(), id.symbol.as_str())),
                        "{relation} names {id}, which this source does not define"
                    );
                    local = true;
                }
                Node::Entity(id) => {
                    check_path(&id.path)?;
                    token(&id.kind, "entity kind")?;
                    text(&id.symbol, "symbol")?;
                    ensure!(
                        c.resolved_against.contains_key(&id.path),
                        "{relation} names {id} without the content it was resolved against"
                    );
                    resolved.insert(id.path.as_str());
                }
                Node::External(x) => {
                    text(&x.name, "external name")?;
                    for part in [&x.namespace, &x.ecosystem].into_iter().flatten() {
                        text(part, "external namespace or ecosystem")?;
                    }
                }
            }
        }
        ensure!(local, "{relation} names no entity this source defines");
        ensure!(
            asserted.insert((&r.from, &r.kind, &r.to)),
            "{relation} is asserted twice"
        );
        let mut sites = HashSet::new();
        for &site in &r.sites {
            span(site, len).context(relation.clone())?;
            ensure!(sites.insert(site), "{relation} repeats a site");
        }
    }
    for path in c.resolved_against.keys() {
        ensure!(
            resolved.contains(path.as_str()),
            "no relation names an entity of `{path}`, which it claims to be resolved against"
        );
    }
    Ok(())
}

/// Kinds and languages are lowercase identifiers, so every frontend spells
/// them one way.
fn token(value: &str, what: &str) -> Result<()> {
    let mut chars = value.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    ensure!(valid, "`{value}` is not a valid {what}");
    Ok(())
}

fn text(value: &str, what: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "{what} `{value}` is empty or contains NUL"
    );
    Ok(())
}

fn span(span: Span, len: u64) -> Result<()> {
    ensure!(
        span.start <= span.end && span.end <= len,
        "span {}..{} is not within the {len} bytes of content",
        span.start,
        span.end
    );
    Ok(())
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}` in `{}`", self.kind, self.symbol, self.path)
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entity(id) => id.fmt(f),
            Self::External(x) => {
                f.write_str("external `")?;
                for part in [&x.ecosystem, &x.namespace].into_iter().flatten() {
                    write!(f, "{part}:")?;
                }
                write!(f, "{}`", x.name)
            }
        }
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Proven => "proven",
            Self::Inferred => "inferred",
        })
    }
}

/// Graph facts reach the store only through [`replace`], which validates
/// them against the accepted content first. Other crates cannot write them
/// directly:
///
/// ```compile_fail
/// # fn f(store: &mut agentctl::state::Store, c: &agentctl::graph::Contribution) {
/// store.replace_graph(c);
/// # }
/// ```
///
/// but can submit them:
///
/// ```no_run
/// # fn f(p: &agentctl::project::Project, store: &mut agentctl::state::Store, c: &agentctl::graph::Contribution) {
/// agentctl::graph::replace(p, store, c).unwrap();
/// # }
/// ```
#[cfg(doctest)]
pub struct GraphWritesAreValidated;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::tests::sample;
    use crate::source::accept_generation;
    use crate::state::tests::objective;
    use Freshness::{Absent, Current, Stale, Unindexed};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    const LANGUAGE: &str = "synthetic";

    pub(crate) struct Fixture {
        _dir: TempDir,
        pub(crate) project: Project,
        pub(crate) store: Store,
    }

    impl Fixture {
        /// A Git repository holding a project with the given source roots.
        pub(crate) fn new(roots: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let status = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["init", "-q"])
                .status()
                .unwrap();
            assert!(status.success());
            let mut config = sample();
            config.codegraph.roots = roots.parse().unwrap();
            let project = Project::create(dir.path(), config).unwrap();
            let store = project.hydrate().unwrap();
            Self {
                _dir: dir,
                project,
                store,
            }
        }

        /// Writes `path` (or removes it, for `None`) and accepts the result
        /// through a generation, returning the accepted hash.
        pub(crate) fn accept(&mut self, path: &str, content: Option<&str>) -> Option<String> {
            let file = self.project.root.join(path);
            match content {
                Some(content) => {
                    fs::create_dir_all(file.parent().unwrap()).unwrap();
                    fs::write(&file, content).unwrap();
                }
                None => fs::remove_file(&file).unwrap(),
            }
            let plan = self.store.create_plan(&objective("change")).unwrap();
            let task = self.store.add_task(plan, "change", &[]).unwrap();
            let generation = self.store.start_generation(task).unwrap();
            accept_generation(&self.project, &mut self.store, generation, &[path]).unwrap();
            self.store.accepted_source(path).unwrap().unwrap().hash
        }

        fn replace(&mut self, c: &Contribution) -> Result<()> {
            replace(&self.project, &mut self.store, c)
        }

        pub(crate) fn reopen(self) -> Self {
            let Self {
                _dir,
                project,
                store,
            } = self;
            drop(store);
            let project = Project::load(&project.root).unwrap();
            let store = project.hydrate().unwrap();
            Self {
                _dir,
                project,
                store,
            }
        }

        pub(crate) fn status(&self, path: &str) -> Freshness<()> {
            self.store.graph_status(path).unwrap()
        }

        pub(crate) fn relations(&self, node: &Node, direction: Direction) -> Freshness<Relations> {
            self.store.relations(node, direction).unwrap()
        }

        fn traverse(&self, start: &Node, direction: Direction, depth: u32) -> Traversal {
            let bounds = Bounds {
                depth,
                kinds: Vec::new(),
                limit: 100,
            };
            self.store.traverse(start, direction, &bounds).unwrap()
        }

        pub(crate) fn rows(&self, table: &str) -> i64 {
            self.store
                .raw()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        }
    }

    /// Distinct content of 64 bytes, enough for every span used here.
    fn content(tag: &str) -> String {
        format!("{tag:<64}")
    }

    fn id(path: &str, kind: &str, symbol: &str) -> EntityId {
        EntityId {
            path: path.into(),
            kind: kind.into(),
            symbol: symbol.into(),
        }
    }

    fn entity(path: &str, symbol: &str) -> Node {
        Node::Entity(id(path, "function", symbol))
    }

    fn external(namespace: &str, name: &str) -> Node {
        Node::External(External {
            ecosystem: Some("pypi".into()),
            namespace: Some(namespace.into()),
            name: name.into(),
        })
    }

    fn def(symbol: &str, start: u64) -> EntityDef {
        EntityDef {
            kind: "function".into(),
            symbol: symbol.into(),
            span: Span {
                start,
                end: start + 4,
            },
        }
    }

    fn rel(from: &Node, kind: &str, to: &Node, evidence: Evidence) -> RelationDef {
        RelationDef {
            from: from.clone(),
            kind: kind.into(),
            to: to.clone(),
            evidence,
            sites: Vec::new(),
        }
    }

    fn contribution(
        path: &str,
        hash: &Option<String>,
        entities: Vec<EntityDef>,
        relations: Vec<RelationDef>,
        resolved_against: &[(&str, &Option<String>)],
    ) -> Contribution {
        Contribution {
            path: path.into(),
            hash: hash.clone().unwrap(),
            language: LANGUAGE.into(),
            entities,
            relations,
            resolved_against: resolved_against
                .iter()
                .map(|&(path, hash)| (path.to_string(), hash.clone().unwrap()))
                .collect(),
        }
    }

    pub(crate) fn current<T: fmt::Debug>(facts: Freshness<T>) -> T {
        match facts {
            Current(facts) => facts,
            other => panic!("not current: {other:?}"),
        }
    }

    /// Each relation as (depth or 0, from, kind, to, evidence), by symbol.
    fn summary<'a>(
        relations: impl IntoIterator<Item = (u32, &'a Relation)>,
    ) -> Vec<(u32, String, String, String, Evidence)> {
        let symbol = |node: &Node| match node {
            Node::Entity(id) => id.symbol.clone(),
            Node::External(x) => x.name.clone(),
        };
        relations
            .into_iter()
            .map(|(depth, r)| {
                (
                    depth,
                    symbol(&r.from),
                    r.kind.clone(),
                    symbol(&r.to),
                    r.evidence,
                )
            })
            .collect()
    }

    fn hops(t: &Traversal) -> Vec<(u32, String, String, String, Evidence)> {
        summary(t.hops.iter().map(|h| (h.depth, &h.relation)))
    }

    pub(crate) fn fails(result: Result<impl fmt::Debug>, expected: &str) {
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains(expected), "{message}");
    }

    const A: &str = "src/a.rs";
    const B: &str = "src/b.rs";
    const C: &str = "src/c.rs";

    #[test]
    fn sources_are_unindexed_until_a_contribution_makes_them_current() {
        let mut fx = Fixture::new("src");
        let h = fx.accept(A, Some(&content("a")));
        assert_eq!(fx.status(A), Unindexed);
        assert_eq!(fx.store.entities(A).unwrap(), Unindexed);
        assert_eq!(fx.store.entity(&id(A, "function", "f")).unwrap(), Unindexed);
        assert_eq!(
            fx.relations(&entity(A, "f"), Direction::Outgoing),
            Unindexed
        );
        fails(
            fx.store.graph_status("src/untracked.rs"),
            "no accepted state",
        );

        // One symbol may name entities of different kinds.
        let mut s = def("f", 10);
        s.kind = "struct".into();
        fx.replace(&contribution(A, &h, vec![def("f", 0), s], vec![], &[]))
            .unwrap();
        assert_eq!(fx.status(A), Current(()));
        let f = Entity {
            id: id(A, "function", "f"),
            language: LANGUAGE.into(),
            location: Location {
                path: A.into(),
                hash: h.clone().unwrap(),
                span: Span { start: 0, end: 4 },
            },
        };
        assert_eq!(
            fx.store.entity(&id(A, "function", "f")).unwrap(),
            Current(Some(f.clone()))
        );
        let entities = current(fx.store.entities(A).unwrap());
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0], f);
        assert_eq!(entities[1].id, id(A, "struct", "f"));
        assert_eq!(
            fx.store.entity(&id(A, "function", "g")).unwrap(),
            Current(None)
        );

        // An unaccepted working-tree change (an executor's candidate) is not
        // accepted source, so the graph stays current.
        fs::write(fx.project.root.join(A), "candidate bytes").unwrap();
        assert_eq!(fx.status(A), Current(()));
        assert_eq!(
            fx.store.entity(&id(A, "function", "f")).unwrap(),
            Current(Some(f))
        );
        fs::remove_file(fx.project.root.join(A)).unwrap();
        assert_eq!(fx.status(A), Current(()));
    }

    #[test]
    fn accepted_changes_make_facts_stale_until_indexed_again() {
        let mut fx = Fixture::new("src");
        let h1 = fx.accept(A, Some(&content("a1")));
        let hb = fx.accept(B, Some(&content("b")));
        fx.replace(&contribution(A, &h1, vec![def("f", 0)], vec![], &[]))
            .unwrap();
        fx.replace(&contribution(B, &hb, vec![def("g", 0)], vec![], &[]))
            .unwrap();

        let h2 = fx.accept(A, Some(&content("a2")));
        assert_eq!(fx.status(A), Stale);
        assert_eq!(fx.store.entities(A).unwrap(), Stale);
        assert_eq!(fx.store.entity(&id(A, "function", "f")).unwrap(), Stale);
        assert_eq!(fx.relations(&entity(A, "f"), Direction::Incoming), Stale);
        let walk = fx.traverse(&entity(A, "f"), Direction::Outgoing, 3);
        assert!(walk.hops.is_empty());
        assert_eq!(walk.unexpanded, [(id(A, "function", "f"), Stale)]);
        assert_eq!(fx.status(B), Current(()));

        fx.replace(&contribution(A, &h2, vec![def("f2", 4)], vec![], &[]))
            .unwrap();
        assert_eq!(fx.status(A), Current(()));
        assert_eq!(
            fx.store.entity(&id(A, "function", "f")).unwrap(),
            Current(None)
        );
        let entities = current(fx.store.entities(A).unwrap());
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].location.hash, h2.unwrap());
    }

    #[test]
    fn stale_input_is_never_published() {
        let mut fx = Fixture::new("src");
        let h1 = fx.accept(A, Some(&content("a1")));
        let indexed_from_h1 = contribution(A, &h1, vec![def("f", 0)], vec![], &[]);
        // Accepted source changes while the frontend is working.
        let h2 = fx.accept(A, Some(&content("a2")));
        fails(fx.replace(&indexed_from_h1), "stale");
        assert_eq!(fx.status(A), Unindexed);

        // So does a source the contribution was resolved against.
        let hb1 = fx.accept(B, Some(&content("b1")));
        let resolved = contribution(
            A,
            &h2,
            vec![def("f", 0)],
            vec![rel(
                &entity(A, "f"),
                "calls",
                &entity(B, "g"),
                Evidence::Proven,
            )],
            &[(B, &hb1)],
        );
        fx.accept(B, Some(&content("b2")));
        fails(fx.replace(&resolved), "stale");
        assert_eq!(fx.status(A), Unindexed);

        // Content accepted at another path is not this path's content, and
        // content never accepted anywhere has no recovery object.
        fails(
            fx.replace(&contribution(A, &hb1, vec![], vec![], &[])),
            "stale",
        );
        let never = Some(format!("{:064x}", 7));
        fails(
            fx.replace(&contribution(A, &never, vec![], vec![], &[])),
            "unavailable",
        );
        fails(
            fx.replace(&contribution("src/untracked.rs", &h2, vec![], vec![], &[])),
            "no accepted state",
        );
        assert_eq!(fx.rows("graph_sources"), 0);
        let indexed = fx
            .store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind.starts_with("graph."))
            .count();
        assert_eq!(indexed, 0);
    }

    #[test]
    fn replacement_is_all_or_nothing() {
        let mut fx = Fixture::new("src");
        let h = fx.accept(A, Some(&content("a")));
        let (f, g) = (entity(A, "f"), entity(A, "g"));
        let mut calls = rel(&f, "calls", &g, Evidence::Proven);
        calls.sites = vec![Span { start: 16, end: 20 }];
        fx.replace(&contribution(
            A,
            &h,
            vec![def("f", 0), def("g", 8)],
            vec![calls],
            &[],
        ))
        .unwrap();
        let snapshot = |fx: &Fixture| {
            (
                fx.store.entities(A).unwrap(),
                fx.relations(&f, Direction::Outgoing),
                [
                    "graph_sources",
                    "graph_entities",
                    "graph_relations",
                    "graph_sites",
                ]
                .map(|table| fx.rows(table)),
            )
        };
        let before = snapshot(&fx);

        // A failure after part of the new contribution has been written.
        fx.store
            .raw()
            .execute_batch(
                "CREATE TEMP TRIGGER fail BEFORE INSERT ON graph_relations
                 WHEN NEW.kind = 'explodes'
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
            )
            .unwrap();
        let replacement = contribution(
            A,
            &h,
            vec![def("x", 0), def("y", 4), def("z", 8)],
            vec![
                rel(&entity(A, "x"), "calls", &entity(A, "y"), Evidence::Proven),
                rel(
                    &entity(A, "y"),
                    "explodes",
                    &entity(A, "z"),
                    Evidence::Proven,
                ),
            ],
            &[],
        );
        fails(fx.replace(&replacement), "injected failure");
        assert_eq!(snapshot(&fx), before);

        // Malformed input changes nothing either.
        let mut malformed = replacement.clone();
        malformed.relations[1].kind = "Explodes".into();
        fails(fx.replace(&malformed), "not a valid relation kind");
        assert_eq!(snapshot(&fx), before);

        fx.store
            .raw()
            .execute_batch("DROP TRIGGER temp.fail")
            .unwrap();
        fx.replace(&replacement).unwrap();
        assert_eq!(current(fx.store.entities(A).unwrap()).len(), 3);
        assert_eq!(fx.rows("graph_relations"), 2);
        assert_eq!(fx.rows("graph_sites"), 0);
    }

    #[test]
    fn locations_are_checked_against_verified_content_only() {
        let mut fx = Fixture::new("src");
        let h = fx.accept(A, Some("abc"));
        let object = fx
            .project
            .root
            .join(crate::project::STATE_DIR)
            .join("objects")
            .join(h.as_ref().unwrap());
        let at = |start, end| {
            let mut e = def("f", 0);
            e.span = Span { start, end };
            contribution(A, &h, vec![e], vec![], &[])
        };
        fx.replace(&at(0, 3)).unwrap();
        fails(fx.replace(&at(9, 10)), "not within");
        let snapshot = |fx: &Fixture| {
            (
                fx.status(A),
                fx.store.entities(A).unwrap(),
                ["graph_sources", "graph_entities"].map(|table| fx.rows(table)),
            )
        };
        let before = snapshot(&fx);
        assert_eq!(before.0, Current(()));

        // Neither longer nor same-length bytes under the accepted name are the
        // accepted content, and neither is a missing object.
        for (bytes, span, expected) in [
            (Some("0123456789"), (9, 10), "corrupt"),
            (Some("xyz"), (0, 3), "corrupt"),
            (None, (0, 3), "unavailable"),
        ] {
            fs::remove_file(&object).unwrap_or(());
            if let Some(bytes) = bytes {
                fs::write(&object, bytes).unwrap();
            }
            fails(fx.replace(&at(span.0, span.1)), expected);
            assert_eq!(snapshot(&fx), before, "{expected}");
        }

        // Nothing was repaired or remembered: the true bytes validate again.
        fs::write(&object, "abc").unwrap();
        fx.replace(&at(1, 3)).unwrap();
        let entities = current(fx.store.entities(A).unwrap());
        assert_eq!(entities[0].location.span, Span { start: 1, end: 3 });
    }

    #[test]
    fn literal_paths_round_trip() {
        let mut fx = Fixture::new("src, app/(customer)/s/[slug]");
        let paths = [
            "app/(customer)/s/[slug]/actions.ts",
            "src/[id].ts",
            "src/(group)/[slug]/page.tsx",
            "src/with space.rs",
            "src/a+b.rs",
            "src/ünïcødé/文件.rs",
            "src/*.rs",
            "src/%.ts",
            "src/_.ts",
        ];
        // What each literal path would match as a glob, LIKE or regex.
        let decoys = [
            "src/(group)/s/page.tsx",
            "src/i.ts",
            "src/a.rs",
            "src/ab.ts",
            "src/x.ts",
            "src/aab.rs",
        ];
        let hashes: Vec<_> = paths
            .iter()
            .map(|path| fx.accept(path, Some(&content(path))))
            .collect();
        for decoy in decoys {
            fx.accept(decoy, Some(&content(decoy)));
        }
        // Each source defines an entity whose symbol is its own path, and
        // calls the next source's entity, around a ring.
        for (i, path) in paths.iter().enumerate() {
            let next = (i + 1) % paths.len();
            fx.replace(&contribution(
                path,
                &hashes[i],
                vec![def(path, 0)],
                vec![rel(
                    &entity(path, path),
                    "calls",
                    &entity(paths[next], paths[next]),
                    Evidence::Proven,
                )],
                &[(paths[next], &hashes[next])],
            ))
            .unwrap();
        }

        let check = |fx: &Fixture| {
            for (i, path) in paths.iter().enumerate() {
                assert_eq!(fx.status(path), Current(()), "{path}");
                let entities = current(fx.store.entities(path).unwrap());
                assert_eq!(entities.len(), 1, "{path}");
                assert_eq!(entities[0].id, id(path, "function", path));
                assert_eq!(entities[0].location.path, *path);
                let previous = paths[(i + paths.len() - 1) % paths.len()];
                let incoming = current(fx.relations(&entity(path, path), Direction::Incoming));
                assert_eq!(incoming.current.len(), 1, "{path}");
                assert_eq!(incoming.current[0].owner, previous);
                assert_eq!(incoming.current[0].from, entity(previous, previous));
            }
            for decoy in decoys {
                assert_eq!(fx.status(decoy), Unindexed, "{decoy}");
                assert_eq!(fx.store.entities(decoy).unwrap(), Unindexed, "{decoy}");
            }
            let walk = fx.traverse(&entity(paths[0], paths[0]), Direction::Outgoing, 99);
            let reached: Vec<_> = walk
                .hops
                .iter()
                .map(|hop| match &hop.relation.to {
                    Node::Entity(id) => id.path.as_str(),
                    Node::External(_) => unreachable!(),
                })
                .collect();
            let mut ring = paths[1..].to_vec();
            ring.push(paths[0]);
            assert_eq!(reached, ring);
        };
        check(&fx);
        let fx = fx.reopen();
        check(&fx);
    }

    #[test]
    fn malformed_contributions_are_rejected() {
        let mut fx = Fixture::new("src");
        let ha = fx.accept(A, Some(&content("a")));
        let hb = fx.accept(B, Some(&content("b")));
        let (f, g) = (entity(A, "f"), entity(A, "g"));
        let valid = contribution(
            A,
            &ha,
            vec![def("f", 0), def("g", 8)],
            vec![
                rel(&f, "calls", &g, Evidence::Proven),
                rel(&f, "calls", &entity(B, "h"), Evidence::Inferred),
                rel(&g, "calls", &external("numpy", "mean"), Evidence::Proven),
            ],
            &[(B, &hb)],
        );
        let span = |start, end| Span { start, end };
        type Change = Box<dyn Fn(&mut Contribution)>;
        let cases: Vec<(&str, Change)> = vec![
            ("defined twice", Box::new(|c| c.entities.push(def("f", 20)))),
            (
                "is empty",
                Box::new(|c| c.entities[0].symbol = String::new()),
            ),
            (
                "contains NUL",
                Box::new(|c| c.entities[0].symbol = "f\0".into()),
            ),
            (
                "not a valid entity kind",
                Box::new(|c| c.entities[0].kind = "Function".into()),
            ),
            (
                "not a valid language",
                Box::new(|c| c.language = String::new()),
            ),
            (
                "not a valid relation kind",
                Box::new(|c| c.relations[0].kind = "calls-into".into()),
            ),
            (
                "does not define",
                Box::new(move |c| c.relations[0].to = entity(A, "nope")),
            ),
            (
                "names no entity this source defines",
                Box::new(|c| {
                    c.relations[2].from = external("flask", "route");
                }),
            ),
            (
                "without the content it was resolved against",
                Box::new(|c| c.resolved_against.clear()),
            ),
            (
                "claims to be resolved against",
                Box::new(|c| {
                    c.resolved_against.insert(C.into(), format!("{:064x}", 1));
                }),
            ),
            (
                "resolved against itself",
                Box::new(|c| {
                    let own = c.hash.clone();
                    c.resolved_against.insert(A.into(), own);
                }),
            ),
            (
                "asserted twice",
                Box::new(|c| {
                    let mut again = c.relations[0].clone();
                    again.evidence = Evidence::Inferred;
                    c.relations.push(again);
                }),
            ),
            (
                "not a canonical",
                Box::new(|c| {
                    c.relations[1].to = entity("src/../b.rs", "h");
                    c.resolved_against =
                        [("src/../b.rs".to_string(), format!("{:064x}", 1))].into();
                }),
            ),
            ("not a SHA-256", Box::new(|c| c.hash = "abc".into())),
            (
                "not within",
                Box::new(move |c| c.entities[0].span = span(5, 4)),
            ),
            (
                "not within",
                Box::new(move |c| c.entities[0].span = span(60, 65)),
            ),
            (
                "not within",
                Box::new(move |c| c.relations[0].sites = vec![span(64, 70)]),
            ),
            (
                "repeats a site",
                Box::new(move |c| c.relations[0].sites = vec![span(1, 2), span(1, 2)]),
            ),
            (
                "external name",
                Box::new(|c| c.relations[2].to = external("numpy", "")),
            ),
            (
                "namespace or ecosystem",
                Box::new(|c| c.relations[2].to = external("", "mean")),
            ),
        ];
        for (expected, change) in cases {
            let mut c = valid.clone();
            change(&mut c);
            fails(fx.replace(&c), expected);
            assert_eq!(fx.status(A), Unindexed, "{expected}");
        }
        assert_eq!(fx.rows("graph_sources"), 0);

        // Empty spans, including at the very end of the content, are exact.
        let mut edges = valid.clone();
        edges.entities[1].span = span(64, 64);
        edges.relations[0].sites = vec![span(0, 0), span(0, 64)];
        fx.replace(&edges).unwrap();
        assert_eq!(fx.status(A), Current(()));
    }

    #[test]
    fn relations_keep_evidence_locations_and_external_targets() {
        let mut fx = Fixture::new("src");
        let ha = fx.accept(A, Some(&content("a")));
        let hb = fx.accept(B, Some(&content("b")));
        let (f, g, h) = (entity(A, "f"), entity(A, "g"), entity(B, "h"));
        let (mean, route) = (external("numpy", "mean"), external("flask", "route"));
        fx.replace(&contribution(
            B,
            &hb,
            vec![def("h", 0)],
            vec![rel(&h, "calls", &mean, Evidence::Inferred)],
            &[],
        ))
        .unwrap();
        let mut calls = rel(&f, "calls", &g, Evidence::Proven);
        calls.sites = vec![Span { start: 24, end: 28 }, Span { start: 16, end: 20 }];
        fx.replace(&contribution(
            A,
            &ha,
            vec![def("f", 0), def("g", 8)],
            vec![
                calls,
                rel(&f, "uses", &h, Evidence::Inferred),
                rel(&route, "calls", &g, Evidence::Proven),
                rel(&g, "calls", &mean, Evidence::Proven),
            ],
            &[(B, &hb)],
        ))
        .unwrap();
        let fx = fx.reopen();

        let site = |start, end| Location {
            path: A.into(),
            hash: ha.clone().unwrap(),
            span: Span { start, end },
        };
        let outgoing = current(fx.relations(&f, Direction::Outgoing));
        assert!(outgoing.stale.is_empty());
        assert_eq!(
            outgoing.current,
            [
                Relation {
                    from: f.clone(),
                    kind: "calls".into(),
                    to: g.clone(),
                    evidence: Evidence::Proven,
                    owner: A.into(),
                    sites: vec![site(16, 20), site(24, 28)],
                },
                Relation {
                    from: f.clone(),
                    kind: "uses".into(),
                    to: h.clone(),
                    evidence: Evidence::Inferred,
                    owner: A.into(),
                    sites: vec![],
                },
            ]
        );
        let incoming = |node| {
            let found = current(fx.relations(node, Direction::Incoming));
            summary(found.current.iter().map(|r| (0, r)))
        };
        let row = |from: &str, kind: &str, to: &str, evidence| {
            (
                0,
                from.to_string(),
                kind.to_string(),
                to.to_string(),
                evidence,
            )
        };
        use Evidence::{Inferred, Proven};
        assert_eq!(
            incoming(&g),
            [
                row("f", "calls", "g", Proven),
                row("route", "calls", "g", Proven)
            ]
        );
        assert_eq!(incoming(&h), [row("f", "uses", "h", Inferred)]);
        // External symbols are matched by value across sources, and never
        // need their own source indexed.
        assert_eq!(
            incoming(&mean),
            [
                row("h", "calls", "mean", Inferred),
                row("g", "calls", "mean", Proven)
            ]
        );
        let elsewhere = external("scipy", "mean");
        assert!(incoming(&elsewhere).is_empty());
        let unqualified = Node::External(External {
            ecosystem: None,
            namespace: Some("numpy".into()),
            name: "mean".into(),
        });
        assert!(incoming(&unqualified).is_empty());
        let from_route = current(fx.relations(&route, Direction::Outgoing));
        assert_eq!(from_route.current[0].to, g);
        assert_eq!(from_route.current[0].owner, A);
    }

    #[test]
    fn traversal_derives_impact_from_direct_hops() {
        let mut fx = Fixture::new("src");
        let ha = fx.accept(A, Some(&content("a")));
        let hb = fx.accept(B, Some(&content("b")));
        let hc = fx.accept(C, Some(&content("c")));
        let (a, inner, b, c) = (
            entity(A, "a"),
            entity(A, "inner"),
            entity(B, "b"),
            entity(C, "c"),
        );
        use Evidence::{Inferred, Proven};
        fx.replace(&contribution(
            A,
            &ha,
            vec![def("a", 0), def("inner", 8)],
            vec![
                rel(&a, "calls", &b, Proven),
                rel(&a, "contains", &inner, Proven),
            ],
            &[(B, &hb)],
        ))
        .unwrap();
        fx.replace(&contribution(
            B,
            &hb,
            vec![def("b", 0)],
            vec![rel(&b, "uses", &c, Inferred)],
            &[(C, &hc)],
        ))
        .unwrap();
        // Closes a cycle back to `a`.
        fx.replace(&contribution(
            C,
            &hc,
            vec![def("c", 0)],
            vec![rel(&c, "calls", &a, Proven)],
            &[(A, &ha)],
        ))
        .unwrap();
        let hop = |depth, from: &str, kind: &str, to: &str, evidence| {
            (
                depth,
                from.to_string(),
                kind.to_string(),
                to.to_string(),
                evidence,
            )
        };

        let walk = fx.traverse(&a, Direction::Outgoing, 10);
        assert_eq!(
            hops(&walk),
            [
                hop(1, "a", "calls", "b", Proven),
                hop(1, "a", "contains", "inner", Proven),
                hop(2, "b", "uses", "c", Inferred),
                hop(3, "c", "calls", "a", Proven),
            ]
        );
        assert!(walk.unexpanded.is_empty() && walk.stale.is_empty() && !walk.truncated);
        // Every hop is the complete, current relation, with its sites.
        assert_eq!(walk.hops[2].relation.owner, B);

        let walk = fx.traverse(&a, Direction::Incoming, 10);
        assert_eq!(
            hops(&walk),
            [
                hop(1, "c", "calls", "a", Proven),
                hop(2, "b", "uses", "c", Inferred),
                hop(3, "a", "calls", "b", Proven),
            ]
        );

        assert_eq!(fx.traverse(&a, Direction::Outgoing, 1).hops.len(), 2);
        assert!(fx.traverse(&a, Direction::Outgoing, 0).hops.is_empty());
        let only_calls = Bounds {
            depth: 10,
            kinds: vec!["calls".into()],
            limit: 100,
        };
        let walk = fx
            .store
            .traverse(&a, Direction::Outgoing, &only_calls)
            .unwrap();
        assert_eq!(hops(&walk), [hop(1, "a", "calls", "b", Proven)]);
        let limited = Bounds {
            depth: 10,
            kinds: Vec::new(),
            limit: 2,
        };
        let walk = fx
            .store
            .traverse(&a, Direction::Outgoing, &limited)
            .unwrap();
        assert_eq!(walk.hops.len(), 2);
        assert!(walk.truncated);

        // Only the three submitted direct relations exist; impact such as
        // `a` reaching `c` is derived, never stored.
        assert_eq!(fx.rows("graph_relations"), 4);
        let a_to_c: i64 = fx
            .store
            .raw()
            .query_row(
                "SELECT count(*) FROM graph_relations WHERE from_symbol = 'a' AND to_symbol = 'c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_to_c, 0);
    }

    #[test]
    fn cross_source_relations_track_both_sources() {
        let mut fx = Fixture::new("src");
        let ha1 = fx.accept(A, Some(&content("a1")));
        let hb1 = fx.accept(B, Some(&content("b1")));
        let (f, g) = (entity(A, "f"), entity(B, "g"));
        let a_calls_g = |fx: &mut Fixture, ha: &Option<String>, hb: &Option<String>| {
            fx.replace(&contribution(
                A,
                ha,
                vec![def("f", 0)],
                vec![rel(&f, "calls", &g, Evidence::Proven)],
                &[(B, hb)],
            ))
        };
        let index_b = |fx: &mut Fixture, hb: &Option<String>| {
            fx.replace(&contribution(B, hb, vec![def("g", 0)], vec![], &[]))
        };
        let owners = |relations: Freshness<Relations>| {
            let relations = current(relations);
            let current: Vec<_> = relations.current.into_iter().map(|r| r.owner).collect();
            (current, relations.stale)
        };
        let none: Vec<String> = Vec::new();
        let a = vec![A.to_string()];
        index_b(&mut fx, &hb1).unwrap();
        a_calls_g(&mut fx, &ha1, &hb1).unwrap();
        assert_eq!(
            owners(fx.relations(&g, Direction::Incoming)),
            (a.clone(), none.clone())
        );

        // A changes: its relation into B is no longer current.
        let ha2 = fx.accept(A, Some(&content("a2")));
        assert_eq!(
            owners(fx.relations(&g, Direction::Incoming)),
            (none.clone(), a.clone())
        );
        assert_eq!(fx.relations(&f, Direction::Outgoing), Stale);
        a_calls_g(&mut fx, &ha2, &hb1).unwrap();
        assert_eq!(
            owners(fx.relations(&g, Direction::Incoming)),
            (a.clone(), none.clone())
        );

        // B changes: A's relation was resolved against B's old content, so
        // it is stale even though A itself did not change, and stays so
        // after B alone is indexed again.
        let hb2 = fx.accept(B, Some(&content("b2")));
        assert_eq!(fx.status(A), Current(()));
        assert_eq!(fx.relations(&g, Direction::Incoming), Stale);
        assert_eq!(
            owners(fx.relations(&f, Direction::Outgoing)),
            (none.clone(), a.clone())
        );
        let walk = fx.traverse(&f, Direction::Outgoing, 5);
        assert!(walk.hops.is_empty());
        assert_eq!(walk.stale, a);
        index_b(&mut fx, &hb2).unwrap();
        assert_eq!(
            owners(fx.relations(&g, Direction::Incoming)),
            (none.clone(), a.clone())
        );
        a_calls_g(&mut fx, &ha2, &hb2).unwrap();
        assert_eq!(
            owners(fx.relations(&g, Direction::Incoming)),
            (a.clone(), none.clone())
        );

        // A relation into a source that is accepted but not indexed is
        // current; the traversal says where it could not continue.
        let hc = fx.accept(C, Some(&content("c")));
        let k = entity(C, "k");
        fx.replace(&contribution(
            A,
            &ha2,
            vec![def("f", 0)],
            vec![
                rel(&f, "calls", &g, Evidence::Proven),
                rel(&f, "calls", &k, Evidence::Inferred),
            ],
            &[(B, &hb2), (C, &hc)],
        ))
        .unwrap();
        let walk = fx.traverse(&f, Direction::Outgoing, 5);
        assert_eq!(walk.hops.len(), 2);
        assert_eq!(walk.unexpanded, [(id(C, "function", "k"), Unindexed)]);

        // B becomes absent: relations into it are stale, and its facts go
        // once reconciled, while A's own facts are undisturbed.
        fx.accept(B, None);
        assert_eq!(fx.status(B), Stale);
        let (current_owners, stale) = owners(fx.relations(&f, Direction::Outgoing));
        assert_eq!((current_owners, stale), (a.clone(), a.clone()));
        assert!(fx.store.remove_absent_graph(B).unwrap());
        assert_eq!(fx.status(B), Absent);
        assert_eq!(fx.relations(&g, Direction::Incoming), Absent);
        assert_eq!(fx.status(A), Current(()));
        assert_eq!(current(fx.store.entities(A).unwrap()).len(), 1);
        fails(a_calls_g(&mut fx, &ha2, &hb2), "accepted as absent");
    }

    #[test]
    fn reconciling_absence_disturbs_no_other_source() {
        let mut fx = Fixture::new("src");
        let ha = fx.accept(A, Some(&content("a")));
        let hb = fx.accept(B, Some(&content("b")));
        let a_contribution = contribution(A, &ha, vec![def("f", 0)], vec![], &[]);
        fx.replace(&a_contribution).unwrap();
        let g = entity(B, "g");
        fx.replace(&contribution(
            B,
            &hb,
            vec![def("g", 0)],
            vec![rel(
                &g,
                "calls",
                &external("numpy", "mean"),
                Evidence::Proven,
            )],
            &[],
        ))
        .unwrap();
        let b_before = (
            fx.store.entities(B).unwrap(),
            fx.relations(&g, Direction::Outgoing),
        );

        fails(fx.store.remove_absent_graph(A), "has accepted content");
        fails(
            fx.store.remove_absent_graph("src/untracked.rs"),
            "no accepted state",
        );
        fx.accept(A, None);
        assert_eq!(fx.status(A), Stale);
        assert_eq!(fx.store.entities(A).unwrap(), Stale);
        // Absence is not an empty file: nothing can be indexed for it.
        fails(fx.replace(&a_contribution), "accepted as absent");

        assert!(fx.store.remove_absent_graph(A).unwrap());
        assert!(!fx.store.remove_absent_graph(A).unwrap());
        assert_eq!(fx.status(A), Absent);
        assert_eq!(fx.store.entities(A).unwrap(), Absent);
        assert_eq!(fx.store.entity(&id(A, "function", "f")).unwrap(), Absent);
        assert_eq!(
            (
                fx.store.entities(B).unwrap(),
                fx.relations(&g, Direction::Outgoing),
            ),
            b_before
        );
        assert_eq!(fx.rows("graph_sources"), 1);
        assert_eq!(fx.rows("graph_entities"), 1);
        let fx = fx.reopen();
        assert_eq!(fx.status(A), Absent);
        assert_eq!(fx.status(B), Current(()));
    }
}
