//! Explainable change-review prioritization, not a defect probability model.
//! Research motivates the dimensions; numeric defaults are Atomic policy.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MODEL: &str = "atomic.change-risk.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Unknown,
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskProfile {
    pub name: String,
    pub broad_functions: usize,
    pub broad_files: usize,
    pub broad_components: usize,
    pub api_functions: usize,
    pub api_components: usize,
    pub api_growth: u64,
    pub propagation_callers: usize,
    pub propagation_files: usize,
    pub moderate_functions: usize,
    pub moderate_files: usize,
    pub moderate_callers: usize,
}

impl Default for RiskProfile {
    fn default() -> Self {
        Self {
            name: "balanced".into(),
            broad_functions: 50,
            broad_files: 5,
            broad_components: 2,
            api_functions: 20,
            api_components: 3,
            api_growth: 10,
            propagation_callers: 25,
            propagation_files: 5,
            moderate_functions: 10,
            moderate_files: 5,
            moderate_callers: 5,
        }
    }
}

impl RiskProfile {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty()
            || self.api_growth == 0
            || [
                self.broad_functions,
                self.broad_files,
                self.broad_components,
                self.api_functions,
                self.api_components,
                self.propagation_callers,
                self.propagation_files,
                self.moderate_functions,
                self.moderate_files,
                self.moderate_callers,
            ]
            .contains(&0)
        {
            return Err("risk profile requires a nonempty name and positive thresholds");
        }
        if self.broad_functions < self.moderate_functions
            || self.broad_files < self.moderate_files
            || self.propagation_callers < self.moderate_callers
        {
            return Err("high-risk thresholds cannot be below corresponding moderate thresholds");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RiskFacts {
    pub available: bool,
    pub production_functions: usize,
    pub existing_production_functions: usize,
    pub signature_changes: usize,
    pub test_support_functions: usize,
    pub structural_alerts: usize,
    pub production_paths: Vec<String>,
    pub components: Vec<String>,
    pub observed_production_callers: usize,
    pub observed_test_support_callers: usize,
    /// Consumers of modified/removed existing declarations, not only new helpers.
    pub existing_production_callers: usize,
    pub existing_caller_files: usize,
    pub outside_change_callers: usize,
    pub public_growth: Option<u64>,
    pub added_file_public_units: Option<u64>,
    pub public_removed: Option<u64>,
    pub dependency_growth: Option<u64>,
    pub non_callable_paths: Vec<String>,
    pub source_selection_complete: Option<bool>,
    /// Unresolved expressions are uncertainty, never observed callers.
    pub unresolved_references: Option<usize>,
    pub surface_measurements_complete: bool,
    pub unknowns: Vec<String>,
    /// Bounded recorded references by dimension: change, calls, surface, dependency.
    pub references: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskRule {
    pub id: String,
    pub level: RiskLevel,
    pub reason: String,
    pub references: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskEvaluation {
    pub model: String,
    pub profile: RiskProfile,
    pub rating: RiskLevel,
    /// Confidence in applicability to observed scope, not calibrated probability.
    pub evidence_complete: bool,
    pub rules: Vec<RiskRule>,
    pub facts: RiskFacts,
}

/// Apply independent rule triggers and take the highest band. Correlated size
/// signals are not summed into a spurious numeric probability or bug count.
pub fn assess(facts: RiskFacts, profile: &RiskProfile) -> Result<RiskEvaluation, &'static str> {
    profile.validate()?;
    if !facts.available {
        return Ok(RiskEvaluation {
            model: MODEL.into(),
            profile: profile.clone(),
            rating: RiskLevel::Unknown,
            evidence_complete: false,
            rules: vec![],
            facts,
        });
    }
    let mut rules = Vec::new();
    let mut trigger = |id: &str, level, reason: String, dimensions: &[&str]| {
        let mut references: Vec<_> = dimensions
            .iter()
            .flat_map(|key| facts.references.get(*key).into_iter().flatten())
            .cloned()
            .collect();
        references.sort();
        references.dedup();
        references.truncate(8);
        rules.push(RiskRule {
            id: id.into(),
            level,
            reason,
            references,
        });
    };
    let public_growth = facts
        .public_growth
        .unwrap_or(0)
        .saturating_add(facts.added_file_public_units.unwrap_or(0));
    let public_churn = public_growth.saturating_add(facts.public_removed.unwrap_or(0));
    if facts.production_functions >= profile.broad_functions
        && facts.production_paths.len() >= profile.broad_files
        && facts.components.len() >= profile.broad_components
    {
        trigger("risk.broad-production-change", RiskLevel::High, format!("{} production functions across {} source files and {} components exceed the broad-change thresholds ({}/{}/{})", facts.production_functions, facts.production_paths.len(), facts.components.len(), profile.broad_functions, profile.broad_files, profile.broad_components), &["change"]);
    }
    if facts.production_functions >= profile.api_functions
        && facts.components.len() >= profile.api_components
        && public_churn >= profile.api_growth
    {
        trigger("risk.cross-component-api-churn", RiskLevel::High, format!("{} production functions across {} components with {} declared public units added/removed meet the integration thresholds ({}/{}/{})", facts.production_functions, facts.components.len(), public_churn, profile.api_functions, profile.api_components, profile.api_growth), &["change", "surface"]);
    }
    if facts.existing_production_callers >= profile.propagation_callers
        && facts.existing_caller_files >= profile.propagation_files
        && facts.existing_production_functions > 0
    {
        trigger("risk.existing-consumer-exposure", RiskLevel::High, format!("Modified existing declarations have {} observed production consumers in {} files (thresholds {}/{})", facts.existing_production_callers, facts.existing_caller_files, profile.propagation_callers, profile.propagation_files), &["change", "calls"]);
    }
    if facts.production_functions >= profile.moderate_functions
        || facts.production_paths.len() >= profile.moderate_files
        || facts.components.len() >= 2
    {
        trigger(
            "risk.change-diffusion",
            RiskLevel::Medium,
            format!(
                "Change spans {} production functions, {} source files and {} components",
                facts.production_functions,
                facts.production_paths.len(),
                facts.components.len()
            ),
            &["change"],
        );
    }
    if public_churn > 0 || facts.signature_changes > 0 {
        trigger("risk.public-contract-churn", RiskLevel::Medium, format!("{} declared public units added/removed and {} modified existing production signatures need compatibility review", public_churn, facts.signature_changes), &["surface", "change"]);
    }
    if facts.existing_production_callers >= profile.moderate_callers
        || facts.outside_change_callers > 0
    {
        trigger("risk.consumer-review", RiskLevel::Medium, format!("{} observed production consumers of existing declarations; {} observed consumers outside the selected source files", facts.existing_production_callers, facts.outside_change_callers), &["calls"]);
    }
    if facts.dependency_growth.is_some_and(|growth| growth > 0) && facts.production_functions > 0 {
        trigger(
            "risk.dependency-growth",
            RiskLevel::Medium,
            format!(
                "Declared dependency fan-out increased by {} units across modified files",
                facts.dependency_growth.unwrap()
            ),
            &["dependency"],
        );
    }
    if !facts.non_callable_paths.is_empty() {
        trigger("risk.module-contract-change", RiskLevel::Medium, format!("{} production source files changed imports/types/module-level declarations beyond function bodies", facts.non_callable_paths.len()), &["change"]);
    }
    if facts.structural_alerts > 0 {
        trigger(
            "risk.structural-alerts",
            RiskLevel::Medium,
            format!(
                "{} introduced/uncertain automated structural alerts require contextual review",
                facts.structural_alerts
            ),
            &["alerts"],
        );
    }
    let evidence_complete = facts.source_selection_complete == Some(true)
        && facts.unresolved_references == Some(0)
        && facts.surface_measurements_complete
        && facts.unknowns.is_empty();
    if rules.is_empty() && facts.available && evidence_complete {
        let references = facts.references.get("change").cloned().unwrap_or_default();
        rules.push(RiskRule { id: "risk.localized-change".into(), level: RiskLevel::Low, reason: if facts.production_functions == 0 && facts.test_support_functions > 0 { "Only conventionally classified test/support functions changed; no measured production exposure trigger" } else { "Localized measured change with no elevated exposure trigger in the analyzed scope" }.into(), references });
    }
    let rating = rules
        .iter()
        .map(|rule| rule.level)
        .max()
        .unwrap_or(RiskLevel::Unknown);
    rules.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.id.cmp(&b.id)));
    Ok(RiskEvaluation {
        model: MODEL.into(),
        profile: profile.clone(),
        rating,
        evidence_complete,
        rules,
        facts,
    })
}

/// Conventional test/support classification, not evidence that tests ran or
/// that these files cover production behavior.
pub fn test_support(path: &str, symbol: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or("");
    let stem = file.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(file);
    let scope = symbol
        .rsplit_once('#')
        .map(|(_, name)| name)
        .unwrap_or(symbol)
        .split("::")
        .collect::<Vec<_>>();
    parts.iter().any(|part| {
        matches!(
            *part,
            "tests"
                | "test"
                | "__tests__"
                | "fixtures"
                | "testdata"
                | "examples"
                | "benches"
                | "benchmarks"
        )
    }) || matches!(stem, "test" | "tests")
        || file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.contains("_test.")
        || scope
            .iter()
            .take(scope.len().saturating_sub(1))
            .any(|part| matches!(*part, "tests" | "test"))
}

/// Stable conventional component grouping; explicit package roots are not
/// inferred from compiler/build configuration.
pub fn component(path: &str) -> String {
    let mut parts = path.split('/');
    match parts.next().unwrap_or("") {
        "crates" | "packages" | "services" | "apps" => {
            path.split('/').take(2).collect::<Vec<_>>().join("/")
        }
        "src" | "lib" | "include" | "" => "<root>".into(),
        first if path.contains('/') => first.into(),
        _ => "<root>".into(),
    }
}

#[cfg(test)]
mod tests;
