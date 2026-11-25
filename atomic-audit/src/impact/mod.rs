//! Recorded-source direct-call binding and compatibility evidence.
//! No filesystem access, builds, compiler invocation or runtime execution.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceLocation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectCall {
    pub caller: String,
    pub callee: String,
    pub location: SourceLocation,
    pub declaration: SourceLocation,
    pub binding: String,
    pub arguments: Option<usize>,
    pub target_present: bool,
    pub visibility_compatible: bool,
    #[serde(default)]
    pub configuration_dependent: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangedFunction {
    pub id: String,
    pub before: Option<SourceLocation>,
    pub after: Option<SourceLocation>,
    pub change: String,
    pub signature_changed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContractIssue {
    pub function: String,
    pub call: DirectCall,
    pub before_declaration: Option<SourceLocation>,
    pub after_declaration: Option<SourceLocation>,
    pub primary_location: SourceLocation,
    pub primary_before: bool,
    pub introduced: bool,
    pub summary: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImpactReport {
    pub change: String,
    pub changed_functions: Vec<ChangedFunction>,
    pub callers_before: Vec<DirectCall>,
    pub callers_after: Vec<DirectCall>,
    pub callees_after: Vec<DirectCall>,
    pub issues: Vec<ContractIssue>,
    pub gaps: Vec<String>,
    pub inspected_files_before: usize,
    pub inspected_files_after: usize,
    #[serde(default)]
    pub searched_files_before: usize,
    #[serde(default)]
    pub searched_files_after: usize,
    pub checked_call_contracts: usize,
    pub compatible_call_contracts: usize,
    pub preexisting_incompatible_contracts: usize,
    pub code_changed: bool,
    pub contract_assessment: ContractAssessment,
    #[serde(default)]
    pub source_selection_complete: Option<bool>,
    #[serde(default)]
    pub unresolved_references: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_callable_changes: Vec<String>,
}

/// Applies only to resolved direct-call availability/access/argument-count checks,
/// not type compatibility or overall implementation correctness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractAssessment {
    #[default]
    NotEvaluated,
    CompatibleResolvedCalls,
    IncompatibleResolvedCalls,
}

#[cfg(feature = "structural")]
mod analysis;
#[cfg(feature = "structural")]
pub use analysis::{analyze, analyze_semantic, analyze_with_coverage};

#[cfg(all(test, feature = "structural"))]
mod tests;
