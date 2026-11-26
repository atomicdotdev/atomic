use super::*;

fn run(changes: Vec<FileChange>) -> Extraction {
    extract_changes(
        &changes,
        &mut ParserRegistry::new(),
        &ExtractionConfig::default(),
    )
    .unwrap()
}

fn added(path: &str, source: &str) -> FileChange {
    FileChange {
        path: path.into(),
        before: Source::Absent,
        after: Source::Text(source.into()),
    }
}

fn value(result: &Extraction, metric: Metric, name: &str) -> Measurement {
    result
        .metrics
        .iter()
        .find(|record| {
            record.observation.metric == metric && record.observation.subject.contains(name)
        })
        .unwrap_or_else(|| panic!("missing {metric:?}/{name}: {:#?}", result.metrics))
        .observation
        .after
        .clone()
}

#[test]
fn all_ten_metrics_have_automatic_results_across_registered_languages() {
    for (path, source, classes) in [
        ("lib.rs", "use std::fs;\npub fn work(x: i32) -> i32 { if x > 0 { x } else { 0 } }", false),
        ("app.ts", "import { x } from './dep';\nexport class C { run(x: number) { if (x > 0) { return x; } return 0; } }", true),
        ("app.tsx", "import { x } from './dep';\nexport class C { run(x: number) { if (x > 0) { return <div />; } return null; } }", true),
        ("app.py", "from pathlib import Path\nclass C:\n    def run(self, x):\n        if x > 0:\n            return x\n        return 0\n", true),
        ("C.java", "import java.util.List;\npublic class C { public int run(int x) { if (x > 0) { return x; } return 0; } }", true),
        ("app.go", "package p\nimport \"fmt\"\nfunc Work(x int) int { if x > 0 { return x }; return 0 }", false),
        ("app.c", "#include <stdio.h>\nint work(int x) { if (x > 0) { return x; } return 0; }", false),
        ("app.cpp", "#include <vector>\nclass C { public: int run(int x) { if (x > 0) { return x; } return 0; } };", true),
        ("app.swift", "import Foundation\npublic class C { public func run(_ x: Int) -> Int { if x > 0 { return x }; return 0 } }", true),
    ] {
        let result = run(vec![added(path, source)]);
        assert!(result.diagnostics.is_empty(), "{path}: {:?}", result.diagnostics);
        assert_eq!(result.metrics.iter().map(|record| record.observation.metric).collect::<BTreeSet<_>>(), Metric::ALL.into_iter().collect(), "{path}");
        for metric in [Metric::MethodLength, Metric::ParameterCount, Metric::NestingDepth, Metric::CyclomaticComplexity, Metric::DependencyFanOut, Metric::PublicSurface, Metric::DuplicatedTokens] {
            assert!(result.metrics.iter().any(|record| record.observation.metric == metric && matches!(record.observation.after, Measurement::Measured { .. })), "{path}: {metric:?}: {:#?}", result.metrics);
        }
        assert!(result.metrics.iter().filter(|record| record.observation.metric == Metric::ParameterCount).all(|record| record.observation.after == measured(1)), "{path}");
        assert!(result.metrics.iter().filter(|record| record.observation.metric == Metric::CyclomaticComplexity).all(|record| record.observation.after == measured(2)), "{path}");
        if classes {
            assert!(result.metrics.iter().filter(|record| record.observation.metric == Metric::MethodCount).all(|record| record.observation.after == measured(1)), "{path}");
            assert!(result.metrics.iter().filter(|record| record.observation.metric == Metric::ClassComplexity).all(|record| record.observation.after == measured(2)), "{path}");
        }
        assert!(result.metrics.iter().filter(|record| [Metric::ClassLength, Metric::MethodCount, Metric::ClassComplexity].contains(&record.observation.metric)).all(|record| {
            if classes { matches!(record.observation.after, Measurement::Measured { .. }) } else { matches!(record.observation.after, Measurement::NotApplicable { .. }) }
        }), "{path}: {:#?}", result.metrics);
    }
}

#[test]
fn method_and_class_lengths_exclude_comments_and_nested_bodies() {
    let source = "class Outer:\n    # comment\n    class Inner:\n        def nested(self):\n            return 1\n    def run(self, x):\n        # if x and bogus\n        text = 'if x and bogus'\n        def local(y):\n            if y:\n                return 1\n            return 0\n        if x:\n            return x\n        return 0\n";
    let result = run(vec![added("app.py", source)]);
    assert_eq!(
        value(&result, Metric::MethodCount, "class:Outer:1"),
        measured(1)
    );
    assert_eq!(
        value(&result, Metric::MethodCount, "class:Outer::Inner:1"),
        measured(1)
    );
    assert_eq!(
        value(
            &result,
            Metric::CyclomaticComplexity,
            "function:Outer::run:1"
        ),
        measured(2)
    );
    assert_eq!(
        value(&result, Metric::ParameterCount, "function:Outer::run:1"),
        measured(1)
    );
    assert_eq!(
        value(&result, Metric::NestingDepth, "function:Outer::run:1"),
        measured(1)
    );
    assert_eq!(
        value(&result, Metric::MethodLength, "function:Outer::run:1"),
        measured(5)
    );
    assert_eq!(
        value(&result, Metric::ClassLength, "class:Outer:1"),
        measured(10)
    );
    assert_eq!(
        value(&result, Metric::ClassComplexity, "class:Outer:1"),
        measured(2)
    );
}

#[test]
fn parameter_conventions_cover_receivers_grouping_destructuring_and_defaults() {
    let typed_self = run(vec![added(
        "lib.rs",
        "struct S; impl S { fn work(self: Box<Self>, x: i32) {} }",
    )]);
    assert_eq!(
        value(&typed_self, Metric::ParameterCount, "work"),
        measured(1)
    );
    for (path, source, expected) in [
        (
            "app.go",
            "package p\nfunc work(a, b int, rest ...string) {}",
            3,
        ),
        (
            "lib.rs",
            "struct S; impl S { fn work(&self, x: i32, y: i32) {} }",
            2,
        ),
        (
            "app.ts",
            "export const work = (thisValue: number, { a, b }: any, rest = 1) => rest;",
            3,
        ),
        (
            "app.py",
            "class C:\n    @staticmethod\n    def work(self, x):\n        pass\n",
            2,
        ),
        (
            "app.py",
            "class C:\n    @classmethod\n    def work(cls, x, *, y=1):\n        pass\n",
            2,
        ),
        ("app.c", "int work(void) { return 0; }", 0),
    ] {
        let result = run(vec![added(path, source)]);
        assert_eq!(
            value(&result, Metric::ParameterCount, "work"),
            measured(expected),
            "{path}: {source}"
        );
    }
}

#[test]
fn named_subjects_pair_across_line_shifts_and_changed_signatures() {
    let result = run(vec![FileChange {
        path: "app.ts".into(),
        before: Source::Text("export function work(a: number) { return a; }".into()),
        after: Source::Text(
            "// shifted\nexport function work(a: number, b: number) {\n return a + b;\n}".into(),
        ),
    }]);
    let record = result
        .metrics
        .iter()
        .find(|record| {
            record.observation.metric == Metric::ParameterCount
                && record.observation.subject.contains("work")
        })
        .unwrap();
    assert_eq!(record.observation.before, measured(1));
    assert_eq!(record.observation.after, measured(2));
    assert_eq!(record.before_location.as_ref().unwrap().start_line, 1);
    assert_eq!(record.after_location.as_ref().unwrap().start_line, 2);
}

#[test]
fn additions_deletions_parse_failures_and_real_zero_are_distinct() {
    let mut change = added("lib.rs", "pub fn work() {}\n");
    let added_result = run(vec![change.clone()]);
    assert!(added_result
        .metrics
        .iter()
        .filter(|record| record.observation.subject.contains("work"))
        .all(|record| matches!(record.observation.before, Measurement::NotApplicable { .. })));
    change.before = change.after;
    change.after = Source::Absent;
    let deleted = run(vec![change.clone()]);
    assert!(deleted
        .metrics
        .iter()
        .filter(|record| record.observation.subject.contains("work"))
        .all(|record| matches!(record.observation.after, Measurement::NotApplicable { .. })));
    change.after = Source::Text("fn broken( {".into());
    let broken = run(vec![change]);
    assert!(broken
        .metrics
        .iter()
        .all(|record| matches!(record.observation.after, Measurement::Unavailable { .. })));
    let empty = run(vec![added("lib.rs", "")]);
    assert_eq!(value(&empty, Metric::PublicSurface, "lib.rs"), measured(0));
    assert_eq!(
        value(&empty, Metric::DependencyFanOut, "lib.rs"),
        measured(0)
    );
    assert!(run(vec![added("readme.md", "text")])
        .metrics
        .iter()
        .all(|record| matches!(record.observation.after, Measurement::Unavailable { .. })));
}

#[test]
fn exact_token_clones_count_changed_positions_once_and_ignore_formatting() {
    let source = "pub fn work(x:i32)->i32 { let total=x+1; let result=total*2; return result; }";
    let changes = vec![
        FileChange {
            path: "a.rs".into(),
            before: Source::Text(source.into()),
            after: Source::Text(format!("// comment\n{source}")),
        },
        added("b.rs", &source.replace("work", "copy")),
    ];
    let config = ExtractionConfig {
        minimum_clone_tokens: 8,
        ..Default::default()
    };
    let result = extract_changes(&changes, &mut ParserRegistry::new(), &config).unwrap();
    assert_eq!(
        value(&result, Metric::DuplicatedTokens, "a.rs"),
        measured(0)
    );
    match value(&result, Metric::DuplicatedTokens, "b.rs") {
        Measurement::Measured { value } => assert!((8..50).contains(&value)),
        other => panic!("{other:?}"),
    }
    assert_eq!(result.comparison_paths, vec!["a.rs", "b.rs"]);
}

#[test]
fn incomplete_comparison_and_work_caps_are_not_clean_clone_results() {
    let changes = vec![added("a.rs", "fn a() {}"), added("b.rs", "fn b() {}")];
    let config = ExtractionConfig {
        max_files: 1,
        ..Default::default()
    };
    let result = extract_changes(&changes, &mut ParserRegistry::new(), &config).unwrap();
    assert!(matches!(
        value(&result, Metric::DuplicatedTokens, "a.rs"),
        Measurement::Unavailable { .. }
    ));
    let changes = vec![FileChange {
        path: "a.rs".into(),
        before: Source::Text("fn a() { let x=1; }".into()),
        after: Source::Text("fn a() { let x=2; }".into()),
    }];
    let config = ExtractionConfig {
        max_diff_work: 0,
        ..Default::default()
    };
    let result = extract_changes(&changes, &mut ParserRegistry::new(), &config).unwrap();
    assert!(matches!(
        value(&result, Metric::DuplicatedTokens, "a.rs"),
        Measurement::Unavailable { .. }
    ));
    assert!(matches!(
        value(&result, Metric::MethodLength, "a"),
        Measurement::Measured { .. }
    ));
}

#[test]
fn dynamic_exports_and_header_macros_are_explicitly_unavailable() {
    for (path, source, metric) in [
        (
            "app.ts",
            "export * from './unknown';",
            Metric::PublicSurface,
        ),
        (
            "app.py",
            "__all__ = build_exports()\n",
            Metric::PublicSurface,
        ),
        (
            "app.c",
            "#include HEADER\nint work(void) { return 0; }",
            Metric::DependencyFanOut,
        ),
        (
            "app.cpp",
            "class C { public: void work(); };",
            Metric::ClassComplexity,
        ),
    ] {
        let result = run(vec![added(path, source)]);
        assert!(
            result
                .metrics
                .iter()
                .filter(|record| record.observation.metric == metric)
                .all(|record| matches!(record.observation.after, Measurement::Unavailable { .. })),
            "{path}: {:#?}",
            result.metrics
        );
    }
}

#[test]
fn public_surface_uses_visibility_bindings_and_static_export_lists() {
    for source in [
        "__all__ = ['C']\n__all__.append('D')\n",
        "__all__ = ['C']\n__all__[0] = 'D'\n",
        "__all__ = [r'C']\n",
    ] {
        assert!(matches!(
            value(
                &run(vec![added("app.py", source)]),
                Metric::PublicSurface,
                "app.py"
            ),
            Measurement::Unavailable { .. }
        ));
    }
    for (path, source, expected) in [
        ("app.py", "__all__ = ['C']\nclass C:\n    def run(self): pass\nclass Hidden:\n    def secret(self): pass\n", 2),
        ("app.py", "from pathlib import Path\na, b = 1, 2\ndef exported(): pass\ndef _hidden(): pass\n", 4),
        ("app.ts", "export class C { public f() {} private h() {} protected p() {} }", 3),
        ("app.ts", "export const {a, b: renamed} = {a: 1, b: 2}; export { a }; const hidden = 1;", 2),
        ("app.ts", "export class C { constructor(public x: number, private y: number, readonly z: number) {} }", 4),
        ("C.java", "public class C { public void f() {} private void h() {} private class Hidden { public void g() {} } }", 2),
        ("app.cpp", "class C { public: void f() {} private: void h() {} class Hidden { public: void g() {} }; };", 2),
        ("app.c", "static int hidden(void) { return 1; } int shown(void) { return 2; }", 1),
        ("app.go", "package p\nvar Exported, hidden int\nfunc Shown() {}\nfunc hiddenFunc() {}", 2),
        ("lib.rs", "struct Hidden; impl Hidden { pub fn h() {} } pub struct V { pub x:u32, y:u32 } impl V { pub fn f(&self) {} fn h(&self) {} } pub enum E { A, B }", 6),
        ("lib.rs", "pub trait T { fn work(&self); } mod hidden { pub fn work() {} }", 2),
        ("lib.rs", "pub use crate::things::{A, B};", 2),
        ("lib.rs", "pub const NAME: &str = env!(\"NAME\");", 1),
        ("lib.rs", "pub(crate) fn internal() {} pub(self) struct Hidden; pub fn shown() {}", 1),
    ] {
        let result = run(vec![added(path, source)]);
        assert_eq!(value(&result, Metric::PublicSurface, path), measured(expected), "{path}: {source}");
    }
}

#[test]
fn declared_dependency_units_are_deduplicated_not_import_statement_counts() {
    for (path, source, expected) in [
        ("app.py", "import os\nimport os as renamed\nfrom pathlib import Path\nfrom pathlib import PurePath\n", 2),
        ("app.ts", "import {a} from './one'; import {b} from './one'; export {c} from './two';", 2),
        ("app.go", "package p\nimport (\"fmt\"; f \"fmt\"; \"io\")", 2),
        ("lib.rs", "use {std::fs, serde::Serialize}; use std::{io, fmt};", 2),
        ("app.c", "#include <stdio.h>\n#include <stdio.h>\n#include \"local.h\"\n", 2),
    ] {
        let result = run(vec![added(path, source)]);
        assert_eq!(value(&result, Metric::DependencyFanOut, path), measured(expected), "{path}: {source}");
    }
}

#[test]
fn overload_and_anonymous_identity_never_fabricates_a_baseline_delta() {
    let result = run(vec![FileChange {
        path: "C.java".into(),
        before: Source::Text(
            "public class C { void work(int x) {} void work(String x) {} }".into(),
        ),
        after: Source::Text("public class C { void work(String x) {} void work(int x) {} }".into()),
    }]);
    assert!(result
        .metrics
        .iter()
        .filter(|record| record.observation.subject.contains("function:C::work"))
        .all(|record| record.delta.is_none()
            && matches!(record.observation.before, Measurement::Unavailable { .. })));
    let changes = vec![added("same.rs", "fn a() {}"), added("same.rs", "fn b() {}")];
    assert!(extract_changes(
        &changes,
        &mut ParserRegistry::new(),
        &ExtractionConfig::default()
    )
    .is_err());
}

#[test]
fn declarations_default_arguments_and_function_pointer_fields_are_not_fake_bodies() {
    let result = run(vec![added("app.cpp", "class C { public: void missing(int x = 0); void (*callback)(int); virtual void abstract() = 0; };" )]);
    assert_eq!(
        value(&result, Metric::MethodCount, "class:C:1"),
        measured(2)
    );
    assert!(matches!(
        value(&result, Metric::ClassComplexity, "class:C:1"),
        Measurement::Unavailable { .. }
    ));
    assert!(matches!(
        value(&result, Metric::MethodLength, "missing"),
        Measurement::Unavailable { .. }
    ));
    assert!(matches!(
        value(&result, Metric::CyclomaticComplexity, "abstract"),
        Measurement::NotApplicable { .. }
    ));
    let result = run(vec![added("app.c", "int declared(int x);")]);
    assert_eq!(
        value(&result, Metric::ParameterCount, "declared"),
        measured(1)
    );
    assert!(matches!(
        value(&result, Metric::MethodLength, "declared"),
        Measurement::Unavailable { .. }
    ));
}

#[test]
fn real_zero_empty_class_and_empty_function_complexity_are_separate() {
    let result = run(vec![added(
        "app.ts",
        "export class Empty {}\nexport function work() {\n // comment\n}\n",
    )]);
    assert_eq!(
        value(&result, Metric::MethodCount, "class:Empty:1"),
        measured(0)
    );
    assert_eq!(
        value(&result, Metric::ClassComplexity, "class:Empty:1"),
        measured(0)
    );
    assert_eq!(
        value(&result, Metric::CyclomaticComplexity, "work"),
        measured(1)
    );
    assert_eq!(value(&result, Metric::NestingDepth, "work"), measured(0));
    assert_eq!(value(&result, Metric::MethodLength, "work"), measured(2));
}

#[test]
fn parser_and_subject_budgets_never_silently_produce_zero() {
    for config in [
        ExtractionConfig {
            max_total_bytes: 1,
            ..Default::default()
        },
        ExtractionConfig {
            max_total_subjects: 0,
            ..Default::default()
        },
    ] {
        let result = extract_changes(
            &[added("lib.rs", "fn work() {}")],
            &mut ParserRegistry::new(),
            &config,
        )
        .unwrap();
        assert!(result
            .metrics
            .iter()
            .all(|record| matches!(record.observation.after, Measurement::Unavailable { .. })));
        assert!(!result.diagnostics.is_empty());
    }
    let result = extract_changes(
        &[added("lib.rs", "fn work() {}")],
        &mut ParserRegistry::new(),
        &ExtractionConfig {
            max_total_tokens: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        value(&result, Metric::DuplicatedTokens, "lib.rs"),
        Measurement::Unavailable { .. }
    ));
    assert_eq!(
        value(&result, Metric::MethodLength, "work"),
        measured(1),
        "a clone-index cap must not erase local AST measurements"
    );
}

#[test]
fn changed_token_scopes_do_not_match_different_languages_or_ignore_real_literals() {
    let config = ExtractionConfig {
        minimum_clone_tokens: 8,
        ..Default::default()
    };
    let changes = vec![
        added("a.ts", "function work(x) { let y = x + 1; return y * 2; }"),
        added("b.cpp", "int work(int x) { int y = x + 1; return y * 2; }"),
    ];
    let result = extract_changes(&changes, &mut ParserRegistry::new(), &config).unwrap();
    assert_eq!(
        value(&result, Metric::DuplicatedTokens, "a.ts"),
        measured(0)
    );
    assert_eq!(
        value(&result, Metric::DuplicatedTokens, "b.cpp"),
        measured(0)
    );
    let changes = vec![
        added(
            "a.rs",
            "fn work() { let text = \"if a b c d e f g h i j k l m n\"; }",
        ),
        added(
            "b.rs",
            "fn other() { let text = \"different a b c d e f g h i j k l m n\"; }",
        ),
    ];
    let result = extract_changes(
        &changes,
        &mut ParserRegistry::new(),
        &ExtractionConfig {
            minimum_clone_tokens: 16,
            ..config
        },
    )
    .unwrap();
    assert_eq!(
        value(&result, Metric::DuplicatedTokens, "a.rs"),
        measured(0)
    );
}

#[test]
fn branch_and_nesting_conventions_cover_chains_switches_matches_and_expressions() {
    for (path, source, complexity, depth) in [
        ("app.ts", "function work(a: boolean, b: boolean) { if (a && b) { return 1; } else if (a || b) { return 2; } return 0; }", 5, 1),
        ("app.ts", "function work(a: boolean, b: boolean) { return a ? (b ? 1 : 2) : 3; }", 3, 2),
        ("app.c", "int work(int x) { switch(x) { case 0: return 0; case 1: return 1; default: return 2; } }", 3, 1),
        ("app.go", "package p\nfunc work(x int) int { switch x { case 0: return 0; default: return 1 } }", 2, 1),
        ("lib.rs", "fn work(x: i32) -> i32 { match x { 0 => 0, _ => 1 } }", 2, 1),
        ("app.py", "def work(x):\n    match x:\n        case 0:\n            return 0\n        case _:\n            return 1\n", 2, 1),
        ("app.swift", "func work(_ x: Int) -> Int { if x > 1 { return 1 } else if x > 0 { return 2 } else { return 0 } }", 3, 1),
    ] {
        let result = run(vec![added(path, source)]);
        assert_eq!(value(&result, Metric::CyclomaticComplexity, "work"), measured(complexity), "{path}: {source}");
        assert_eq!(value(&result, Metric::NestingDepth, "work"), measured(depth), "{path}: {source}; {}", ParserRegistry::new().parse_tree(path, source).unwrap().root_node().to_sexp());
    }
}

#[test]
fn real_extractor_source_runs_without_repository_materialization() {
    let source = include_str!("languages.rs");
    let start = std::time::Instant::now();
    let result = run(vec![FileChange {
        path: "collector.rs".into(),
        before: Source::Text(source.into()),
        after: Source::Text(source.into()),
    }]);
    assert!(result
        .metrics
        .iter()
        .any(|record| record.observation.metric == Metric::MethodLength
            && matches!(record.observation.after, Measurement::Measured { value } if value > 0)));
    eprintln!(
        "metric extraction over real source: {} observations in {:?}",
        result.metrics.len(),
        start.elapsed()
    );
}
