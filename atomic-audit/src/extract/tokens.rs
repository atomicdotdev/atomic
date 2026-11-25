use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};

use atomic_semantic::SyntaxNode;

use super::languages;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Token {
    pub kind: String,
    pub text: String,
}

pub(crate) fn comment(node: SyntaxNode<'_>) -> bool {
    node.kind().contains("comment")
}

pub(crate) fn children(node: SyntaxNode<'_>) -> Vec<SyntaxNode<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

pub(crate) fn named(node: SyntaxNode<'_>) -> Vec<SyntaxNode<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub(crate) fn text<'a>(node: SyntaxNode<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub(crate) fn leaf_tokens(
    root: SyntaxNode<'_>,
    source: &str,
    max: usize,
) -> Result<Vec<Token>, String> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    let mut visited = 0;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > 300_000 {
            return Err("AST node budget exceeded".into());
        }
        if comment(node) {
            continue;
        }
        // Keep literals atomic. Quotes/fragments must not turn unrelated string
        // contents into apparent code clones or conditional operators.
        if node.child_count() == 0 || languages::literal(node) {
            let value = text(node, source);
            if !value.trim().is_empty() {
                result.push(Token {
                    kind: node.kind().into(),
                    text: value.into(),
                });
                if result.len() > max {
                    return Err("Token budget exceeded".into());
                }
            }
        } else {
            stack.extend(children(node).into_iter().rev());
        }
    }
    Ok(result)
}

/// Count physical code-bearing lines from AST tokens, not text/comment regexes.
pub(crate) fn code_lines(
    root: SyntaxNode<'_>,
    source: &str,
    language: atomic_semantic::Language,
    skip_functions: bool,
) -> u32 {
    let mut lines = BTreeSet::new();
    let mut stack = vec![(root, true)];
    while let Some((node, first)) = stack.pop() {
        if comment(node) {
            continue;
        }
        if !first
            && (languages::type_boundary(node, language)
                || (skip_functions && languages::callable(node, language)))
        {
            let body = languages::body(node, language).map(|body| body.id());
            stack.extend(
                children(node)
                    .into_iter()
                    .rev()
                    .filter(|child| Some(child.id()) != body)
                    .map(|child| (child, false)),
            );
            continue;
        }
        if node.child_count() == 0 || languages::literal(node) {
            for (offset, part) in text(node, source).split('\n').enumerate() {
                if !part.trim().is_empty() {
                    lines.insert(node.start_position().row + offset);
                }
            }
        } else {
            stack.extend(children(node).into_iter().rev().map(|child| (child, false)));
        }
    }
    lines.len() as u32
}

pub(super) fn changed(
    old: &[Token],
    new: &[Token],
    work: &mut usize,
) -> Result<(Vec<bool>, Vec<bool>), String> {
    if old == new {
        return Ok((vec![false; old.len()], vec![false; new.len()]));
    }
    let cost = old.len().saturating_mul(new.len());
    if cost > *work {
        return Err("Changed-token diff work budget exceeded".into());
    }
    *work -= cost;
    let mut before = vec![false; old.len()];
    let mut after = vec![false; new.len()];
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, old, new) {
        if op.tag() != similar::DiffTag::Equal {
            for index in op.old_range() {
                before[index] = true;
            }
            for index in op.new_range() {
                after[index] = true;
            }
        }
    }
    Ok((before, after))
}

/// Exact same-language token windows. Verify equality after hashing; collisions
/// never become clone evidence. Overlaps are unioned and never counted twice.
pub(super) fn duplicated(
    files: &[(&str, &[Token])],
    minimum: usize,
    pairs: &mut usize,
) -> Result<Vec<Vec<bool>>, String> {
    let mut positions: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    let mut matches: Vec<Vec<bool>> = files
        .iter()
        .map(|(_, tokens)| vec![false; tokens.len()])
        .collect();
    for (file, (language, tokens)) in files.iter().enumerate() {
        for start in 0..tokens.len().saturating_add(1).saturating_sub(minimum) {
            let window = &tokens[start..start + minimum];
            let mut hash = DefaultHasher::new();
            language.hash(&mut hash);
            window.hash(&mut hash);
            let previous = positions.entry(hash.finish()).or_default();
            for &(other_file, other_start) in previous.iter() {
                if *pairs == 0 {
                    return Err("Clone comparison work budget exceeded".into());
                }
                *pairs -= 1;
                if other_file == file && start.abs_diff(other_start) < minimum {
                    continue;
                }
                if files[other_file].0 == *language
                    && files[other_file].1[other_start..other_start + minimum] == *window
                {
                    matches[file][start..start + minimum].fill(true);
                    matches[other_file][other_start..other_start + minimum].fill(true);
                }
            }
            previous.push((file, start));
        }
    }
    Ok(matches)
}
