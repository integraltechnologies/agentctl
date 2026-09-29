//! The Python frontend: CodeGraph facts from one source's accepted Python
//! content, by syntax alone. It parses with Tree-sitter and does no import
//! resolution, type inference or evaluation, and never reads another file:
//! `import a.b` is a relation to the name `a.b`, not a reason to open it.
//!
//! Entities, by kind, with `.`-qualified symbols:
//! - `module`: `self`, the file's module;
//! - `class`: `A`, and `A.B` when nested in a class;
//! - `function`: `f`; `method`: a function directly in a class, `A.m`.
//!
//! A definition is found at module or class level, including within `if`,
//! `try`, `with`, loop and `match` statements there; definitions inside
//! function bodies are not extracted. A decorated definition spans its
//! decorators. When a symbol recurs within its kind, as in conditional
//! alternatives, later ones get `#2`, `#3`, … in source order.
//!
//! Relations:
//! - `contains` (proven): an entity to each entity lexically directly in it;
//! - `imports`: the entity holding an import statement to each name it
//!   imports: `a.b` for `import a.b`, `a.b.c` for `from a.b import c`, `.m.c`
//!   for `from .m import c`, `a.*` for a wildcard;
//! - `calls`: to each callee written as a dotted name, such as `f` or
//!   `self.items.get`;
//! - `extends`: a class to each base written as a dotted name.
//!
//! What a statement or definition evaluates, decorators, parameter defaults
//! and bodies included, belongs to the entity it defines, and what is
//! evaluated at module or class level to that module or class. All but
//! `contains` name things as written, which syntax cannot resolve, so they
//! are inferred, and their targets are `External` symbols in ecosystem
//! `python`, named as written and qualified by the source path.

use anyhow::Result;
use tree_sitter::Node as Syntax;

use super::syntax::{Builder, named_children, parse, span};
use super::{Contribution, EntityId, External};
use crate::source::AcceptedContent;

const LANGUAGE: &str = "python";

/// Statements of a module or class body that hold further body statements,
/// so the definitions in them are still at that level.
const STRUCTURAL: &[&str] = &[
    "block",
    "if_statement",
    "elif_clause",
    "else_clause",
    "try_statement",
    "except_clause",
    "except_group_clause",
    "finally_clause",
    "with_statement",
    "for_statement",
    "while_statement",
    "match_statement",
    "case_clause",
];

/// The graph of the Python source `path` with accepted content `content`,
/// or why that content is not valid Python.
pub(crate) fn contribution(path: &str, content: &AcceptedContent) -> Result<Contribution> {
    let (text, tree) = parse(&tree_sitter_python::LANGUAGE.into(), content.bytes())?;
    let root = tree.root_node();
    let mut x = Extractor {
        text,
        graph: Builder::new(path),
    };
    let file = x.graph.define("module", "self".into(), span(root));
    x.body(root, &file, "");
    Ok(x.graph.finish(content.hash(), LANGUAGE))
}

struct Extractor<'a> {
    text: &'a str,
    graph: Builder<'a>,
}

impl Extractor<'_> {
    /// The statements of a module or class body, or of a structural
    /// statement in one: its definitions, and what else it evaluates.
    fn body(&mut self, node: Syntax, owner: &EntityId, prefix: &str) {
        for child in named_children(node) {
            match child.kind() {
                "decorated_definition" => match child.child_by_field_name("definition") {
                    Some(definition) => self.definition(child, definition, owner, prefix),
                    None => self.evaluated(child, owner),
                },
                "function_definition" | "class_definition" => {
                    self.definition(child, child, owner, prefix)
                }
                kind if STRUCTURAL.contains(&kind) => self.body(child, owner, prefix),
                _ => self.evaluated(child, owner),
            }
        }
    }

    /// A class or function `definition`, spanning `whole` with its
    /// decorators, if any.
    fn definition(&mut self, whole: Syntax, definition: Syntax, owner: &EntityId, prefix: &str) {
        let Some(name) = definition.child_by_field_name("name") else {
            return;
        };
        let class = definition.kind() == "class_definition";
        let kind = match (class, owner.kind.as_str()) {
            (true, _) => "class",
            (false, "class") => "method",
            (false, _) => "function",
        };
        let name = &self.text[name.byte_range()];
        let symbol = match prefix {
            "" => name.to_string(),
            prefix => format!("{prefix}.{name}"),
        };
        let id = self.graph.define(kind, symbol.clone(), span(whole));
        self.graph.contains(owner, &id);
        if class {
            for decorator in named_children(whole).filter(|d| d.kind() == "decorator") {
                self.evaluated(decorator, &id);
            }
            if let Some(bases) = definition.child_by_field_name("superclasses") {
                for base in named_children(bases) {
                    if let Some(name) = self.dotted(base) {
                        self.relate(&id, "extends", name, base);
                    }
                }
                self.evaluated(bases, &id);
            }
            if let Some(body) = definition.child_by_field_name("body") {
                self.body(body, &id, &symbol);
            }
        } else {
            self.evaluated(whole, &id);
        }
    }

    /// `imports` and `calls` within `node`, to the end of what it
    /// evaluates.
    fn evaluated(&mut self, node: Syntax, from: &EntityId) {
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "import_statement" => {
                    for name in named_children(node) {
                        self.import(from, "", name);
                    }
                }
                "import_from_statement" | "future_import_statement" => self.import_from(node, from),
                "call" => {
                    if let Some(name) = node
                        .child_by_field_name("function")
                        .and_then(|f| self.dotted(f))
                    {
                        self.relate(from, "calls", name, node);
                    }
                }
                _ => {}
            }
            stack.extend(named_children(node).rev());
        }
    }

    fn import_from(&mut self, statement: Syntax, from: &EntityId) {
        let module = match statement.child_by_field_name("module_name") {
            Some(m) => self.text[m.byte_range()].split_whitespace().collect(),
            None => "__future__".to_string(),
        };
        let prefix = match module.as_str() {
            "" => String::new(),
            m if m.ends_with('.') => m.to_string(),
            m => format!("{m}."),
        };
        for name in named_children(statement) {
            match name.kind() {
                "wildcard_import" => self.relate(from, "imports", format!("{prefix}*"), name),
                // The module, not a name imported from it.
                _ if statement.child_by_field_name("module_name") == Some(name) => {}
                _ => self.import(from, &prefix, name),
            }
        }
    }

    /// An imported `dotted_name`, or `aliased_import` of one, after `prefix`.
    fn import(&mut self, from: &EntityId, prefix: &str, name: Syntax) {
        let dotted = match name.kind() {
            "aliased_import" => name.child_by_field_name("name"),
            "dotted_name" => Some(name),
            _ => None,
        };
        if let Some(dotted) = dotted {
            let written: String = self.text[dotted.byte_range()].split_whitespace().collect();
            self.relate(from, "imports", format!("{prefix}{written}"), name);
        }
    }

    /// An identifier, or attribute access of one, as written; nothing for
    /// any other expression, whose value syntax cannot name.
    fn dotted(&self, node: Syntax) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text[node.byte_range()].to_string()),
            "attribute" => {
                let object = self.dotted(node.child_by_field_name("object")?)?;
                let name = node.child_by_field_name("attribute")?;
                Some(format!("{object}.{}", &self.text[name.byte_range()]))
            }
            _ => None,
        }
    }

    fn relate(&mut self, from: &EntityId, kind: &str, name: String, site: Syntax) {
        let to = External {
            ecosystem: Some(LANGUAGE.into()),
            namespace: Some(self.graph.path.into()),
            name,
        };
        self.graph.unresolved(from, kind, to, span(site));
    }
}

#[cfg(test)]
mod tests {
    use super::super::syntax::tests::{entities, indexed, relations, same, sorted};
    use super::super::tests::{Fixture, current};
    use super::super::{Derivation, Direction, EntityId, Freshness, Node, Span, derive};

    const MODULE: &str = "src/app.py";

    /// Several constructs together, with multi-byte text before and among
    /// definitions.
    const SAMPLE: &str = r#""""Módulo — “doc” ✓"""
import os, a.b as ab
from .m import c, d as e
from .. import up
from x.y import *
from __future__ import annotations
import ünï.çødé

try:
    import json
except ImportError:
    json = None

@decorate(1)
@other.deco
def größe(x=default()):
    def inner(): hidden()
    return helper(x)

class Base(pkg.Root, metaclass=Meta):
    x = make()
    if FLAG:
        def cond(self): pass
    else:
        def cond(self): pass

    class Inner(Other):
        def m(self): self.items.get(1)

    @staticmethod
    def m(a): return a.b().c(1)

    def m(self): pass

    with ctx():
        class InWith: pass

def f(): pass
def f(): pass
class Base: pass
"#;

    fn fixture(source: &str) -> Fixture {
        let mut fx = Fixture::new("src");
        indexed(&mut fx, MODULE, source);
        fx
    }

    #[test]
    fn definitions_have_distinct_symbols_and_exact_byte_spans() {
        let fx = fixture(SAMPLE);
        let expected = [
            ("module self", SAMPLE),
            (
                "function größe",
                "@decorate(1)\n@other.deco\ndef größe(x=default()):\n    def inner(): hidden()\n    return helper(x)",
            ),
            (
                "class Base",
                "class Base(pkg.Root, metaclass=Meta):\n    x = make()\n    if FLAG:\n        def cond(self): pass\n    else:\n        def cond(self): pass\n\n    class Inner(Other):\n        def m(self): self.items.get(1)\n\n    @staticmethod\n    def m(a): return a.b().c(1)\n\n    def m(self): pass\n\n    with ctx():\n        class InWith: pass",
            ),
            ("method Base.cond", "def cond(self): pass"),
            ("method Base.cond#2", "def cond(self): pass"),
            (
                "class Base.Inner",
                "class Inner(Other):\n        def m(self): self.items.get(1)",
            ),
            ("method Base.Inner.m", "def m(self): self.items.get(1)"),
            (
                "method Base.m",
                "@staticmethod\n    def m(a): return a.b().c(1)",
            ),
            ("method Base.m#2", "def m(self): pass"),
            ("class Base.InWith", "class InWith: pass"),
            ("function f", "def f(): pass"),
            ("function f#2", "def f(): pass"),
            ("class Base#2", "class Base: pass"),
        ];
        let expected = expected.map(|(e, text)| (e.to_string(), text.to_string()));
        assert_eq!(sorted(entities(&fx, MODULE)), sorted(expected));
        // Byte offsets, not characters: what precedes `größe` is non-ASCII.
        let start = SAMPLE.find("@decorate").unwrap() as u64;
        let id = |kind: &str, symbol: &str| EntityId {
            path: MODULE.into(),
            kind: kind.into(),
            symbol: symbol.into(),
        };
        let f = current(fx.store.entity(&id("function", "größe")).unwrap()).unwrap();
        assert_eq!(f.language, "python");
        assert_eq!(f.location.span.start, start);
        assert!(start as usize > SAMPLE.chars().take_while(|&c| c != '@').count());
        // Functions in function bodies are not entities.
        assert!(current(fx.store.entity(&id("function", "inner")).unwrap()).is_none());
    }

    #[test]
    fn relations_are_direct_and_claim_no_resolution() {
        let fx = fixture(SAMPLE);
        same(
            relations(&fx, MODULE, "python"),
            &[
                "self --contains--> Base",
                "self --contains--> Base#2",
                "self --contains--> f",
                "self --contains--> f#2",
                "self --contains--> größe",
                "Base --contains--> Base.InWith",
                "Base --contains--> Base.Inner",
                "Base --contains--> Base.cond",
                "Base --contains--> Base.cond#2",
                "Base --contains--> Base.m",
                "Base --contains--> Base.m#2",
                "Base.Inner --contains--> Base.Inner.m",
                // Imports as written; nothing is resolved to a file.
                "self --imports--> os",
                "self --imports--> a.b",
                "self --imports--> .m.c",
                "self --imports--> .m.d",
                "self --imports--> ..up",
                "self --imports--> x.y.*",
                "self --imports--> __future__.annotations",
                "self --imports--> ünï.çødé",
                "self --imports--> json",
                // Calls of dotted names only, each by what evaluates it.
                "größe --calls--> decorate",
                "größe --calls--> default",
                "größe --calls--> helper",
                "größe --calls--> hidden",
                "Base --calls--> make",
                "Base --calls--> ctx",
                "Base.Inner.m --calls--> self.items.get",
                "Base.m --calls--> a.b",
                // Bases as written; keywords are not bases.
                "Base --extends--> pkg.Root",
                "Base.Inner --extends--> Other",
            ],
        );
    }

    #[test]
    fn sites_are_exact_byte_ranges_of_the_accepted_content() {
        let source = "import ñ\ndef f():\n    g(1); g(2)\n";
        let fx = fixture(source);
        let get = |kind: &str, symbol: &str, to: &str, relation: &str| {
            let from = Node::Entity(EntityId {
                path: MODULE.into(),
                kind: kind.into(),
                symbol: symbol.into(),
            });
            let out = current(fx.relations(&from, Direction::Outgoing)).current;
            let r = out
                .into_iter()
                .find(|r| r.kind == relation && matches!(&r.to, Node::External(x) if x.name == to))
                .unwrap();
            r.sites.into_iter().map(|s| s.span).collect::<Vec<_>>()
        };
        let at = |text: &str| source.find(text).unwrap() as u64;
        assert_eq!(
            get("function", "f", "g", "calls"),
            [
                Span {
                    start: at("g(1)"),
                    end: at("g(1)") + 4
                },
                Span {
                    start: at("g(2)"),
                    end: at("g(2)") + 4
                },
            ]
        );
        assert_eq!(
            get("module", "self", "ñ", "imports"),
            [Span {
                start: at("ñ"),
                end: at("ñ") + 2
            }]
        );
    }

    #[test]
    fn only_syntactically_valid_python_is_indexed() {
        let mut fx = Fixture::new("src");
        fx.accept(MODULE, Some("def f(:\n"));
        assert!(matches!(
            derive(&fx.project, &fx.store, MODULE).unwrap(),
            Derivation::Declined
        ));
        assert_eq!(fx.status(MODULE), Freshness::Unindexed);
        for path in ["src/a.py", "src/stubs.pyi"] {
            fx.accept(path, Some("class C: ...\n"));
            assert!(matches!(
                derive(&fx.project, &fx.store, path).unwrap(),
                Derivation::Indexed(c) if c.language == "python"
            ));
        }
        // Extension, not content, decides the language.
        fx.accept("src/a.pyc", Some("def f(): pass\n"));
        assert!(matches!(
            derive(&fx.project, &fx.store, "src/a.pyc").unwrap(),
            Derivation::Unsupported
        ));
    }

    #[test]
    fn the_planner_map_lists_python_entities_like_any_others() {
        let fx = fixture("class A:\n    def m(self): pass\n");
        let map = crate::planner::repository(&fx.project, &fx.store).unwrap();
        let sources = map["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["path"], MODULE);
        assert_eq!(sources[0]["graph"], "current");
        let entities: Vec<String> = sources[0]["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e["kind"].as_str().unwrap(),
                    e["symbol"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(sorted(entities), ["class A", "method A.m", "module self"]);
    }
}
