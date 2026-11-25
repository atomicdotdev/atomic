use crate::structural::{inspect, Rule};
use atomic_semantic::ParserRegistry;

fn rules(path: &str, source: &str) -> Vec<Rule> {
    let inspection = inspect(&mut ParserRegistry::new(), path, source).unwrap();
    assert!(
        !inspection.has_syntax_errors,
        "fixture must parse: {source}"
    );
    inspection
        .issues
        .into_iter()
        .map(|issue| issue.rule)
        .collect()
}

fn long_function(branches: usize, lines: usize) -> String {
    let mut source = String::from("fn large(x:i32) {\n");
    for i in 0..branches {
        source.push_str(&format!("if x == {i} {{ work(); }}\n"));
    }
    for _ in 0..lines {
        source.push_str("work();\n");
    }
    source.push_str("}\n");
    source
}

#[test]
fn every_substantive_family_has_an_executable_positive_fixture() {
    let large = long_function(15, 100);
    for (rule, source) in [
        (Rule::FailureMasking, "fn save()->Result<(),()> { match write() { Ok(()) => Ok(()), Err(_) => Ok(()) } }"),
        (Rule::DuplicatedBehavior, "fn f(flag:bool) { if flag { send(1); } else { send(1); } }"),
        (Rule::UnnecessaryMachinery, "fn implementation(x:i32)->i32 { x+1 } fn middle(x:i32)->i32 { implementation(x) } fn outer(x:i32)->i32 { middle(x) }"),
        (Rule::Manageability, large.as_str()),
        (Rule::AsyncResourceSafety, "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); work().await; drop(guard); }"),
        (Rule::WeakVerification, "#[test] fn verifies() { let actual=work(); assert_eq!(actual, actual); }"),
        (Rule::UnsafeShortcuts, "fn f(bits:u32)->f32 { unsafe { std::mem::transmute::<u32,f32>(bits) } }"),
    ] {
        let inspection = inspect(&mut ParserRegistry::new(), "lib.rs", source).unwrap();
        assert!(!inspection.has_syntax_errors, "{rule:?} fixture must parse");
        let issue = inspection.issues.iter().find(|issue| issue.rule == rule)
            .unwrap_or_else(|| panic!("{rule:?} did not execute; found {:?}", inspection.issues));
        assert!(!issue.detail.is_empty() && !issue.source.is_empty());
        assert!(issue.start_line > 0 && issue.end_line >= issue.start_line);
    }
}

#[test]
fn real_error_propagation_and_distinct_branches_are_not_failures() {
    for source in [
        "fn f()->Result<(),()> { match work() { Ok(x)=>Ok(x), Err(e)=>Err(e) } }",
        "fn f(b:bool) { if b { send(1); } else { send(2); } }",
        "fn f(x:Option<i32>) { let x=1; if let Some(x)=x { send(x); } else { send(x); } }",
        "fn f() { let text=\"Err(_) => Ok(()); unsafe { transmute(x) }\"; /* if b {same()}else{same()} */ }",
    ] {
        let found = rules("lib.rs", source);
        assert!(!found.contains(&Rule::FailureMasking) && !found.contains(&Rule::DuplicatedBehavior), "{found:?}");
    }
    assert!(rules(
        "lib.rs",
        "fn f()->Result<(),()> { if let Err(e)=work() { return Ok(()); } Ok(()) }"
    )
    .contains(&Rule::FailureMasking));
}

#[test]
fn typed_optional_file_absence_is_not_failure_masking() {
    for source in [
        "fn runtime_store()->Result<Option<Store>,std::io::Error> { match open() { Ok(x)=>Ok(Some(x)), Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None), Err(e)=>Err(e) } }",
        "fn manifest_plan()->Result<Option<Plan>,std::io::Error> { match read() { Ok(x)=>Ok(Some(x)), Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None), Err(error)=>return Err(error) } }",
    ] { assert!(!rules("lib.rs",source).contains(&Rule::FailureMasking)); }
    assert!(rules("lib.rs", "fn f()->Result<Option<Value>,Error> { match read() { Ok(value)=>Ok(Some(value)), Err(_)=>Ok(None) } }").contains(&Rule::FailureMasking));
}

#[test]
fn branching_normalization_ignores_comments_but_preserves_literals() {
    assert!(rules(
        "lib.rs",
        "fn f(b:bool) { if b { /* one */ work(1); } else { work(1); /* two */ } }"
    )
    .contains(&Rule::DuplicatedBehavior));
    assert!(!rules(
        "lib.rs",
        "fn f(b:bool) { if b { work(\"first\"); } else { work(\"second\"); } }"
    )
    .contains(&Rule::DuplicatedBehavior));
}

#[test]
fn single_wrappers_public_boundaries_and_adapters_are_not_machinery_findings() {
    for source in [
        "fn implementation(x:i32)->i32{x+1} fn named_boundary(x:i32)->i32{implementation(x)}",
        "fn core(x:i32)->i32{x+1} fn middle(x:i32)->i32{core(x)} pub fn api(x:i32)->i32{middle(x)}",
        "fn core(x:i32)->i32{x+1} fn middle(x:i32)->i32{core(x)} #[inline] fn outer(x:i32)->i32{middle(x)}",
        "fn core(x:i32)->i32{x+1} fn middle(x:i32)->i32{core(x)} fn outer(x:i32)->i32{middle(x+1)}",
        "fn a(x:i32)->i32{b(x)} fn b(x:i32)->i32{a(x)}",
        "fn core<T>(x:T)->T{x} fn middle<T>(x:T)->T{core(x)} fn outer<T>(x:T)->T{middle(x)}",
    ] { assert!(!rules("lib.rs", source).contains(&Rule::UnnecessaryMachinery), "{source}"); }
}

#[test]
fn manageability_requires_joint_evidence_not_a_single_large_measurement() {
    assert!(!rules("lib.rs", &long_function(0, 200)).contains(&Rule::Manageability));
    assert!(!rules("lib.rs", &long_function(20, 0)).contains(&Rule::Manageability));
    assert!(rules("lib.rs", &long_function(15, 100)).contains(&Rule::Manageability));
}

#[test]
fn synchronous_guards_released_before_await_and_async_mutexes_are_excluded() {
    for source in [
        "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); drop(guard); work().await; }",
        "async fn f() { let m=std::sync::Mutex::new(0); { let guard=m.lock().unwrap(); use_guard(&guard); } work().await; }",
        "async fn f() { let m=tokio::sync::Mutex::new(0); let guard=m.lock().await; work().await; drop(guard); }",
        "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); let callback=async { work().await; }; drop(guard); callback.await; }",
        "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); release(guard); work().await; }",
        "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); return; work().await; }",
    ] { assert!(!rules("lib.rs", source).contains(&Rule::AsyncResourceSafety), "{source}"); }
    assert!(rules("lib.rs", "async fn f() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); let guard=42; work().await; }").contains(&Rule::AsyncResourceSafety));
    assert!(rules("lib.rs", "async fn f(m:&std::sync::Mutex<i32>) { let guard=m.lock().unwrap(); work().await; drop(guard); }").contains(&Rule::AsyncResourceSafety));
}

#[test]
fn joint_manageability_reuses_all_registered_language_conventions() {
    for (path, header, branch, call, footer) in [
        (
            "app.ts",
            "function large(x:number) {\n",
            "if(x===0){work();}\n",
            "work();\n",
            "}\n",
        ),
        (
            "app.tsx",
            "function large(x:number) {\n",
            "if(x===0){work();}\n",
            "work();\n",
            "}\n",
        ),
        (
            "app.py",
            "def large(x):\n",
            "    if x == 0:\n        work()\n",
            "    work()\n",
            "\n",
        ),
        (
            "app.go",
            "package main\nfunc large(x int) {\n",
            "if x==0 {work()}\n",
            "work()\n",
            "}\n",
        ),
        (
            "App.java",
            "class App { void large(int x) {\n",
            "if(x==0){work();}\n",
            "work();\n",
            "} }\n",
        ),
        (
            "app.c",
            "void large(int x) {\n",
            "if(x==0){work();}\n",
            "work();\n",
            "}\n",
        ),
        (
            "app.cpp",
            "void large(int x) {\n",
            "if(x==0){work();}\n",
            "work();\n",
            "}\n",
        ),
        (
            "app.swift",
            "func large(_ x: Int) {\n",
            "if x == 0 {work()}\n",
            "work()\n",
            "}\n",
        ),
    ] {
        let source = format!("{header}{}{}{footer}", branch.repeat(15), call.repeat(100));
        assert!(
            rules(path, &source).contains(&Rule::Manageability),
            "{path}"
        );
    }
}

#[test]
fn deeply_nested_audit_work_is_explicitly_incomplete() {
    let source = format!("fn f() {{ {}work();{} }}", "{".repeat(300), "}".repeat(300));
    let error = inspect(&mut ParserRegistry::new(), "lib.rs", &source).unwrap_err();
    assert!(error.contains("budget exceeded"), "{error}");
}

#[test]
fn weak_verification_checks_actual_tests_without_blanket_no_assert_heuristics() {
    for source in [
        "fn example() { assert!(true); }",
        "#[test] fn verifies() { let actual=work(); assert_eq!(actual, expected()); }",
        "#[test] fn verifies() { assert_eq!(next(), next()); }",
        "#[test] #[should_panic] fn verifies() {}",
        "#[test] fn verifies()->Result<(),Error> { work()?; Ok(()) }",
    ] {
        assert!(
            !rules("lib.rs", source).contains(&Rule::WeakVerification),
            "{source}"
        );
    }
    assert!(rules("lib.rs", "#[test] fn empty() {}").contains(&Rule::WeakVerification));
    assert!(rules("lib.rs", "#[test] fn tautology() { assert!(true); }")
        .contains(&Rule::WeakVerification));
}

#[test]
fn safety_justifications_and_narrow_suppressions_are_not_blanket_bypasses() {
    for source in [
        "fn f(bits:u32)->f32 { // SAFETY: every u32 bit pattern represents a valid f32.\n unsafe { std::mem::transmute::<u32,f32>(bits) } }",
        "fn f(bits:u32)->f32 { unsafe { // SAFETY: every u32 bit pattern represents a valid f32.\n std::mem::transmute::<u32,f32>(bits) } }",
        "#[allow(dead_code)] fn f() {}",
        "// Reason: generated bindings intentionally mirror the external interface.\n #[allow(warnings)] fn f() {}",
    ] { assert!(!rules("lib.rs", source).contains(&Rule::UnsafeShortcuts), "{source}"); }
    assert!(rules("lib.rs", "#[allow(warnings)] fn f() {}").contains(&Rule::UnsafeShortcuts));
    assert!(rules("lib.rs", "fn f(x:u32)->f32 { let text=\"SAFETY: pretend\"; unsafe { std::mem::transmute::<u32,f32>(x) } }").contains(&Rule::UnsafeShortcuts));
}

#[test]
fn typescript_audits_execute_and_exclude_shadowed_promise() {
    for (rule, source) in [
        (
            Rule::FailureMasking,
            "function f() { try { work(); } catch(e) { return true; } }",
        ),
        (
            Rule::DuplicatedBehavior,
            "function f(b:boolean) { if(b) { work(1); } else { work(1); } }",
        ),
        (
            Rule::AsyncResourceSafety,
            "const result = new Promise(async (resolve,reject) => { await work(); resolve(1); });",
        ),
    ] {
        assert!(rules("app.ts", source).contains(&rule), "{rule:?}");
    }
    assert!(!rules(
        "app.ts",
        "class Promise {} const result = new Promise(async () => { await work(); });"
    )
    .contains(&Rule::AsyncResourceSafety));
    assert!(!rules(
        "app.ts",
        "const result = new Promise((resolve,reject) => { work().then(resolve,reject); });"
    )
    .contains(&Rule::AsyncResourceSafety));
}
