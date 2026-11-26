//! Executable anti-slop checks over the registry's existing syntax trees.
//! Findings state observed constructs, not whole-program behavioral proof.

mod branching;
mod rust;

use crate::extract::{languages, tokens};
use crate::structural::{ManageabilityEvidence, Rule, StructuralIssue};
use atomic_semantic::{Language, SyntaxNode as Node};
use std::collections::BTreeMap;

pub(super) struct Context<'a> {
    source: &'a str,
    path: &'a str,
    language: Language,
    remaining: usize,
    promise_shadowed: bool,
    subjects: BTreeMap<String, usize>,
}

impl Context<'_> {
    fn charge(&mut self, node: Node<'_>) -> Result<(), &'static str> {
        self.remaining = self
            .remaining
            .checked_sub(node.end_byte() - node.start_byte())
            .ok_or("Substantive audit work budget exceeded; inspection is incomplete")?;
        Ok(())
    }

    fn normalized(&mut self, node: Node<'_>) -> Result<Vec<tokens::Token>, &'static str> {
        self.charge(node)?;
        tokens::leaf_tokens(node, self.source, 80_000)
            .map_err(|_| "Substantive audit token budget exceeded")
    }

    fn compact(&mut self, node: Node<'_>) -> Result<String, &'static str> {
        Ok(self
            .normalized(node)?
            .into_iter()
            .map(|token| token.text)
            .collect())
    }

    fn issue(&self, rule: Rule, node: Node<'_>, detail: impl Into<String>) -> StructuralIssue {
        StructuralIssue {
            rule,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32
                + u32::from(
                    node.end_position().column > 0
                        || node.end_position().row == node.start_position().row,
                ),
            source: tokens::text(node, self.source).into(),
            detail: detail.into(),
            manageability: None,
        }
    }
}

fn code_children(node: Node<'_>) -> Vec<Node<'_>> {
    tokens::named(node)
        .into_iter()
        .filter(|node| !tokens::comment(*node))
        .collect()
}

fn tail(mut node: Node<'_>) -> Node<'_> {
    loop {
        if matches!(
            node.kind(),
            "block"
                | "statement_block"
                | "return_expression"
                | "return_statement"
                | "expression_statement"
                | "parenthesized_expression"
        ) {
            if let Some(child) = code_children(node).last() {
                node = *child;
                continue;
            }
        }
        return node;
    }
}

pub(crate) fn inspect(
    language: Language,
    path: &str,
    source: &str,
    nodes: &[Node<'_>],
    issues: &mut Vec<StructuralIssue>,
) -> Result<Vec<String>, &'static str> {
    let promise_shadowed = nodes.iter().any(|node| {
        node.child_by_field_name("name")
            .or_else(|| node.child_by_field_name("pattern"))
            .is_some_and(|name| tokens::text(name, source) == "Promise")
            || (node.kind() == "identifier"
                && tokens::text(*node, source) == "Promise"
                && node.parent().is_some_and(|parent| {
                    matches!(
                        parent.kind(),
                        "required_parameter"
                            | "optional_parameter"
                            | "import_specifier"
                            | "import_clause"
                    )
                }))
    });
    let mut functions = BTreeMap::<(usize, String), Vec<Node<'_>>>::new();
    for &node in nodes.iter().filter(|node| node.kind() == "function_item") {
        if let (Some(parent), Some(name)) = (node.parent(), node.child_by_field_name("name")) {
            functions
                .entry((parent.id(), tokens::text(name, source).into()))
                .or_default()
                .push(node);
        }
    }
    let mut subjects = BTreeMap::<String, usize>::new();
    for &node in nodes
        .iter()
        .filter(|node| languages::callable(**node, language))
    {
        let (subject, stable) = languages::qualified(node, source, language);
        if stable {
            *subjects.entry(subject).or_default() += 1;
        }
    }
    let mut context = Context {
        source,
        path,
        language,
        remaining: 32 * 1024 * 1024,
        promise_shadowed,
        subjects,
    };
    for &node in nodes {
        if matches!(
            language,
            Language::Rust | Language::TypeScript | Language::Tsx
        ) {
            branching::inspect(node, &mut context, issues)?;
        }
        if language == Language::Rust {
            rust::inspect(node, &functions, &mut context, issues)?;
        }
        if languages::callable(node, language) {
            manageability(node, &mut context, issues)?;
        }
        if issues.len() > 100 {
            return Err("Substantive audit issue budget exceeded; inspection is incomplete");
        }
    }
    Ok(context
        .subjects
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect())
}

// Atomic policy, not a universal defect threshold: require BOTH a long body
// and meaningful branching/nesting, not a single large syntax measurement.
fn manageability(
    node: Node<'_>,
    context: &mut Context<'_>,
    issues: &mut Vec<StructuralIssue>,
) -> Result<(), &'static str> {
    let Some(body) = languages::body(node, context.language) else {
        return Ok(());
    };
    let (subject, stable) = languages::qualified(node, context.source, context.language);
    if !stable || context.subjects.get(&subject) != Some(&1) {
        return Ok(());
    }
    context.charge(body)?;
    let code_lines = tokens::code_lines(body, context.source, context.language, true);
    let (complexity, nesting) = languages::control_flow(body, context.source, context.language);
    if code_lines >= 100 && (complexity >= 15 || nesting >= 4) {
        let mut issue = context.issue(Rule::Manageability, node, format!("{subject}: {code_lines} code-bearing body lines, cyclomatic complexity {complexity}, nesting {nesting}. atomic.manageability.v1 requires at least 100 body lines AND complexity >=15 or nesting >=4. This joint concentration increases control-flow review burden; it is not a defect or missing-test verdict."));
        issue.manageability = Some(ManageabilityEvidence {
            subject,
            code_lines,
            complexity,
            nesting,
        });
        issues.push(issue);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
