//! The Rust frontend: CodeGraph facts from one source's accepted Rust
//! content, by syntax alone. It parses with Tree-sitter and does no name
//! resolution, macro expansion or type inference, and never reads another
//! file: `mod foo;` is an entity, not a reason to open `foo.rs`.
//!
//! Entities, by kind, with symbols relative to the file's own module:
//! - `module`: `self` for the file's module (all of its content), `a` and
//!   `a::b` for the modules it declares, inline or not;
//! - `function`, `struct`, `enum`, `trait`, `type_alias`, `const`, `static`:
//!   the item's path, such as `a::f`;
//! - `impl`: its header, such as `impl Foo` or `a::impl Display for Foo`;
//! - `method`: any `fn` of an impl or trait, with or without `self`, as
//!   `Foo::new`, `<Foo as Display>::fmt` or `Trait::m`; associated types and
//!   consts are `type_alias` and `const` symbols of the same shape;
//! - `variant`: `E::V`; `field`: `S::x`, `S::0`, `E::V::x`.
//!
//! Written text in symbols keeps its tokens with whitespace collapsed. When
//! a symbol recurs within its kind, as `#[cfg]` alternatives do, later ones
//! get `#2`, `#3`, … in source order. Items inside function bodies or macro
//! invocations are not extracted.
//!
//! Relations:
//! - `contains` (proven): an entity to each entity lexically directly in it;
//! - `imports`: a module to each path its `use` declarations name;
//! - `calls`: a function, method, const or static to each callee path, or
//!   for a method call its receiver and name exactly as written, such as
//!   `self.items.get`, in its body;
//! - `constructs`: likewise, to the path of each struct literal;
//! - `references`: an entity to each type path in its signature, field type,
//!   aliased type, bounds or impl header;
//! - `implements` and `self_type`: an impl to its trait and self type path.
//!
//! All but `contains` name paths as written, which syntax cannot resolve:
//! imports, locals, generics, globs and macros decide what they denote. So
//! they are inferred, and their targets are `External` symbols in ecosystem
//! `rust`, named by the path as written and qualified by where it was
//! written: the source path, then `//` and the module, if not the file's
//! own (a canonical path never holds `//`). Paths starting with a generic
//! parameter are skipped, and `Self` becomes the impl's self type when that
//! is a plain path, or is skipped too.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use tree_sitter::{Node as Syntax, Parser};

use super::{Contribution, EntityDef, EntityId, Evidence, External, Node, RelationDef, Span};
use crate::project::Project;
use crate::source::{self, AcceptedContent};
use crate::state::Store;

const LANGUAGE: &str = "rust";

/// Indexes the accepted content of the Rust source `path`, replacing its
/// graph. Fails, changing nothing, if that content is not valid Rust or is
/// no longer accepted when the graph would be replaced.
pub fn index(project: &Project, store: &mut Store, path: &str) -> Result<()> {
    let content = source::read_accepted(project, store, path)?;
    let contribution =
        contribution(path, &content).with_context(|| format!("indexing `{path}` as Rust"))?;
    super::replace(project, store, &contribution)
}

fn contribution(path: &str, content: &AcceptedContent) -> Result<Contribution> {
    let text = std::str::from_utf8(content.bytes()).context("content is not UTF-8")?;
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser.parse(text, None).context("parsing was cancelled")?;
    let root = tree.root_node();
    if let Some(error) = first_error(root) {
        bail!("syntax error at byte {}", error.start_byte());
    }
    let mut x = Extractor {
        path,
        text,
        entities: Vec::new(),
        seen: HashMap::new(),
        relations: BTreeMap::new(),
    };
    let file = x.define("module", "self".into(), root);
    x.items(root, "", &file);
    Ok(Contribution {
        path: path.into(),
        hash: content.hash().into(),
        language: LANGUAGE.into(),
        entities: x.entities,
        relations: x
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
    })
}

/// The first error or missing node Tree-sitter recovered from, if any: its
/// recovery makes a tree of any input, so only one without them is Rust.
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

/// Where a path is written: the module it resolves in, generic parameters
/// that shadow it, and what `Self` stands for.
#[derive(Clone, Default)]
struct Scope<'s> {
    module: &'s str,
    generics: Vec<String>,
    self_path: Option<String>,
}

type Key = (Node, String, Node);

struct Extractor<'a> {
    path: &'a str,
    text: &'a str,
    entities: Vec<EntityDef>,
    /// How often each (kind, symbol) was defined, to number repeats.
    seen: HashMap<(&'static str, String), u32>,
    relations: BTreeMap<Key, (Evidence, Vec<Span>)>,
}

impl<'a> Extractor<'a> {
    /// The items of a module body: the file, or an inline module's block.
    fn items(&mut self, body: Syntax, module: &str, parent: &EntityId) {
        let scope = Scope {
            module,
            ..Scope::default()
        };
        for item in named_children(body) {
            let kind = match item.kind() {
                "use_declaration" => {
                    self.uses(item, parent, module);
                    continue;
                }
                "impl_item" => {
                    self.implementation(item, parent, module);
                    continue;
                }
                "mod_item" => "module",
                "function_item" => "function",
                "struct_item" => "struct",
                "enum_item" => "enum",
                "trait_item" => "trait",
                "type_item" => "type_alias",
                "const_item" => "const",
                "static_item" => "static",
                _ => continue,
            };
            let Some(name) = item.child_by_field_name("name") else {
                continue;
            };
            let symbol = qualify(module, &self.written(name));
            let id = self.define(kind, symbol, item);
            self.contains(parent, &id);
            let scope = scope.with_generics(self, item);
            match kind {
                "module" => {
                    if let Some(body) = item.child_by_field_name("body") {
                        self.items(body, &id.symbol, &id);
                    }
                }
                "struct" => {
                    self.signature(item, &id, &scope);
                    if let Some(body) = item.child_by_field_name("body") {
                        self.fields(body, &id, &scope);
                    }
                }
                "enum" => {
                    self.signature(item, &id, &scope);
                    let body = item.child_by_field_name("body");
                    for variant in body.into_iter().flat_map(named_children) {
                        let Some(name) = variant.child_by_field_name("name") else {
                            continue;
                        };
                        let symbol = format!("{}::{}", id.symbol, self.written(name));
                        let v = self.define("variant", symbol, variant);
                        self.contains(&id, &v);
                        if let Some(fields) = variant.child_by_field_name("body") {
                            self.fields(fields, &v, &scope);
                        }
                    }
                }
                "trait" => {
                    self.signature(item, &id, &scope);
                    if let Some(body) = item.child_by_field_name("body") {
                        let prefix = id.symbol.clone();
                        self.members(body, &prefix, &id, &scope);
                    }
                }
                _ => self.definition(item, &id, &scope),
            }
        }
    }

    /// An impl block: an entity for it, relations to its trait and self
    /// type, and its members.
    fn implementation(&mut self, item: Syntax, parent: &EntityId, module: &str) {
        let Some(ty) = item.child_by_field_name("type") else {
            return;
        };
        let target = self.written(ty);
        let trait_ = item.child_by_field_name("trait");
        let (header, prefix) = match trait_ {
            Some(t) => {
                let t = self.written(t);
                (
                    format!("impl {t} for {target}"),
                    format!("<{target} as {t}>"),
                )
            }
            None => (format!("impl {target}"), target),
        };
        let id = self.define("impl", qualify(module, &header), item);
        self.contains(parent, &id);
        let mut scope = Scope {
            module,
            ..Scope::default()
        }
        .with_generics(self, item);
        let own = outer_path(ty).and_then(|p| self.plain(p, &scope).map(|path| (p, path)));
        if let Some((node, path)) = &own {
            self.unresolved(&id, "self_type", path.clone(), &scope, *node);
        }
        let mut skip = own
            .map(|(node, _)| node.id())
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(t) = trait_
            && let Some(node) = outer_path(t)
            && let Some(path) = self.plain(node, &scope)
        {
            self.unresolved(&id, "implements", path, &scope, node);
            skip.push(node.id());
        }
        self.types(item, &id, &scope, &skip);
        scope.self_path = outer_path(ty)
            .filter(|p| p.kind() != "generic_type")
            .and_then(|p| self.plain(p, &scope));
        if let Some(body) = item.child_by_field_name("body") {
            self.members(body, &qualify(module, &prefix), &id, &scope);
        }
    }

    /// Functions, types and consts of an impl or trait body.
    fn members(&mut self, body: Syntax, prefix: &str, parent: &EntityId, scope: &Scope) {
        for item in named_children(body) {
            let kind = match item.kind() {
                "function_item" | "function_signature_item" => "method",
                "type_item" | "associated_type" => "type_alias",
                "const_item" => "const",
                _ => continue,
            };
            let Some(name) = item.child_by_field_name("name") else {
                continue;
            };
            let symbol = format!("{prefix}::{}", self.written(name));
            let id = self.define(kind, symbol, item);
            self.contains(parent, &id);
            let scope = scope.with_generics(self, item);
            self.definition(item, &id, &scope);
        }
    }

    /// A function, alias, const or static: the types it names, and what its
    /// body calls and constructs.
    fn definition(&mut self, item: Syntax, id: &EntityId, scope: &Scope) {
        self.signature(item, id, scope);
        if let Some(body) = item
            .child_by_field_name("body")
            .or_else(|| item.child_by_field_name("value"))
        {
            self.body(body, id, scope);
        }
    }

    /// References from the types an item names outside its body and fields.
    fn signature(&mut self, item: Syntax, id: &EntityId, scope: &Scope) {
        for part in named_children(item) {
            match part.kind() {
                "parameters" => {
                    for p in named_children(part) {
                        if let Some(ty) = p.child_by_field_name("type") {
                            self.types(ty, id, scope, &[]);
                        }
                    }
                }
                "type_parameters" | "where_clause" | "trait_bounds" => {
                    self.types(part, id, scope, &[]);
                }
                _ => {}
            }
        }
        for field in ["return_type", "type"] {
            if let Some(ty) = item.child_by_field_name(field) {
                self.types(ty, id, scope, &[]);
            }
        }
    }

    fn fields(&mut self, list: Syntax, owner: &EntityId, scope: &Scope) {
        if list.kind() == "ordered_field_declaration_list" {
            let mut cursor = list.walk();
            let types: Vec<_> = list.children_by_field_name("type", &mut cursor).collect();
            for (i, ty) in types.into_iter().enumerate() {
                let id = self.define("field", format!("{}::{i}", owner.symbol), ty);
                self.contains(owner, &id);
                self.types(ty, &id, scope, &[]);
            }
            return;
        }
        for field in named_children(list) {
            let (Some(name), Some(ty)) = (
                field.child_by_field_name("name"),
                field.child_by_field_name("type"),
            ) else {
                continue;
            };
            let symbol = format!("{}::{}", owner.symbol, self.written(name));
            let id = self.define("field", symbol, field);
            self.contains(owner, &id);
            self.types(ty, &id, scope, &[]);
        }
    }

    /// `references` to each type path within `node`, except nodes in `skip`.
    fn types(&mut self, node: Syntax, from: &EntityId, scope: &Scope, skip: &[usize]) {
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            if skip.contains(&node.id()) {
                continue;
            }
            match node.kind() {
                "type_identifier" | "scoped_type_identifier" => {
                    if let Some(path) = self.plain(node, scope) {
                        self.unresolved(from, "references", path, scope, node);
                        continue;
                    }
                    if node.kind() == "type_identifier" {
                        continue;
                    }
                    // Only a qualifier such as `<T as Trait>` or `Vec<T>`
                    // holds type paths; the name after it resolves through
                    // them.
                    stack.extend(node.child_by_field_name("path"));
                }
                // Items and bodies within types, such as a const argument's
                // block, are not signatures.
                "block" | "declaration_list" => {}
                _ => stack.extend(named_children(node).rev()),
            }
        }
    }

    /// `calls` and `constructs` in a body, not descending into nested items.
    fn body(&mut self, body: Syntax, from: &EntityId, scope: &Scope) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "call_expression" => {
                    if let Some(callee) = node.child_by_field_name("function") {
                        self.call(callee, from, scope);
                    }
                }
                "struct_expression" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let name = name.child_by_field_name("type").unwrap_or(name);
                        if let Some(path) = self.plain(name, scope) {
                            self.unresolved(from, "constructs", path, scope, name);
                        }
                    }
                }
                kind if kind.ends_with("_item")
                    || matches!(kind, "use_declaration" | "macro_definition") =>
                {
                    continue;
                }
                _ => {}
            }
            stack.extend(named_children(node).rev());
        }
    }

    fn call(&mut self, callee: Syntax, from: &EntityId, scope: &Scope) {
        let callee = match callee.kind() {
            "generic_function" => match callee.child_by_field_name("function") {
                Some(f) => f,
                None => return,
            },
            _ => callee,
        };
        match callee.kind() {
            "identifier" | "scoped_identifier" => {
                if let Some(path) = self.plain(callee, scope) {
                    self.unresolved(from, "calls", path, scope, callee);
                }
            }
            "field_expression" => {
                if let Some(name) = callee.child_by_field_name("field")
                    && name.kind() == "field_identifier"
                {
                    // Receiver and method exactly as written: syntax alone
                    // cannot tell `a.run` from `b.run`, so neither merges.
                    let method = self.text[callee.byte_range()].to_string();
                    self.unresolved(from, "calls", method, scope, name);
                }
            }
            _ => {}
        }
    }

    /// `imports` of each path a `use` declaration names.
    fn uses(&mut self, item: Syntax, module_id: &EntityId, module: &str) {
        let scope = Scope {
            module,
            ..Scope::default()
        };
        let mut stack: Vec<(Syntax, String)> = item
            .child_by_field_name("argument")
            .map(|a| (a, String::new()))
            .into_iter()
            .collect();
        while let Some((node, prefix)) = stack.pop() {
            let join = |x: &Self, n: Syntax| match (prefix.as_str(), x.path_text(n)) {
                (p, s) if s == "self" && !p.is_empty() => p.to_string(),
                ("", s) => s,
                (p, s) => format!("{p}::{s}"),
            };
            let path = match node.kind() {
                "use_as_clause" => match node.child_by_field_name("path") {
                    Some(p) => join(self, p),
                    None => continue,
                },
                "use_wildcard" => match node.named_child(0) {
                    Some(p) => format!("{}::*", join(self, p)),
                    None if prefix.is_empty() => "*".into(),
                    None => format!("{prefix}::*"),
                },
                "use_list" => {
                    let children = named_children(node).rev();
                    stack.extend(children.map(|c| (c, prefix.clone())));
                    continue;
                }
                "scoped_use_list" => {
                    let inner = match node.child_by_field_name("path") {
                        Some(p) => join(self, p),
                        None => prefix.clone(),
                    };
                    stack.extend(node.child_by_field_name("list").map(|l| (l, inner)));
                    continue;
                }
                _ => join(self, node),
            };
            self.unresolved(module_id, "imports", path, &scope, node);
        }
    }

    fn define(&mut self, kind: &'static str, symbol: String, node: Syntax) -> EntityId {
        let n = self.seen.entry((kind, symbol.clone())).or_default();
        *n += 1;
        let symbol = match *n {
            1 => symbol,
            n => format!("{symbol}#{n}"),
        };
        self.entities.push(EntityDef {
            kind: kind.into(),
            symbol: symbol.clone(),
            span: span(node),
        });
        EntityId {
            path: self.path.into(),
            kind: kind.into(),
            symbol,
        }
    }

    fn contains(&mut self, parent: &EntityId, child: &EntityId) {
        let key = (
            Node::Entity(parent.clone()),
            "contains".to_string(),
            Node::Entity(child.clone()),
        );
        self.relations.insert(key, (Evidence::Proven, Vec::new()));
    }

    /// An inferred relation to `name` as written in `scope`, evidenced at
    /// `site`.
    fn unresolved(
        &mut self,
        from: &EntityId,
        kind: &str,
        name: String,
        scope: &Scope,
        site: Syntax,
    ) {
        let namespace = match scope.module {
            "" => self.path.to_string(),
            module => format!("{}//{module}", self.path),
        };
        let to = Node::External(External {
            ecosystem: Some(LANGUAGE.into()),
            namespace: Some(namespace),
            name,
        });
        let key = (Node::Entity(from.clone()), kind.to_string(), to);
        let (_, sites) = self
            .relations
            .entry(key)
            .or_insert((Evidence::Inferred, Vec::new()));
        let site = span(site);
        if !sites.contains(&site) {
            sites.push(site);
        }
    }

    /// A path node's text, if it is a plain path (`a::b::C`, no generics or
    /// qualified types) whose meaning does not rest on a generic parameter;
    /// a leading `Self` becomes the self type, if known.
    fn plain(&self, node: Syntax, scope: &Scope) -> Option<String> {
        let path = self.path_text(node);
        let plain = path
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '#'));
        if !plain {
            return None;
        }
        let (first, rest) = match path.split_once("::") {
            Some((first, rest)) => (first, Some(rest)),
            None => (path.as_str(), None),
        };
        if first == "Self" {
            let self_path = scope.self_path.as_ref()?;
            return Some(match rest {
                Some(rest) => format!("{self_path}::{rest}"),
                None => self_path.clone(),
            });
        }
        (!scope.generics.iter().any(|g| g == first)).then_some(path)
    }

    /// A path's text without whitespace, which never separates its tokens.
    fn path_text(&self, node: Syntax) -> String {
        self.text[node.byte_range()].split_whitespace().collect()
    }

    /// Text as written, with whitespace collapsed.
    fn written(&self, node: Syntax) -> String {
        let text = &self.text[node.byte_range()];
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

impl Scope<'_> {
    /// This scope within an item, shadowed by the item's generic parameters.
    fn with_generics(&self, x: &Extractor, item: Syntax) -> Self {
        let mut scope = self.clone();
        let params = item.child_by_field_name("type_parameters");
        for param in params.into_iter().flat_map(named_children) {
            if param.kind() == "type_parameter"
                && let Some(name) = param.child_by_field_name("name")
            {
                scope.generics.push(x.written(name));
            }
        }
        scope
    }
}

/// The node naming a type's own path, without its generic arguments.
fn outer_path(ty: Syntax) -> Option<Syntax> {
    match ty.kind() {
        "type_identifier" | "scoped_type_identifier" => Some(ty),
        "generic_type" => ty.child_by_field_name("type").and_then(outer_path),
        _ => None,
    }
}

fn qualify(module: &str, name: &str) -> String {
    match module {
        "" => name.into(),
        module => format!("{module}::{name}"),
    }
}

fn span(node: Syntax) -> Span {
    Span {
        start: node.start_byte() as u64,
        end: node.end_byte() as u64,
    }
}

fn named_children(node: Syntax) -> impl DoubleEndedIterator<Item = Syntax> {
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    children.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::tests::{Fixture, current, fails};
    use crate::graph::{Direction, Freshness};
    use crate::project::STATE_DIR;
    use crate::state::tests::objective;
    use std::fs;

    const LIB: &str = "src/lib.rs";

    /// Several constructs together, with multi-byte text before and among
    /// declarations.
    const SAMPLE: &str = r#"//! Ünïcødé — “store” ✓
use std::collections::{self, HashMap as Map};
use crate::config::Config;

pub mod store;

fn größe() -> f64 { 0.0 }

/// Holds “items”.
pub struct Store<T: Clone> {
    items: Map<String, T>,
    config: Config,
}

pub struct Pair(pub u8, Box<dyn Fn(u8) -> Id>);

pub enum Event { Added { id: Id }, Removed(Id), Cleared }

pub trait Named: Clone {
    type Key;
    fn name(&self) -> String;
    fn shout(&self) -> String { Self::normalize(self.name()) }
}

impl<T: Clone> Store<T> {
    pub fn new(config: Config) -> Self {
        let items = Map::new();
        Self { items, config }
    }

    fn get(&self, key: &str) -> Option<&T> {
        helper(key);
        self.items.get(key)
    }
}

impl Pair {
    pub fn new() -> Self { Pair(0, Box::new(|_| 0)) }
}

impl std::fmt::Display for Pair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Named for Pair {
    type Key = Id;
    fn name(&self) -> String { Self::describe() }
}

type Id = u64;
const MAX: Id = compute(4);
static NAME: &str = "ñ";

fn helper<K: AsRef<str>>(key: K) -> Event {
    let _ = K::default();
    fn nested() { hidden() }
    Event::Added { id: parse::<Id>(key) }
}

mod inner {
    use super::helper;
    pub fn helper() { super::helper("x"); }
}

#[cfg(unix)]
fn platform() {}
#[cfg(not(unix))]
fn platform() {}
"#;

    fn index_at(fx: &mut Fixture, path: &str) -> Result<()> {
        index(&fx.project, &mut fx.store, path)
    }

    fn indexed(source: &str) -> Fixture {
        let mut fx = Fixture::new("src");
        fx.accept(LIB, Some(source));
        index_at(&mut fx, LIB).unwrap();
        fx
    }

    /// Each entity as `kind symbol`, with the accepted text at its span.
    fn entities(fx: &Fixture, path: &str) -> Vec<(String, String)> {
        let content = source::read_accepted(&fx.project, &fx.store, path).unwrap();
        current(fx.store.entities(path).unwrap())
            .into_iter()
            .map(|e| {
                let span = e.location.span.start as usize..e.location.span.end as usize;
                let text = std::str::from_utf8(&content.bytes()[span]).unwrap();
                let first = text.lines().next().unwrap_or_default();
                (format!("{} {}", e.id.kind, e.id.symbol), first.to_string())
            })
            .collect()
    }

    /// Every relation from `path`'s entities as `from --kind--> to`, naming
    /// an unresolved target by its written path and, if written in an inner
    /// module, `@module`. Checks the evidence policy of each on the way.
    fn relations(fx: &Fixture, path: &str) -> Vec<String> {
        let mut found = Vec::new();
        for e in current(fx.store.entities(path).unwrap()) {
            let from = Node::Entity(e.id.clone());
            let out = current(fx.relations(&from, Direction::Outgoing));
            for r in out.current {
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
                        assert_eq!(x.ecosystem.as_deref(), Some("rust"));
                        match x.namespace.as_deref().unwrap().split_once("//") {
                            None => {
                                assert_eq!(x.namespace.as_deref(), Some(path));
                                x.name.clone()
                            }
                            Some((p, module)) => {
                                assert_eq!(p, path);
                                format!("{}@{module}", x.name)
                            }
                        }
                    }
                };
                found.push(format!("{} --{}--> {to}", e.id.symbol, r.kind));
            }
        }
        found
    }

    fn sorted<T: Ord>(items: impl IntoIterator<Item = T>) -> Vec<T> {
        let mut items: Vec<T> = items.into_iter().collect();
        items.sort();
        items
    }

    fn has(found: &[String], expected: &[&str]) {
        for e in expected {
            assert!(found.iter().any(|f| f == e), "missing {e} in {found:#?}");
        }
    }

    #[test]
    fn items_get_distinct_symbols_and_exact_byte_spans() {
        let fx = indexed(SAMPLE);
        let expected = [
            ("module self", "//! Ünïcødé — “store” ✓"),
            ("module store", "pub mod store;"),
            ("function größe", "fn größe() -> f64 { 0.0 }"),
            ("struct Store", "pub struct Store<T: Clone> {"),
            ("field Store::items", "items: Map<String, T>"),
            ("field Store::config", "config: Config"),
            (
                "struct Pair",
                "pub struct Pair(pub u8, Box<dyn Fn(u8) -> Id>);",
            ),
            ("field Pair::0", "u8"),
            ("field Pair::1", "Box<dyn Fn(u8) -> Id>"),
            (
                "enum Event",
                "pub enum Event { Added { id: Id }, Removed(Id), Cleared }",
            ),
            ("variant Event::Added", "Added { id: Id }"),
            ("field Event::Added::id", "id: Id"),
            ("variant Event::Removed", "Removed(Id)"),
            ("field Event::Removed::0", "Id"),
            ("variant Event::Cleared", "Cleared"),
            ("trait Named", "pub trait Named: Clone {"),
            ("type_alias Named::Key", "type Key;"),
            ("method Named::name", "fn name(&self) -> String;"),
            (
                "method Named::shout",
                "fn shout(&self) -> String { Self::normalize(self.name()) }",
            ),
            ("impl impl Store<T>", "impl<T: Clone> Store<T> {"),
            (
                "method Store<T>::new",
                "pub fn new(config: Config) -> Self {",
            ),
            (
                "method Store<T>::get",
                "fn get(&self, key: &str) -> Option<&T> {",
            ),
            ("impl impl Pair", "impl Pair {"),
            (
                "method Pair::new",
                "pub fn new() -> Self { Pair(0, Box::new(|_| 0)) }",
            ),
            (
                "impl impl std::fmt::Display for Pair",
                "impl std::fmt::Display for Pair {",
            ),
            (
                "method <Pair as std::fmt::Display>::fmt",
                "fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
            ),
            ("impl impl Named for Pair", "impl Named for Pair {"),
            ("type_alias <Pair as Named>::Key", "type Key = Id;"),
            (
                "method <Pair as Named>::name",
                "fn name(&self) -> String { Self::describe() }",
            ),
            ("type_alias Id", "type Id = u64;"),
            ("const MAX", "const MAX: Id = compute(4);"),
            ("static NAME", "static NAME: &str = \"ñ\";"),
            (
                "function helper",
                "fn helper<K: AsRef<str>>(key: K) -> Event {",
            ),
            ("module inner", "mod inner {"),
            (
                "function inner::helper",
                "pub fn helper() { super::helper(\"x\"); }",
            ),
            ("function platform", "fn platform() {}"),
            ("function platform#2", "fn platform() {}"),
        ];
        let expected = expected.map(|(e, text)| (e.to_string(), text.to_string()));
        assert_eq!(sorted(entities(&fx, LIB)), sorted(expected));
        // The file module spans exactly the accepted bytes.
        let file = current(
            fx.store
                .entity(&EntityId {
                    path: LIB.into(),
                    kind: "module".into(),
                    symbol: "self".into(),
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            file.location.span,
            Span {
                start: 0,
                end: SAMPLE.len() as u64
            }
        );
    }

    #[test]
    fn relations_are_direct_and_claim_no_resolution() {
        let fx = indexed(SAMPLE);
        let found = relations(&fx, LIB);
        has(
            &found,
            &[
                // Direct lexical containment only.
                "self --contains--> inner",
                "inner --contains--> inner::helper",
                "Event --contains--> Event::Added",
                "Event::Added --contains--> Event::Added::id",
                "Named --contains--> Named::name",
                "impl Named for Pair --contains--> <Pair as Named>::name",
                "impl Store<T> --contains--> Store<T>::get",
                // Imports, as written where written.
                "self --imports--> std::collections",
                "self --imports--> std::collections::HashMap",
                "self --imports--> crate::config::Config",
                "inner --imports--> super::helper@inner",
                // Calls: paths, methods with their receivers as written,
                // `Self` as the impl's self type.
                "Store<T>::get --calls--> helper",
                "Store<T>::get --calls--> self.items.get",
                "Store<T>::new --calls--> Map::new",
                "<Pair as Named>::name --calls--> Pair::describe",
                "MAX --calls--> compute",
                "helper --calls--> parse",
                "inner::helper --calls--> super::helper@inner",
                "Pair::new --calls--> Pair",
                "Pair::new --calls--> Box::new",
                // Construction.
                "Store<T>::new --constructs--> Store",
                "helper --constructs--> Event::Added",
                // Type references in signatures, fields, bounds, aliases.
                "Store::items --references--> Map",
                "Store::items --references--> String",
                "Store --references--> Clone",
                "Pair::1 --references--> Fn",
                "Pair::1 --references--> Id",
                "Event::Removed::0 --references--> Id",
                "Named --references--> Clone",
                "helper --references--> AsRef",
                "helper --references--> Event",
                "<Pair as std::fmt::Display>::fmt --references--> std::fmt::Formatter",
                "<Pair as Named>::Key --references--> Id",
                "MAX --references--> Id",
                // Impls name their trait and self type without resolving
                // them, though `Named` and `Pair` are defined right here.
                "impl Named for Pair --implements--> Named",
                "impl Named for Pair --self_type--> Pair",
                "impl std::fmt::Display for Pair --implements--> std::fmt::Display",
                "impl Store<T> --self_type--> Store",
                "impl Store<T> --references--> Clone",
            ],
        );
        for absent in [
            // Not transitive containment.
            "self --contains--> inner::helper",
            "self --contains--> Store::items",
            // Generic parameters and `Self` of a trait are not paths to
            // anything.
            "helper --calls--> K::default",
            "Store::items --references--> T",
            "Named::shout --calls--> Self::normalize",
            // Nested items in bodies are neither entities nor callers'
            // calls; macros are not expanded.
            "helper --calls--> hidden",
            "<Pair as std::fmt::Display>::fmt --calls--> write",
        ] {
            assert!(!found.iter().any(|f| f == absent), "{absent}");
        }
        // Primitive types are not paths.
        assert!(
            !found
                .iter()
                .any(|f| f.ends_with("--> u8") || f.ends_with("--> str"))
        );
        // Only this source's facts exist, and none names another source.
        assert!(found.iter().all(|f| !f.contains("--> inner::helper@")));
    }

    #[test]
    fn sites_are_exact_byte_ranges_of_the_accepted_content() {
        let fx = indexed(SAMPLE);
        let get = EntityId {
            path: LIB.into(),
            kind: "method".into(),
            symbol: "Store<T>::get".into(),
        };
        let out = current(fx.relations(&Node::Entity(get), Direction::Outgoing));
        let texts: Vec<_> = out
            .current
            .iter()
            .map(|r| {
                let sites: Vec<_> = r
                    .sites
                    .iter()
                    .map(|l| &SAMPLE[l.span.start as usize..l.span.end as usize])
                    .collect();
                (r.kind.as_str(), sites)
            })
            .collect();
        assert!(texts.contains(&("calls", vec!["helper"])));
        assert!(texts.contains(&("calls", vec!["get"])));
        assert!(texts.contains(&("references", vec!["Option"])));

        // A method named twice on one line keeps both sites, by byte.
        let fx = indexed("fn é() { ü.run(); ü.run(); }");
        let f = EntityId {
            path: LIB.into(),
            kind: "function".into(),
            symbol: "é".into(),
        };
        let out = current(fx.relations(&Node::Entity(f), Direction::Outgoing));
        let sites: Vec<_> = out.current[0].sites.iter().map(|l| l.span).collect();
        // Character offsets would be 11..14 and 20..23.
        assert_eq!(
            sites,
            [Span { start: 13, end: 16 }, Span { start: 23, end: 26 }]
        );
    }

    #[test]
    fn method_calls_keep_their_receivers_apart() {
        let fx = indexed(
            "fn receivers(left: Alpha, right: Beta) { left.run(); right.run(); }\n\
             fn repeated(left: Alpha) { left.run(); left.run(); left.stop(); }\n\
             fn fields(x: Pair) { x.left.run(); x.right.run(); }\n\
             fn chained(left: Alpha, right: Beta) { left.child().run(); right.child().run(); }\n",
        );
        // Each target is external and inferred, never an entity.
        let found = relations(&fx, LIB);
        let calls = |from: &str| {
            sorted(found.iter().filter_map(|f| {
                f.strip_prefix(&format!("{from} --calls--> "))
                    .map(String::from)
            }))
        };
        assert_eq!(calls("receivers"), ["left.run", "right.run"]);
        assert_eq!(calls("repeated"), ["left.run", "left.stop"]);
        assert_eq!(calls("fields"), ["x.left.run", "x.right.run"]);
        assert_eq!(
            calls("chained"),
            [
                "left.child",
                "left.child().run",
                "right.child",
                "right.child().run"
            ]
        );

        // The same receiver and method is one target with a site per call.
        let f = EntityId {
            path: LIB.into(),
            kind: "function".into(),
            symbol: "repeated".into(),
        };
        let out = current(fx.relations(&Node::Entity(f), Direction::Outgoing));
        let sites = out.current.iter().filter(|r| r.kind == "calls");
        assert_eq!(sorted(sites.map(|r| r.sites.len())), [1, 2]);
    }

    #[test]
    fn use_forms_import_each_path() {
        let fx = indexed(
            "use foo::bar;\n\
             use foo::{baz, qux::{self, deep as d}};\n\
             use foo::bar as renamed;\n\
             use crate::x::Y;\n\
             use super::x;\n\
             use self::local;\n\
             use foo::*;\n\
             use ::std::io;\n\
             pub use r#type::Item;\n",
        );
        let mut imports: Vec<_> = relations(&fx, LIB)
            .into_iter()
            .filter_map(|r| r.strip_prefix("self --imports--> ").map(String::from))
            .collect();
        imports.sort();
        assert_eq!(
            imports,
            [
                "::std::io",
                "crate::x::Y",
                "foo::*",
                "foo::bar",
                "foo::baz",
                "foo::qux",
                "foo::qux::deep",
                "r#type::Item",
                "self::local",
                "super::x",
            ]
        );
        // `foo::bar` is imported twice, once renamed: one relation, a site
        // for each.
        let file = Node::Entity(EntityId {
            path: LIB.into(),
            kind: "module".into(),
            symbol: "self".into(),
        });
        let out = current(fx.relations(&file, Direction::Outgoing));
        let bar = out
            .current
            .iter()
            .find(|r| matches!(&r.to, Node::External(x) if x.name == "foo::bar"))
            .unwrap();
        assert_eq!(bar.sites.len(), 2);
    }

    #[test]
    fn calls_are_recorded_only_directly() {
        let mut fx = indexed("fn a() { b() }\nfn b() { c() }\nfn c() {}\n");
        let found = relations(&fx, LIB);
        let calls: Vec<_> = found.iter().filter(|r| r.contains("--calls-->")).collect();
        assert_eq!(calls, ["a --calls--> b", "b --calls--> c"]);
        // Traversal stops at the unresolved `b`: nothing claims it is the
        // local `b`, so no path from `a` to `c` is derived either.
        let a = Node::Entity(EntityId {
            path: LIB.into(),
            kind: "function".into(),
            symbol: "a".into(),
        });
        let bounds = crate::graph::Bounds {
            depth: 10,
            kinds: vec!["calls".into()],
            limit: 100,
        };
        let walk = fx.store.traverse(&a, Direction::Outgoing, &bounds).unwrap();
        assert_eq!(walk.hops.len(), 1);
        // A source indexed again changes nothing: the same bytes give the
        // same graph.
        let before = (entities(&fx, LIB), relations(&fx, LIB));
        index_at(&mut fx, LIB).unwrap();
        assert_eq!((entities(&fx, LIB), relations(&fx, LIB)), before);
    }

    #[test]
    fn the_graph_follows_accepted_content_only() {
        let mut fx = Fixture::new("src");
        fx.accept(LIB, Some("fn accepted() {}\nmod decl;\n"));
        // Neither a candidate edit nor the declared module's file is read.
        fs::write(fx.project.root.join(LIB), "fn candidate() {}").unwrap();
        fs::write(fx.project.root.join("src/decl.rs"), "not rust {{{").unwrap();
        fs::create_dir_all(fx.project.root.join("src/decl")).unwrap();
        fs::write(fx.project.root.join("src/decl/mod.rs"), "not rust {{{").unwrap();
        index_at(&mut fx, LIB).unwrap();
        let snapshot = |fx: &Fixture| (entities(fx, LIB), relations(fx, LIB));
        let v1 = snapshot(&fx);
        assert_eq!(
            sorted(v1.0.iter().map(|(e, _)| e.as_str())),
            ["function accepted", "module decl", "module self"]
        );
        fails(fx.store.graph_status("src/decl.rs"), "no accepted state");

        index_at(&mut fx, LIB).unwrap();
        assert_eq!(snapshot(&fx), v1);
        let mut fx = fx.reopen();
        assert_eq!(snapshot(&fx), v1);

        // Newly accepted content makes the graph stale until indexed again.
        fx.accept(LIB, Some("fn next() {}\n"));
        assert_eq!(fx.status(LIB), Freshness::Stale);
        assert_eq!(fx.store.entities(LIB).unwrap(), Freshness::Stale);
        index_at(&mut fx, LIB).unwrap();
        assert_eq!(fx.status(LIB), Freshness::Current(()));
        assert_eq!(entities(&fx, LIB)[0].0, "function next");
    }

    #[test]
    fn a_contribution_from_replaced_content_cannot_commit() {
        let mut fx = Fixture::new("src");
        fx.accept(LIB, Some("fn old() {}"));
        let content = source::read_accepted(&fx.project, &fx.store, LIB).unwrap();
        let prepared = contribution(LIB, &content).unwrap();
        fx.accept(LIB, Some("fn new() {}"));
        fails(
            crate::graph::replace(&fx.project, &mut fx.store, &prepared),
            "stale",
        );
        assert_eq!(fx.status(LIB), Freshness::Unindexed);
        assert_eq!(fx.rows("graph_entities"), 0);
    }

    #[test]
    fn unparsable_or_unverifiable_content_changes_nothing() {
        let mut fx = indexed("fn f() { g() }");
        let tables = [
            "graph_sources",
            "graph_entities",
            "graph_relations",
            "graph_sites",
        ];
        let before = tables.map(|t| fx.rows(t));
        let v1 = (entities(&fx, LIB), relations(&fx, LIB));

        // A corrupt or missing recovery object is never parsed, and the
        // current graph stays as it was.
        let hash = fx
            .store
            .accepted_source(LIB)
            .unwrap()
            .unwrap()
            .hash
            .unwrap();
        let object = fx.project.root.join(STATE_DIR).join("objects").join(&hash);
        let original = fs::read(&object).unwrap();
        fs::write(&object, "fn f() { h() }").unwrap();
        fails(index_at(&mut fx, LIB), "corrupt");
        fs::remove_file(&object).unwrap();
        fails(index_at(&mut fx, LIB), "unavailable");
        fs::write(&object, &original).unwrap();
        assert_eq!(tables.map(|t| fx.rows(t)), before);
        assert_eq!((entities(&fx, LIB), relations(&fx, LIB)), v1);

        // Invalid Rust, including a missing token Tree-sitter would insert,
        // is refused rather than published in part.
        for (invalid, expected) in [
            ("fn f( {}\nfn g() {}", "syntax error"),
            ("fn f() { let x = 1 }", "syntax error"),
            ("struct S { a: }", "syntax error"),
            ("fn f() {}\n\u{0}", "syntax error"),
        ] {
            fx.accept(LIB, Some(invalid));
            fails(index_at(&mut fx, LIB), expected);
            assert_eq!(fx.status(LIB), Freshness::Stale, "{invalid}");
            assert_eq!(tables.map(|t| fx.rows(t)), before, "{invalid}");
        }
        let file = fx.project.root.join(LIB);
        fs::write(&file, b"fn f() {}\xff").unwrap();
        let plan = fx.store.create_plan(&objective("bytes")).unwrap();
        let task = fx.store.add_task(plan, "bytes", &[]).unwrap();
        let generation = fx.store.start_generation(task).unwrap();
        source::accept_generation(&fx.project, &mut fx.store, generation, &[LIB]).unwrap();
        fails(index_at(&mut fx, LIB), "not UTF-8");
        assert_eq!(tables.map(|t| fx.rows(t)), before);

        fx.accept(LIB, None);
        fails(index_at(&mut fx, LIB), "accepted as absent");
        fails(index_at(&mut fx, "src/untracked.rs"), "no accepted state");
    }

    #[test]
    fn repository_paths_stay_literal() {
        let mut fx = Fixture::new("src");
        let paths = [
            "src/app/(customer)/[slug]/page.rs",
            "src/[id].rs",
            "src/with space+(x).rs",
            "src/ünïcødé/文件.rs",
            "src/%_*.rs",
        ];
        let decoys = ["src/app/(customer)/s/page.rs", "src/i.rs", "src/ab.rs"];
        for path in paths.iter().chain(&decoys) {
            fx.accept(path, Some("pub fn f() { g() }"));
        }
        for path in paths {
            index_at(&mut fx, path).unwrap();
        }
        let fx = fx.reopen();
        for path in paths {
            let found = current(fx.store.entities(path).unwrap());
            assert!(
                found
                    .iter()
                    .all(|e| e.id.path == path && e.location.path == path)
            );
            assert_eq!(
                sorted(relations(&fx, path)),
                ["f --calls--> g", "self --contains--> f"]
            );
        }
        for decoy in decoys {
            assert_eq!(fx.status(decoy), Freshness::Unindexed, "{decoy}");
        }
    }

    #[test]
    fn graph_facts_are_published_only_through_replace() {
        let frontend = include_str!("rust.rs");
        let frontend = &frontend[..frontend.find("#[cfg(test)]").unwrap()];
        for bypass in [
            "replace_graph",
            ".raw()",
            "rusqlite",
            "execute",
            "state::Store::",
        ] {
            assert!(!frontend.contains(bypass), "{bypass}");
        }
        assert!(frontend.contains("super::replace(project, store, &contribution)"));
    }
}
