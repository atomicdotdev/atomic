use crate::{relationships::SourceGraph, ParserRegistry, Reference};

#[test]
fn shared_entity_and_reference_apis_keep_qualified_targets_and_owners() {
    let source = "struct Reader; impl Reader { fn value(&self, x:i32) {} fn run(&self) { self.value(1); } } fn value(x:i32) {} fn consumer(reader:&Reader) { reader.value(2); value(3); module::value(4); }";
    let mut parser = ParserRegistry::new();
    let analysis = parser.analyze_source("lib.rs", source).unwrap();
    assert_eq!(
        serde_json::to_value(&analysis.entities).unwrap(),
        serde_json::to_value(parser.extract("lib.rs", source)).unwrap()
    );
    let member = analysis
        .calls
        .iter()
        .find(|call| call.expression == "self.value")
        .unwrap();
    assert_eq!(member.receiver.as_deref(), Some("self"));
    assert_eq!(member.caller, "lib.rs#Reader::run");
    assert!(analysis
        .calls
        .iter()
        .any(|call| call.target == ["module", "value"] && call.receiver.is_none()));
    let refs = parser.extract_references("lib.rs", source);
    assert!(refs
        .iter()
        .filter_map(|reference| reference.call.as_ref())
        .any(|context| context.target == "self.value" && context.caller == "lib.rs#Reader::run"));
    assert!(refs
        .iter()
        .filter_map(|reference| reference.call.as_ref())
        .any(|context| context.target == "module::value"));
    let graph = SourceGraph::new(vec![analysis], ["lib.rs".into()], true);
    let calls = graph.bind_calls(None);
    assert_eq!(
        calls.len(),
        3,
        "unresolved module::value must not match an unrelated free function"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.symbol.id == "lib.rs#Reader::value")
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.symbol.id == "lib.rs#value")
            .count(),
        1
    );
}

#[test]
fn typescript_this_and_declared_receiver_types_do_not_collapse_to_method_names() {
    let source = "class Reader { value(x:number) { return x; } run() { return this.value(1); } } function consumer(reader: Reader) { return reader.value(2); } function value(x:number) { return x; }";
    let mut parser = ParserRegistry::new();
    let file = parser.analyze_source("app.ts", source).unwrap();
    let graph = SourceGraph::new(vec![file], ["app.ts".into()], true);
    let calls = graph.bind_calls(None);
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|call| call.symbol.id == "app.ts#Reader::value"));
}

#[test]
fn normal_attributes_and_macros_retain_semantic_source_facts() {
    let mut parser = ParserRegistry::new();
    let source = "#[cfg(test)] mod tests { #[allow(dead_code)] fn work() {} fn consumer() { assert!(true); work(); } }";
    let file = parser.analyze_source("lib.rs", source).unwrap();
    assert_eq!(file.symbols.len(), 2);
    let graph = SourceGraph::new(vec![file], ["lib.rs".into()], true);
    let calls = graph.bind_calls(None);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].symbol.id, "lib.rs#tests::work");
    assert!(calls[0].configuration_dependent);
}

#[test]
fn lexical_block_boundaries_and_shadowed_names_do_not_create_wrong_callers() {
    let source = "fn work(x:i32) {} fn consumer() { if true { fn work(x:i32,y:i32) {} work(1,2); } work(1); } fn shadow(work:fn(i32)) { work(1); }";
    let mut parser = ParserRegistry::new();
    let file = parser.analyze_source("lib.rs", source).unwrap();
    let graph = SourceGraph::new(vec![file], ["lib.rs".into()], true);
    let calls = graph.bind_calls(None);
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .any(|call| call.symbol.id == "lib.rs#consumer::work" && call.call.arguments == Some(2)));
    assert!(calls
        .iter()
        .any(|call| call.symbol.id == "lib.rs#work" && call.call.arguments == Some(1)));
}

#[test]
fn legacy_reference_deserialization_and_optional_call_context_remain_compatible() {
    let reference: Reference = serde_json::from_value(
        serde_json::json!({"symbol":"work","file":"lib.rs","line":1,"confidence":"high"}),
    )
    .unwrap();
    assert!(reference.call.is_none());
    assert!(serde_json::to_value(reference)
        .unwrap()
        .get("call")
        .is_none());
}

#[test]
fn top_level_calls_keep_the_module_owner_and_do_not_disappear() {
    let mut parser = ParserRegistry::new();
    let file = parser
        .analyze_source("app.ts", "function work(x:number) {} work(1);")
        .unwrap();
    let graph = SourceGraph::new(vec![file], ["app.ts".into()], true);
    let calls = graph.bind_calls(None);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call.caller, "app.ts#<module>");
    assert_eq!(calls[0].symbol.id, "app.ts#work");
}

#[test]
fn custom_visibility_remains_explicit_context_not_a_guessed_access_failure() {
    let source = "mod parent { pub(in crate) mod api { pub fn work() {} } } fn consumer() { crate::parent::api::work(); }";
    let mut parser = ParserRegistry::new();
    let file = parser.analyze_source("lib.rs", source).unwrap();
    assert!(file
        .modules
        .iter()
        .any(|module| module.visibility.as_deref() == Some("pub(in crate)")));
    let graph = SourceGraph::new(vec![file], ["lib.rs".into()], true);
    assert!(graph.bind_calls(None).is_empty());
}

#[test]
fn existing_language_parameter_bindings_do_not_turn_callbacks_into_global_calls() {
    let mut parser = ParserRegistry::new();
    for (path, source) in [
        (
            "app.py",
            "def work():\n    pass\ndef consumer(work):\n    work()\n",
        ),
        (
            "app.go",
            "package p\nfunc work() {}\nfunc consumer(a, work func()) { work() }",
        ),
        (
            "app.c",
            "void work(void) {} void consumer(void (*work)(void)) { work(); }",
        ),
    ] {
        let file = parser.analyze_source(path, source).unwrap();
        let graph = SourceGraph::new(vec![file], [path.into()], true);
        assert!(graph.bind_calls(None).is_empty(), "{path}");
    }
}
