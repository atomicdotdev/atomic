//! Context retained from the registered grammars for shared code intelligence.
//! Entity extraction remains owned by the existing language parsers; this module
//! annotates those entities and call nodes instead of reducing calls to a name.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};

use crate::{Entity, EntityKind, Language};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceSpan {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub entity: Entity,
    pub scope: Vec<String>,
    pub module_scope: Vec<String>,
    pub owner: Option<String>,
    pub location: SourceSpan,
    pub signature: Vec<String>,
    pub tokens: Vec<String>,
    pub min: usize,
    pub max: Option<usize>,
    pub arity_known: bool,
    pub receiver_parameter: bool,
    pub public: bool,
    pub restricted: bool,
    pub parent_visible: bool,
    pub conditional: bool,
    pub expansion_dependent: bool,
    pub member_private: bool,
    pub shadows: BTreeSet<String>,
    pub parameter_types: BTreeMap<String, String>,
    pub lexical_range: (usize, usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSite {
    pub caller: String,
    pub scope: Vec<String>,
    /// Qualified AST spelling, never just the final method/function name.
    pub expression: String,
    pub target: Vec<String>,
    pub receiver: Option<String>,
    pub arguments: Option<usize>,
    pub location: SourceSpan,
    pub tokens: Vec<String>,
    pub start_byte: usize,
    pub column: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportBinding {
    pub scope: Vec<String>,
    pub local: String,
    pub target: Vec<String>,
    pub script_module: Option<String>,
    pub namespace: bool,
    pub lexical_range: (usize, usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleDeclaration {
    pub scope: Vec<String>,
    pub external: bool,
    pub public: bool,
    pub visibility: Option<String>,
    pub conditional: bool,
}

#[derive(Clone, Debug)]
pub struct SourceAnalysis {
    pub path: String,
    pub language: Language,
    pub entities: Vec<Entity>,
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<ImportBinding>,
    pub modules: Vec<ModuleDeclaration>,
    pub globals: BTreeMap<Vec<String>, BTreeSet<String>>,
    pub tokens: Vec<String>,
    pub outside_tokens: Vec<String>,
    pub exports_complete: bool,
}

/// A selected source set and the complete path inventory it was selected from.
/// Completeness concerns selection/availability, not compiler/runtime proof.
#[derive(Clone, Debug, Default)]
pub struct SourceSet {
    pub files: Vec<SourceAnalysis>,
    pub paths: Vec<String>,
    pub gaps: Vec<String>,
    pub complete: bool,
    pub paths_complete: bool,
    pub searched_files: usize,
}

pub(crate) fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub(crate) fn qualified_path(value: &str, separator: &str) -> Option<Vec<String>> {
    let parts: Vec<_> = value
        .trim()
        .trim_start_matches("::")
        .split(separator)
        .map(str::trim)
        .collect();
    parts
        .iter()
        .all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '#')
        })
        .then(|| parts.into_iter().map(str::to_owned).collect())
}

fn callable(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "function_declaration"
            | "function_definition"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "init_declaration"
    )
}

fn enclosing_callable(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "closure_expression"
                | "arrow_function"
                | "lambda_expression"
                | "lambda"
                | "lambda_literal"
        ) {
            return None;
        }
        if callable(parent) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn lexical_container(mut node: Node<'_>) -> Node<'_> {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "block"
                | "statement_block"
                | "source_file"
                | "program"
                | "declaration_list"
                | "class_body"
        ) {
            return parent;
        }
        node = parent;
    }
    node
}

fn parameters(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..16 {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            return Some(parameters);
        }
        node = node.child_by_field_name("declarator")?;
    }
    None
}

fn owner(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "impl_item" => node
            .child_by_field_name("type")
            .map(|node| nominal(text(node, source))),
        "class_declaration"
        | "class_definition"
        | "class_specifier"
        | "struct_specifier"
        | "trait_item"
        | "interface_declaration" => node
            .child_by_field_name("name")
            .map(|node| text(node, source).into()),
        _ => None,
    }
}

fn nominal(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .split('<')
        .next()
        .unwrap_or(value)
        .trim()
        .into()
}

fn scope(mut node: Node<'_>, source: &str) -> (Vec<String>, Option<String>) {
    let (mut parts, mut receiver) = (Vec::new(), None);
    let mut nested_function = false;
    while let Some(parent) = node.parent() {
        if let Some(name) = owner(parent, source) {
            if receiver.is_none() && !nested_function {
                receiver = Some(name.clone());
            }
            parts.push(name);
        } else if callable(parent)
            || matches!(
                parent.kind(),
                "mod_item" | "namespace_definition" | "internal_module"
            )
        {
            if let Some(name) = parent.child_by_field_name("name") {
                parts.push(text(name, source).into());
            }
            if callable(parent) {
                nested_function = true;
            }
        }
        node = parent;
    }
    parts.reverse();
    (parts, receiver)
}

fn module_scope(mut node: Node<'_>, source: &str) -> Vec<String> {
    let mut parts = Vec::new();
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "mod_item" | "namespace_definition" | "internal_module"
        ) {
            if let Some(name) = parent.child_by_field_name("name") {
                parts.push(text(name, source).into());
            }
        }
        node = parent;
    }
    parts.reverse();
    parts
}

fn span(node: Node<'_>, path: &str) -> SourceSpan {
    SourceSpan {
        path: path.into(),
        start_line: node.start_position().row as u32 + 1,
        end_line: (node.end_position().row
            + usize::from(
                node.end_position().column > 0
                    || node.end_position().row == node.start_position().row,
            )) as u32,
    }
}

fn tokens(
    root: Node<'_>,
    source: &str,
    skip_body: Option<usize>,
    skip_functions: bool,
) -> Vec<String> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind().contains("comment")
            || Some(node.id()) == skip_body
            || (skip_functions && callable(node))
        {
            continue;
        }
        if node.child_count() == 0 {
            if !text(node, source).trim().is_empty() {
                result.push(text(node, source).into());
            }
        } else {
            let mut cursor = node.walk();
            stack.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    result
}

fn binding_names(root: Node<'_>, source: &str, names: &mut BTreeSet<String>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) {
            names.insert(text(node, source).into());
        } else {
            stack.extend(children(node));
        }
    }
}

fn conditional(mut node: Node<'_>, source: &str) -> bool {
    loop {
        let mut previous = node.prev_named_sibling();
        while let Some(attribute) = previous.filter(|node| node.kind() == "attribute_item") {
            if text(attribute, source).contains("cfg") {
                return true;
            }
            previous = attribute.prev_named_sibling();
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn rust_import(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    prefix: &[String],
    imports: &mut Vec<ImportBinding>,
) {
    match node.kind() {
        "use_list" => {
            for child in children(node) {
                rust_import(child, source, scope, prefix, imports);
            }
        }
        "scoped_use_list" => {
            let Some(qualified) = node
                .child_by_field_name("path")
                .and_then(|node| qualified_path(text(node, source), "::"))
            else {
                return;
            };
            let mut prefix = prefix.to_vec();
            prefix.extend(qualified);
            for list in children(node)
                .into_iter()
                .filter(|node| node.kind() == "use_list")
            {
                rust_import(list, source, scope, &prefix, imports);
            }
        }
        "use_wildcard" => {}
        _ => {
            // A scoped_identifier's `path` is only its prefix; unwrapping it
            // drops the imported symbol. Only an alias wraps a complete path.
            let argument = if node.kind() == "use_as_clause" {
                node.child_by_field_name("path").unwrap_or(node)
            } else {
                node
            };
            let Some(target) = qualified_path(text(argument, source), "::") else {
                return;
            };
            let mut full = prefix.to_vec();
            full.extend(target);
            if full.last().is_some_and(|name| name == "self") {
                full.pop();
            }
            if let Some(local) = node
                .child_by_field_name("alias")
                .map(|node| text(node, source).to_owned())
                .or_else(|| full.last().cloned())
            {
                let lexical = lexical_container(node);
                imports.push(ImportBinding {
                    scope: scope.to_vec(),
                    local,
                    target: full,
                    script_module: None,
                    namespace: false,
                    lexical_range: (lexical.start_byte(), lexical.end_byte()),
                });
            }
        }
    }
}

impl SourceAnalysis {
    pub(crate) fn from_tree(
        path: &str,
        source: &str,
        language: Language,
        tree: &Tree,
        extract: impl FnOnce() -> Vec<Entity>,
    ) -> Result<Self, String> {
        if tree.root_node().has_error() {
            return Err("Source has recovered/missing syntax; semantic facts unavailable".into());
        }
        let mut nodes = Vec::new();
        let mut stack = vec![(tree.root_node(), 0)];
        while let Some((node, depth)) = stack.pop() {
            if nodes.len() >= 100_000 || depth > 256 {
                return Err("Semantic source analysis budget exceeded".into());
            }
            nodes.push(node);
            stack.extend(
                children(node)
                    .into_iter()
                    .rev()
                    .map(|node| (node, depth + 1)),
            );
        }
        let mut result = Self {
            path: path.into(),
            language,
            entities: extract(),
            symbols: vec![],
            calls: vec![],
            imports: vec![],
            modules: vec![],
            globals: BTreeMap::new(),
            tokens: tokens(tree.root_node(), source, None, false),
            outside_tokens: tokens(tree.root_node(), source, None, true),
            exports_complete: true,
        };
        let mut used = BTreeSet::new();
        let mut declarations = BTreeMap::<(u32, String), Vec<Node<'_>>>::new();
        let mut declarations_by_line = BTreeMap::<u32, Vec<Node<'_>>>::new();
        for node in nodes.iter().filter(|node| callable(**node)) {
            declarations_by_line
                .entry(node.start_position().row as u32 + 1)
                .or_default()
                .push(*node);
            if let Some(name) = node.child_by_field_name("name") {
                declarations
                    .entry((
                        node.start_position().row as u32 + 1,
                        text(name, source).into(),
                    ))
                    .or_default()
                    .push(*node);
            }
        }
        let mut symbol_nodes = BTreeMap::new();
        let file_conditional = children(tree.root_node()).iter().any(|node| {
            node.kind() == "inner_attribute_item" && text(*node, source).contains("cfg")
        });
        for entity in result
            .entities
            .iter()
            .filter(|entity| matches!(entity.kind, EntityKind::Function | EntityKind::Method))
        {
            let Some(&node) = declarations
                .get(&(entity.line, entity.name.clone()))
                .and_then(|nodes| nodes.iter().find(|node| !used.contains(&node.id())))
                .or_else(|| {
                    declarations_by_line.get(&entity.line).and_then(|nodes| {
                        nodes.iter().find(|node| {
                            !used.contains(&node.id())
                                && node.end_position().row as u32 + 1 == entity.end_line
                        })
                    })
                })
            else {
                continue;
            };
            used.insert(node.id());
            let (scope, owner) = scope(node, source);
            let name = entity.name.clone();
            let id = format!(
                "{path}#{}",
                scope
                    .iter()
                    .chain(std::iter::once(&name))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("::")
            );
            let body = node.child_by_field_name("body");
            let parameters = parameters(node);
            let (mut min, mut max) = (0, Some(0));
            let mut arity_known = parameters.is_some()
                && matches!(
                    language,
                    Language::Rust | Language::TypeScript | Language::Tsx
                );
            if node.kind() == "method_definition" {
                let mut cursor = node.walk();
                if node
                    .children(&mut cursor)
                    .any(|child| matches!(child.kind(), "get" | "set"))
                {
                    // `reader.value(args)` can call the value returned by a
                    // getter, not pass those arguments to the accessor itself.
                    arity_known = false;
                }
            }
            let mut parameters_conditional = false;
            let mut receiver_parameter = node.kind() == "method_definition"
                && !tokens(node, source, body.map(|body| body.id()), false)
                    .iter()
                    .any(|token| token == "static");
            let (mut shadows, mut parameter_types) = (BTreeSet::new(), BTreeMap::new());
            for parameter in parameters
                .map(children)
                .unwrap_or_else(|| {
                    children(node)
                        .into_iter()
                        .filter(|node| node.kind() == "parameter")
                        .collect()
                })
                .into_iter()
                .filter(|node| !node.kind().contains("comment"))
            {
                if parameter.kind() == "attribute_item"
                    || children(parameter)
                        .iter()
                        .any(|node| node.kind() == "attribute_item")
                {
                    arity_known = false;
                    parameters_conditional = true;
                }
                if parameter.kind() == "self_parameter"
                    || parameter
                        .child_by_field_name("pattern")
                        .is_some_and(|node| {
                            text(node, source) == "self"
                                || (language != Language::Rust && text(node, source) == "this")
                        })
                {
                    receiver_parameter = true;
                    continue;
                }
                let mut patterns = Vec::new();
                let mut parameter_cursor = parameter.walk();
                patterns.extend(parameter.children_by_field_name("name", &mut parameter_cursor));
                if patterns.is_empty() {
                    patterns.extend(
                        parameter
                            .child_by_field_name("pattern")
                            .or_else(|| parameter.child_by_field_name("declarator"))
                            .or_else(|| (parameter.kind() == "identifier").then_some(parameter))
                            .or_else(|| {
                                children(parameter).into_iter().find(|node| {
                                    node.kind() == "identifier"
                                        && parameter
                                            .child_by_field_name("type")
                                            .is_none_or(|kind| kind.id() != node.id())
                                })
                            }),
                    );
                }
                for pattern in patterns {
                    binding_names(pattern, source, &mut shadows);
                    if let Some(kind) = parameter.child_by_field_name("type") {
                        parameter_types.insert(
                            text(pattern, source).into(),
                            nominal(text(kind, source).trim_start_matches(':').trim()),
                        );
                    }
                }
                let spelling = tokens(parameter, source, None, false);
                if spelling.iter().any(|part| part == "...") {
                    max = None;
                } else {
                    if let Some(max) = &mut max {
                        *max += 1;
                    }
                    if parameter.kind() != "optional_parameter"
                        && parameter.child_by_field_name("value").is_none()
                    {
                        min = max.unwrap_or(min + 1);
                    }
                }
            }
            let begin = nodes.partition_point(|item| item.start_byte() < node.start_byte());
            for item in nodes[begin..]
                .iter()
                .take_while(|item| item.start_byte() < node.end_byte())
                .filter(|item| {
                    enclosing_callable(**item).is_some_and(|owner| owner.id() == node.id())
                })
            {
                if matches!(
                    item.kind(),
                    "let_declaration"
                        | "variable_declarator"
                        | "assignment"
                        | "augmented_assignment"
                        | "assignment_expression"
                        | "augmented_assignment_expression"
                        | "for_expression"
                        | "for_in_statement"
                        | "catch_clause"
                        | "let_condition"
                ) {
                    if let Some(pattern) = item
                        .child_by_field_name("pattern")
                        .or_else(|| item.child_by_field_name("name"))
                        .or_else(|| item.child_by_field_name("left"))
                        .or_else(|| item.child_by_field_name("parameter"))
                    {
                        binding_names(pattern, source, &mut shadows);
                    }
                }
            }
            let visibility = children(node)
                .into_iter()
                .find(|node| node.kind() == "visibility_modifier")
                .map(|node| text(node, source));
            let exported = node.parent().is_some_and(|node| {
                node.kind() == "export_statement"
                    && !tokens(node, source, None, false)
                        .iter()
                        .any(|token| token == "default")
            });
            let public = if language == Language::Rust {
                matches!(visibility, Some("pub" | "pub(crate)"))
            } else {
                exported || entity.exported
            };
            let lexical = lexical_container(node);
            let expansion_dependent = nodes[begin..]
                .iter()
                .take_while(|item| item.start_byte() < node.end_byte())
                .any(|item| item.kind() == "macro_invocation");
            let member_private = name.starts_with('#')
                || children(node).iter().any(|node| {
                    node.kind() == "accessibility_modifier" && text(*node, source) == "private"
                });
            symbol_nodes.insert(node.id(), result.symbols.len());
            result.symbols.push(Symbol {
                id,
                entity: entity.clone(),
                scope,
                module_scope: module_scope(node, source),
                owner,
                location: span(node, path),
                signature: tokens(node, source, body.map(|body| body.id()), false),
                tokens: tokens(node, source, None, false),
                min,
                max,
                arity_known,
                receiver_parameter,
                public,
                restricted: visibility.is_some_and(|value| {
                    !matches!(value, "pub" | "pub(crate)" | "pub(self)" | "pub(super)")
                }),
                parent_visible: visibility == Some("pub(super)"),
                conditional: conditional(node, source)
                    || file_conditional
                    || parameters_conditional,
                expansion_dependent,
                member_private,
                shadows,
                parameter_types,
                lexical_range: (lexical.start_byte(), lexical.end_byte()),
            });
        }
        let mut initializers = BTreeMap::new();
        for node in &nodes {
            match node.kind() {
                "call_expression" | "call" | "method_invocation" => {
                    let caller_index = if let Some(index) = enclosing_callable(*node)
                        .and_then(|node| symbol_nodes.get(&node.id()))
                        .copied()
                    {
                        index
                    } else {
                        let mut parent = node.parent();
                        let mut nested = false;
                        while let Some(item) = parent {
                            if callable(item)
                                || matches!(
                                    item.kind(),
                                    "closure_expression"
                                        | "arrow_function"
                                        | "lambda"
                                        | "lambda_expression"
                                        | "lambda_literal"
                                )
                            {
                                nested = true;
                                break;
                            }
                            parent = item.parent();
                        }
                        let (scope, owner) = scope(*node, source);
                        if nested || owner.is_some() {
                            continue;
                        }
                        if let Some(index) = initializers.get(&scope) {
                            *index
                        } else {
                            let id = if scope.is_empty() {
                                format!("{path}#<module>")
                            } else {
                                format!("{path}#{}::<module>", scope.join("::"))
                            };
                            let index = result.symbols.len();
                            result.symbols.push(Symbol {
                                id,
                                entity: Entity::new(
                                    "<module>",
                                    EntityKind::Module,
                                    path,
                                    1,
                                    tree.root_node().end_position().row as u32 + 1,
                                ),
                                scope: scope.clone(),
                                module_scope: scope.clone(),
                                owner: None,
                                location: span(tree.root_node(), path),
                                signature: vec![],
                                tokens: vec![],
                                min: 0,
                                max: None,
                                arity_known: false,
                                receiver_parameter: false,
                                public: false,
                                restricted: false,
                                parent_visible: false,
                                conditional: conditional(*node, source) || file_conditional,
                                expansion_dependent: false,
                                member_private: false,
                                shadows: BTreeSet::new(),
                                parameter_types: BTreeMap::new(),
                                lexical_range: (0, source.len()),
                            });
                            initializers.insert(scope, index);
                            index
                        }
                    };
                    let caller = &result.symbols[caller_index];
                    let Some(function) = node
                        .child_by_field_name("function")
                        .or_else(|| node.child_by_field_name("name"))
                        .or_else(|| {
                            (language == Language::Swift)
                                .then(|| children(*node).into_iter().next())
                                .flatten()
                        })
                    else {
                        continue;
                    };
                    let function = if function.kind() == "generic_function" {
                        function.child_by_field_name("function").unwrap_or(function)
                    } else {
                        function
                    };
                    let receiver = function
                        .child_by_field_name("value")
                        .or_else(|| function.child_by_field_name("object"))
                        .or_else(|| function.child_by_field_name("operand"))
                        .or_else(|| function.child_by_field_name("argument"))
                        .or_else(|| node.child_by_field_name("object"))
                        .map(|node| text(node, source).to_owned());
                    let expression = if node.kind() == "method_invocation" {
                        receiver
                            .as_ref()
                            .map(|receiver| format!("{receiver}.{}", text(function, source)))
                            .unwrap_or_else(|| text(function, source).into())
                    } else {
                        text(function, source).to_owned()
                    };
                    let target = qualified_path(
                        &expression,
                        if receiver.is_some() || expression.contains('.') {
                            "."
                        } else {
                            "::"
                        },
                    )
                    .unwrap_or_default();
                    let arguments = node.child_by_field_name("arguments").and_then(|arguments| {
                        let args: Vec<_> = children(arguments)
                            .into_iter()
                            .filter(|node| !node.kind().contains("comment"))
                            .collect();
                        (!args.iter().any(|node| node.kind() == "spread_element"))
                            .then_some(args.len())
                    });
                    result.calls.push(CallSite {
                        caller: caller.id.clone(),
                        scope: caller.scope.clone(),
                        expression,
                        target,
                        receiver,
                        arguments,
                        location: span(*node, path),
                        tokens: tokens(*node, source, None, false),
                        start_byte: node.start_byte(),
                        column: node.start_position().column as u32,
                    });
                }
                "use_declaration" => {
                    if let Some(argument) = node.child_by_field_name("argument") {
                        rust_import(
                            argument,
                            source,
                            &scope(*node, source).0,
                            &[],
                            &mut result.imports,
                        );
                    }
                }
                "import_statement" => {
                    let (scope, _) = scope(*node, source);
                    if tokens(*node, source, None, false)
                        .iter()
                        .any(|token| token == "type")
                    {
                        continue;
                    }
                    let Some(module) = node
                        .child_by_field_name("source")
                        .map(|node| text(node, source))
                    else {
                        continue;
                    };
                    if module.len() < 2 || module.contains('\\') {
                        continue;
                    }
                    for item in nodes.iter().filter(|item| {
                        item.start_byte() >= node.start_byte() && item.end_byte() <= node.end_byte()
                    }) {
                        if item.kind() == "import_specifier" {
                            if let Some(name) = item.child_by_field_name("name") {
                                result.imports.push(ImportBinding {
                                    scope: scope.clone(),
                                    local: text(
                                        item.child_by_field_name("alias").unwrap_or(name),
                                        source,
                                    )
                                    .into(),
                                    target: vec![text(name, source).into()],
                                    script_module: Some(module[1..module.len() - 1].into()),
                                    namespace: false,
                                    lexical_range: (0, source.len()),
                                });
                            }
                        } else if item.kind() == "namespace_import" {
                            if let Some(name) = children(*item)
                                .into_iter()
                                .find(|node| node.kind() == "identifier")
                            {
                                result.imports.push(ImportBinding {
                                    scope: scope.clone(),
                                    local: text(name, source).into(),
                                    target: vec![],
                                    script_module: Some(module[1..module.len() - 1].into()),
                                    namespace: true,
                                    lexical_range: (0, source.len()),
                                });
                            }
                        }
                    }
                }
                "mod_item" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let mut module = scope(*node, source).0;
                        module.push(text(name, source).into());
                        let public = children(*node)
                            .into_iter()
                            .find(|node| node.kind() == "visibility_modifier")
                            .is_some_and(|node| matches!(text(node, source), "pub" | "pub(crate)"));
                        result.modules.push(ModuleDeclaration {
                            scope: module,
                            external: node.child_by_field_name("body").is_none(),
                            public,
                            visibility: children(*node)
                                .into_iter()
                                .find(|node| node.kind() == "visibility_modifier")
                                .map(|node| text(node, source).into()),
                            conditional: conditional(*node, source) || file_conditional,
                        });
                    }
                }
                "variable_declarator"
                | "const_item"
                | "static_item"
                | "function_signature"
                | "assignment"
                | "augmented_assignment" => {
                    if let Some(name) = node
                        .child_by_field_name("name")
                        .or_else(|| node.child_by_field_name("left"))
                    {
                        binding_names(
                            name,
                            source,
                            result.globals.entry(scope(*node, source).0).or_default(),
                        );
                    }
                }
                "export_statement" if node.child_by_field_name("declaration").is_none() => {
                    result.exports_complete = false
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
