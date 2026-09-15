//! Tree-sitter is confined to this adapter. Resolution is intentionally conservative:
//! a relation gets a target only when exactly one compatible declaration is
//! syntactically visible, and never through a name a local binding shadows.
use super::{
    Edge, Entity, EntityKind, Language, Provenance, RelationKind, ResolutionRule, SourceRange,
    model::stable_id,
};
use crate::{
    local::{Error, Result, require},
    protocol::GraphEntityId,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use tree_sitter::{Node, ParseOptions, Parser};

pub(super) struct Derivation {
    pub entities: Vec<Entity>,
    pub edges: Vec<Edge>,
}

pub(super) fn extract(source: &str, provenance: Provenance) -> Result<Derivation> {
    let language = match provenance.language {
        Language::Rust => tree_sitter_rust::LANGUAGE,
        Language::Python => tree_sitter_python::LANGUAGE,
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
        Language::JavaScript => tree_sitter_javascript::LANGUAGE,
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language.into())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let started = Instant::now();
    let mut cancelled = |_: &tree_sitter::ParseState| started.elapsed() > Duration::from_secs(2);
    let tree = parser
        .parse_with_options(
            &mut |offset, _| &source.as_bytes()[offset..],
            None,
            Some(ParseOptions::new().progress_callback(&mut cancelled)),
        )
        .ok_or_else(|| Error::Invalid("parser exceeded 2 second limit".into()))?;
    require(
        !tree.root_node().has_error(),
        "syntax error or missing syntax; previous file facts invalidated",
    )?;
    let mut builder = Builder {
        source,
        provenance,
        entities: vec![],
        edges: vec![],
        shadowed: vec![],
        scopes: vec![],
        globs: BTreeSet::new(),
        occurrences: BTreeMap::new(),
        visited: 0,
    };
    let root = tree.root_node();
    let path = builder.provenance.path.clone();
    let file = builder.entity(root, EntityKind::File, &path, None)?;
    let module_name = path
        .rsplit_once('.')
        .map_or(path.as_str(), |(stem, _)| stem)
        .replace('/', "::");
    let module = builder.entity(root, EntityKind::Module, &module_name, Some(file))?;
    builder.walk(root, module, 0)?;
    builder.resolve_local();
    Ok(Derivation {
        entities: builder.entities,
        edges: builder.edges,
    })
}

struct Builder<'a> {
    source: &'a str,
    provenance: Provenance,
    entities: Vec<Entity>,
    edges: Vec<Edge>,
    /// Parallel to `edges`: the target name is bound by an enclosing local scope.
    shadowed: Vec<bool>,
    /// Names bound (parameters, lets, assignments) by each enclosing function.
    scopes: Vec<BTreeSet<String>>,
    /// Modules declaring `use super::*;`.
    globs: BTreeSet<usize>,
    occurrences: BTreeMap<String, usize>,
    visited: usize,
}

fn range(node: Node<'_>) -> SourceRange {
    SourceRange {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

pub(super) fn compact(value: &str, limit: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}::{segment}")
    }
}

/// Module key from a path: `src/local/mod.rs` → `src::local`, `src/lib.rs` →
/// `src`, `pkg/__init__.py` → `pkg`, `web/index.ts` → `web`.
fn module_key(path: &str) -> String {
    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    let mut segments: Vec<&str> = stem.split('/').collect();
    if segments.len() > 1
        && matches!(
            segments.last(),
            Some(&("mod" | "lib" | "main" | "__init__" | "index"))
        )
    {
        segments.pop();
    }
    segments.join("::")
}

/// Rust crate root for `crate::` paths: through the last `src` directory, or the
/// file module itself (integration tests, examples, build scripts).
fn crate_root(path: &str) -> String {
    let key = module_key(path);
    let segments: Vec<&str> = key.split("::").collect();
    match segments.iter().rposition(|s| *s == "src") {
        Some(i) => segments[..=i].join("::"),
        None => key,
    }
}

fn parent_key(key: &str) -> Option<String> {
    key.rsplit_once("::").map(|(parent, _)| parent.to_string())
}

fn is_ident(s: &str) -> bool {
    let s = s.strip_prefix("r#").unwrap_or(s);
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Index just past the bracket group opened at `open`, ignoring `->` arrows.
fn skip_group(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'<' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `a::<T>::b` → `a::b`; whitespace removed. Leaves anything unbalanced as-is.
fn normalize_path(text: &str) -> String {
    let compact: String = text.split_whitespace().collect();
    let mut out = String::with_capacity(compact.len());
    let mut i = 0;
    while i < compact.len() {
        if compact[i..].starts_with("::<")
            && let Some(end) = skip_group(&compact, i + 2)
        {
            i = end;
            continue;
        }
        let c = compact[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// First top-level (outside `<>`) occurrence of `separator`.
fn split_top_level<'a>(s: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' => depth = depth.saturating_sub(1),
            _ if depth == 0 && s[i..].starts_with(separator) => {
                return Some((&s[..i], &s[i + separator.len()..]));
            }
            _ => {}
        }
    }
    None
}

/// The self type named by a Rust `impl` header: `impl<'a> Runtime<'a>` →
/// `Runtime`, `impl TryFrom<String> for RepositoryId` → `RepositoryId`.
fn impl_self_type(header: &str) -> Option<String> {
    let header = header.trim();
    let header = header
        .strip_prefix("unsafe ")
        .unwrap_or(header)
        .trim_start();
    let mut rest = header.strip_prefix("impl")?.trim_start();
    if rest.starts_with('<') {
        rest = rest[skip_group(rest, 0)?..].trim_start();
    }
    let target = split_top_level(rest, " for ").map_or(rest, |(_, t)| t);
    let target = split_top_level(target, " where ").map_or(target, |(t, _)| t);
    let target = target
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches("dyn ")
        .trim();
    let base = split_top_level(target, "<").map_or(target, |(base, _)| base);
    let name = base.rsplit("::").next()?.trim();
    is_ident(name).then(|| name.to_string())
}

impl Builder<'_> {
    fn text(&self, node: Node<'_>) -> &str {
        &self.source[node.byte_range()]
    }

    fn entity(
        &mut self,
        node: Node<'_>,
        kind: EntityKind,
        name: &str,
        parent: Option<usize>,
    ) -> Result<usize> {
        require(
            self.entities.len() < 10_000,
            "file exceeds 10000 graph entities",
        )?;
        let name = compact(name, 160);
        let qualified = match parent {
            Some(p) if self.entities[p].kind != EntityKind::File => {
                format!("{}::{name}", self.entities[p].qualified_name)
            }
            _ => name.clone(),
        };
        require(
            qualified.len() <= 4096,
            "qualified symbol name exceeds 4096 bytes",
        )?;
        let key = match parent {
            None => String::new(),
            Some(p) if self.entities[p].kind == EntityKind::File => {
                module_key(&self.provenance.path)
            }
            Some(p) => {
                let segment = if kind == EntityKind::Other {
                    impl_self_type(&name).unwrap_or_else(|| name.clone())
                } else {
                    name.clone()
                };
                join(&self.entities[p].key, &segment)
            }
        };
        let occurrence = format!("{kind:?}:{qualified}");
        let ordinal = self.occurrences.entry(occurrence.clone()).or_default();
        let id = GraphEntityId::new(stable_id(&[
            "entity-v1",
            self.provenance.repository_id.as_str(),
            &self.provenance.path,
            &format!("{:?}", self.provenance.language),
            &occurrence,
            &ordinal.to_string(),
        ]))
        .map_err(Error::Invalid)?;
        *ordinal += 1;
        let signature_end = node
            .child_by_field_name("body")
            .or_else(|| {
                node.child_by_field_name("value")
                    .and_then(|v| v.child_by_field_name("body"))
            })
            .map_or(node.end_byte(), |n| n.start_byte());
        let signature = if matches!(kind, EntityKind::File | EntityKind::Module)
            || kind == EntityKind::Test && node.kind() == "call_expression"
        {
            name.clone()
        } else {
            compact(&self.source[node.start_byte()..signature_end], 240)
        };
        let visibility = if self.provenance.language == Language::Rust {
            node.named_children(&mut node.walk())
                .find(|n| n.kind() == "visibility_modifier")
                .map(|n| compact(self.text(n), 64))
        } else {
            None
        };
        let index = self.entities.len();
        self.entities.push(Entity {
            id,
            kind,
            name,
            qualified_name: qualified,
            key,
            parent: parent.map(|p| self.entities[p].id.clone()),
            range: range(node),
            signature,
            visibility,
            provenance: self.provenance.clone(),
        });
        if let Some(p) = parent {
            self.edge(p, Some(index), RelationKind::Contains, "", node)?;
        }
        if kind == EntityKind::Test {
            // A test's lexical container is known; naming is NOT proof of what it verifies.
            if let Some(p) = parent {
                self.edge(index, Some(p), RelationKind::TestRelatedTo, "", node)?;
            }
        }
        Ok(index)
    }

    fn edge(
        &mut self,
        source: usize,
        target: Option<usize>,
        kind: RelationKind,
        target_name: &str,
        node: Node<'_>,
    ) -> Result<()> {
        require(
            self.edges.len() < 20_000,
            "file exceeds 20000 graph relations",
        )?;
        let source = self.entities[source].id.clone();
        let target = target.map(|t| self.entities[t].id.clone());
        let target_name = compact(target_name, 240);
        let id = stable_id(&[
            "edge-v1",
            source.as_str(),
            target.as_ref().map_or("", GraphEntityId::as_str),
            &format!("{kind:?}"),
            &target_name,
            &node.start_byte().to_string(),
            &self.edges.len().to_string(),
        ]);
        let bare = normalize_path(&target_name);
        self.shadowed
            .push(!bare.is_empty() && self.scopes.iter().any(|s| s.contains(&bare)));
        self.edges.push(Edge {
            id,
            source,
            target,
            target_name,
            kind,
            resolution: None,
            path_hint: None,
            range: range(node),
            provenance: self.provenance.clone(),
        });
        Ok(())
    }

    fn is_test(&self, node: Node<'_>, name: &str, parent: usize) -> bool {
        match self.provenance.language {
            Language::Rust => {
                let mut sibling = node.prev_named_sibling();
                while let Some(n) = sibling {
                    if n.kind() != "attribute_item" {
                        break;
                    }
                    if self.text(n).split_whitespace().collect::<String>() == "#[test]" {
                        return true;
                    }
                    sibling = n.prev_named_sibling();
                }
                false
            }
            Language::Python => {
                let file = self.provenance.path.rsplit('/').next().unwrap_or("");
                name.starts_with("test_")
                    && (file.starts_with("test_")
                        || file.ends_with("_test.py")
                        || self.entities[parent].name.starts_with("Test"))
            }
            _ => false,
        }
    }

    fn declaration(&self, node: Node<'_>, parent: usize) -> Option<(EntityKind, String)> {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.text(n).to_string());
        let kind = match (self.provenance.language, node.kind()) {
            (Language::Rust, "function_item" | "function_signature_item")
            | (Language::Python, "function_definition")
            | (
                Language::TypeScript | Language::Tsx | Language::JavaScript,
                "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "method_signature",
            ) => {
                if self.is_test(node, name.as_deref().unwrap_or(""), parent) {
                    EntityKind::Test
                } else if matches!(
                    self.entities[parent].kind,
                    EntityKind::Type | EntityKind::Trait | EntityKind::Other
                ) {
                    EntityKind::Method
                } else {
                    EntityKind::Function
                }
            }
            (Language::Rust, "struct_item" | "union_item" | "type_item")
            | (Language::Python, "class_definition")
            | (
                Language::TypeScript | Language::Tsx | Language::JavaScript,
                "class_declaration" | "abstract_class_declaration" | "type_alias_declaration",
            ) => EntityKind::Type,
            (Language::Rust, "enum_item")
            | (Language::TypeScript | Language::Tsx, "enum_declaration") => EntityKind::Enum,
            (Language::Rust, "trait_item")
            | (Language::TypeScript | Language::Tsx, "interface_declaration") => EntityKind::Trait,
            (Language::Rust, "mod_item")
            | (Language::TypeScript | Language::Tsx, "internal_module") => EntityKind::Module,
            (Language::Rust, "const_item" | "static_item") => EntityKind::Constant,
            (Language::Rust, "impl_item") => {
                let end = node
                    .child_by_field_name("body")
                    .map_or(node.end_byte(), |n| n.start_byte());
                return Some((
                    EntityKind::Other,
                    compact(&self.source[node.start_byte()..end], 160),
                ));
            }
            (
                Language::TypeScript | Language::Tsx | Language::JavaScript,
                "variable_declarator",
            ) => {
                let value = node.child_by_field_name("value")?;
                if matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                ) {
                    EntityKind::Function
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        name.map(|name| (kind, name))
    }

    /// A node whose own bindings shadow outer names for code inside it.
    fn is_scope(&self, node: Node<'_>) -> bool {
        matches!(
            (self.provenance.language, node.kind()),
            (Language::Rust, "function_item")
                | (Language::Python, "function_definition" | "class_definition")
        )
    }

    /// The part of `node` that introduces local bindings, if any.
    fn binding_pattern<'t>(&self, node: Node<'t>) -> Option<Node<'t>> {
        match (self.provenance.language, node.kind()) {
            (
                Language::Rust,
                "parameter" | "let_declaration" | "for_expression" | "let_condition" | "match_arm",
            ) => node.child_by_field_name("pattern"),
            (Language::Rust, "closure_parameters")
            | (
                Language::Python,
                "parameters" | "lambda_parameters" | "import_statement" | "import_from_statement",
            ) => Some(node),
            (
                Language::Python,
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause",
            ) => node.child_by_field_name("left"),
            (Language::Python, "named_expression") => node.child_by_field_name("name"),
            (Language::Python, "as_pattern") => node.child_by_field_name("alias"),
            _ => None,
        }
    }

    /// Every name bound anywhere in a function (over-approximated: closure and
    /// block bindings count for the whole function), excluding nested scopes.
    fn binders(&self, scope: Node<'_>) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            if node != scope && self.is_scope(node) {
                continue;
            }
            if let Some(pattern) = self.binding_pattern(node) {
                let mut inner = vec![pattern];
                while let Some(n) = inner.pop() {
                    if n.kind() == "identifier" {
                        names.insert(self.text(n).to_string());
                    }
                    inner.extend(n.named_children(&mut n.walk()));
                }
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        names
    }

    fn walk(&mut self, node: Node<'_>, parent: usize, depth: usize) -> Result<()> {
        self.visited += 1;
        require(
            depth <= 128 && self.visited <= 200_000,
            "syntax tree exceeds extraction depth/node limit",
        )?;
        let mut owner = parent;
        if let Some((kind, name)) = self.declaration(node, parent) {
            owner = self.entity(node, kind, &name, Some(parent))?;
        }
        match (self.provenance.language, node.kind()) {
            (Language::Rust, "use_declaration")
            | (Language::Python, "import_statement" | "import_from_statement")
            | (Language::TypeScript | Language::Tsx | Language::JavaScript, "import_statement") => {
                let name = self.text(node).to_string();
                self.edge(owner, None, RelationKind::Imports, &name, node)?;
                if self.provenance.language == Language::Rust
                    && self.entities[owner].kind == EntityKind::Module
                    && node
                        .child_by_field_name("argument")
                        .is_some_and(|a| normalize_path(self.text(a)) == "super::*")
                {
                    self.globs.insert(owner);
                }
            }
            (Language::TypeScript | Language::Tsx | Language::JavaScript, "export_statement") => {
                if let Some(source) = node.child_by_field_name("source") {
                    let name = self.text(source).to_string();
                    self.edge(owner, None, RelationKind::DependsOn, &name, node)?;
                }
            }
            (Language::Rust, "mod_item") if node.child_by_field_name("body").is_none() => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or("")
                    .to_string();
                self.edge(owner, None, RelationKind::DependsOn, &name, node)?;
            }
            (Language::Rust, "impl_item") => {
                if let Some(trait_node) = node.child_by_field_name("trait") {
                    let name = self.text(trait_node).to_string();
                    self.edge(owner, None, RelationKind::Implements, &name, trait_node)?;
                }
            }
            (_, "call_expression" | "call") => {
                if let Some(function) = node.child_by_field_name("function") {
                    let target = self.text(function).to_string();
                    if matches!(
                        self.provenance.language,
                        Language::TypeScript | Language::Tsx | Language::JavaScript
                    ) && ["test", "it"].contains(&target.as_str())
                        && (self.provenance.path.contains(".test.")
                            || self.provenance.path.contains(".spec."))
                        && let Some(args) = node.child_by_field_name("arguments")
                        && let Some(title) = args.named_child(0).filter(|n| n.kind() == "string")
                    {
                        let name = format!("{target}:{}", self.text(title));
                        owner = self.entity(node, EntityKind::Test, &name, Some(parent))?;
                    }
                    self.edge(owner, None, RelationKind::Calls, &target, function)?;
                }
            }
            (Language::Rust, "type_identifier" | "scoped_type_identifier") => {
                // A declaration's own name is not a reference to itself.
                let declared = node
                    .parent()
                    .and_then(|p| p.child_by_field_name("name"))
                    .is_some_and(|n| n == node);
                if !declared {
                    let name = self.text(node).to_string();
                    self.edge(owner, None, RelationKind::References, &name, node)?;
                }
            }
            (Language::Python, "class_definition") => {
                if let Some(bases) = node.child_by_field_name("superclasses") {
                    for base in bases.named_children(&mut bases.walk()) {
                        let name = self.text(base).to_string();
                        self.edge(owner, None, RelationKind::References, &name, base)?;
                    }
                }
            }
            (Language::TypeScript | Language::Tsx, "implements_clause") => {
                for target in node.named_children(&mut node.walk()) {
                    let name = self.text(target).to_string();
                    self.edge(owner, None, RelationKind::Implements, &name, target)?;
                }
            }
            _ => {}
        }
        let scoped = self.is_scope(node);
        if scoped {
            let names = self.binders(node);
            self.scopes.push(names);
        }
        for child in node.named_children(&mut node.walk()) {
            self.walk(child, owner, depth + 1)?;
        }
        if scoped {
            self.scopes.pop();
        }
        Ok(())
    }

    /// Resolves relations whose unique target is visible in this file, and
    /// records a normalized path hint for Rust qualified paths that may name a
    /// declaration in another file (resolved later, workspace-wide).
    fn resolve_local(&mut self) {
        let scope = LocalScope::new(self.provenance.language, &self.entities, &self.globs);
        let root = crate_root(&self.provenance.path);
        for (edge, &shadowed) in self.edges.iter_mut().zip(&self.shadowed) {
            if edge.target.is_some()
                || !matches!(
                    edge.kind,
                    RelationKind::Calls | RelationKind::References | RelationKind::Implements
                )
            {
                continue;
            }
            let source = scope.index[&edge.source];
            let text = normalize_path(&edge.target_name);
            if let Some((target, rule)) = scope.resolve(source, edge.kind, &text, shadowed) {
                edge.target = Some(self.entities[target].id.clone());
                edge.resolution = Some(rule);
            } else if self.provenance.language == Language::Rust {
                edge.path_hint = scope.hint(source, &text, &root);
            }
        }
    }
}

enum Scope {
    Module(usize),
    Type(usize, String),
}

/// One file's declarations, indexed for lexical lookups.
struct LocalScope<'a> {
    language: Language,
    entities: &'a [Entity],
    globs: &'a BTreeSet<usize>,
    index: BTreeMap<GraphEntityId, usize>,
    parent: Vec<Option<usize>>,
    /// Nearest enclosing module (itself for a module).
    module: Vec<usize>,
    children: BTreeMap<(usize, String), Vec<usize>>,
    impl_type: Vec<Option<String>>,
    /// (module of the impl, self type, method name) → methods.
    methods: BTreeMap<(usize, String, String), Vec<usize>>,
}

impl<'a> LocalScope<'a> {
    fn new(language: Language, entities: &'a [Entity], globs: &'a BTreeSet<usize>) -> Self {
        let index: BTreeMap<_, _> = entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let parent: Vec<Option<usize>> = entities
            .iter()
            .map(|e| e.parent.as_ref().map(|p| index[p]))
            .collect();
        // Entities are created parent-first, so one forward pass suffices.
        let mut module = Vec::with_capacity(entities.len());
        for (i, e) in entities.iter().enumerate() {
            module.push(if e.kind == EntityKind::Module {
                i
            } else {
                parent[i].map_or(i, |p| module[p])
            });
        }
        let mut children: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
        for (i, p) in parent.iter().enumerate() {
            if let Some(p) = p {
                children
                    .entry((*p, entities[i].name.clone()))
                    .or_default()
                    .push(i);
            }
        }
        let impl_type: Vec<Option<String>> = entities
            .iter()
            .map(|e| {
                (language == Language::Rust && e.kind == EntityKind::Other)
                    .then(|| impl_self_type(&e.name))
                    .flatten()
            })
            .collect();
        let mut methods: BTreeMap<(usize, String, String), Vec<usize>> = BTreeMap::new();
        for (i, e) in entities.iter().enumerate() {
            if e.kind == EntityKind::Method
                && let Some(p) = parent[i]
                && let Some(ty) = &impl_type[p]
            {
                methods
                    .entry((module[p], ty.clone(), e.name.clone()))
                    .or_default()
                    .push(i);
            }
        }
        Self {
            language,
            entities,
            globs,
            index,
            parent,
            module,
            children,
            impl_type,
            methods,
        }
    }

    fn compatible(&self, kind: RelationKind, target: usize, bare: bool) -> bool {
        let t = self.entities[target].kind;
        match kind {
            RelationKind::Calls if bare => {
                t == EntityKind::Function
                    || self.language == Language::Python && t == EntityKind::Type
            }
            RelationKind::Calls => {
                matches!(
                    t,
                    EntityKind::Function | EntityKind::Method | EntityKind::Test
                )
            }
            RelationKind::References => {
                matches!(t, EntityKind::Type | EntityKind::Enum | EntityKind::Trait)
            }
            RelationKind::Implements => t == EntityKind::Trait,
            _ => false,
        }
    }

    fn unique(
        &self,
        candidates: Option<&Vec<usize>>,
        keep: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let mut matching = candidates?.iter().copied().filter(|&c| keep(c));
        let first = matching.next()?;
        matching.next().is_none().then_some(first)
    }

    fn named(&self, scope: usize, name: &str) -> Option<&Vec<usize>> {
        self.children.get(&(scope, name.to_string()))
    }

    fn enclosing(&self, mut i: usize, found: impl Fn(usize) -> bool) -> Option<usize> {
        loop {
            if found(i) {
                return Some(i);
            }
            if self.entities[i].kind == EntityKind::Module {
                return None;
            }
            i = self.parent[i]?;
        }
    }

    fn parent_module(&self, module: usize) -> Option<usize> {
        let outer = self.module[self.parent[module]?];
        (outer != module && self.entities[outer].kind == EntityKind::Module).then_some(outer)
    }

    /// The unique declaration a bare name denotes in the lexical scope chain; an
    /// incompatible or ambiguous inner declaration stops the search.
    fn lexical(&self, source: usize, name: &str, kind: RelationKind) -> Option<usize> {
        let mut current = Some(source);
        while let Some(s) = current {
            let e = &self.entities[s];
            if matches!(
                e.kind,
                EntityKind::Function | EntityKind::Method | EntityKind::Test | EntityKind::Module
            ) && let Some(all) = self.named(s, name)
            {
                return (all.len() == 1 && self.compatible(kind, all[0], true)).then(|| all[0]);
            }
            if e.kind == EntityKind::Module {
                if self.language == Language::Rust && self.globs.contains(&s) {
                    let outer = self.parent_module(s)?;
                    let all = self.named(outer, name)?;
                    return (all.len() == 1 && self.compatible(kind, all[0], true)).then(|| all[0]);
                }
                return None;
            }
            current = self.parent[s];
        }
        None
    }

    fn resolve(
        &self,
        source: usize,
        kind: RelationKind,
        text: &str,
        shadowed: bool,
    ) -> Option<(usize, ResolutionRule)> {
        match self.language {
            Language::Rust => self.rust(source, kind, text, shadowed),
            Language::Python => {
                if kind != RelationKind::Calls {
                    return None;
                }
                if let Some(method) = text.strip_prefix("self.") {
                    return self.receiver_method(source, method);
                }
                (is_ident(text) && !shadowed)
                    .then(|| self.lexical(source, text, kind))
                    .flatten()
                    .map(|t| (t, ResolutionRule::LexicalScope))
            }
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                let method = text.strip_prefix("this.")?;
                (kind == RelationKind::Calls)
                    .then(|| self.receiver_method(source, method))
                    .flatten()
            }
        }
    }

    /// `self.m()` / `this.m()` to a method of the enclosing class.
    fn receiver_method(&self, source: usize, method: &str) -> Option<(usize, ResolutionRule)> {
        if !is_ident(method) {
            return None;
        }
        let class = self.enclosing(source, |i| self.entities[i].kind == EntityKind::Type)?;
        self.unique(self.named(class, method), |c| {
            self.entities[c].kind == EntityKind::Method
        })
        .map(|t| (t, ResolutionRule::EnclosingType))
    }

    fn rust(
        &self,
        source: usize,
        kind: RelationKind,
        text: &str,
        shadowed: bool,
    ) -> Option<(usize, ResolutionRule)> {
        let module = self.module[source];
        if let Some(method) = text.strip_prefix("self.") {
            if kind != RelationKind::Calls || !is_ident(method) {
                return None;
            }
            let imp = self.enclosing(source, |i| self.impl_type[i].is_some())?;
            let ty = self.impl_type[imp].as_ref()?;
            return self
                .unique(
                    self.methods
                        .get(&(self.module[imp], ty.clone(), method.to_string())),
                    |_| true,
                )
                .map(|t| (t, ResolutionRule::EnclosingType));
        }
        let segments: Vec<&str> = text.split("::").collect();
        if segments.iter().any(|s| !is_ident(s)) {
            return None;
        }
        let (last, path) = segments.split_last()?;
        if path.is_empty() {
            return (kind == RelationKind::Calls && !shadowed)
                .then(|| self.lexical(source, last, kind))
                .flatten()
                .map(|t| (t, ResolutionRule::LexicalScope));
        }
        let (mut scope, rest) = match path[0] {
            "self" => (Scope::Module(module), &path[1..]),
            "super" => (Scope::Module(self.parent_module(module)?), &path[1..]),
            "Self" => {
                let imp = self.enclosing(source, |i| self.impl_type[i].is_some())?;
                (
                    Scope::Type(self.module[imp], self.impl_type[imp].clone()?),
                    &path[1..],
                )
            }
            "crate" => return None,
            _ => (Scope::Module(module), path),
        };
        for segment in rest {
            scope = match scope {
                Scope::Module(m) if *segment == "super" => Scope::Module(self.parent_module(m)?),
                Scope::Module(m) => {
                    if let Some(child) = self.unique(self.named(m, segment), |c| {
                        self.entities[c].kind == EntityKind::Module
                    }) {
                        Scope::Module(child)
                    } else {
                        self.unique(self.named(m, segment), |c| {
                            matches!(
                                self.entities[c].kind,
                                EntityKind::Type | EntityKind::Enum | EntityKind::Trait
                            )
                        })?;
                        Scope::Type(m, segment.to_string())
                    }
                }
                Scope::Type(..) => return None,
            };
        }
        match scope {
            Scope::Module(m) => self
                .unique(self.named(m, last), |c| self.compatible(kind, c, false))
                .map(|t| (t, ResolutionRule::LexicalScope)),
            Scope::Type(m, ty) => (kind == RelationKind::Calls)
                .then(|| self.unique(self.methods.get(&(m, ty, last.to_string())), |_| true))
                .flatten()
                .map(|t| (t, ResolutionRule::EnclosingType)),
        }
    }

    /// `abs:<key>` for anchored paths, `rel:<crate root>|<path>` otherwise.
    fn hint(&self, source: usize, text: &str, crate_root: &str) -> Option<String> {
        let segments: Vec<&str> = text.split("::").collect();
        if segments.len() < 2 || segments.iter().any(|s| !is_ident(s)) {
            return None;
        }
        let module_key = &self.entities[self.module[source]].key;
        match segments[0] {
            "std" | "core" | "alloc" | "Self" => None,
            "self" => Some(format!(
                "abs:{}",
                join(module_key, &segments[1..].join("::"))
            )),
            "super" => {
                let mut key = module_key.clone();
                let mut i = 0;
                while segments[i] == "super" {
                    key = parent_key(&key)?;
                    i += 1;
                }
                (i < segments.len())
                    .then(|| format!("abs:{}", join(&key, &segments[i..].join("::"))))
            }
            "crate" => Some(format!(
                "abs:{}",
                join(crate_root, &segments[1..].join("::"))
            )),
            _ => Some(format!("rel:{crate_root}|{}", segments.join("::"))),
        }
    }
}
