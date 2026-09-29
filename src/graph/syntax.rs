//! What the syntax-only frontends share: parsing that refuses invalid
//! source, and building a [`Contribution`] of numbered entities, proven
//! `contains` relations and inferred relations to unresolved names.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use tree_sitter::{Language, Node as Syntax, Parser, Tree};

use super::{Contribution, EntityDef, EntityId, Evidence, External, Node, RelationDef, Span};

/// The syntax tree of `content`, unless it is not UTF-8 or Tree-sitter had to
/// recover from an error: its recovery makes a tree of any input, so only a
/// tree without errors is valid source.
pub(super) fn parse<'t>(language: &Language, content: &'t [u8]) -> Result<(&'t str, Tree)> {
    let text = std::str::from_utf8(content).context("content is not UTF-8")?;
    let mut parser = Parser::new();
    parser.set_language(language)?;
    let tree = parser.parse(text, None).context("parsing was cancelled")?;
    if let Some(error) = first_error(tree.root_node()) {
        bail!("syntax error at byte {}", error.start_byte());
    }
    Ok((text, tree))
}

/// The first error or missing node Tree-sitter recovered from, if any.
fn first_error(root: Syntax) -> Option<Syntax> {
    if !root.has_error() {
        return None;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .children(&mut cursor)
            .filter(|c| c.has_error())
            .collect();
        stack.extend(children.into_iter().rev());
    }
    Some(root)
}

pub(super) fn span(node: Syntax) -> Span {
    Span {
        start: node.start_byte() as u64,
        end: node.end_byte() as u64,
    }
}

pub(super) fn named_children(node: Syntax) -> impl DoubleEndedIterator<Item = Syntax> {
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    children.into_iter()
}

type Key = (Node, String, Node);

/// The entities and relations one source's frontend has found so far.
pub(super) struct Builder<'a> {
    pub(super) path: &'a str,
    entities: Vec<EntityDef>,
    /// How often each (kind, symbol) was defined, to number repeats.
    seen: HashMap<(&'static str, String), u32>,
    relations: BTreeMap<Key, (Evidence, Vec<Span>)>,
}

impl<'a> Builder<'a> {
    pub(super) fn new(path: &'a str) -> Self {
        Self {
            path,
            entities: Vec::new(),
            seen: HashMap::new(),
            relations: BTreeMap::new(),
        }
    }

    /// Defines an entity. A (kind, symbol) defined again gets `#2`, `#3`, …
    /// in source order.
    pub(super) fn define(&mut self, kind: &'static str, symbol: String, span: Span) -> EntityId {
        let n = self.seen.entry((kind, symbol.clone())).or_default();
        *n += 1;
        let symbol = match *n {
            1 => symbol,
            n => format!("{symbol}#{n}"),
        };
        self.entities.push(EntityDef {
            kind: kind.into(),
            symbol: symbol.clone(),
            span,
        });
        EntityId {
            path: self.path.into(),
            kind: kind.into(),
            symbol,
        }
    }

    pub(super) fn contains(&mut self, parent: &EntityId, child: &EntityId) {
        let key = (
            Node::Entity(parent.clone()),
            "contains".to_string(),
            Node::Entity(child.clone()),
        );
        self.relations.insert(key, (Evidence::Proven, Vec::new()));
    }

    /// An inferred relation to the name as written, evidenced at `site`.
    pub(super) fn unresolved(&mut self, from: &EntityId, kind: &str, to: External, site: Span) {
        let key = (
            Node::Entity(from.clone()),
            kind.to_string(),
            Node::External(to),
        );
        let (_, sites) = self
            .relations
            .entry(key)
            .or_insert((Evidence::Inferred, Vec::new()));
        if !sites.contains(&site) {
            sites.push(site);
        }
    }

    /// The contribution for content `hash`, which resolves against no other
    /// source.
    pub(super) fn finish(self, hash: &str, language: &str) -> Contribution {
        Contribution {
            path: self.path.into(),
            hash: hash.into(),
            language: language.into(),
            entities: self.entities,
            relations: self
                .relations
                .into_iter()
                .map(|((from, kind, to), (evidence, sites))| RelationDef {
                    from,
                    kind,
                    to,
                    evidence,
                    sites,
                })
                .collect(),
            resolved_against: BTreeMap::new(),
        }
    }
}

/// What every frontend's tests check the same way.
#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::{Fixture, current};
    use super::super::{Derivation, Direction, Evidence, Node, derive, replace};
    use crate::source;

    /// Accepts `content` as `path` and indexes it with its frontend.
    pub(crate) fn indexed(fx: &mut Fixture, path: &str, content: &str) {
        fx.accept(path, Some(content));
        index(fx, path);
    }

    /// Indexes the accepted content of `path`, which its frontend accepts.
    pub(crate) fn index(fx: &mut Fixture, path: &str) {
        match derive(&fx.project, &fx.store, path).unwrap() {
            Derivation::Indexed(c) => replace(&fx.project, &mut fx.store, &c).unwrap(),
            other => panic!("{path} is not indexed: {other:?}"),
        }
    }

    /// Each entity as `kind symbol`, with the accepted text at its span.
    pub(crate) fn entities(fx: &Fixture, path: &str) -> Vec<(String, String)> {
        let content = source::read_accepted(&fx.project, &fx.store, path).unwrap();
        current(fx.store.entities(path).unwrap())
            .into_iter()
            .map(|e| {
                let span = e.location.span.start as usize..e.location.span.end as usize;
                let text = std::str::from_utf8(&content.bytes()[span]).unwrap();
                (format!("{} {}", e.id.kind, e.id.symbol), text.to_string())
            })
            .collect()
    }

    /// Every relation from `path`'s entities as `from --kind--> to`, naming
    /// an unresolved target by the name as written. Checks the evidence
    /// policy of each, and that each is qualified by `path` in `ecosystem`.
    pub(crate) fn relations(fx: &Fixture, path: &str, ecosystem: &str) -> Vec<String> {
        let mut found = Vec::new();
        for e in current(fx.store.entities(path).unwrap()) {
            let from = Node::Entity(e.id.clone());
            for r in current(fx.relations(&from, Direction::Outgoing)).current {
                if r.from != from {
                    continue;
                }
                let to = match &r.to {
                    Node::Entity(id) => {
                        assert_eq!(
                            (r.kind.as_str(), r.evidence),
                            ("contains", Evidence::Proven)
                        );
                        assert_eq!(id.path, path);
                        id.symbol.clone()
                    }
                    Node::External(x) => {
                        assert_eq!(r.evidence, Evidence::Inferred, "{}", x.name);
                        assert_eq!(x.ecosystem.as_deref(), Some(ecosystem));
                        assert_eq!(x.namespace.as_deref(), Some(path));
                        x.name.clone()
                    }
                };
                found.push(format!("{} --{}--> {to}", e.id.symbol, r.kind));
            }
        }
        found.sort();
        found.dedup();
        found
    }

    pub(crate) fn sorted<T: Ord>(items: impl IntoIterator<Item = T>) -> Vec<T> {
        let mut items: Vec<T> = items.into_iter().collect();
        items.sort();
        items
    }

    /// Fails unless `found` is exactly `expected`, in any order.
    pub(crate) fn same(found: Vec<String>, expected: &[&str]) {
        let expected = sorted(expected.iter().map(|e| e.to_string()));
        assert_eq!(sorted(found), expected);
    }
}
