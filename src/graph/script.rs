//! The JavaScript and TypeScript frontend: CodeGraph facts from one source's
//! accepted content, by syntax alone. It parses with Tree-sitter and does no
//! module resolution, type checking or evaluation, and never reads another
//! file: `import "./a"` is a relation to the name `./a`, not a reason to
//! open it.
//!
//! The grammar follows the extension: JavaScript for `.js`, `.mjs`, `.cjs`
//! and `.jsx`; TypeScript for `.ts`, `.mts` and `.cts`; TSX for `.tsx`. The
//! language is `javascript` or `typescript`.
//!
//! Entities, by kind, with `.`-qualified symbols:
//! - `module`: `self`, the file's module;
//! - `class`: `A`; `method`: `A.m`, also for a class field holding a
//!   function;
//! - `function`: a function declaration, or a top-level `const`, `let` or
//!   `var` of a plain identifier holding a function or arrow function;
//! - `variable`: any other such top-level declaration; destructuring is
//!   skipped;
//! - TypeScript: `interface`, `type_alias`, `enum` and `namespace` (also for
//!   `module` blocks), whose declarations are qualified by its name.
//!
//! An anonymous default export is `default`: a `function`, a `class` or, for
//! any other expression but a plain name, a `variable`. An entity spans its
//! declaration, without any `export`. Definitions inside function bodies
//! and blocks are not extracted. When a symbol recurs within its kind, later
//! ones get `#2`, `#3`, … in source order.
//!
//! Relations:
//! - `contains` (proven): an entity to each entity lexically directly in it;
//! - `imports`: the entity evaluating an `import` or `export … from`
//!   statement, a literal `require("…")` or a literal `import("…")` to the
//!   module specifier as written; a non-literal one names nothing;
//! - `calls` and `constructs` (`new`): to each callee written as a dotted
//!   name, such as `f` or `this.items.get`;
//! - `extends` and `implements`: a class or interface to each dotted name
//!   its heritage names; a type's type arguments are ignored.
//!
//! What a declaration evaluates, decorators, parameter defaults and bodies
//! included, belongs to the entity it defines, and what module or class
//! level evaluates to that module or class. All but `contains` name things
//! as written, which syntax cannot resolve, so they are inferred, and their
//! targets are `External` symbols in the ecosystem of the language, named as
//! written and qualified by the source path.

use anyhow::{Context, Result};
use tree_sitter::{Language, Node as Syntax};

use super::syntax::{Builder, named_children, parse, span};
use super::{Contribution, EntityId, External};
use crate::source::AcceptedContent;

/// The language and grammar of the source `path`, by its extension.
pub(crate) fn grammar(path: &str) -> Option<(&'static str, Language)> {
    Some(match path.rsplit_once('.')?.1 {
        "js" | "mjs" | "cjs" | "jsx" => ("javascript", tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "mts" | "cts" => (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        "tsx" => ("typescript", tree_sitter_typescript::LANGUAGE_TSX.into()),
        _ => return None,
    })
}

/// The graph of the JavaScript or TypeScript source `path` with accepted
/// content `content`, or why that content is not valid.
pub(crate) fn contribution(path: &str, content: &AcceptedContent) -> Result<Contribution> {
    let (language, grammar) = grammar(path).context("not a JavaScript or TypeScript path")?;
    let (text, tree) = parse(&grammar, content.bytes())?;
    let root = tree.root_node();
    let mut x = Extractor {
        text,
        language,
        graph: Builder::new(path),
    };
    let file = x.graph.define("module", "self".into(), span(root));
    x.statements(root, &file, "");
    Ok(x.graph.finish(content.hash(), language))
}

struct Extractor<'a> {
    text: &'a str,
    language: &'static str,
    graph: Builder<'a>,
}

impl Extractor<'_> {
    /// The statements of a module or namespace body.
    fn statements(&mut self, body: Syntax, owner: &EntityId, prefix: &str) {
        for item in named_children(body) {
            self.statement(item, owner, prefix, false);
        }
    }

    /// A statement of a module or namespace: what it declares, and what else
    /// it evaluates. `default` names an anonymous declaration `default`.
    fn statement(&mut self, item: Syntax, owner: &EntityId, prefix: &str, default: bool) {
        let name = |x: &Self| match item.child_by_field_name("name") {
            Some(name) => Some(x.written(name)),
            None => default.then(|| "default".to_string()),
        };
        let simple = match item.kind() {
            "import_statement" => {
                self.import_source(item, owner);
                return;
            }
            "export_statement" => {
                self.import_source(item, owner);
                for decorator in named_children(item).filter(|d| d.kind() == "decorator") {
                    self.evaluated(decorator, owner);
                }
                if let Some(declaration) = item.child_by_field_name("declaration") {
                    let default = self.is_default(item);
                    self.statement(declaration, owner, prefix, default);
                } else if let Some(value) = item.child_by_field_name("value") {
                    self.default_export(value, owner);
                }
                return;
            }
            "ambient_declaration" => {
                for declaration in named_children(item) {
                    self.statement(declaration, owner, prefix, false);
                }
                return;
            }
            "expression_statement" => {
                match item
                    .named_child(0)
                    .filter(|m| m.kind() == "internal_module")
                {
                    Some(module) => self.statement(module, owner, prefix, false),
                    None => self.evaluated(item, owner),
                }
                return;
            }
            "class_declaration" | "abstract_class_declaration" => {
                match name(self) {
                    Some(name) => self.class(item, name, owner, prefix),
                    None => self.evaluated(item, owner),
                }
                return;
            }
            "lexical_declaration" | "variable_declaration" => {
                for declarator in named_children(item) {
                    self.declarator(declarator, owner, prefix);
                }
                return;
            }
            "function_declaration" | "generator_function_declaration" => "function",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type_alias",
            "enum_declaration" => "enum",
            "internal_module" | "module" => "namespace",
            _ => {
                self.evaluated(item, owner);
                return;
            }
        };
        let Some(name) = name(self) else {
            self.evaluated(item, owner);
            return;
        };
        let name = match item.child_by_field_name("name") {
            Some(n) if n.kind() == "string" => literal(self.text, n).unwrap_or(name),
            _ => name,
        };
        let symbol = qualify(prefix, &name);
        let id = self.graph.define(simple, symbol.clone(), span(item));
        self.graph.contains(owner, &id);
        if simple == "namespace" {
            if let Some(body) = item.child_by_field_name("body") {
                self.statements(body, &id, &symbol);
            }
            return;
        }
        for part in named_children(item) {
            if part.kind() == "extends_type_clause" {
                self.heritage(part, &id);
            }
        }
        self.evaluated(item, &id);
    }

    /// Whether an `export` statement is `export default`.
    fn is_default(&self, export: Syntax) -> bool {
        let mut cursor = export.walk();
        export.children(&mut cursor).any(|c| c.kind() == "default")
    }

    /// `export default` of an expression: a function or class is that, a
    /// plain name is only referred to, anything else is a variable.
    fn default_export(&mut self, value: Syntax, owner: &EntityId) {
        match value.kind() {
            "identifier" => self.evaluated(value, owner),
            "class" => match value.child_by_field_name("name") {
                Some(name) => {
                    let name = self.written(name);
                    self.class(value, name, owner, "")
                }
                None => self.class(value, "default".into(), owner, ""),
            },
            kind => {
                let entity = if is_function(kind) {
                    "function"
                } else {
                    "variable"
                };
                let id = self.graph.define(entity, "default".into(), span(value));
                self.graph.contains(owner, &id);
                self.evaluated(value, &id);
            }
        }
    }

    /// A `const`, `let` or `var` declarator.
    fn declarator(&mut self, declarator: Syntax, owner: &EntityId, prefix: &str) {
        let name = declarator
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier");
        let Some(name) = name else {
            self.evaluated(declarator, owner);
            return;
        };
        let function = declarator
            .child_by_field_name("value")
            .is_some_and(|v| is_function(v.kind()));
        let kind = if function { "function" } else { "variable" };
        let symbol = qualify(prefix, &self.written(name));
        let id = self.graph.define(kind, symbol, span(declarator));
        self.graph.contains(owner, &id);
        self.evaluated(declarator, &id);
    }

    fn class(&mut self, item: Syntax, name: String, owner: &EntityId, prefix: &str) {
        let symbol = qualify(prefix, &name);
        let id = self.graph.define("class", symbol.clone(), span(item));
        self.graph.contains(owner, &id);
        for part in named_children(item) {
            match part.kind() {
                "class_body" => self.members(part, &id, &symbol),
                "class_heritage" => self.heritage(part, &id),
                _ => self.evaluated(part, &id),
            }
        }
    }

    fn members(&mut self, body: Syntax, class: &EntityId, prefix: &str) {
        for member in named_children(body) {
            let method = match member.kind() {
                "method_definition" => member.child_by_field_name("name"),
                "field_definition" | "public_field_definition" => member
                    .child_by_field_name("value")
                    .filter(|v| is_function(v.kind()))
                    .and_then(|_| {
                        member
                            .child_by_field_name("property")
                            .or_else(|| member.child_by_field_name("name"))
                    }),
                _ => None,
            };
            match method {
                Some(name) => {
                    let symbol = qualify(prefix, &self.written(name));
                    let id = self.graph.define("method", symbol, span(member));
                    self.graph.contains(class, &id);
                    self.evaluated(member, &id);
                }
                None => self.evaluated(member, class),
            }
        }
    }

    /// `extends` and `implements` of a class or interface, from the
    /// heritage of a class or the extended types of an interface.
    fn heritage(&mut self, heritage: Syntax, class: &EntityId) {
        for clause in named_children(heritage) {
            match clause.kind() {
                "implements_clause" => self.names(clause, class, "implements"),
                "extends_clause" => self.names(clause, class, "extends"),
                // JavaScript: the expression itself.
                _ => {
                    if let Some(name) = self.dotted(clause) {
                        self.relate(class, "extends", name, clause);
                    }
                }
            }
            self.evaluated(clause, class);
        }
    }

    /// A relation to each named type or expression directly in `clause`.
    fn names(&mut self, clause: Syntax, from: &EntityId, kind: &str) {
        for node in named_children(clause) {
            if let Some(name) = self.dotted(node) {
                self.relate(from, kind, name, node);
            }
        }
    }

    /// The module an `import` or `export … from` statement names.
    fn import_source(&mut self, statement: Syntax, from: &EntityId) {
        let source = statement.child_by_field_name("source").or_else(|| {
            let clause = named_children(statement).find(|c| c.kind() == "import_require_clause");
            clause?.child_by_field_name("source")
        });
        if let Some(source) = source
            && let Some(module) = literal(self.text, source)
        {
            self.relate(from, "imports", module, statement);
        }
    }

    /// `imports`, `calls` and `constructs` within `node`, to the end of what
    /// it evaluates.
    fn evaluated(&mut self, node: Syntax, from: &EntityId) {
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "call_expression" => self.call(node, from),
                "new_expression" => {
                    let target = node
                        .child_by_field_name("constructor")
                        .and_then(|c| self.dotted(c));
                    if let Some(name) = target {
                        self.relate(from, "constructs", name, node);
                    }
                }
                _ => {}
            }
            stack.extend(named_children(node).rev());
        }
    }

    fn call(&mut self, call: Syntax, from: &EntityId) {
        let Some(callee) = call.child_by_field_name("function") else {
            return;
        };
        let module = |x: &Self| {
            let first = call.child_by_field_name("arguments")?.named_child(0)?;
            literal(x.text, first)
        };
        if callee.kind() == "import"
            || (callee.kind() == "identifier" && self.written(callee) == "require")
        {
            if let Some(module) = module(self) {
                self.relate(from, "imports", module, call);
            }
        } else if let Some(name) = self.dotted(callee) {
            self.relate(from, "calls", name, call);
        }
    }

    /// A name, or member access of names, as written; nothing for any other
    /// expression, whose value syntax cannot name.
    fn dotted(&self, node: Syntax) -> Option<String> {
        match node.kind() {
            "identifier"
            | "type_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "this"
            | "super" => Some(self.text[node.byte_range()].to_string()),
            "member_expression" | "nested_identifier" => self.member(node, "object", "property"),
            "nested_type_identifier" => self.member(node, "module", "name"),
            "generic_type" => self.dotted(node.child_by_field_name("name")?),
            "non_null_expression" => self.dotted(node.named_child(0)?),
            _ => None,
        }
    }

    /// `object.property` of the two fields of `node`, each dotted.
    fn member(&self, node: Syntax, object: &str, property: &str) -> Option<String> {
        let object = self.dotted(node.child_by_field_name(object)?)?;
        let property = self.dotted(node.child_by_field_name(property)?)?;
        Some(format!("{object}.{property}"))
    }

    fn relate(&mut self, from: &EntityId, kind: &str, name: String, site: Syntax) {
        let to = External {
            ecosystem: Some(self.language.into()),
            namespace: Some(self.graph.path.into()),
            name,
        };
        self.graph.unresolved(from, kind, to, span(site));
    }

    /// Text as written, with whitespace removed.
    fn written(&self, node: Syntax) -> String {
        self.text[node.byte_range()].split_whitespace().collect()
    }
}

fn qualify(prefix: &str, name: &str) -> String {
    match prefix {
        "" => name.into(),
        prefix => format!("{prefix}.{name}"),
    }
}

fn is_function(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

/// The text of a plain string literal, if it names anything.
fn literal(text: &str, node: Syntax) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let quoted = &text[node.byte_range()];
    let inner = quoted.get(1..quoted.len().checked_sub(1)?)?;
    (!inner.is_empty() && !inner.contains('\0')).then(|| inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::syntax::tests::{entities, indexed, relations, same, sorted};
    use super::super::tests::{Fixture, current};
    use super::super::{Derivation, Direction, EntityId, Freshness, Node, Span, derive};

    const TS: &str = "src/app.ts";
    const JS: &str = "src/app.js";

    /// Several constructs together, with multi-byte text before and among
    /// declarations.
    const TS_SAMPLE: &str = r#"// Ünïcødé — “ts” ✓
import d, { a as b } from "./m";
import * as ns from 'lib';
import type { T } from "types";
import eq = require("legacy");
export { x } from "./re";
export * from "./all";
const r = require("req");
const lazy = import("./lazy");
const unknown = import(name);
require(name);
export default function () { return größe(1) }
namespace N.M { export function inner() {} export class K {} }
declare module "ambient" { export const v: number }
declare function sig(): void;
export abstract class Ab<T> extends Base<T> implements I, J.K {
  field = () => 1;
  other = 2;
  static s(): void { new Foo.Bar(); this.x!.y(); }
  get p() { return 1 }
  set p(v) {}
}
interface I2 extends A, B.C<D> { m(): void }
type Al = string;
enum E { A, B = f() }
const arrow = (a) => a, fe = function () {}, plain = 1, { d1 } = obj;
export const ex = 5;
"#;

    const JS_SAMPLE: &str = r#"import React, { useState } from "react";
export * as util from "./util.js";
const fs = require("fs"), path = require('path');
class A extends mix(B) {
  #p = 1;
  static { init() }
  [Symbol.iterator]() {}
  handle = function () { this.go() };
}
class C extends ns.Base {
  constructor() { super(); new A(); }
}
function* g() { yield helper(); }
export default class {}
var v = 1;
let { skipped } = obj, [alsoSkipped] = arr;
const make = async () => (await import("./lazy"));
(function () { iife() })();
module.exports = { a: 1 };
"#;

    fn fixture(path: &str, source: &str) -> Fixture {
        let mut fx = Fixture::new("src");
        indexed(&mut fx, path, source);
        fx
    }

    /// The text from the start of `from` to the end of the first `to` at or
    /// after it.
    fn text(source: &str, from: &str, to: &str) -> String {
        let start = source.find(from).unwrap();
        let end = start + source[start..].find(to).unwrap() + to.len();
        source[start..end].to_string()
    }

    fn expect(fx: &Fixture, path: &str, expected: &[(&str, String)]) {
        let expected = expected.iter().map(|(e, t)| (e.to_string(), t.clone()));
        assert_eq!(sorted(entities(fx, path)), sorted(expected));
    }

    #[test]
    fn typescript_declarations_have_distinct_symbols_and_exact_byte_spans() {
        let fx = fixture(TS, TS_SAMPLE);
        let t = |from, to| text(TS_SAMPLE, from, to);
        expect(
            &fx,
            TS,
            &[
                ("module self", TS_SAMPLE.to_string()),
                ("variable r", t("r = ", "\"req\")")),
                ("variable lazy", t("lazy = ", "\"./lazy\")")),
                ("variable unknown", t("unknown = ", "(name)")),
                // Anonymous default exports are `default`.
                ("function default", t("function () {", "größe(1) }")),
                ("namespace N.M", t("namespace N.M", "class K {} }")),
                ("function N.M.inner", t("function inner", "{}")),
                ("class N.M.K", t("class K", "{}")),
                ("namespace ambient", t("module \"ambient\"", "number }")),
                ("variable ambient.v", t("v: number", "number")),
                ("class Ab", t("abstract class", "set p(v) {}\n}")),
                ("method Ab.field", t("field = () => 1", "1")),
                ("method Ab.s", t("static s()", "y(); }")),
                ("method Ab.p", t("get p()", "1 }")),
                ("method Ab.p#2", t("set p(v)", "{}")),
                ("interface I2", t("interface I2", "void }")),
                ("type_alias Al", t("type Al", "string;")),
                ("enum E", t("enum E", "f() }")),
                ("function arrow", t("arrow = ", "=> a")),
                ("function fe", t("fe = ", "{}")),
                ("variable plain", t("plain = ", "1")),
                ("variable ex", t("ex = ", "5")),
            ],
        );
        let id = |kind: &str, symbol: &str| EntityId {
            path: TS.into(),
            kind: kind.into(),
            symbol: symbol.into(),
        };
        let entity = |kind, symbol| current(fx.store.entity(&id(kind, symbol)).unwrap());
        // Byte offsets: multi-byte characters precede and follow the start.
        let interface = entity("interface", "I2").unwrap();
        assert_eq!(interface.language, "typescript");
        assert_eq!(
            interface.location.span.start,
            TS_SAMPLE.find("interface I2").unwrap() as u64
        );
        // Not modelled: destructuring, overload-like signatures, exports lists.
        assert!(entity("variable", "d1").is_none());
        assert!(entity("function", "sig").is_none());
    }

    #[test]
    fn typescript_relations_are_direct_and_claim_no_resolution() {
        let fx = fixture(TS, TS_SAMPLE);
        same(
            relations(&fx, TS, "typescript"),
            &[
                "self --contains--> Ab",
                "self --contains--> Al",
                "self --contains--> E",
                "self --contains--> I2",
                "self --contains--> N.M",
                "self --contains--> ambient",
                "self --contains--> arrow",
                "self --contains--> default",
                "self --contains--> lazy",
                "self --contains--> ex",
                "self --contains--> fe",
                "self --contains--> unknown",
                "self --contains--> plain",
                "self --contains--> r",
                "N.M --contains--> N.M.K",
                "N.M --contains--> N.M.inner",
                "ambient --contains--> ambient.v",
                "Ab --contains--> Ab.field",
                "Ab --contains--> Ab.p",
                "Ab --contains--> Ab.p#2",
                "Ab --contains--> Ab.s",
                // ESM, `export … from` and `import = require`, by specifier
                // as written; not resolved to files or packages.
                "self --imports--> ./m",
                "self --imports--> lib",
                "self --imports--> types",
                "self --imports--> legacy",
                "self --imports--> ./re",
                "self --imports--> ./all",
                // Literal `require` and `import()` belong to what evaluates
                // them; a non-literal one names nothing.
                "r --imports--> req",
                "lazy --imports--> ./lazy",
                "default --calls--> größe",
                "E --calls--> f",
                "Ab.s --calls--> this.x.y",
                "Ab.s --constructs--> Foo.Bar",
                "Ab --extends--> Base",
                "Ab --implements--> I",
                "Ab --implements--> J.K",
                "I2 --extends--> A",
                "I2 --extends--> B.C",
            ],
        );
    }

    #[test]
    fn javascript_declarations_and_relations() {
        let fx = fixture(JS, JS_SAMPLE);
        let t = |from, to| text(JS_SAMPLE, from, to);
        expect(
            &fx,
            JS,
            &[
                ("module self", JS_SAMPLE.to_string()),
                ("variable fs", t("fs = ", "\"fs\")")),
                ("variable path", t("path = ", "'path')")),
                ("class A", t("class A", "this.go() };\n}")),
                ("method A.[Symbol.iterator]", t("[Symbol.iterator]()", "{}")),
                ("method A.handle", t("handle = ", "this.go() }")),
                ("class C", t("class C", "new A(); }\n}")),
                ("method C.constructor", t("constructor()", "new A(); }")),
                ("function g", t("function* g", "helper(); }")),
                ("class default", "class {}".to_string()),
                ("variable v", "v = 1".to_string()),
                ("function make", t("make = ", "\"./lazy\"))")),
            ],
        );
        same(
            relations(&fx, JS, "javascript"),
            &[
                "self --contains--> fs",
                "self --contains--> path",
                "self --contains--> A",
                "self --contains--> C",
                "self --contains--> g",
                "self --contains--> default",
                "self --contains--> v",
                "self --contains--> make",
                "A --contains--> A.[Symbol.iterator]",
                "A --contains--> A.handle",
                "C --contains--> C.constructor",
                "self --imports--> react",
                "self --imports--> ./util.js",
                "fs --imports--> fs",
                "path --imports--> path",
                "make --imports--> ./lazy",
                // A call in an `extends` expression is a call, not a base.
                "A --calls--> mix",
                "A --calls--> init",
                "A.handle --calls--> this.go",
                "C --extends--> ns.Base",
                "C.constructor --calls--> super",
                "C.constructor --constructs--> A",
                "g --calls--> helper",
                // Evaluated at module level, inside an expression.
                "self --calls--> iife",
            ],
        );
    }

    #[test]
    fn default_exports_are_stable_entities() {
        let fx = fixture(
            "src/a.js",
            "export default { run() { go() } };\nexport function named() {}\n",
        );
        same(
            relations(&fx, "src/a.js", "javascript"),
            &[
                "self --contains--> default",
                "self --contains--> named",
                "default --calls--> go",
            ],
        );
        let named = fixture("src/b.js", "export default function named() {}\n");
        same(
            relations(&named, "src/b.js", "javascript"),
            &["self --contains--> named"],
        );
        // A plain name is only referred to.
        let name = fixture("src/c.js", "export default foo;\n");
        same(relations(&name, "src/c.js", "javascript"), &[]);
    }

    #[test]
    fn sites_are_exact_byte_ranges_of_the_accepted_content() {
        let source = "import \"./ñ\";\nfunction f() { g(1); new g(2); }\n";
        let fx = fixture(JS, source);
        let sites = |kind: &str, symbol: &str, relation: &str, to: &str| {
            let from = Node::Entity(EntityId {
                path: JS.into(),
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
        let span = |text: &str| Span {
            start: at(text),
            end: at(text) + text.len() as u64,
        };
        assert_eq!(sites("function", "f", "calls", "g"), [span("g(1)")]);
        assert_eq!(
            sites("function", "f", "constructs", "g"),
            [span("new g(2)")]
        );
        assert_eq!(
            sites("module", "self", "imports", "./ñ"),
            [span("import \"./ñ\";")]
        );
    }

    #[test]
    fn the_extension_selects_the_grammar() {
        let mut fx = Fixture::new("src");
        let derived = |fx: &Fixture, path: &str| derive(&fx.project, &fx.store, path).unwrap();
        for (path, language) in [
            ("src/a.js", "javascript"),
            ("src/a.mjs", "javascript"),
            ("src/a.cjs", "javascript"),
            ("src/a.jsx", "javascript"),
            ("src/a.ts", "typescript"),
            ("src/a.mts", "typescript"),
            ("src/a.cts", "typescript"),
            ("src/a.tsx", "typescript"),
            ("src/a.d.ts", "typescript"),
        ] {
            fx.accept(path, Some("export function f() { return 1 }\n"));
            let Derivation::Indexed(c) = derived(&fx, path) else {
                panic!("{path} is not indexed");
            };
            assert_eq!(c.language, language, "{path}");
        }
        // Each grammar accepts only its own syntax.
        let mixed = [
            ("src/types.js", "let x: number = 1;\n"),
            ("src/jsx.ts", "const e = <div/>;\n"),
            ("src/assertion.tsx", "const y = <number>z;\n"),
            ("src/broken.js", "function f( {\n"),
            ("src/broken.ts", "interface {\n"),
        ];
        for (path, content) in mixed {
            fx.accept(path, Some(content));
            assert!(matches!(derived(&fx, path), Derivation::Declined), "{path}");
            assert_eq!(fx.status(path), Freshness::Unindexed, "{path}");
        }
        for (path, content) in [
            ("src/jsx.jsx", "const e = <div>{f()}</div>;\n"),
            ("src/jsx.tsx", "const e = <div>{f()}</div>;\n"),
            ("src/assertion.ts", "const y = <number>z;\n"),
        ] {
            fx.accept(path, Some(content));
            assert!(
                matches!(derived(&fx, path), Derivation::Indexed(_)),
                "{path}"
            );
        }
        for path in ["src/a.json", "src/a.vue", "src/a.ts.map", "src/README"] {
            fx.accept(path, Some("export function f() {}\n"));
            assert!(
                matches!(derived(&fx, path), Derivation::Unsupported),
                "{path}"
            );
        }
    }
}
