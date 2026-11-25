//! Small, explicit structural rules over the existing parser registry.
//!
//! Matches are inspection candidates. They do not establish reachability,
//! requirement satisfaction, or whether suppression/placeholders are intentional.

use atomic_semantic::{Language, ParserRegistry};

pub use atomic_semantic::MAX_SOURCE_BYTES;
pub const RULESET_VERSION: &str = "2";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    Syntax,
    RustPlaceholder,
    EmptyCatch,
    FailureMasking,
    DuplicatedBehavior,
    UnnecessaryMachinery,
    Manageability,
    AsyncResourceSafety,
    WeakVerification,
    UnsafeShortcuts,
}

impl Rule {
    pub const ALL: [Self; 10] = [
        Self::Syntax,
        Self::RustPlaceholder,
        Self::EmptyCatch,
        Self::FailureMasking,
        Self::DuplicatedBehavior,
        Self::UnnecessaryMachinery,
        Self::Manageability,
        Self::AsyncResourceSafety,
        Self::WeakVerification,
        Self::UnsafeShortcuts,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Syntax => "structural.syntax",
            Self::RustPlaceholder => "anti-slop.rust-placeholder",
            Self::EmptyCatch => "anti-slop.empty-catch",
            Self::FailureMasking => "anti-slop.failure-masking",
            Self::DuplicatedBehavior => "anti-slop.duplicated-behavior",
            Self::UnnecessaryMachinery => "anti-slop.unnecessary-machinery",
            Self::Manageability => "anti-slop.manageability",
            Self::AsyncResourceSafety => "anti-slop.async-resource-safety",
            Self::WeakVerification => "anti-slop.weak-verification",
            Self::UnsafeShortcuts => "anti-slop.unsafe-shortcuts",
        }
    }

    pub fn supports(self, language: Language) -> bool {
        match self {
            Self::Syntax => true,
            Self::RustPlaceholder => language == Language::Rust,
            Self::EmptyCatch => matches!(language, Language::TypeScript | Language::Tsx),
            Self::FailureMasking | Self::DuplicatedBehavior | Self::AsyncResourceSafety => {
                matches!(
                    language,
                    Language::Rust | Language::TypeScript | Language::Tsx
                )
            }
            Self::UnnecessaryMachinery | Self::WeakVerification | Self::UnsafeShortcuts => {
                language == Language::Rust
            }
            Self::Manageability => true,
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::Syntax => "Parser reports invalid or missing syntax",
            Self::RustPlaceholder => "Placeholder macro requires review",
            Self::EmptyCatch => "Empty catch may silently discard a failure",
            Self::FailureMasking => "Failure path converts an observed error into apparent success",
            Self::DuplicatedBehavior => "Conditional alternatives execute identical code",
            Self::UnnecessaryMachinery => {
                "Private wrapper forwards an unchanged contract without behavior"
            }
            Self::Manageability => {
                "Function combines a long body with substantial branching or nesting"
            }
            Self::AsyncResourceSafety => {
                "Async work violates an observed resource or completion contract"
            }
            Self::WeakVerification => "Test has empty or non-independent verification",
            Self::UnsafeShortcuts => "Safety or verification bypass lacks a local justification",
        }
    }

    /// Exact implemented pattern coverage; completing these checks is not an
    /// assertion that every concern in the broad audit card was proved absent.
    pub fn coverage(self) -> &'static str {
        match self {
            Self::Syntax => "Registered-language syntax error/missing nodes",
            Self::RustPlaceholder => "Rust todo!/unimplemented! AST invocations",
            Self::EmptyCatch => "TypeScript/TSX catch bodies without behavior or comments",
            Self::FailureMasking => "Rust Err-pattern branches returning Ok; TypeScript/TSX catch returning true",
            Self::DuplicatedBehavior => "Rust/TypeScript/TSX identical nonempty if/else token bodies, excluding Rust if-let bindings",
            Self::UnnecessaryMachinery => "Rust same-module private unchanged forwarding chains; excludes public/attributed/generic/adapter boundaries",
            Self::Manageability => "Unique named bodies with >=100 code lines AND complexity >=15 or nesting >=4; existing concerns need >=3 additional decisions or a new nesting level",
            Self::AsyncResourceSafety => "Rust same-block known synchronous guards across await; TypeScript/TSX unshadowed async Promise executors",
            Self::WeakVerification => "Rust empty tests (except should_panic) and test assertions of true or pure self-comparisons",
            Self::UnsafeShortcuts => "Rust selected qualified unsafe memory operations without local SAFETY argument, or unexplained blanket warnings/clippy::all suppression",
        }
    }

    pub fn correction(self) -> &'static str {
        match self {
            Self::Syntax => "Inspect the syntax error and verify with the language toolchain; grammar support may be incomplete",
            Self::RustPlaceholder => "Implement the required behavior, or explicitly justify this placeholder and exercise its intended behavior",
            Self::EmptyCatch => "Handle or propagate the failure, or document why ignoring it is correct for this requirement",
            Self::FailureMasking => "Preserve the failure signal or make the intentional degraded-success contract explicit and test its callers",
            Self::DuplicatedBehavior => "Check for a mistaken branch body; otherwise remove the redundant distinction only if condition evaluation and branch-local bindings are preserved",
            Self::UnnecessaryMachinery => "Check for a real naming or architectural boundary; otherwise call the existing implementation rather than maintaining an identical private contract",
            Self::Manageability => "Review the combined control-flow and responsibility growth; simplify cohesive paths where justified and verify relevant branches, rather than splitting solely to satisfy a count",
            Self::AsyncResourceSafety => "Release synchronous guards before suspension, or use an appropriate async primitive; return/await failures through the owning task or promise contract",
            Self::WeakVerification => "Exercise the requested behavior and compare its result with an independent expectation, including relevant failure cases",
            Self::UnsafeShortcuts => "Restore validation/diagnostics or document the precise preconditions and narrow justification for the bypass, with tests of the affected boundary",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ManageabilityEvidence {
    pub subject: String,
    pub code_lines: u32,
    pub complexity: u32,
    pub nesting: u32,
}

#[derive(Debug)]
pub struct StructuralIssue {
    pub rule: Rule,
    pub start_line: u32,
    pub end_line: u32,
    /// Exact AST node bytes, retained for baseline comparison before display clipping.
    pub source: String,
    pub detail: String,
    pub manageability: Option<ManageabilityEvidence>,
}

#[derive(Debug)]
pub struct StructuralInspection {
    pub issues: Vec<StructuralIssue>,
    /// Anti-slop checks are not evaluated on error-recovered trees.
    pub has_syntax_errors: bool,
    pub ambiguous_manageability_subjects: Vec<String>,
}

/// Inspect full recorded UTF-8 content with the registry's configured grammar.
/// Errors are explicit coverage gaps, never empty successful inspections.
pub fn inspect(
    registry: &mut ParserRegistry,
    path: &str,
    source: &str,
) -> Result<StructuralInspection, &'static str> {
    let language = Language::detect(path).ok_or("Unsupported language")?;
    if source.len() > MAX_SOURCE_BYTES {
        return Err("Source exceeds the 1 MiB structural inspection budget");
    }
    let tree = registry
        .parse_tree(path, source)
        .ok_or("Parser returned no syntax tree")?;
    let has_syntax_errors = tree.root_node().has_error();
    let mut issues = Vec::new();
    let mut nodes = Vec::new();
    let mut stack = vec![(tree.root_node(), 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 256 {
            return Err("Audit AST nesting budget exceeded; inspection is incomplete");
        }
        nodes.push(node);
        if nodes.len() > 300_000 {
            return Err("Audit AST node budget exceeded");
        }
        let rule = if has_syntax_errors {
            (node.is_error() || node.is_missing()).then_some(Rule::Syntax)
        } else if language == Language::Rust && node.kind() == "macro_invocation" {
            node.child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .filter(|name| {
                    matches!(
                        name.rsplit("::").next().map(str::trim),
                        Some("todo" | "unimplemented")
                    )
                })
                .map(|_| Rule::RustPlaceholder)
        } else if matches!(language, Language::TypeScript | Language::Tsx)
            && node.kind() == "catch_clause"
        {
            // A comment may explain intentional suppression. Leave that case to
            // substantive review rather than calling it an undocumented discard.
            node.child_by_field_name("body")
                .filter(|body| body.named_child_count() == 0)
                .map(|_| Rule::EmptyCatch)
        } else {
            None
        };
        if let Some(rule) = rule {
            let text = node
                .utf8_text(source.as_bytes())
                .map_err(|_| "Invalid AST source range")?;
            issues.push(StructuralIssue {
                rule,
                start_line: node.start_position().row as u32 + 1,
                end_line: node.end_position().row as u32
                    + u32::from(
                        node.end_position().column > 0
                            || node.end_position().row == node.start_position().row,
                    ),
                source: text.into(),
                detail: rule.summary().into(),
                manageability: None,
            });
            if has_syntax_errors {
                // One syntax finding is sufficient; recovered subtrees are not
                // independent anti-slop evidence.
                break;
            }
            if issues.len() > 100 {
                return Err("Structural issue budget exceeded; inspect this file manually");
            }
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }
    let ambiguous_manageability_subjects = if !has_syntax_errors {
        crate::substantive::inspect(language, path, source, &nodes, &mut issues)?
    } else {
        Vec::new()
    };
    Ok(StructuralInspection {
        issues,
        has_syntax_errors,
        ambiguous_manageability_subjects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_placeholders_are_ast_matches_not_strings_or_comments() {
        let mut registry = ParserRegistry::new();
        let result = inspect(
            &mut registry,
            "lib.rs",
            "// todo!()\nfn f() { let _ = \"unimplemented!()\"; std::todo!(); }",
        )
        .unwrap();
        assert!(!result.has_syntax_errors);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].source, "std::todo!()");
        assert_eq!(result.issues[0].start_line, 2);
    }

    #[test]
    fn empty_catch_does_not_flag_handler_logic_or_explanatory_comments() {
        let mut registry = ParserRegistry::new();
        let result = inspect(&mut registry, "app.ts", "try { work(); } catch (e) {}\ntry { work(); } catch { throw Error(); }\ntry { optional(); } catch { /* explicitly optional */ }").unwrap();
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].rule, Rule::EmptyCatch);
    }

    #[test]
    fn parse_errors_are_not_clean_or_anti_slop_evidence() {
        let mut registry = ParserRegistry::new();
        let result = inspect(&mut registry, "lib.rs", "fn broken( { todo!();").unwrap();
        assert!(result.has_syntax_errors);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].rule, Rule::Syntax);
        assert!(inspect(&mut registry, "unknown.txt", "hello").is_err());
        assert!(inspect(&mut registry, "lib.rs", &" ".repeat(MAX_SOURCE_BYTES + 1)).is_err());
    }

    #[test]
    fn existing_grammar_registry_handles_every_supported_language() {
        let mut registry = ParserRegistry::new();
        for (path, source) in [
            ("app.ts", "const x = 1;"),
            ("app.tsx", "const x = <div />;"),
            ("app.py", "x = 1\n"),
            ("lib.rs", "fn main() {}"),
            ("main.go", "package main\nfunc main() {}"),
            ("App.java", "class App {}"),
            ("app.cpp", "int main() {}"),
            ("app.c", "int main() {}"),
            ("app.swift", "let x = 1"),
        ] {
            let result = inspect(&mut registry, path, source).unwrap();
            assert!(!result.has_syntax_errors, "{path}");
            assert!(result.issues.is_empty(), "{path}");
        }
    }

    #[test]
    fn issue_budget_is_an_explicit_failure_not_a_truncated_clean_result() {
        let mut registry = ParserRegistry::new();
        let source = format!("fn unfinished() {{ {} }}", "todo!();".repeat(101));
        assert!(inspect(&mut registry, "lib.rs", &source)
            .unwrap_err()
            .contains("budget exceeded"));
    }
}
