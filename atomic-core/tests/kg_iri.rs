use atomic_core::kg::{iri::*, project_id::ProjectId};
use atomic_core::types::Hash;
use quickcheck_macros::quickcheck;

#[quickcheck]
fn names_and_paths_round_trip(value: String) -> bool {
    let project = ProjectId::generate();
    let pairs = [
        (mint_file(&project, &value), parse_file as fn(&str) -> _),
        (mint_memory(&project, &value), parse_memory),
        (mint_goal(&project, &value), parse_goal),
        (mint_skill(&project, &value), parse_skill),
        (mint_tool_result(&project, &value), parse_tool_result),
        (mint_concept(&project, &value), parse_concept),
        (mint_view(&project, &value), parse_view),
        (mint_session(&project, &value), parse_session),
        (mint_named_identity(&project, &value), parse_named_identity),
    ];
    pairs
        .into_iter()
        .all(|(iri, parse)| parse(&iri).unwrap() == (project, value.clone()))
        && parse_todo(&mint_todo(&project, &value, &value)).unwrap()
            == (project, value.clone(), value)
}

#[test]
fn all_global_ids_intents_findings_and_identity_forms_round_trip() {
    let project = ProjectId::generate();
    let hash = Hash::of(b"global object");
    assert_eq!(parse_project(&mint_project(&project)).unwrap(), project);
    for (iri, parse) in [
        (mint_change(&hash), parse_change as fn(&str) -> _),
        (mint_provenance(&hash), parse_provenance),
        (mint_attestation(&hash), parse_attestation),
        (mint_project_identity(&hash), parse_project_identity),
    ] {
        assert_eq!(parse(&iri).unwrap(), hash);
    }
    let uid = ulid::Ulid::new().to_string();
    assert_eq!(
        parse_intent(&mint_intent(&project, &uid).unwrap()).unwrap(),
        (project, uid.clone())
    );
    for kind in [
        ChildKind::AcceptanceCriterion,
        ChildKind::Task,
        ChildKind::Scope,
        ChildKind::Constraint,
    ] {
        assert_eq!(
            parse_child(&mint_child(&project, &uid, kind, "child:#").unwrap()).unwrap(),
            (project, uid.clone(), kind, "CHILD:#".into())
        );
    }
    let subject = mint_change(&hash);
    assert_eq!(
        parse_finding(&mint_finding(&project, "MissingPlanId", &subject)).unwrap(),
        (
            project,
            "MissingPlanId".into(),
            Hash::of(subject.as_bytes())
        )
    );
    let did = format!("did:atomic:{}", project.to_base32());
    assert_eq!(
        parse_identity(&mint_identity(&project, &did)).unwrap(),
        IdentityIri::Did(did)
    );
    assert_eq!(
        parse_identity(&mint_identity(&project, "Alice Smith")).unwrap(),
        IdentityIri::Named(project, "Alice Smith".into())
    );
    assert!(parse_identity("did:atomic:not-a-fingerprint").is_err());
}

#[test]
fn strict_identifiers_and_scoped_resources() {
    let project = ProjectId::generate();
    assert_eq!(
        ProjectId::from_base32(&project.to_base32()).unwrap(),
        project
    );
    assert!(ProjectId::from_base32(&project.to_base32().to_lowercase()).is_err());
    let hash = Hash::of(b"change");
    let change = mint_change(&hash);
    assert_eq!(parse_change(&change).unwrap(), hash);
    assert!(parse_change(&change.to_lowercase()).is_err());
    assert!(parse_change(&(change.clone() + "=")).is_err());
    assert!(parse_change(&change[..change.len() - 1]).is_err());
    let mut nonzero = mint_change(&Hash::of(b"trailing bits"));
    let last = nonzero.pop().unwrap();
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    nonzero.push(alphabet.as_bytes()[alphabet.find(last).unwrap() + 1] as char);
    assert!(parse_change(&nonzero).is_err());
    let value = "a b/#:%/日本語";
    let file = mint_file(&project, value);
    assert!(file.ends_with("a%20b/%23%3A%25/%E6%97%A5%E6%9C%AC%E8%AA%9E"));
    assert!(parse_file(&file.replace("%3A", "%3a")).is_err());
    assert!(parse_file(&mint_file(&project, "A").replace(":file:A", ":file:%41")).is_err());
    assert_ne!(
        mint_todo(&project, "one", "t1"),
        mint_todo(&project, "two", "t1")
    );
    assert_eq!(
        parse_turn(&mint_turn(&project, "session:id", 42)).unwrap(),
        (project, "session:id".into(), 42)
    );
    let uid = ulid::Ulid::new().to_string();
    let child = mint_child(&project, &uid, ChildKind::Task, "task-a").unwrap();
    assert_eq!(parse_child(&child).unwrap().3, "TASK-A");
    assert!(mint_intent(&project, &uid.to_lowercase()).is_err());
}
