//! Original, readable audit and metric cards, independent of a vendor rule set.

use crate::metrics::Metric;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    /// Implemented by this crate's optional structural adapter, for its explicitly
    /// supported languages. Matches still require contextual judgment.
    Structural,
    /// A readable review audit, not a claim of implemented automatic detection.
    Substantive,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct AuditCard {
    pub id: &'static str,
    pub title: &'static str,
    pub question: &'static str,
    pub evidence: &'static str,
    pub exceptions: &'static str,
    pub automation: Automation,
}

pub const AUDIT_CARDS: &[AuditCard] = &[
    AuditCard {
        id: "structural.syntax", title: "Invalid or missing syntax",
        question: "Does recorded source parse with the declared language grammar?",
        evidence: "Error/missing AST node, recorded location and grammar version; verify with the language toolchain when needed.",
        exceptions: "An incomplete grammar can reject valid language features. Parser evidence alone is not a compiler verdict.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.rust-placeholder", title: "Unfinished required behavior",
        question: "Does a Rust todo!/unimplemented! invocation stand in for required behavior?",
        evidence: "Actual macro invocation, reachable requirement context and recorded baseline; strings/comments are not invocations.",
        exceptions: "Intentional scaffolding, test fixtures and shadowed macros need contextual inspection.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.empty-catch", title: "Undocumented empty catch",
        question: "Does a TypeScript/TSX handler discard a failure without behavior or explanation?",
        evidence: "Catch AST node, affected operation/caller and its failure contract.",
        exceptions: "Explicitly optional operations can legitimately ignore failures. Comments require judgment, not automatic approval.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.failure-masking", title: "Failure reported as success",
        question: "Can an operation fail while its caller receives apparent success or an unexplained default?",
        evidence: "Failure path, handler, returned value and caller expectations across the relevant language's error model.",
        exceptions: "Documented optional behavior and deliberate degraded operation may be correct.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.duplicated-behavior", title: "Copy/paste logic and repeated behavior",
        question: "Did the change introduce inconsistent copies, identical conditional branches or repeated conditions?",
        evidence: "Both source ranges, structural similarity, behavioral differences and the recorded baseline.",
        exceptions: "Independent contracts, generated code and intentionally parallel fixtures can justify duplication. Do not create an abstraction solely to reduce a count.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.unnecessary-machinery", title: "Machinery without a demonstrated purpose",
        question: "Do wrappers, extension points, configuration or generic parameters solve a current requirement?",
        evidence: "Added abstraction, actual consumers, stated requirement and existing simpler alternatives.",
        exceptions: "A single implementation can still justify an interface for a real boundary. Data records are not inherently poor design.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.manageability", title: "Growing responsibility or reasoning burden",
        question: "Did method/class growth, nesting, complexity or coupling make a component harder to change safely?",
        evidence: "Before/after metric observations, chosen profile, affected responsibilities and concrete maintenance consequences.",
        exceptions: "Long declarative tables, generated code and straightforward parsers/dispatchers may be appropriate. A threshold is a review trigger, not a defect.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.async-resource-safety", title: "Async work and resource lifetime",
        question: "Are failures, pending work, cleanup and synchronization handled according to the language's ownership/concurrency model?",
        evidence: "Relevant call/lifetime/control-flow path; type or data-flow evidence where syntax cannot establish behavior.",
        exceptions: "Ownership-based cleanup, framework-managed resources and explicitly supervised background work can be correct.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.weak-verification", title: "Tests or completion claims without behavioral evidence",
        question: "Do tests and documented completion claims establish the requested observable behavior, including relevant failure cases?",
        evidence: "Actual assertions, exercised behavior, matching test/coverage artifacts and criterion text.",
        exceptions: "Framework assertions, snapshot checks and expected-exception tests may not contain ordinary assert calls. Missing coverage is unknown, not zero.",
        automation: Automation::Structural,
    },
    AuditCard {
        id: "anti-slop.unsafe-shortcuts", title: "Safety bypasses and unexplained suppressions",
        question: "Does new code bypass validation, verification, authorization or meaningful diagnostics?",
        evidence: "Changed trust boundary, validation/suppression point and relevant data/caller path; redact secret values.",
        exceptions: "Narrow documented exceptions may be justified. A suspicious API alone does not prove an exploitable vulnerability.",
        automation: Automation::Structural,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricScope {
    Function,
    Class,
    Module,
    Change,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    CodeLines,
    Count,
    Tokens,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetricCard {
    pub metric: Metric,
    pub title: &'static str,
    pub scope: MetricScope,
    pub unit: MetricUnit,
    pub definition: &'static str,
    pub interpretation: &'static str,
    pub exceptions: &'static str,
}

pub const METRIC_CARDS: &[MetricCard] = &[
    MetricCard {
        metric: Metric::MethodLength, title: "Method/function length", scope: MetricScope::Function, unit: MetricUnit::CodeLines,
        definition: "Count physical lines containing code in the function body, excluding signature-only lines, blank/comment-only lines and nested function/type bodies; include body delimiters where present.",
        interpretation: "Review growth together with responsibility, nesting and branching. Use the same language/counting convention for the baseline.",
        exceptions: "Generated code, tables, parsers and simple dispatchers can legitimately be long.",
    },
    MetricCard {
        metric: Metric::ClassLength, title: "Class length", scope: MetricScope::Class, unit: MetricUnit::CodeLines,
        definition: "Count physical code-containing lines in the class body, including its methods but excluding nested type bodies, blank lines and comment-only lines.",
        interpretation: "Review concentration of responsibilities and dependencies; compare within a suitable language and code-role profile.",
        exceptions: "Not applicable to constructs without class semantics. Do not substitute zero for unsupported extraction or treat all records/structs as classes.",
    },
    MetricCard {
        metric: Metric::MethodCount, title: "Methods per class", scope: MetricScope::Class, unit: MetricUnit::Count,
        definition: "Count directly declared syntactic methods and constructors, including abstract/overload signatures but excluding inherited methods, nested-type methods and function-valued fields; follow the versioned language convention.",
        interpretation: "Inspect interface size and responsibility accumulation alongside method complexity.",
        exceptions: "Generated bindings and framework-required methods may justify large counts.",
    },
    MetricCard {
        metric: Metric::ParameterCount, title: "Function parameter count", scope: MetricScope::Function, unit: MetricUnit::Count,
        definition: "Count declared input parameter slots, excluding implicit receivers and return slots; a destructured or variadic parameter occupies one slot.",
        interpretation: "Inspect calling conventions and related data; parameter count alone does not justify adding builders or parameter objects.",
        exceptions: "Foreign-function interfaces, callbacks and framework signatures can constrain the parameter list.",
    },
    MetricCard {
        metric: Metric::NestingDepth, title: "Control-flow nesting depth", scope: MetricScope::Function, unit: MetricUnit::Count,
        definition: "Measure maximum nested control-flow constructs within a function, excluding plain lexical blocks and separate nested function bodies; declare language-specific else-if handling.",
        interpretation: "Review how much branch context a reader must retain and which error paths become obscured.",
        exceptions: "Pattern matching and declarative/generated structures need language-aware interpretation.",
    },
    MetricCard {
        metric: Metric::CyclomaticComplexity, title: "Function cyclomatic complexity", scope: MetricScope::Function, unit: MetricUnit::Count,
        definition: "Use one plus decision points under a declared, versioned language counting convention; do not combine results from different conventions.",
        interpretation: "A structural proxy for path/testing burden, not a measurement of human understanding or proof of missing tests.",
        exceptions: "Generated state machines and decision tables can be intentionally branch-heavy.",
    },
    MetricCard {
        metric: Metric::ClassComplexity, title: "Aggregate class complexity", scope: MetricScope::Class, unit: MetricUnit::Count,
        definition: "Sum directly declared executable source-method complexity under one convention, excluding inherited/nested-type methods and abstract/compiler-generated bodies. Missing out-of-line/native implementations make the aggregate unavailable, not zero.",
        interpretation: "Find cumulative complexity that is hidden when each method is considered separately.",
        exceptions: "Not applicable without class semantics. Incomplete method measurements make the aggregate unavailable, not smaller.",
    },
    MetricCard {
        metric: Metric::DependencyFanOut, title: "Declared dependency fan-out", scope: MetricScope::Module, unit: MetricUnit::Count,
        definition: "Count distinct explicit dependency units under a versioned language granularity: import source paths, Rust crate roots, Java import targets or C/C++ headers. De-duplicate aliases and repeated declarations; unresolved explicit paths are unavailable. This is not whole-program/runtime coupling.",
        interpretation: "Inspect coupling growth and change responsibilities, not simply import statement counts.",
        exceptions: "Composition roots and public facade modules may intentionally coordinate many dependencies.",
    },
    MetricCard {
        metric: Metric::PublicSurface, title: "Declared public surface", scope: MetricScope::Module, unit: MetricUnit::Count,
        definition: "Count unique named source-visible bindings and visible members under the versioned language convention; overload sets count once. Exclude private/local and compiler-generated bindings. Unresolved wildcard/dynamic exports are unavailable; this is not a compiled ABI measurement.",
        interpretation: "Review growth of the supported compatibility and testing contract.",
        exceptions: "Public bindings and intentionally broad compatibility facades need context.",
    },
    MetricCard {
        metric: Metric::DuplicatedTokens, title: "Duplicated tokens in changed code", scope: MetricScope::Change, unit: MetricUnit::Tokens,
        definition: "Count unique token positions in changed source that participate in clone matches under declared normalization and minimum-match settings; count overlapping positions once.",
        interpretation: "Inspect repeated behavior and synchronization burden against the baseline. State the searched comparison scope.",
        exceptions: "Independent contracts, generated code, tables and fixtures can justify repeated source. No comparison index means unavailable, not zero.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn cards_have_unique_identities_definitions_and_interpretation_limits() {
        assert_eq!(METRIC_CARDS.len(), Metric::ALL.len());
        assert_eq!(
            AUDIT_CARDS
                .iter()
                .map(|card| card.id)
                .collect::<BTreeSet<_>>()
                .len(),
            AUDIT_CARDS.len()
        );
        assert_eq!(
            METRIC_CARDS
                .iter()
                .map(|card| card.metric)
                .collect::<BTreeSet<_>>(),
            Metric::ALL.into_iter().collect()
        );
        for card in AUDIT_CARDS {
            assert!([
                card.id,
                card.title,
                card.question,
                card.evidence,
                card.exceptions
            ]
            .iter()
            .all(|text| !text.trim().is_empty()));
        }
        for card in METRIC_CARDS {
            assert!([
                card.title,
                card.definition,
                card.interpretation,
                card.exceptions
            ]
            .iter()
            .all(|text| !text.trim().is_empty()));
        }
        assert!(METRIC_CARDS
            .iter()
            .any(|card| card.metric == Metric::MethodLength));
        assert!(METRIC_CARDS
            .iter()
            .any(|card| card.metric == Metric::ClassLength));
        serde_json::to_value(METRIC_CARDS).unwrap();
    }

    #[cfg(feature = "structural")]
    #[test]
    fn every_automated_rule_has_a_readable_card_and_vice_versa() {
        let rules: BTreeSet<_> = crate::structural::Rule::ALL
            .into_iter()
            .map(|rule| rule.id())
            .collect();
        let cards = AUDIT_CARDS
            .iter()
            .filter(|card| card.automation == Automation::Structural)
            .map(|card| card.id)
            .collect();
        assert_eq!(rules, cards);
    }
}
