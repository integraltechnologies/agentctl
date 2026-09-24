//! Import statements: what they import, and the local names they bind.
//!
//! Rust `use a::b as c`, Python `from a import b as c` and TypeScript
//! `import { b as c } from "./a"` all say the same two things — *this file
//! imports `a::b`*, and *in this file, `c` means `a::b`*. Extraction records
//! both facts once per language. The first becomes an IMPORTS relation per
//! imported path; the second lets resolution reuse the qualified-path machinery,
//! so a bare `c()` is resolved exactly as if the source had written `a::b()`.
//!
//! This is deliberately syntax only. An import says where a name came from, not
//! what it is: nothing here infers types, follows re-exports through another
//! file, or expands a glob. Those stay UNKNOWN for a semantic provider.
//!
//! Paths are `::`-separated. A leading `::` marks a key anchored at the
//! repository root, which only a path relative to the importing file (a
//! TypeScript `./m` or a Python `.m`) can prove; anything else is a path as
//! written, resolved by the workspace pass.
use super::parser::normalize_path;
use super::*;
use tree_sitter::Node;

/// `local name` → normalized `::` path, as written at the import site.
pub(super) type Binding = (String, String);

/// What one import statement says.
pub(super) struct Import<'t> {
    /// Each imported path, with the node naming it.
    pub targets: Vec<(String, Node<'t>)>,
    pub bindings: Vec<Binding>,
}

/// Everything an import statement imports and binds. Unsupported or dynamic
/// forms (globs, package specifiers, `::extern` paths) yield nothing rather
/// than a guess; the statement is still observed, as an unresolved import.
pub(super) fn from_import<'t>(
    language: Language,
    path: &str,
    node: Node<'t>,
    text: &dyn Fn(Node) -> String,
) -> Import<'t> {
    let mut out = Import {
        targets: vec![],
        bindings: vec![],
    };
    match language {
        Language::Rust => {
            if let Some(argument) = node.child_by_field_name("argument") {
                rust(argument, "", text, &mut out);
            }
        }
        Language::Python => python(path, node, text, &mut out),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            typescript(path, node, text, &mut out)
        }
    }
    out.targets.retain(|(path, _)| valid(path));
    out.bindings
        .retain(|(local, path)| !local.is_empty() && valid(path));
    out
}

fn valid(path: &str) -> bool {
    !path.is_empty() && path != "::"
}

/// The last segment of a path is the name it binds when no alias renames it.
fn leaf(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).to_string()
}

/// Walks one `use` argument, carrying the prefix accumulated so far. Handles
/// `use a::b`, `use a::b as c`, `use a::{self, b, c as d}` uniformly;
/// wildcards bind and import nothing here. A `::name` path names an external
/// crate, never the repository, so it is dropped.
fn rust<'t>(node: Node<'t>, prefix: &str, text: &dyn Fn(Node) -> String, out: &mut Import<'t>) {
    let external = |path: &str| path.starts_with("::");
    match node.kind() {
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
            let path = join_path(prefix, &normalize_path(&text(node)));
            // `use a::b::{self}` imports and binds the module `a::b` itself.
            let path = path.strip_suffix("::self").unwrap_or(&path).to_string();
            if !external(&path) {
                out.bindings.push((leaf(&path), path.clone()));
                out.targets.push((path, node));
            }
        }
        "use_as_clause" => {
            let (Some(target), Some(alias)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) else {
                return;
            };
            let path = join_path(prefix, &normalize_path(&text(target)));
            let path = path.strip_suffix("::self").unwrap_or(&path).to_string();
            if !external(&path) {
                out.bindings.push((text(alias), path.clone()));
                out.targets.push((path, target));
            }
        }
        "scoped_use_list" => {
            let prefix = match node.child_by_field_name("path") {
                Some(path) => join_path(prefix, &normalize_path(&text(path))),
                None => prefix.to_string(),
            };
            if let Some(list) = node.child_by_field_name("list") {
                for child in named(list) {
                    rust(child, &prefix, text, out);
                }
            }
        }
        "use_list" => {
            for child in named(node) {
                rust(child, prefix, text, out);
            }
        }
        _ => {}
    }
}

/// `a.b` → `a::b`.
fn dotted(text: &str) -> String {
    normalize_path(text).replace('.', "::")
}

/// Directory segments of a repository path: `pkg/sub/m.py` → `["pkg", "sub"]`.
fn directory(path: &str) -> Vec<&str> {
    let mut segments: Vec<&str> = path.split('/').collect();
    segments.pop();
    segments
}

/// `from ..a import b` is anchored at the importing file's package: each dot
/// past the first climbs one directory. `None` when it climbs out of the
/// repository.
fn python_relative(path: &str, module: &str) -> Option<String> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    let mut base = directory(path);
    for _ in 1..dots {
        base.pop()?;
    }
    let rest = dotted(&module[dots..]);
    let mut key = base.join("::");
    if !rest.is_empty() {
        key = join_path(&key, &rest);
    }
    Some(format!("::{key}"))
}

fn python<'t>(path: &str, node: Node<'t>, text: &dyn Fn(Node) -> String, out: &mut Import<'t>) {
    // `from a.b import c as d` imports `a::b::c` and binds `d` to it;
    // `import a.b` imports `a::b` but binds only `a` (to `a`), which is what
    // Python itself does; `import a.b as c` binds `c` to `a::b`.
    let from = node.kind() == "import_from_statement";
    let module_node = node.child_by_field_name("module_name");
    let module = match module_node {
        Some(m) if m.kind() == "relative_import" => {
            let Some(anchored) = python_relative(path, &normalize_path(&text(m))) else {
                return;
            };
            anchored
        }
        Some(m) => dotted(&text(m)),
        None => String::new(),
    };
    for child in named(node) {
        if module_node.is_some_and(|m| m == child) {
            continue;
        }
        let (name, alias) = match child.kind() {
            "dotted_name" | "identifier" => (child, None),
            "aliased_import" => {
                let (Some(name), Some(alias)) = (
                    child.child_by_field_name("name"),
                    child.child_by_field_name("alias"),
                ) else {
                    continue;
                };
                (name, Some(text(alias)))
            }
            _ => continue,
        };
        let name = dotted(&text(name));
        let target = if from {
            join_path(&module, &name)
        } else {
            name.clone()
        };
        let binding = match alias {
            Some(alias) => (alias, target.clone()),
            None if from => (leaf(&name), target.clone()),
            None => {
                let head = name.split("::").next().unwrap_or(&name).to_string();
                (head.clone(), head)
            }
        };
        out.bindings.push(binding);
        out.targets.push((target, child));
    }
}

/// `"./foo/bar"` relative to `web/app/x.ts` → `::web::app::foo::bar`. A bare
/// package specifier names nothing in this workspace's key space.
fn typescript_module(path: &str, literal: &str) -> Option<String> {
    if !literal.starts_with('.') {
        return None;
    }
    let mut key = directory(path);
    for segment in literal.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                key.pop()?;
            }
            segment => key.push(segment),
        }
    }
    let last = key.pop()?;
    let stem = [".d.ts", ".tsx", ".ts", ".jsx", ".mjs", ".cjs", ".js"]
        .iter()
        .find_map(|ext| last.strip_suffix(ext))
        .unwrap_or(last);
    // `./dir/index` and `./dir` name the same module key.
    if stem != "index" || key.is_empty() {
        key.push(stem);
    }
    Some(format!("::{}", key.join("::")))
}

fn typescript<'t>(path: &str, node: Node<'t>, text: &dyn Fn(Node) -> String, out: &mut Import<'t>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let raw = text(source);
    let Some(module) = typescript_module(path, raw.trim_matches(['"', '\'', '`'])) else {
        return;
    };
    let mut clauses = named(node)
        .filter(|n| n.kind() == "import_clause")
        .peekable();
    // `import "./m"` imports the module for its effects and binds nothing.
    if clauses.peek().is_none() {
        out.targets.push((module, source));
        return;
    }
    for clause in clauses {
        for child in named(clause) {
            match child.kind() {
                // `import { a, b as c } from "./m"`
                "named_imports" => {
                    for specifier in named(child).filter(|n| n.kind() == "import_specifier") {
                        let Some(name) = specifier.child_by_field_name("name") else {
                            continue;
                        };
                        let target = join_path(&module, &text(name));
                        let local = specifier
                            .child_by_field_name("alias")
                            .map(text)
                            .unwrap_or_else(|| text(name));
                        out.bindings.push((local, target.clone()));
                        out.targets.push((target, specifier));
                    }
                }
                // `import d from "./m"` binds the module's default export and
                // `import * as ns from "./m"` its namespace; the module itself
                // is the strongest thing syntax proves either way.
                "identifier" => {
                    out.bindings.push((text(child), module.clone()));
                    out.targets.push((module.clone(), child));
                }
                "namespace_import" => {
                    if let Some(local) = named(child).find(|n| n.kind() == "identifier") {
                        out.bindings.push((text(local), module.clone()));
                        out.targets.push((module.clone(), child));
                    }
                }
                _ => {}
            }
        }
    }
}

fn named(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    children.into_iter()
}

fn join_path(prefix: &str, path: &str) -> String {
    if prefix == "::" {
        return format!("::{path}");
    }
    match (prefix.is_empty(), path.is_empty()) {
        (true, _) => path.to_string(),
        (_, true) => prefix.to_string(),
        _ => format!("{prefix}::{path}"),
    }
}
