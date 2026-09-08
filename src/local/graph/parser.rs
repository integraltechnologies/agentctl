//! Tree-sitter is confined to this adapter. Resolution is intentionally conservative.
use super::{
    Edge, Entity, EntityKind, Language, Provenance, RelationKind, SourceRange, model::stable_id,
};
use crate::{
    local::{Error, Result, require},
    protocol::GraphEntityId,
};
use std::{
    collections::BTreeMap,
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
    builder.resolve_rust_local();
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
        let key = format!("{kind:?}:{qualified}");
        let ordinal = self.occurrences.entry(key.clone()).or_default();
        let id = GraphEntityId::new(stable_id(&[
            "entity-v1",
            self.provenance.repository_id.as_str(),
            &self.provenance.path,
            &format!("{:?}", self.provenance.language),
            &key,
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
        self.edges.push(Edge {
            id,
            source,
            target,
            target_name,
            kind,
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
                    {
                        if let Some(args) = node.child_by_field_name("arguments") {
                            if let Some(title) =
                                args.named_child(0).filter(|n| n.kind() == "string")
                            {
                                let name = format!("{target}:{}", self.text(title));
                                owner = self.entity(node, EntityKind::Test, &name, Some(parent))?;
                            }
                        }
                    }
                    self.edge(owner, None, RelationKind::Calls, &target, function)?;
                }
            }
            (Language::Rust, "type_identifier" | "scoped_type_identifier") => {
                let name = self.text(node).to_string();
                self.edge(owner, None, RelationKind::References, &name, node)?;
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
        for child in node.named_children(&mut node.walk()) {
            self.walk(child, owner, depth + 1)?;
        }
        Ok(())
    }

    fn resolve_rust_local(&mut self) {
        if self.provenance.language != Language::Rust {
            return;
        }
        // Explicit self::name references to unique same-module declarations only.
        // Index the local namespace once, rather than joining every edge to every entity.
        let by_id: BTreeMap<_, _> = self
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let mut modules: Vec<Option<GraphEntityId>> = vec![];
        let mut declarations: BTreeMap<(GraphEntityId, String), Vec<usize>> = BTreeMap::new();
        for (i, entity) in self.entities.iter().enumerate() {
            let module = if entity.kind == EntityKind::Module {
                Some(entity.id.clone())
            } else {
                entity
                    .parent
                    .as_ref()
                    .and_then(|id| modules[by_id[id]].clone())
            };
            modules.push(module);
            if let Some(parent) = &entity.parent {
                declarations
                    .entry((parent.clone(), entity.name.clone()))
                    .or_default()
                    .push(i);
            }
        }
        for edge in &mut self.edges {
            if !matches!(
                edge.kind,
                RelationKind::Calls | RelationKind::References | RelationKind::Implements
            ) {
                continue;
            }
            let Some(name) = edge.target_name.strip_prefix("self::") else {
                continue;
            };
            if name.contains("::") || name.contains('<') {
                continue;
            }
            let Some(module) = &modules[by_id[&edge.source]] else {
                continue;
            };
            let Some(matches) = declarations.get(&(module.clone(), name.to_string())) else {
                continue;
            };
            if matches.len() != 1 {
                continue;
            }
            let target = &self.entities[matches[0]];
            let compatible = match edge.kind {
                RelationKind::Calls => {
                    matches!(target.kind, EntityKind::Function | EntityKind::Test)
                }
                RelationKind::References => matches!(
                    target.kind,
                    EntityKind::Type | EntityKind::Enum | EntityKind::Trait
                ),
                RelationKind::Implements => target.kind == EntityKind::Trait,
                _ => false,
            };
            if compatible {
                edge.target = Some(target.id.clone());
            }
        }
    }
}
