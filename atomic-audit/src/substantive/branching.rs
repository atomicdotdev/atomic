use super::*;

pub(super) fn inspect(
    node: Node<'_>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    let rust = context.language == Language::Rust;
    if (rust && node.kind() == "if_expression") || (!rust && node.kind() == "if_statement") {
        if let (Some(condition), Some(left), Some(right)) = (
            node.child_by_field_name("condition"),
            node.child_by_field_name("consequence"),
            node.child_by_field_name("alternative"),
        ) {
            let right = if right.kind() == "else_clause" {
                code_children(right).first().copied()
            } else {
                Some(right)
            };
            if let Some(right) = right {
                // `if let` can bind a name differently from the else scope.
                // Identical spelling would not establish identical references.
                let binds_names = rust
                    && context
                        .normalized(condition)?
                        .iter()
                        .any(|token| token.kind == "let");
                if !binds_names
                    && matches!(right.kind(), "block" | "statement_block")
                    && !code_children(left).is_empty()
                    && context.normalized(left)? == context.normalized(right)?
                {
                    issues.push(context.issue(Rule::DuplicatedBehavior, node, format!("The nonempty alternatives at lines {} and {} have identical syntax tokens (comments excluded, literal contents preserved). The conditional does not select different branch code; check for a copied or mistaken body. Condition effects still matter.", left.start_position().row + 1, right.start_position().row + 1)));
                }
            }
        }
        if rust {
            if let (Some(condition), Some(left)) = (
                node.child_by_field_name("condition"),
                node.child_by_field_name("consequence"),
            ) {
                if condition.kind() == "let_condition" {
                    if let Some(pattern) = condition.child_by_field_name("pattern") {
                        if error_pattern(pattern, context)? && success(tail(left), context)? {
                            issues.push(context.issue(Rule::FailureMasking, node, "An Err-pattern branch ends by constructing Ok(...). The observed error is converted into a successful result; verify that callers explicitly expect degraded success rather than failure propagation."));
                        }
                    }
                }
            }
        }
    }
    if rust && node.kind() == "match_arm" {
        if let (Some(pattern), Some(value)) = (
            node.child_by_field_name("pattern"),
            node.child_by_field_name("value"),
        ) {
            if error_pattern(pattern, context)?
                && success(tail(value), context)?
                && !optional_not_found(pattern, tail(value), context)?
            {
                issues.push(context.issue(Rule::FailureMasking, node, "A match arm selecting Err(...) returns Ok(...). Error provenance is discarded at this branch and callers receive the success variant; check the declared fallback contract and its tests."));
            }
        }
    }
    if !rust && node.kind() == "catch_clause" {
        if let Some(body) = node.child_by_field_name("body") {
            let last = code_children(body).last().copied();
            if last.is_some_and(|last| last.kind() == "return_statement")
                && context.compact(tail(body))? == "true"
            {
                issues.push(context.issue(Rule::FailureMasking, node, "The catch handler ends with `return true`, reporting success after an exception. Verify an explicit degraded-success contract rather than silently replacing failure."));
            }
        }
    }
    if !rust && node.kind() == "new_expression" && !context.promise_shadowed {
        if let (Some(constructor), Some(arguments)) = (
            node.child_by_field_name("constructor"),
            node.child_by_field_name("arguments"),
        ) {
            if tokens::text(constructor, context.source) == "Promise" {
                let executor = code_children(arguments).first().copied();
                if let Some(executor) = executor.filter(|executor| {
                    matches!(
                        executor.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                }) {
                    if tokens::children(executor)
                        .iter()
                        .any(|child| child.kind() == "async")
                    {
                        issues.push(context.issue(Rule::AsyncResourceSafety, node, "An async function is used as the Promise executor. Promise ignores the executor's returned promise, so exceptions/rejections from the async executor do not automatically reject the constructed promise; it can remain pending. This check assumes the unshadowed standard Promise constructor."));
                    }
                }
            }
        }
    }
    Ok(())
}

fn error_pattern(node: Node<'_>, context: &mut Context<'_>) -> Result<bool, &'static str> {
    let pattern = context.compact(node)?;
    Ok([
        "Err(",
        "Result::Err(",
        "std::result::Result::Err(",
        "core::result::Result::Err(",
    ]
    .iter()
    .any(|prefix| pattern.starts_with(prefix)))
}

fn success(node: Node<'_>, context: &mut Context<'_>) -> Result<bool, &'static str> {
    if node.kind() != "call_expression" {
        return Ok(false);
    }
    let Some(function) = node.child_by_field_name("function") else {
        return Ok(false);
    };
    Ok(matches!(
        context.compact(function)?.as_str(),
        "Ok" | "Result::Ok" | "std::result::Result::Ok" | "core::result::Result::Ok"
    ))
}

fn optional_not_found(
    pattern: Node<'_>,
    value: Node<'_>,
    context: &mut Context<'_>,
) -> Result<bool, &'static str> {
    // A typed NotFound guard plus an absent optional value is an explicit
    // absence contract, not an unexplained catch-all success fallback.
    if context.compact(value)? != "Ok(None)" {
        return Ok(false);
    }
    let pattern = context.compact(pattern)?;
    let Some(rest) = pattern.strip_prefix("Err(") else {
        return Ok(false);
    };
    let Some((binding, guard)) = rest.split_once(")if") else {
        return Ok(false);
    };
    if binding.is_empty()
        || !binding
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Ok(false);
    }
    Ok(
        guard == format!("{binding}.kind()==std::io::ErrorKind::NotFound")
            || guard == format!("std::io::ErrorKind::NotFound=={binding}.kind()"),
    )
}
