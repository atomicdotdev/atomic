use super::*;
use atomic_semantic::ParserRegistry;

fn sources(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(path, source)| ((*path).into(), (*source).into()))
        .collect()
}

fn compare(before: &[(&str, &str)], after: &[(&str, &str)], paths: &[&str]) -> ImpactReport {
    analyze(
        &sources(before),
        &sources(after),
        &paths.iter().map(|path| (*path).into()).collect::<Vec<_>>(),
        &mut ParserRegistry::new(),
    )
}

#[test]
fn rust_direct_calls_measure_real_contract_regression_not_shared_names() {
    let report = compare(
        &[
            (
                "src/lib.rs",
                "fn work(x: i32) {} fn consumer() { work(1); }",
            ),
            ("other.rs", "fn work() {} fn unrelated() { work(); }"),
        ],
        &[
            (
                "src/lib.rs",
                "fn work(x: i32, y: i32) {} fn consumer() { work(1); }",
            ),
            ("other.rs", "fn work() {} fn unrelated() { work(); }"),
        ],
        &["src/lib.rs"],
    );
    assert_eq!(report.changed_functions.len(), 1);
    assert_eq!(report.callers_after.len(), 1);
    assert!(report.callers_after[0].caller.ends_with("#consumer"));
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.checked_call_contracts, 1);
    assert_eq!(report.issues[0].call.arguments, Some(1));
}

#[test]
fn rust_declared_modules_aliases_and_visibility_resolve_without_global_names() {
    let before = [
        ("src/lib.rs", "mod api; mod user;"),
        ("src/api.rs", "pub fn work(x: i32) {} fn private() {}"),
        (
            "src/user.rs",
            "use crate::api::work as invoke; fn consumer() { invoke(1); crate::api::private(); }",
        ),
    ];
    let after = [
        ("src/lib.rs", "mod api; mod user;"),
        (
            "src/api.rs",
            "pub fn work(x: i32, y: i32) {} fn private() {}",
        ),
        (
            "src/user.rs",
            "use crate::api::work as invoke; fn consumer() { invoke(1); crate::api::private(); }",
        ),
    ];
    let report = compare(&before, &after, &["src/api.rs"]);
    assert_eq!(
        report.callers_after.len(),
        1,
        "private sibling functions are not accessible callers"
    );
    assert_eq!(report.issues.len(), 1);
    assert!(report.callers_after[0]
        .binding
        .contains("Rust import/module"));
}

#[test]
fn rust_inline_modules_nested_imports_and_private_module_boundaries() {
    let old = "mod api { pub fn work(x:i32) {} mod hidden { pub fn secret() {} } } mod user { use crate::api::{work as invoke}; fn consumer() { invoke(1); crate::api::hidden::secret(); } }";
    let new = old.replace("work(x:i32)", "work(x:i32, y:i32)");
    let report = compare(
        &[("src/lib.rs", old)],
        &[("src/lib.rs", &new)],
        &["src/lib.rs"],
    );
    assert_eq!(report.callers_after.len(), 1);
    assert_eq!(report.issues.len(), 1);
}

#[test]
fn rust_parent_visibility_and_explicit_module_aliases_bind_to_the_declared_parent() {
    let old = "mod parent { mod api { pub(super) fn work(x:i32) {} } use self::api as implementation; fn consumer() { implementation::work(1); } } mod unrelated { fn work() {} }";
    let new = old.replace("work(x:i32)", "work(x:i32,y:i32)");
    let report = compare(
        &[("src/lib.rs", old)],
        &[("src/lib.rs", &new)],
        &["src/lib.rs"],
    );
    assert_eq!(report.callers_after.len(), 1);
    assert_eq!(report.issues.len(), 1);
}

#[test]
fn typescript_explicit_relative_alias_and_namespace_calls_bind_exported_functions() {
    for import in [
        "import { work as invoke } from './api'; function consumer() { invoke(1); }",
        "import * as api from './api'; function consumer() { api.work(1); }",
    ] {
        let report = compare(
            &[
                ("src/api.ts", "export function work(x: number) {}"),
                ("src/user.ts", import),
            ],
            &[
                (
                    "src/api.ts",
                    "export function work(x: number, y: number) {}",
                ),
                ("src/user.ts", import),
            ],
            &["src/api.ts"],
        );
        assert_eq!(report.callers_after.len(), 1);
        assert_eq!(report.issues.len(), 1);
    }
}

#[test]
fn removed_declarations_keep_before_callers_and_detect_surviving_broken_calls() {
    let report = compare(
        &[
            ("api.ts", "export function work() {}"),
            (
                "user.ts",
                "import {work} from './api'; function consumer() { work(); }",
            ),
        ],
        &[(
            "user.ts",
            "import {work} from './api'; function consumer() { work(); }",
        )],
        &["api.ts"],
    );
    assert_eq!(report.changed_functions[0].change, "removed");
    assert_eq!(report.callers_before.len(), 1);
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].after_declaration.is_none());
}

#[test]
fn shadowed_dynamic_and_overloaded_calls_are_not_guessed() {
    for source in [
        "fn work(x:i32) {} fn consumer(work: fn(i32)) { work(1); }",
        "fn work(x:i32) {} fn consumer() { let work = other; work(1); }",
        "fn work(x:i32) {} fn consumer() { object.work(1); }",
    ] {
        let new = source.replace("work(x:i32)", "work(x:i32,y:i32)");
        let report = compare(
            &[("src/lib.rs", source)],
            &[("src/lib.rs", &new)],
            &["src/lib.rs"],
        );
        assert!(report.callers_after.is_empty(), "{source}");
        assert!(report.issues.is_empty(), "{source}");
        assert!(!report.gaps.is_empty(), "{source}");
    }
    let old = "function work(x: number): void; function work(x: number) {} function consumer() { work(1); }";
    let new = old.replace(
        "function work(x: number) {}",
        "function work(x: number, y: number) {}",
    );
    let report = compare(&[("api.ts", old)], &[("api.ts", &new)], &["api.ts"]);
    assert!(report.issues.is_empty());
}

#[test]
fn comments_unchanged_functions_and_repaired_consumers_do_not_inflate_risk() {
    let old = "fn work(x:i32) {} fn untouched() {} fn consumer() { work(1); }";
    let comment = format!("// comment\n{old}");
    let report = compare(&[("lib.rs", old)], &[("lib.rs", &comment)], &["lib.rs"]);
    assert!(report.changed_functions.is_empty() && report.issues.is_empty());
    let new = "fn work(x:i32,y:i32) {} fn untouched() {} fn consumer() { work(1,2); }";
    let report = compare(&[("lib.rs", old)], &[("lib.rs", new)], &["lib.rs"]);
    assert_eq!(report.changed_functions.len(), 2);
    assert!(report.issues.is_empty());
    assert_eq!(report.callees_after.len(), 1);
}

#[test]
fn optional_default_and_rest_parameters_do_not_create_fake_arity_breaks() {
    for parameters in [
        "x: number, y?: number",
        "x: number, y = 2",
        "x: number, ...rest: number[]",
    ] {
        let new = format!("function work({parameters}) {{}} function consumer() {{ work(1); }}");
        let report = compare(
            &[(
                "api.ts",
                "function work(x:number) {} function consumer() { work(1); }",
            )],
            &[("api.ts", &new)],
            &["api.ts"],
        );
        assert!(report.issues.is_empty(), "{parameters}");
        assert_eq!(report.compatible_call_contracts, 1, "{parameters}");
    }
}

#[test]
fn missing_syntax_or_replaced_binding_is_not_a_proven_deletion() {
    for after in [
        "fn work( {",
        "#[cfg(feature=\"x\")] fn work() {} fn consumer() { work(); }",
        "const work: fn() = other; fn consumer() { work(); }",
    ] {
        let report = compare(
            &[("lib.rs", "fn work() {} fn consumer() { work(); }")],
            &[("lib.rs", after)],
            &["lib.rs"],
        );
        assert!(report.issues.is_empty(), "{after}");
        assert!(!report.gaps.is_empty());
    }
}

#[test]
fn changed_and_added_callers_are_checked_against_unchanged_or_new_contracts() {
    let report = compare(
        &[("lib.rs", "fn work(x:i32) {} fn caller() { work(1); }")],
        &[("lib.rs", "fn work(x:i32) {} fn caller() { work(); }")],
        &["lib.rs"],
    );
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].introduced);
    assert!(!report.issues[0].primary_before);
    assert_eq!(
        report.contract_assessment,
        ContractAssessment::IncompatibleResolvedCalls
    );
    let report = compare(
        &[],
        &[("lib.rs", "fn work(x:i32) {} fn caller() { work(); }")],
        &["lib.rs"],
    );
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].before_declaration.is_none());
    assert!(report.issues[0].introduced);
}

#[test]
fn preexisting_incompatibility_is_not_attributed_to_an_unrelated_body_edit() {
    let report = compare(
        &[("lib.rs", "fn work(x:i32) {} fn caller() { work(); }")],
        &[(
            "lib.rs",
            "fn work(x:i32) {} fn caller() { work(); let x = 1; }",
        )],
        &["lib.rs"],
    );
    assert!(report.issues.is_empty());
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.contains("already existed")));
    assert_eq!(report.preexisting_incompatible_contracts, 1);
    assert_eq!(
        report.contract_assessment,
        ContractAssessment::IncompatibleResolvedCalls
    );
}

#[test]
fn truncated_snapshots_withhold_cross_file_bindings_instead_of_inventing_uniqueness() {
    let before = sources(&[
        ("api.ts", "export function work(x:number) {}"),
        (
            "caller.ts",
            "import {work} from './api'; function caller() { work(1); }",
        ),
    ]);
    let after = sources(&[
        ("api.ts", "export function work(x:number,y:number) {}"),
        (
            "caller.ts",
            "import {work} from './api'; function caller() { work(1); }",
        ),
    ]);
    let report = analyze_with_coverage(
        &before,
        &after,
        &["api.ts".into()],
        false,
        &mut ParserRegistry::new(),
    );
    assert!(report.callers_after.is_empty() && report.issues.is_empty());
    assert!(!report.gaps.is_empty());
}

#[test]
fn unresolved_module_alternatives_and_type_only_imports_are_not_real_callers() {
    let old = [
        ("api.ts", "export function work(x:number) {}"),
        ("api.tsx", "broken {{{"),
        (
            "caller.ts",
            "import {work} from './api'; function caller() { work(1); }",
        ),
    ];
    let new = [
        ("api.ts", "export function work(x:number,y:number) {}"),
        ("api.tsx", "broken {{{"),
        (
            "caller.ts",
            "import {work} from './api'; function caller() { work(1); }",
        ),
    ];
    assert!(compare(&old, &new, &["api.ts"]).issues.is_empty());
    let report = compare(
        &[
            ("api.ts", "export function work(x:number) {}"),
            (
                "caller.ts",
                "import type {work} from './api'; function caller() { work(1); }",
            ),
        ],
        &[
            ("api.ts", "export function work(x:number,y:number) {}"),
            (
                "caller.ts",
                "import type {work} from './api'; function caller() { work(1); }",
            ),
        ],
        &["api.ts"],
    );
    assert!(report.callers_after.is_empty());
}

#[test]
fn mixed_global_and_function_edits_leave_an_explicit_implementation_gap() {
    let report = compare(
        &[(
            "lib.rs",
            "const VALUE:i32=1; fn work() -> i32 { VALUE } fn other() {}",
        )],
        &[(
            "lib.rs",
            "const VALUE:i32=2; fn work() -> i32 { VALUE } fn other() { let x = 1; }",
        )],
        &["lib.rs"],
    );
    assert_eq!(report.changed_functions.len(), 1);
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.contains("imports/types/globals")));
}

#[test]
fn impact_budgets_and_duplicate_paths_are_explicit_not_clean_results() {
    let large = " ".repeat(atomic_semantic::MAX_SOURCE_BYTES + 1);
    let report = compare(&[], &[("lib.rs", &large)], &["lib.rs"]);
    assert!(report.issues.is_empty() && report.changed_functions.is_empty());
    assert!(report.gaps.iter().any(|gap| gap.contains("byte limit")));
    let duplicate = sources(&[("lib.rs", "fn work() {}"), ("lib.rs", "fn work(x:i32) {}")]);
    let report = analyze(
        &[],
        &duplicate,
        &["lib.rs".into()],
        &mut ParserRegistry::new(),
    );
    assert!(report.issues.is_empty());
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.contains("Duplicate semantic source paths")));
}

#[test]
fn shared_semantic_facts_keep_normal_macros_and_cfg_declarations() {
    for source in [
        "fn work(x:i32) {} fn consumer() { println!(\"hello\"); work(1); }",
        "#[cfg(feature=\"optional\")] fn work(x:i32) {} fn consumer() { work(1); }",
        "#![cfg(feature=\"optional\")] fn work(x:i32) {} fn consumer() { work(1); }",
    ] {
        let new = source.replace("work(x:i32)", "work(x:i32,y:i32)");
        let report = compare(
            &[("src/lib.rs", source)],
            &[("src/lib.rs", &new)],
            &["src/lib.rs"],
        );
        assert_eq!(report.callers_after.len(), 1, "{source}");
        assert!(report.callers_after[0].configuration_dependent, "{source}");
        assert_eq!(report.issues.len(), 1);
    }
    let source = "fn work(#[cfg(feature=\"optional\")] x:i32) {} fn consumer() { work(1); }";
    let report = compare(&[], &[("src/lib.rs", source)], &["src/lib.rs"]);
    assert_eq!(report.callers_after.len(), 1);
    assert_eq!(report.checked_call_contracts, 0);
    assert!(report.issues.is_empty());
}

#[test]
fn accessor_return_values_do_not_inherit_the_accessors_argument_count() {
    let before = "class Reader { get value() { return (x:number) => x; } } function consumer(reader:Reader) { return reader.value(1); }";
    let after = "class Reader { get value() { return (x:number,y:number) => x+y; } } function consumer(reader:Reader) { return reader.value(1); }";
    let report = compare(&[("app.ts", before)], &[("app.ts", after)], &["app.ts"]);
    assert!(
        report.issues.is_empty(),
        "getter arity is not the returned function's contract"
    );
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.contains("unavailable argument-count conventions")));
}

#[test]
fn lost_exports_and_private_visibility_are_concrete_contract_regressions() {
    let report = compare(
        &[
            ("api.ts", "export function work() {}"),
            (
                "caller.ts",
                "import {work} from './api'; function consumer() { work(); }",
            ),
        ],
        &[
            ("api.ts", "function work() {}"),
            (
                "caller.ts",
                "import {work} from './api'; function consumer() { work(); }",
            ),
        ],
        &["api.ts"],
    );
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].introduced);
    assert!(!report.callers_after[0].visibility_compatible);
    assert!(report.issues[0].summary.contains("visibility/export"));
    let old = "mod api { pub fn work() {} } mod consumer { fn run() { crate::api::work(); } }";
    let new = old.replace("pub fn work", "fn work");
    let report = compare(&[("lib.rs", old)], &[("lib.rs", &new)], &["lib.rs"]);
    assert_eq!(report.issues.len(), 1);
    assert!(!report.callers_after[0].visibility_compatible);
}

#[test]
fn unresolved_export_expansion_is_not_misreported_as_private_access_failure() {
    let old = "function work(x:number) {} export {work};";
    let new = "function work(x:number,y:number) {} export {work};";
    let caller = "import {work} from './api'; function consumer() { work(1); }";
    let report = compare(
        &[("api.ts", old), ("caller.ts", caller)],
        &[("api.ts", new), ("caller.ts", caller)],
        &["api.ts"],
    );
    assert!(report.issues.is_empty() && report.callers_after.is_empty());
    assert!(!report.gaps.is_empty());
}
