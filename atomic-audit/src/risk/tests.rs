use super::*;

fn localized() -> RiskFacts {
    RiskFacts {
        available: true,
        production_functions: 1,
        existing_production_functions: 1,
        production_paths: vec!["src/lib.rs".into()],
        components: vec!["<root>".into()],
        public_growth: Some(0),
        public_removed: Some(0),
        added_file_public_units: Some(0),
        dependency_growth: Some(0),
        source_selection_complete: Some(true),
        unresolved_references: Some(0),
        surface_measurements_complete: true,
        references: BTreeMap::from([("change".into(), vec!["selected:src/lib.rs".into()])]),
        ..Default::default()
    }
}

#[test]
fn known_localized_change_can_be_low_without_a_bug_or_probability_claim() {
    let result = assess(localized(), &RiskProfile::default()).unwrap();
    assert_eq!(result.rating, RiskLevel::Low);
    assert!(result.evidence_complete);
    assert_eq!(result.rules[0].id, "risk.localized-change");
}

#[test]
fn unavailable_facts_are_not_assumed_zero_risk() {
    assert_eq!(
        assess(RiskFacts::default(), &RiskProfile::default())
            .unwrap()
            .rating,
        RiskLevel::Unknown
    );
    let mut facts = localized();
    facts.public_growth = None;
    facts.surface_measurements_complete = false;
    assert_eq!(
        assess(facts, &RiskProfile::default()).unwrap().rating,
        RiskLevel::Unknown
    );
}

#[test]
fn fqn_recorded_summary_is_high_despite_no_contract_failure_and_partial_coverage() {
    // Measured FQNWHCPNXXO5 summary; test/helper functions have been excluded.
    let mut facts = localized();
    facts.production_functions = 53;
    facts.existing_production_functions = 4;
    facts.test_support_functions = 23;
    facts.production_paths = [
        "atomic-audit/src/extract/languages.rs",
        "atomic-audit/src/extract/mod.rs",
        "atomic-audit/src/extract/tokens.rs",
        "atomic-audit/src/metrics.rs",
        "atomic-cli/src/commands/review/analysis.rs",
        "atomic-cli/src/commands/review/graph.rs",
        "atomic-canonical/src/review_finding.rs",
        "atomic-semantic/src/lib.rs",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    facts.components = [
        "atomic-audit",
        "atomic-cli",
        "atomic-canonical",
        "atomic-semantic",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    facts.observed_production_callers = 31;
    facts.observed_test_support_callers = 22;
    facts.public_growth = Some(19);
    facts.added_file_public_units = Some(24);
    facts.unresolved_references = Some(1328);
    let result = assess(facts, &RiskProfile::default()).unwrap();
    assert_eq!(result.rating, RiskLevel::High);
    assert!(!result.evidence_complete);
    assert!(result
        .rules
        .iter()
        .any(|rule| rule.id == "risk.cross-component-api-churn"));
    assert!(result
        .rules
        .iter()
        .any(|rule| rule.id == "risk.broad-production-change"));
}

#[test]
fn replay_actual_optimized_fqn_capture() {
    let facts: RiskFacts = serde_json::from_str(include_str!("fqn_facts.json")).unwrap();
    let result = assess(facts, &RiskProfile::default()).unwrap();
    assert_eq!(result.rating, RiskLevel::High);
    assert!(!result.evidence_complete);
    assert_eq!(result.facts.production_functions, 53);
    assert_eq!(result.facts.observed_production_callers, 31);
    assert_eq!(result.facts.unresolved_references, Some(1328));
    assert_eq!(
        result
            .rules
            .iter()
            .filter(|rule| rule.level == RiskLevel::High)
            .map(|rule| rule.id.as_str())
            .collect::<Vec<_>>(),
        [
            "risk.broad-production-change",
            "risk.cross-component-api-churn"
        ]
    );
    assert!(result.rules.iter().all(|rule| !rule.references.is_empty()));
}

#[test]
fn unresolved_names_never_inflate_callers_or_severity() {
    let mut facts = localized();
    facts.unresolved_references = Some(100_000);
    let result = assess(facts, &RiskProfile::default()).unwrap();
    assert_eq!(result.rating, RiskLevel::Unknown);
    assert!(!result.rules.iter().any(|rule| rule.id.contains("consumer")));
    assert_eq!(result.facts.observed_production_callers, 0);
}

#[test]
fn broad_test_support_changes_do_not_become_production_exposure() {
    let mut facts = localized();
    facts.production_functions = 0;
    facts.existing_production_functions = 0;
    facts.production_paths.clear();
    facts.components.clear();
    facts.test_support_functions = 500;
    facts.observed_test_support_callers = 1000;
    assert_eq!(
        assess(facts, &RiskProfile::default()).unwrap().rating,
        RiskLevel::Low
    );
    for path in [
        "src/extract/tests.rs",
        "test/client.mjs",
        "examples/demo.rs",
        "src/a.test.ts",
    ] {
        assert!(test_support(path, ""), "{path}");
    }
    assert!(test_support("src/lib.rs", "src/lib.rs#tests::helper"));
    assert!(!test_support("src/lib.rs", "src/lib.rs#test_connection"));
}

#[test]
fn integration_threshold_is_inclusive_and_profile_is_explicit() {
    let mut facts = localized();
    facts.production_functions = 20;
    facts.public_growth = Some(10);
    facts.components = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(
        assess(facts.clone(), &RiskProfile::default())
            .unwrap()
            .rating,
        RiskLevel::High
    );
    facts.production_functions = 19;
    assert_eq!(
        assess(facts.clone(), &RiskProfile::default())
            .unwrap()
            .rating,
        RiskLevel::Medium
    );
    let profile = RiskProfile {
        api_growth: 11,
        ..Default::default()
    };
    facts.production_functions = 20;
    let result = assess(facts, &profile).unwrap();
    assert_eq!(result.rating, RiskLevel::Medium);
    assert_eq!(result.profile.api_growth, 11);
}

#[test]
fn propagation_and_api_removal_need_no_invented_defect() {
    let mut facts = localized();
    facts.existing_production_callers = 25;
    facts.existing_caller_files = 5;
    assert_eq!(
        assess(facts, &RiskProfile::default()).unwrap().rating,
        RiskLevel::High
    );
    let mut facts = localized();
    facts.public_removed = Some(1);
    assert_eq!(
        assess(facts, &RiskProfile::default()).unwrap().rating,
        RiskLevel::Medium
    );
    let mut facts = localized();
    facts.structural_alerts = 1;
    assert_eq!(
        assess(facts, &RiskProfile::default()).unwrap().rating,
        RiskLevel::Medium
    );
}

#[test]
fn invalid_profiles_fail_closed_and_defaults_deserialize() {
    for profile in [
        RiskProfile {
            name: "".into(),
            ..Default::default()
        },
        RiskProfile {
            api_growth: 0,
            ..Default::default()
        },
        RiskProfile {
            broad_functions: 1,
            ..Default::default()
        },
    ] {
        assert!(profile.validate().is_err());
    }
    assert!(serde_json::from_str::<RiskProfile>(r#"{"typo":1}"#).is_err());
    let profile: RiskProfile = serde_json::from_str(r#"{"name":"team","api_growth":30}"#).unwrap();
    assert_eq!(profile.api_growth, 30);
    assert_eq!(profile.broad_functions, 50);
}
