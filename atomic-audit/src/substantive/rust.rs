use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn inspect(
    node: Node<'_>,
    functions: &BTreeMap<(usize, String), Vec<Node<'_>>>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    match node.kind() {
        "function_item" => {
            forwarding_wrapper(node, functions, context, issues)?;
            if is_test(node, context) && !has_attribute(node, context, "should_panic") {
                if let Some(body) = node.child_by_field_name("body") {
                    if code_children(body).is_empty() {
                        issues.push(context.issue(Rule::WeakVerification, node, "This #[test] function has an empty executable body and is not a should_panic test. It succeeds without exercising behavior or checking a result."));
                    }
                }
            }
        }
        "macro_invocation" => vacuous_assertion(node, context, issues)?,
        "block" => guard_across_await(node, context, issues)?,
        "unsafe_block" => {
            context.charge(node)?;
            if !justified(node, context, true) {
                let body = tokens::named(node)
                    .into_iter()
                    .find(|child| child.kind() == "block");
                if let Some(body) = body {
                    let mut stack = vec![body];
                    let mut operation = None;
                    while let Some(child) = stack.pop() {
                        if child.kind() == "call_expression" {
                            if let Some(function) = child.child_by_field_name("function") {
                                let target = context.compact(function)?;
                                if [
                                    "std::mem::transmute",
                                    "core::mem::transmute",
                                    "std::ptr::read",
                                    "core::ptr::read",
                                    "std::slice::from_raw_parts",
                                    "core::slice::from_raw_parts",
                                ]
                                .iter()
                                .any(|prefix| {
                                    target == *prefix || target.starts_with(&format!("{prefix}::<"))
                                }) {
                                    operation = Some(target);
                                    break;
                                }
                            }
                        }
                        stack.extend(code_children(child));
                    }
                    if let Some(operation) = operation {
                        issues.push(context.issue(Rule::UnsafeShortcuts, node, format!("Unsafe operation `{operation}` has no immediately associated SAFETY explanation of the required memory/layout/lifetime preconditions. This is a missing safety argument at the observed unchecked operation, not proof of undefined behavior.")));
                    }
                }
            }
        }
        "attribute_item" | "inner_attribute_item" => {
            let attribute = context.compact(node)?;
            if matches!(
                attribute.as_str(),
                "#[allow(warnings)]"
                    | "#![allow(warnings)]"
                    | "#[allow(clippy::all)]"
                    | "#![allow(clippy::all)]"
            ) && !justified(node, context, false)
            {
                issues.push(context.issue(Rule::UnsafeShortcuts, node, "A blanket warnings/clippy::all suppression disables unrelated current and future diagnostics without a local reason. Narrow the disabled diagnostic or record why the broad bypass is needed."));
            }
        }
        _ => {}
    }
    Ok(())
}

fn attributes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut previous = node.prev_named_sibling();
    while let Some(node) = previous {
        if !matches!(
            node.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        ) {
            break;
        }
        if node.kind() == "attribute_item" {
            result.push(node);
        }
        previous = node.prev_named_sibling();
    }
    result
}

fn has_attribute(node: Node<'_>, context: &Context<'_>, expected: &str) -> bool {
    attributes(node).into_iter().any(|attribute| {
        tokens::named(attribute)
            .into_iter()
            .filter(|node| node.kind() == "attribute")
            .any(|attribute| {
                code_children(attribute).first().is_some_and(|path| {
                    tokens::text(*path, context.source).rsplit("::").next() == Some(expected)
                })
            })
    })
}

fn is_test(node: Node<'_>, context: &Context<'_>) -> bool {
    has_attribute(node, context, "test")
}

fn justified(mut node: Node<'_>, context: &Context<'_>, safety: bool) -> bool {
    let explains = |node| {
        let text = tokens::text(node, context.source);
        let marker = if safety { "SAFETY:" } else { "Reason:" };
        tokens::comment(node)
            && text
                .split_once(marker)
                .is_some_and(|(_, reason)| reason.trim().len() >= 8)
    };
    // Support a comment inside the unsafe block or immediately before its
    // enclosing statement. Strings containing SAFETY are never comments.
    if code_comment_inside(node, &explains) {
        return true;
    }
    for _ in 0..4 {
        if node.prev_named_sibling().is_some_and(explains) {
            return true;
        }
        let Some(parent) = node.parent() else {
            break;
        };
        if matches!(parent.kind(), "function_item" | "source_file" | "block") {
            break;
        }
        node = parent;
    }
    false
}

fn code_comment_inside<'tree>(node: Node<'tree>, explains: &impl Fn(Node<'tree>) -> bool) -> bool {
    // Only direct comments in the unsafe body count, not a nested operation's
    // unrelated safety note.
    tokens::named(node).into_iter().any(|child| {
        explains(child)
            || (child.kind() == "block" && tokens::named(child).into_iter().any(explains))
    })
}

fn forwarding_wrapper(
    node: Node<'_>,
    functions: &BTreeMap<(usize, String), Vec<Node<'_>>>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    let Some(parent) = node
        .parent()
        .filter(|parent| matches!(parent.kind(), "source_file" | "declaration_list"))
    else {
        return Ok(());
    };
    if parent.kind() == "declaration_list"
        && !parent
            .parent()
            .is_some_and(|owner| owner.kind() == "mod_item")
    {
        return Ok(());
    }
    if !attributes(node).is_empty()
        || node.prev_named_sibling().is_some_and(tokens::comment)
        || tokens::named(node).iter().any(|child| {
            matches!(
                child.kind(),
                "visibility_modifier" | "function_modifiers" | "type_parameters" | "where_clause"
            )
        })
    {
        return Ok(());
    }
    let (Some(name), Some(parameters), Some(body)) = (
        node.child_by_field_name("name"),
        node.child_by_field_name("parameters"),
        node.child_by_field_name("body"),
    ) else {
        return Ok(());
    };
    if code_children(body).len() != 1 {
        return Ok(());
    }
    let call = tail(body);
    if call.kind() != "call_expression" {
        return Ok(());
    }
    let (Some(target), Some(arguments)) = (
        call.child_by_field_name("function"),
        call.child_by_field_name("arguments"),
    ) else {
        return Ok(());
    };
    if target.kind() != "identifier"
        || tokens::text(target, context.source) == tokens::text(name, context.source)
    {
        return Ok(());
    }
    let params = code_children(parameters);
    let args = code_children(arguments);
    if params.len() != args.len() {
        return Ok(());
    }
    for (parameter, argument) in params.iter().zip(&args) {
        let Some(pattern) = parameter.child_by_field_name("pattern") else {
            return Ok(());
        };
        if pattern.kind() != "identifier"
            || argument.kind() != "identifier"
            || tokens::text(pattern, context.source) != tokens::text(*argument, context.source)
            || tokens::text(pattern, context.source) == tokens::text(target, context.source)
        {
            return Ok(());
        }
    }
    let Some(targets) = functions.get(&(parent.id(), tokens::text(target, context.source).into()))
    else {
        return Ok(());
    };
    let [target_function] = targets.as_slice() else {
        return Ok(());
    };
    if !attributes(*target_function).is_empty()
        || tokens::named(*target_function).iter().any(|child| {
            matches!(
                child.kind(),
                "visibility_modifier" | "function_modifiers" | "type_parameters" | "where_clause"
            )
        })
    {
        return Ok(());
    }
    let Some(target_parameters) = target_function.child_by_field_name("parameters") else {
        return Ok(());
    };
    let target_params = code_children(target_parameters);
    if target_params.len() != params.len() {
        return Ok(());
    }
    for (left, right) in params.iter().zip(&target_params) {
        let (Some(left), Some(right)) = (
            left.child_by_field_name("type"),
            right.child_by_field_name("type"),
        ) else {
            return Ok(());
        };
        if context.normalized(left)? != context.normalized(right)? {
            return Ok(());
        }
    }
    let returns = |node: Node<'_>| {
        node.child_by_field_name("return_type")
            .map(|node| tokens::text(node, context.source).to_string())
    };
    if returns(node) != returns(*target_function) {
        return Ok(());
    }
    // A single naming wrapper may be a useful boundary. Require a chain of
    // private unchanged forwarders rather than flagging every ordinary caller.
    let Some(target_body) = target_function.child_by_field_name("body") else {
        return Ok(());
    };
    if code_children(target_body).len() != 1 {
        return Ok(());
    }
    let next = tail(target_body);
    if next.kind() != "call_expression" {
        return Ok(());
    }
    let (Some(next_function), Some(next_arguments)) = (
        next.child_by_field_name("function"),
        next.child_by_field_name("arguments"),
    ) else {
        return Ok(());
    };
    if next_function.kind() != "identifier"
        || tokens::text(next_function, context.source) == tokens::text(name, context.source)
        || tokens::text(next_function, context.source) == tokens::text(target, context.source)
    {
        return Ok(());
    }
    if !functions
        .get(&(
            parent.id(),
            tokens::text(next_function, context.source).into(),
        ))
        .is_some_and(|targets| targets.len() == 1)
    {
        return Ok(());
    }
    let next_args = code_children(next_arguments);
    if next_args.len() != target_params.len() {
        return Ok(());
    }
    for (parameter, argument) in target_params.iter().zip(next_args) {
        let Some(pattern) = parameter.child_by_field_name("pattern") else {
            return Ok(());
        };
        if pattern.kind() != "identifier"
            || argument.kind() != "identifier"
            || tokens::text(pattern, context.source) != tokens::text(argument, context.source)
            || tokens::text(pattern, context.source) == tokens::text(next_function, context.source)
        {
            return Ok(());
        }
    }
    issues.push(context.issue(Rule::UnnecessaryMachinery, node, format!("Private standalone `{}` only calls the same-module private `{}` (declared at line {}), which itself only forwards unchanged arguments to `{}`. The first two contracts have identical parameter and return types and no attributes, generics, receiver or visibility boundary; check the purpose of this extra private indirection chain.", tokens::text(name, context.source), tokens::text(target, context.source), target_function.start_position().row + 1, tokens::text(next_function,context.source))));
    Ok(())
}

fn vacuous_assertion(
    node: Node<'_>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    let mut ancestor = node.parent();
    let mut test = crate::risk::test_support(context.path, "");
    while let Some(parent) = ancestor {
        if parent.kind() == "function_item" {
            test |= is_test(parent, context);
            break;
        }
        ancestor = parent.parent();
    }
    if !test {
        return Ok(());
    }
    let Some(name) = node.child_by_field_name("macro") else {
        return Ok(());
    };
    let name = tokens::text(name, context.source)
        .rsplit("::")
        .next()
        .unwrap_or("");
    let Some(tree) = tokens::named(node)
        .into_iter()
        .find(|node| node.kind() == "token_tree")
    else {
        return Ok(());
    };
    let tokens = context.normalized(tree)?;
    let values: Vec<_> = tokens.iter().map(|token| token.text.as_str()).collect();
    let assertion = values.get(1..values.len().saturating_sub(1)).unwrap_or(&[]);
    let comma = assertion
        .iter()
        .position(|token| *token == ",")
        .unwrap_or(assertion.len());
    let vacuous = if name == "assert" {
        assertion[..comma] == ["true"]
    } else if name == "assert_eq" && comma < assertion.len() {
        let left = &assertion[..comma];
        let rest = &assertion[comma + 1..];
        let end = rest
            .iter()
            .position(|token| *token == ",")
            .unwrap_or(rest.len());
        let simple = |tokens: &[&str]| {
            tokens.iter().all(|token| {
                *token == "."
                    || *token == "::"
                    || token.chars().all(|c| c.is_alphanumeric() || c == '_')
            })
        };
        !left.is_empty() && left == &rest[..end] && simple(left)
    } else {
        false
    };
    if vacuous {
        let detail = if name == "assert" {
            "This test asserts the literal true, so this assertion passes without checking behavior. Other assertions in the test may still be useful."
        } else {
            "This test compares the same simple expression with itself instead of an independent expected result. Verify that this is deliberately testing reflexivity (for example a non-NaN property), rather than a placeholder or copied expected expression; type-specific equality is not resolved here."
        };
        issues.push(context.issue(Rule::WeakVerification, node, detail));
    }
    Ok(())
}

fn guard_across_await(
    block: Node<'_>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    context.charge(block)?;
    let mut synchronous = BTreeSet::<String>::new();
    let mut ancestor = block.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "function_item" {
            if let Some(parameters) = parent.child_by_field_name("parameters") {
                for parameter in code_children(parameters) {
                    if let (Some(pattern), Some(ty)) = (
                        parameter.child_by_field_name("pattern"),
                        parameter.child_by_field_name("type"),
                    ) {
                        let ty = context.compact(ty)?;
                        if pattern.kind() == "identifier"
                            && [
                                "std::sync::Mutex<",
                                "std::sync::RwLock<",
                                "parking_lot::Mutex<",
                                "parking_lot::RwLock<",
                            ]
                            .iter()
                            .any(|prefix| ty.trim_start_matches('&').starts_with(prefix))
                        {
                            synchronous.insert(tokens::text(pattern, context.source).into());
                        }
                    }
                }
            }
            break;
        }
        ancestor = parent.parent();
    }
    let mut guards = BTreeMap::<String, usize>::new();
    let mut shadowed = Vec::<(String, usize)>::new();
    for statement in code_children(block) {
        if statement.kind() == "let_declaration" {
            if let Some(pattern) = statement
                .child_by_field_name("pattern")
                .filter(|pattern| pattern.kind() == "identifier")
            {
                let name = tokens::text(pattern, context.source).to_string();
                // Shadowing does not run the old binding's destructor.
                if let Some(line) = guards.remove(&name) {
                    shadowed.push((name.clone(), line));
                }
                synchronous.remove(&name);
                if let Some(initializer) = statement.child_by_field_name("value") {
                    let expression = context.compact(initializer)?;
                    if [
                        "std::sync::Mutex::new(",
                        "std::sync::RwLock::new(",
                        "parking_lot::Mutex::new(",
                        "parking_lot::RwLock::new(",
                    ]
                    .iter()
                    .any(|prefix| expression.starts_with(prefix))
                    {
                        synchronous.insert(name.clone());
                    }
                    if [
                        ".lock().unwrap()",
                        ".read().unwrap()",
                        ".write().unwrap()",
                        ".lock()",
                        ".read()",
                        ".write()",
                    ]
                    .iter()
                    .any(|suffix| {
                        expression
                            .strip_suffix(suffix)
                            .is_some_and(|receiver| synchronous.contains(receiver))
                    }) {
                        guards.insert(name, statement.start_position().row + 1);
                    }
                }
            }
        }
        let compact = context.compact(statement)?;
        guards.retain(|name, _| {
            compact != format!("drop({name});")
                && compact != format!("std::mem::drop({name});")
                && compact != format!("core::mem::drop({name});")
        });
        // A by-value use may transfer or release a guard. Do not claim a
        // binding is held when its ownership path is unresolved.
        let moved = possible_moves(statement, &guards, context.source);
        guards.retain(|name, _| !moved.contains(name));
        if !guards.is_empty() || !shadowed.is_empty() {
            let mut stack = vec![statement];
            while let Some(child) = stack.pop() {
                if child.kind() == "await_expression" {
                    let (guard, line) = guards
                        .first_key_value()
                        .or_else(|| shadowed.first().map(|(name, line)| (name, line)))
                        .unwrap();
                    issues.push(context.issue(Rule::AsyncResourceSafety, block, format!("Synchronous lock guard `{guard}`, acquired at line {line}, is still lexically live at await line {} in the same block. Holding a std/parking_lot guard across suspension can block executor progress; an explicit drop or a completed inner scope was not observed. Tokio async mutexes are excluded.", child.start_position().row + 1)));
                    return Ok(());
                }
                if child.id() != statement.id()
                    && (matches!(child.kind(), "block" | "closure_expression" | "async_block")
                        || languages::callable(child, Language::Rust))
                {
                    continue;
                }
                stack.extend(code_children(child));
            }
        }
        let expression = if statement.kind() == "expression_statement" {
            code_children(statement)
                .first()
                .copied()
                .unwrap_or(statement)
        } else {
            statement
        };
        if matches!(
            expression.kind(),
            "return_expression" | "break_expression" | "continue_expression"
        ) {
            break;
        }
    }
    Ok(())
}

fn possible_moves(
    statement: Node<'_>,
    guards: &BTreeMap<String, usize>,
    source: &str,
) -> BTreeSet<String> {
    let mut moved = BTreeSet::new();
    let mut stack = vec![statement];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" && guards.contains_key(tokens::text(node, source)) {
            if let Some(parent) = node.parent() {
                let binding = parent.kind() == "let_declaration"
                    && parent
                        .child_by_field_name("pattern")
                        .is_some_and(|pattern| pattern.id() == node.id());
                let borrowed = parent.kind() == "reference_expression"
                    || (parent.kind() == "unary_expression"
                        && tokens::text(parent, source).trim_start().starts_with('*'));
                if !binding && !borrowed {
                    moved.insert(tokens::text(node, source).into());
                }
            }
        }
        stack.extend(code_children(node));
    }
    moved
}
