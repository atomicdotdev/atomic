//! User-defined vault kinds and the knowledge graph's closed vocabulary.

use std::collections::BTreeSet;

use atomic_canonical::kind_spec::{
    is_reserved_kind_name, is_valid_kind_name, is_valid_ulid, RESERVED_KIND_NAMES,
};
use atomic_core::kg::iri;
use atomic_core::kg::ontology::Class;
use atomic_core::kg::project_id::ProjectId;

/// `ToolResult` from `atomic:ToolResult`.
fn local_name(class: Class) -> &'static str {
    class.curie().strip_prefix("atomic:").unwrap()
}

/// `tool<separator>result` from `ToolResult`.
fn separated(local: &str, separator: char) -> String {
    let mut out = String::new();
    for c in local.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push(separator);
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// `atomic-canonical` cannot depend on `atomic-core`, so this test is what
/// keeps `RESERVED_KIND_NAMES` equal to its rule: the normalized local name of
/// every ontology class, plus five names no class has.
#[test]
fn reserved_kind_names_track_the_ontology() {
    let mut expected: BTreeSet<String> = Class::ALL
        .iter()
        .map(|class| local_name(*class).to_ascii_lowercase())
        .collect();
    assert_eq!(expected.len(), Class::ALL.len());
    expected.extend(
        ["intent", "kind", "kinds", "urn", "did"]
            .into_iter()
            .map(String::from),
    );
    let listed: BTreeSet<String> = RESERVED_KIND_NAMES.iter().map(|s| s.to_string()).collect();
    assert_eq!(listed, expected);

    for class in Class::ALL {
        let local = local_name(*class);
        for spelling in [
            local.to_ascii_lowercase(),
            separated(local, '-'),
            separated(local, '_'),
        ] {
            assert!(is_valid_kind_name(&spelling), "{spelling}");
            assert!(is_reserved_kind_name(&spelling), "{spelling}");
        }
    }
}

/// A SplitMix64 stream, so a failure reproduces with the same cases.
struct Cases(u64);

impl Cases {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % below as u64) as usize
    }

    fn pick(&mut self, alphabet: &[u8]) -> char {
        alphabet[self.next(alphabet.len())] as char
    }

    fn kind_name(&mut self) -> String {
        const REST: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";
        let mut name = String::from(self.pick(&REST[..26]));
        for _ in 0..self.next(63) {
            name.push(self.pick(REST));
        }
        name
    }

    fn ulid(&mut self) -> String {
        const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut ulid = String::from(self.pick(&CROCKFORD[..8]));
        for _ in 0..25 {
            ulid.push(self.pick(CROCKFORD));
        }
        ulid
    }
}

/// The name of every resource parser other than `parse_concept` that accepts
/// `iri_text`.
fn accepted_by_other_parsers(iri_text: &str) -> Vec<&'static str> {
    let checks = [
        ("project", iri::parse_project(iri_text).is_ok()),
        ("change", iri::parse_change(iri_text).is_ok()),
        ("provenance", iri::parse_provenance(iri_text).is_ok()),
        ("attestation", iri::parse_attestation(iri_text).is_ok()),
        (
            "project_identity",
            iri::parse_project_identity(iri_text).is_ok(),
        ),
        ("goal", iri::parse_goal(iri_text).is_ok()),
        ("memory", iri::parse_memory(iri_text).is_ok()),
        ("skill", iri::parse_skill(iri_text).is_ok()),
        ("tool_result", iri::parse_tool_result(iri_text).is_ok()),
        ("view", iri::parse_view(iri_text).is_ok()),
        ("file", iri::parse_file(iri_text).is_ok()),
        ("session", iri::parse_session(iri_text).is_ok()),
        (
            "named_identity",
            iri::parse_named_identity(iri_text).is_ok(),
        ),
        ("identity", iri::parse_identity(iri_text).is_ok()),
        ("intent", iri::parse_intent(iri_text).is_ok()),
        ("child", iri::parse_child(iri_text).is_ok()),
        ("turn", iri::parse_turn(iri_text).is_ok()),
        ("todo", iri::parse_todo(iri_text).is_ok()),
        ("finding", iri::parse_finding(iri_text).is_ok()),
    ];
    checks
        .into_iter()
        .filter_map(|(name, accepted)| accepted.then_some(name))
        .collect()
}

/// Instance and hub names survive the concept IRI, and no other resource
/// parser claims the result. Replaces the retired `<k>:<ULID>` identifier test.
#[test]
fn kind_identity_is_a_concept_iri_and_nothing_else() {
    let project = ProjectId::generate();
    let mut cases = Cases(0x4b49_4e44);
    let mut checked = 0;
    while checked < 2000 {
        let kind = cases.kind_name();
        let uid = cases.ulid();
        assert!(is_valid_kind_name(&kind), "{kind}");
        assert!(is_valid_ulid(&uid), "{uid}");
        if is_reserved_kind_name(&kind) {
            continue;
        }
        checked += 1;

        for name in [format!("{kind}.{uid}"), format!("kind.{kind}")] {
            let minted = iri::mint_concept(&project, &name);
            assert_eq!(
                iri::parse_concept(&minted).unwrap(),
                (project, name.clone()),
                "{minted}"
            );
            let accepted = accepted_by_other_parsers(&minted);
            assert!(accepted.is_empty(), "{minted}: {accepted:?}");
            let (prefix, suffix) = name.split_once('.').unwrap();
            assert!(!suffix.contains('.'), "{name}");
            assert!(prefix == "kind" || prefix == kind, "{name}");
        }
    }
}
