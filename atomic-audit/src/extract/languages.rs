//! Explicit source-syntax conventions. No compiler/type/runtime claims.

use super::tokens::{children, named, text};
use crate::metrics::Measurement;
use atomic_semantic::{Language, SyntaxNode as Node};
use std::collections::BTreeSet;

pub(super) fn literal(node: Node<'_>) -> bool {
    let kind = node.kind();
    (kind.contains("string") || kind.contains("char_literal") || kind == "template_string")
        && !named(node).iter().any(|child| {
            child.kind().contains("interpolation") || child.kind().contains("substitution")
        })
}

pub(super) fn class_like(node: Node<'_>, language: Language) -> bool {
    match language {
        Language::TypeScript | Language::Tsx => matches!(
            node.kind(),
            "class_declaration" | "abstract_class_declaration" | "class"
        ),
        Language::Python => node.kind() == "class_definition",
        Language::Java => matches!(
            node.kind(),
            "class_declaration" | "record_declaration" | "enum_declaration"
        ),
        Language::Cpp => matches!(node.kind(), "class_specifier" | "struct_specifier"),
        Language::Swift => {
            node.kind() == "class_declaration"
                && node
                    .child_by_field_name("declaration_kind")
                    .is_some_and(|kind| kind.kind() == "class")
        }
        Language::Rust | Language::Go | Language::C => false,
    }
}

pub(super) fn type_boundary(node: Node<'_>, language: Language) -> bool {
    class_like(node, language)
        || matches!(
            node.kind(),
            "struct_item"
                | "enum_item"
                | "trait_item"
                | "interface_declaration"
                | "protocol_declaration"
                | "struct_specifier"
                | "enum_specifier"
        )
        || (language == Language::Swift && node.kind() == "class_declaration")
}

pub(crate) fn callable(node: Node<'_>, language: Language) -> bool {
    match language {
        Language::Rust => matches!(
            node.kind(),
            "function_item" | "function_signature_item" | "closure_expression"
        ),
        Language::TypeScript | Language::Tsx => matches!(
            node.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "function"
                | "generator_function"
                | "arrow_function"
                | "method_definition"
                | "abstract_method_signature"
                | "method_signature"
        ),
        Language::Python => matches!(node.kind(), "function_definition" | "lambda"),
        Language::Java => matches!(
            node.kind(),
            "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration"
                | "lambda_expression"
        ),
        Language::Go => matches!(
            node.kind(),
            "function_declaration" | "method_declaration" | "func_literal" | "method_elem"
        ),
        Language::C | Language::Cpp => {
            matches!(node.kind(), "function_definition" | "lambda_expression")
                || (matches!(node.kind(), "field_declaration" | "declaration")
                    && declarator(node)
                        .and_then(|declaration| declaration.child_by_field_name("declarator"))
                        .is_some_and(|name| {
                            matches!(
                                name.kind(),
                                "identifier"
                                    | "field_identifier"
                                    | "qualified_identifier"
                                    | "operator_name"
                                    | "destructor_name"
                            )
                        }))
        }
        Language::Swift => matches!(
            node.kind(),
            "function_declaration"
                | "protocol_function_declaration"
                | "init_declaration"
                | "deinit_declaration"
                | "lambda_literal"
        ),
    }
}

fn declarator(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..16 {
        if matches!(
            node.kind(),
            "function_declarator" | "abstract_function_declarator" | "lambda_declarator"
        ) {
            return Some(node);
        }
        node = node.child_by_field_name("declarator")?;
    }
    None
}

pub(crate) fn body(node: Node<'_>, language: Language) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        if language == Language::Swift && node.kind() == "lambda_literal" {
            named(node)
                .into_iter()
                .find(|child| child.kind() == "statements")
        } else {
            None
        }
    })
}

pub(super) fn body_is_missing(node: Node<'_>, source: &str, language: Language) -> bool {
    if body(node, language).is_some() {
        return false;
    }
    match language {
        Language::C => true,
        Language::Cpp => {
            let implicit = named(node).iter().any(|child| {
                matches!(
                    child.kind(),
                    "pure_virtual_clause" | "default_method_clause" | "delete_method_clause"
                )
            }) || node
                .child_by_field_name("default_value")
                .is_some_and(|value| text(value, source).trim() == "0");
            !implicit
        }
        Language::Java => modifier(node, source, "native"),
        _ => false,
    }
}

pub(super) fn owner_class(mut node: Node<'_>, language: Language) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if callable(parent, language) {
            return None;
        }
        if class_like(parent, language) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

pub(super) fn method(node: Node<'_>, language: Language) -> bool {
    owner_class(node, language).is_some()
        && !matches!(
            node.kind(),
            "arrow_function"
                | "lambda"
                | "lambda_expression"
                | "lambda_literal"
                | "closure_expression"
                | "function_expression"
                | "func_literal"
        )
}

pub(super) fn node_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(text(name, source).into());
    }
    if let Some(function) = declarator(node) {
        if let Some(name) = function.child_by_field_name("declarator") {
            return Some(text(name, source).into());
        }
    }
    if let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "variable_declarator" | "public_field_definition" | "assignment" | "let_declaration"
        ) {
            return parent
                .child_by_field_name("name")
                .or_else(|| parent.child_by_field_name("left"))
                .or_else(|| parent.child_by_field_name("pattern"))
                .map(|name| text(name, source).into());
        }
    }
    None
}

pub(crate) fn qualified(node: Node<'_>, source: &str, language: Language) -> (String, bool) {
    let mut reliable = true;
    let mut parts = Vec::new();
    let mut next = Some(node);
    while let Some(item) = next {
        if callable(item, language)
            || type_boundary(item, language)
            || matches!(
                item.kind(),
                "namespace_definition" | "mod_item" | "impl_item"
            )
        {
            match node_name(item, source).or_else(|| {
                item.child_by_field_name("type")
                    .map(|kind| text(kind, source).into())
            }) {
                Some(name) if !name.is_empty() => parts.push(name),
                _ => {
                    parts.push(format!("<anonymous@{}>", item.start_byte()));
                    reliable = false;
                }
            }
        }
        if parts.iter().map(String::len).sum::<usize>() > 2048 {
            reliable = false;
            break;
        }
        next = item.parent();
    }
    parts.reverse();
    (parts.join("::"), reliable)
}

pub(super) fn parameters(node: Node<'_>, source: &str, language: Language) -> Measurement {
    let container = node
        .child_by_field_name("parameters")
        .or_else(|| declarator(node).and_then(|decl| decl.child_by_field_name("parameters")));
    let mut params = if let Some(container) = container {
        if matches!(container.kind(), "identifier" | "inferred_parameter") {
            vec![container]
        } else {
            named(container)
        }
    } else if let Some(parameter) = node.child_by_field_name("parameter") {
        vec![parameter]
    } else if language == Language::Swift && node.kind() != "lambda_literal" {
        named(node)
            .into_iter()
            .filter(|child| child.kind() == "parameter")
            .collect()
    } else if (language == Language::Cpp && node.kind() == "lambda_expression")
        || node.kind() == "compact_constructor_declaration"
        || node.kind() == "deinit_declaration"
    {
        vec![]
    } else {
        return Measurement::Unavailable {
            reason: "Parameter syntax is inferred or not supported by this convention".into(),
        };
    };
    params.retain(|parameter| {
        !matches!(
            parameter.kind(),
            "comment"
                | "line_comment"
                | "block_comment"
                | "attribute_item"
                | "self_parameter"
                | "positional_separator"
                | "keyword_separator"
                | "receiver_parameter"
        ) && !(language == Language::Rust
            && parameter
                .child_by_field_name("pattern")
                .is_some_and(|pattern| text(pattern, source) == "self"))
            && !(matches!(language, Language::TypeScript | Language::Tsx)
                && parameter
                    .child_by_field_name("pattern")
                    .is_some_and(|pattern| text(pattern, source) == "this"))
    });
    if language == Language::Python && method(node, language) {
        let is_static = node
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
            .is_some_and(|parent| {
                named(parent).iter().any(|child| {
                    child.kind() == "decorator"
                        && matches!(
                            text(*child, source).trim(),
                            "@staticmethod" | "@builtins.staticmethod"
                        )
                })
            });
        if !is_static && !params.is_empty() {
            params.remove(0);
        }
    }
    let count: usize = params
        .iter()
        .map(|parameter| {
            if language == Language::Go {
                let mut cursor = parameter.walk();
                parameter
                    .children_by_field_name("name", &mut cursor)
                    .count()
                    .max(1)
            } else if matches!(language, Language::C | Language::Cpp)
                && text(*parameter, source) == "void"
            {
                0
            } else {
                1
            }
        })
        .sum();
    Measurement::Measured {
        value: count as u32,
    }
}

fn is_else_if(node: Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while previous.is_some_and(super::tokens::comment) {
        previous = previous.and_then(|item| item.prev_named_sibling());
    }
    node.kind() == "elif_clause"
        || (matches!(node.kind(), "if_statement" | "if_expression")
            && node.parent().is_some_and(|parent| {
                parent.kind() == "else_clause"
                    || (matches!(parent.kind(), "if_statement" | "if_expression")
                        && previous.is_some_and(|item| item.kind() == "else"))
                    || parent
                        .child_by_field_name("alternative")
                        .is_some_and(|alternative| alternative.id() == node.id())
            }))
}

pub(crate) fn control_flow(root: Node<'_>, source: &str, language: Language) -> (u32, u32) {
    let mut complexity = 1;
    let mut maximum = 0;
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if node.id() != root.id() && (callable(node, language) || type_boundary(node, language)) {
            continue;
        }
        if super::tokens::comment(node) || literal(node) {
            continue;
        }
        let go_switch = matches!(
            node.kind(),
            "expression_switch_statement" | "type_switch_statement"
        );
        let control = go_switch
            || matches!(
                node.kind(),
                "if_statement"
                    | "if_expression"
                    | "elif_clause"
                    | "for_statement"
                    | "for_expression"
                    | "for_in_statement"
                    | "while_statement"
                    | "while_expression"
                    | "do_statement"
                    | "do_while_statement"
                    | "loop_expression"
                    | "catch_clause"
                    | "except_clause"
                    | "guard_statement"
                    | "switch_statement"
                    | "switch_expression"
                    | "match_expression"
                    | "match_statement"
                    | "select_statement"
                    | "conditional_expression"
                    | "ternary_expression"
            );
        let next_depth = depth + u32::from(control && !is_else_if(node));
        maximum = maximum.max(next_depth);
        if control
            && !go_switch
            && !matches!(
                node.kind(),
                "switch_statement"
                    | "switch_expression"
                    | "match_expression"
                    | "match_statement"
                    | "select_statement"
            )
        {
            complexity += 1;
        }
        if matches!(
            node.kind(),
            "conjunction_expression" | "disjunction_expression"
        ) {
            complexity += 1;
        }
        if matches!(node.kind(), "binary_expression" | "boolean_operator")
            && node
                .child_by_field_name("operator")
                .is_some_and(|operator| {
                    matches!(text(operator, source), "&&" | "||" | "and" | "or")
                })
        {
            complexity += 1;
        }
        if matches!(
            node.kind(),
            "match_arm"
                | "switch_case"
                | "switch_label"
                | "expression_case"
                | "type_case"
                | "communication_case"
                | "case_clause"
                | "case_statement"
                | "switch_entry"
        ) {
            let pattern = node
                .child_by_field_name("pattern")
                .or_else(|| node.child_by_field_name("value"))
                .or_else(|| {
                    named(node)
                        .into_iter()
                        .find(|child| child.kind().contains("pattern"))
                });
            let guarded = node.child_by_field_name("guard").is_some()
                || named(node).iter().any(|child| {
                    matches!(
                        child.kind(),
                        "if_clause" | "match_condition_clause" | "guard"
                    )
                });
            if !text(node, source).trim_start().starts_with("default")
                && !(pattern.is_some_and(|pattern| text(pattern, source).trim() == "_") && !guarded)
            {
                complexity += 1;
            }
        }
        stack.extend(
            children(node)
                .into_iter()
                .rev()
                .map(|child| (child, next_depth)),
        );
    }
    (complexity, maximum)
}

fn unquote(value: &str) -> Option<String> {
    let value = value.trim();
    let stripped = if (value.starts_with('\"') && value.ends_with('\"'))
        || (value.starts_with('\'') && value.ends_with('\''))
        || (value.starts_with('`') && value.ends_with('`'))
        || (value.starts_with('<') && value.ends_with('>'))
    {
        &value[1..value.len().checked_sub(1)?]
    } else {
        value
    };
    if stripped.is_empty() || stripped.contains(['\\', '$', '\n']) {
        None
    } else {
        Some(stripped.into())
    }
}

/// Distinct explicit dependency units, not a type-resolved coupling metric.
fn rust_roots(node: Node<'_>, source: &str, result: &mut BTreeSet<String>) {
    if node.kind() == "use_list" {
        for child in named(node) {
            rust_roots(child, source, result);
        }
    } else if node.kind() == "use_as_clause" {
        if let Some(path) = node.child_by_field_name("path") {
            rust_roots(path, source, result);
        }
    } else {
        let path = node.child_by_field_name("path").unwrap_or(node);
        let root = text(path, source)
            .trim_start_matches("::")
            .split("::")
            .next()
            .unwrap_or_default()
            .trim();
        if !root.is_empty() {
            result.insert(root.into());
        }
    }
}

fn rust_bindings(node: Node<'_>, source: &str, prefix: Option<&str>, result: &mut Vec<String>) {
    match node.kind() {
        "use_list" => {
            for child in named(node) {
                rust_bindings(child, source, prefix, result);
            }
        }
        "scoped_use_list" => {
            let parent = node
                .child_by_field_name("path")
                .map(|path| text(path, source));
            for list in named(node)
                .into_iter()
                .filter(|child| child.kind() == "use_list")
            {
                rust_bindings(list, source, parent, result);
            }
        }
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                result.push(text(alias, source).into());
            }
        }
        "self" => result.push(
            prefix
                .unwrap_or("self")
                .rsplit("::")
                .next()
                .unwrap_or("self")
                .into(),
        ),
        _ => result.push(
            text(node, source)
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .trim()
                .into(),
        ),
    }
}

pub(super) fn dependencies(nodes: &[Node<'_>], source: &str, language: Language) -> Measurement {
    let mut units = BTreeSet::new();
    let mut incomplete = false;
    for &node in nodes {
        let path = match (language, node.kind()) {
            (Language::TypeScript | Language::Tsx, "import_statement" | "export_statement") => node
                .child_by_field_name("source")
                .map(|path| text(path, source).to_owned()),
            (Language::Python, "import_from_statement") => node
                .child_by_field_name("module_name")
                .map(|path| text(path, source).into()),
            (Language::Python, "import_statement") => {
                for child in named(node) {
                    let child = child.child_by_field_name("name").unwrap_or(child);
                    units.insert(text(child, source).to_owned());
                }
                None
            }
            (Language::Rust, "use_declaration") => {
                if let Some(path) = node.child_by_field_name("argument") {
                    rust_roots(path, source, &mut units);
                }
                None
            }
            (Language::Rust, "extern_crate_declaration") => node
                .child_by_field_name("name")
                .map(|path| text(path, source).into()),
            (Language::Go, "import_spec") => node
                .child_by_field_name("path")
                .map(|path| text(path, source).into()),
            (Language::Java, "import_declaration") => {
                let raw = text(node, source)
                    .trim_start_matches("import")
                    .trim()
                    .trim_end_matches(';')
                    .trim();
                Some(
                    raw.trim_start_matches("static ")
                        .trim_end_matches(".*")
                        .into(),
                )
            }
            (Language::C | Language::Cpp, "preproc_include") => {
                let path = node.child_by_field_name("path");
                if path.is_none_or(|path| {
                    !matches!(path.kind(), "string_literal" | "system_lib_string")
                }) {
                    incomplete = true;
                    None
                } else {
                    path.map(|path| text(path, source).into())
                }
            }
            (Language::Swift, "import_declaration") => named(node)
                .into_iter()
                .find(|child| child.kind() == "identifier")
                .map(|path| {
                    text(path, source)
                        .split('.')
                        .next()
                        .unwrap_or_default()
                        .into()
                }),
            _ => None,
        };
        if let Some(path) = path {
            if let Some(path) = unquote(&path) {
                units.insert(path);
            } else {
                incomplete = true;
            }
        }
        if matches!(language, Language::TypeScript | Language::Tsx)
            && node.kind() == "import_require_clause"
        {
            if let Some(path) = named(node)
                .into_iter()
                .find(|child| child.kind() == "string")
                .and_then(|path| unquote(text(path, source)))
            {
                units.insert(path);
            } else {
                incomplete = true;
            }
        }
    }
    if incomplete {
        Measurement::Unavailable {
            reason: "One or more explicit dependency paths cannot be statically identified".into(),
        }
    } else {
        Measurement::Measured {
            value: units.len() as u32,
        }
    }
}

fn modifier(node: Node<'_>, source: &str, expected: &str) -> bool {
    let mut stack: Vec<_> = named(node)
        .into_iter()
        .filter(|child| {
            matches!(
                child.kind(),
                "modifiers"
                    | "visibility_modifier"
                    | "accessibility_modifier"
                    | "storage_class_specifier"
            )
        })
        .collect();
    while let Some(item) = stack.pop() {
        if item.kind() == "visibility_modifier" && expected == "pub" {
            if text(item, source).trim() == "pub" {
                return true;
            }
            continue;
        }
        if matches!(
            item.kind(),
            "annotation" | "marker_annotation" | "attribute" | "decorator"
        ) || literal(item)
        {
            continue;
        }
        if text(item, source).trim() == expected {
            return true;
        }
        stack.extend(children(item));
    }
    false
}

fn in_function(mut node: Node<'_>, language: Language) -> bool {
    while let Some(parent) = node.parent() {
        if callable(parent, language) {
            return true;
        }
        node = parent;
    }
    false
}

fn cpp_access(node: Node<'_>, source: &str) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "field_declaration_list" {
            let mut public = parent
                .parent()
                .is_some_and(|owner| owner.kind() == "struct_specifier");
            for child in named(parent) {
                if child.id() == current.id() {
                    break;
                }
                if child.kind() == "access_specifier" {
                    public = matches!(text(child, source), "public" | "protected");
                }
            }
            if !public {
                return false;
            }
        }
        if parent.kind() == "namespace_definition" && parent.child_by_field_name("name").is_none() {
            return false;
        }
        current = parent;
    }
    true
}

fn exported_ts(mut node: Node<'_>, source: &str) -> bool {
    loop {
        if modifier(node, source, "private")
            || node
                .child_by_field_name("name")
                .is_some_and(|name| text(name, source).starts_with('#'))
        {
            return false;
        }
        if node.kind() == "export_statement" {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn names(node: Node<'_>, source: &str) -> Vec<String> {
    if matches!(node.kind(), "import_statement" | "import_from_statement") {
        let mut cursor = node.walk();
        return node
            .children_by_field_name("name", &mut cursor)
            .map(|name| {
                name.child_by_field_name("alias")
                    .map(|alias| text(alias, source).to_owned())
                    .unwrap_or_else(|| {
                        text(name, source)
                            .split('.')
                            .next()
                            .unwrap_or_default()
                            .into()
                    })
            })
            .collect();
    }
    let mut cursor = node.walk();
    let mut bindings: Vec<_> = node.children_by_field_name("name", &mut cursor).collect();
    if bindings.is_empty() {
        bindings.extend(
            node.child_by_field_name("left")
                .or_else(|| node.child_by_field_name("pattern")),
        );
    }
    if !bindings.is_empty() {
        let mut result = Vec::new();
        while let Some(binding) = bindings.pop() {
            match binding.kind() {
                "tuple_pattern" | "list_pattern" | "pattern_list" | "tuple" | "list"
                | "array_pattern" | "object_pattern" => bindings.extend(named(binding)),
                "pair_pattern" => bindings.extend(binding.child_by_field_name("value")),
                "assignment_pattern" | "object_assignment_pattern" => {
                    bindings.extend(binding.child_by_field_name("left"))
                }
                "rest_pattern" => bindings.extend(named(binding)),
                _ => result.push(text(binding, source).into()),
            }
        }
        return result;
    }
    if let Some(function) =
        declarator(node).and_then(|function| function.child_by_field_name("declarator"))
    {
        return vec![text(function, source).into()];
    }
    if matches!(node.kind(), "declaration" | "field_declaration") {
        let mut cursor = node.walk();
        return node
            .children_by_field_name("declarator", &mut cursor)
            .map(|declaration| {
                let declaration = declaration
                    .child_by_field_name("declarator")
                    .unwrap_or(declaration);
                text(declaration, source).to_owned()
            })
            .collect();
    }
    Vec::new()
}

/// Count source-declared visibility, not a compiled ABI or dynamic export set.
pub(super) fn public_surface(nodes: &[Node<'_>], source: &str, language: Language) -> Measurement {
    let unavailable = |reason: &str| Measurement::Unavailable {
        reason: reason.into(),
    };
    let mut explicit_python = None;
    if language == Language::Python {
        for &node in nodes {
            if in_function(node, language) || owner_class(node, language).is_some() {
                continue;
            }
            if node.kind() == "call"
                && node
                    .child_by_field_name("function")
                    .is_some_and(|function| text(function, source).starts_with("__all__."))
            {
                return unavailable("Python __all__ is used through a potentially mutating call");
            }
            if matches!(node.kind(), "assignment" | "augmented_assignment")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| text(left, source).starts_with("__all__["))
            {
                return unavailable("Python __all__ is mutated through indexing");
            }
            if matches!(node.kind(), "assignment" | "augmented_assignment")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| text(left, source) == "__all__")
            {
                if explicit_python.is_some() || node.kind() == "augmented_assignment" {
                    return unavailable("Dynamic or multiply assigned Python __all__");
                }
                let Some(value) = node
                    .child_by_field_name("right")
                    .filter(|value| matches!(value.kind(), "list" | "tuple"))
                else {
                    return unavailable("Python __all__ is not a static literal sequence");
                };
                let mut exports = BTreeSet::new();
                for item in named(value) {
                    if item.kind() != "string" {
                        return unavailable("Python __all__ contains nonliteral names");
                    }
                    let raw = text(item, source);
                    if !raw.starts_with(['\'', '"'])
                        || raw.starts_with("\"\"\"")
                        || raw.starts_with("'''")
                    {
                        return unavailable("Prefixed/triple-quoted export names are not decoded by this convention");
                    }
                    let Some(name) = unquote(raw) else {
                        return unavailable("Python __all__ contains encoded or dynamic names");
                    };
                    exports.insert(name);
                }
                explicit_python = Some(exports);
            }
        }
    }
    let mut result = BTreeSet::new();
    for &node in nodes {
        if matches!(language, Language::TypeScript | Language::Tsx)
            && matches!(node.kind(), "required_parameter" | "optional_parameter")
            && (named(node)
                .iter()
                .any(|child| child.kind() == "accessibility_modifier")
                || children(node)
                    .iter()
                    .any(|child| child.kind() == "readonly"))
            && !modifier(node, source, "private")
        {
            let mut parent = node.parent();
            while let Some(owner) = parent {
                if callable(owner, language) {
                    if owner.kind() == "method_definition"
                        && node_name(owner, source).as_deref() == Some("constructor")
                    {
                        if let Some(class) =
                            owner_class(owner, language).filter(|class| exported_ts(*class, source))
                        {
                            for name in names(node, source) {
                                result.insert(format!(
                                    "{}::{name}",
                                    qualified(class, source, language).0
                                ));
                            }
                        }
                    }
                    break;
                }
                parent = owner.parent();
            }
        }
        if in_function(node, language) {
            continue;
        }
        if matches!(language, Language::TypeScript | Language::Tsx) {
            if matches!(node.kind(), "internal_module" | "module") {
                return unavailable(
                    "Namespace/module export expansion is not resolved by this convention",
                );
            }
            if node.kind() == "export_statement"
                && node.child_by_field_name("declaration").is_none()
            {
                if children(node).iter().any(|child| child.kind() == "*") {
                    return unavailable("Wildcard re-exports require an external export index");
                }
                if children(node).iter().any(|child| child.kind() == "default") {
                    result.insert("default".to_owned());
                }
            }
            if node.kind() == "export_specifier" {
                if let Some(name) = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"))
                {
                    result.insert(text(name, source).into());
                }
            }
            if matches!(node.kind(), "assignment_expression" | "call_expression")
                && (text(node, source).starts_with("module.exports")
                    || text(node, source).starts_with("exports[")
                    || text(node, source).starts_with("exports."))
            {
                return unavailable("CommonJS dynamic export assignments are not resolved");
            }
        }
        if matches!(language, Language::C | Language::Cpp)
            && matches!(
                node.kind(),
                "preproc_if" | "preproc_ifdef" | "preproc_function_def"
            )
        {
            return unavailable("Preprocessing can change the public declarations");
        }
        if language == Language::Rust
            && node.kind() == "use_declaration"
            && modifier(node, source, "pub")
            && text(node, source).contains('*')
        {
            return unavailable("Wildcard Rust public re-exports require a resolved export index");
        }
        if language == Language::Rust
            && node.kind() == "use_declaration"
            && modifier(node, source, "pub")
        {
            if let Some(argument) = node.child_by_field_name("argument") {
                let mut exported = Vec::new();
                rust_bindings(argument, source, None, &mut exported);
                result.extend(exported);
            }
        }
        if language == Language::Rust
            && node.kind() == "macro_invocation"
            && node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "source_file" | "declaration_list")
                    || (parent.kind() == "expression_statement"
                        && parent.parent().is_some_and(|owner| {
                            matches!(owner.kind(), "source_file" | "declaration_list")
                        }))
            })
        {
            return unavailable("Module-level macro expansion can alter public declarations");
        }
        if language == Language::Python
            && node.kind() == "wildcard_import"
            && explicit_python.is_none()
        {
            return unavailable("Python wildcard imports require a resolved export index");
        }

        let declaration = match language {
            Language::Rust => matches!(
                node.kind(),
                "function_item"
                    | "function_signature_item"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "const_item"
                    | "static_item"
                    | "type_item"
                    | "mod_item"
                    | "field_declaration"
                    | "enum_variant"
            ),
            Language::TypeScript | Language::Tsx => matches!(
                node.kind(),
                "function_declaration"
                    | "generator_function_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "variable_declarator"
                    | "method_definition"
                    | "abstract_method_signature"
                    | "public_field_definition"
                    | "method_signature"
                    | "property_signature"
            ),
            Language::Python => matches!(
                node.kind(),
                "function_definition"
                    | "class_definition"
                    | "assignment"
                    | "import_statement"
                    | "import_from_statement"
            ),
            Language::Java => matches!(
                node.kind(),
                "class_declaration"
                    | "record_declaration"
                    | "enum_declaration"
                    | "interface_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "field_declaration"
            ),
            Language::Go => matches!(
                node.kind(),
                "function_declaration"
                    | "method_declaration"
                    | "type_spec"
                    | "var_spec"
                    | "const_spec"
                    | "field_declaration"
                    | "method_elem"
            ),
            Language::C | Language::Cpp => matches!(
                node.kind(),
                "function_definition"
                    | "declaration"
                    | "field_declaration"
                    | "class_specifier"
                    | "struct_specifier"
                    | "enum_specifier"
            ),
            Language::Swift => matches!(
                node.kind(),
                "function_declaration"
                    | "class_declaration"
                    | "protocol_declaration"
                    | "init_declaration"
                    | "property_declaration"
                    | "typealias_declaration"
            ),
        };
        if !declaration {
            continue;
        }
        let mut declared_names = names(node, source);
        if language == Language::Python {
            if explicit_python.is_some() && owner_class(node, language).is_none() {
                continue;
            }
            declared_names.retain(|name| {
                !name.starts_with('_')
                    || (owner_class(node, language).is_some()
                        && name.starts_with("__")
                        && name.ends_with("__"))
            });
        }
        if language == Language::Go {
            declared_names.retain(|name| name.chars().next().is_some_and(char::is_uppercase));
        }
        let visible = match language {
            Language::Rust => {
                let mut parent = node.parent();
                let mut implicit = false;
                while let Some(owner) = parent {
                    if matches!(owner.kind(), "trait_item" | "enum_item") {
                        implicit = modifier(owner, source, "pub")
                            && matches!(
                                node.kind(),
                                "function_signature_item" | "function_item" | "enum_variant"
                            );
                        break;
                    }
                    parent = owner.parent();
                }
                modifier(node, source, "pub") || implicit
            }
            Language::TypeScript | Language::Tsx => exported_ts(node, source),
            Language::Python => declared_names.iter().any(|name| {
                if owner_class(node, language).is_none() {
                    explicit_python
                        .as_ref()
                        .map(|exports| exports.contains(name))
                        .unwrap_or(!name.starts_with('_'))
                } else {
                    !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"))
                }
            }),
            Language::Java => {
                modifier(node, source, "public")
                    || modifier(node, source, "protected")
                    || node
                        .parent()
                        .is_some_and(|parent| parent.kind() == "interface_body")
            }
            Language::Go => declared_names
                .iter()
                .any(|name| name.chars().next().is_some_and(char::is_uppercase)),
            Language::C | Language::Cpp => {
                (!modifier(node, source, "static") || owner_class(node, language).is_some())
                    && cpp_access(node, source)
            }
            Language::Swift => modifier(node, source, "public") || modifier(node, source, "open"),
        };
        if !visible {
            continue;
        }
        if declared_names.is_empty() {
            return unavailable("A visible declaration has unsupported binding syntax");
        }
        // Visibility of enclosing classes/types matters as well as member flags.
        let mut ancestor = node.parent();
        let mut hidden = false;
        while let Some(parent) = ancestor {
            if language == Language::Rust && parent.kind() == "impl_item" {
                if let Some(target) = parent.child_by_field_name("type") {
                    let target = text(target, source).split('<').next().unwrap_or_default();
                    let types: Vec<_> = nodes
                        .iter()
                        .filter(|candidate| {
                            matches!(candidate.kind(), "struct_item" | "enum_item" | "type_item")
                                && node_name(**candidate, source).as_deref() == Some(target)
                        })
                        .collect();
                    if types.len() != 1 {
                        return unavailable("Public inherent impl target is not uniquely resolved in this source file");
                    }
                    hidden |= !modifier(*types[0], source, "pub");
                }
            }
            if language == Language::Rust && parent.kind() == "mod_item" {
                hidden |= !modifier(parent, source, "pub");
            }
            if type_boundary(parent, language) {
                hidden |= match language {
                    Language::Rust => !modifier(parent, source, "pub"),
                    Language::Java => {
                        !modifier(parent, source, "public")
                            && !modifier(parent, source, "protected")
                    }
                    Language::Python => node_name(parent, source).is_some_and(|name| {
                        if owner_class(parent, language).is_none() {
                            explicit_python
                                .as_ref()
                                .map(|exports| !exports.contains(&name))
                                .unwrap_or(name.starts_with('_'))
                        } else {
                            name.starts_with('_')
                        }
                    }),
                    Language::Swift => {
                        !modifier(parent, source, "public") && !modifier(parent, source, "open")
                    }
                    _ => false,
                };
            }
            ancestor = parent.parent();
        }
        if hidden {
            continue;
        }
        let prefix = qualified(node, source, language).0;
        for name in declared_names {
            result.insert(
                if callable(node, language) || type_boundary(node, language) {
                    prefix.clone()
                } else if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}::{name}")
                },
            );
        }
    }
    if let Some(exports) = explicit_python {
        result.extend(exports);
    }
    Measurement::Measured {
        value: result.len() as u32,
    }
}
