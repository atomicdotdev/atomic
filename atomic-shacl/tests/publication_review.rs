//! Shape-level fixtures model verifier observations. Production callers cannot
//! submit these RDF assertions to the repository's review-authorization API.
use atomic_core::types::{Base32, Hash};
use atomic_shacl::{validate_ntriples, Profile, ValidationRequest};

fn dataset() -> String {
    let change = Hash::of(b"reviewed change").to_base32();
    let artifact = Hash::of(b"review artifact").to_base32();
    format!(
        r#"
<urn:atomic:review:evaluation> <https://atomic.dev/ns/workflow#evaluatesReview> <urn:atomic:attestation:blake3-256:{artifact}> .
<urn:atomic:attestation:blake3-256:{artifact}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#PublicationReview> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#verificationResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#authorityResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#freshnessResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#signedBy> <urn:reviewer> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#reviewsClosure> <urn:subject> .
<urn:atomic:attestation:blake3-256:{artifact}> <https://atomic.dev/ns/workflow#reviewIntent> <urn:review> .
<urn:subject> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#ReviewSubject> .
<urn:subject> <https://atomic.dev/ns/workflow#reviewedState> "pinned-state" .
<urn:subject> <https://atomic.dev/ns/workflow#reviewedContext> "urn:atomic:view-set:pinned-set" .
<urn:subject> <https://atomic.dev/ns/workflow#reviewsChange> <urn:atomic:change:blake3-256:{change}> .
<urn:subject> <https://atomic.dev/ns/workflow#includesIntent> <urn:work> .
<urn:review> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#ReviewIntent> .
<urn:review> <https://atomic.dev/ns/workflow#status> "done" .
<urn:review> <https://atomic.dev/ns/workflow#verificationResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:review> <https://atomic.dev/ns/workflow#freshnessResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:review> <https://atomic.dev/ns/workflow#canonicalConformance> <https://atomic.dev/ns/workflow#Verified> .
<urn:review> <https://atomic.dev/ns/workflow#authoredBy> <urn:reviewer> .
<urn:review> <https://atomic.dev/ns/workflow#reviews> <urn:work> .
<urn:review> <https://atomic.dev/ns/workflow#hasAcceptanceCriterion> <urn:criterion> .
<urn:work> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#WorkIntent> .
<urn:work> <https://atomic.dev/ns/workflow#verificationResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:work> <https://atomic.dev/ns/workflow#freshnessResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:work> <https://atomic.dev/ns/workflow#canonicalConformance> <https://atomic.dev/ns/workflow#Verified> .
<urn:work> <https://atomic.dev/ns/workflow#authoredBy> <urn:author> .
<urn:work> <https://atomic.dev/ns/workflow#declaresImplementation> <urn:atomic:change:blake3-256:{change}> .
<urn:work> <https://atomic.dev/ns/workflow#hasAcceptanceCriterion> <urn:criterion> .
<urn:author> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#Identity> .
<urn:reviewer> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#Identity> .
<urn:author> <https://atomic.dev/ns/workflow#signingKey> <urn:author-key> .
<urn:reviewer> <https://atomic.dev/ns/workflow#signingKey> <urn:reviewer-key> .
<urn:author-key> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#SigningKey> .
<urn:reviewer-key> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#SigningKey> .
<urn:criterion> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#AcceptanceCriterion> .
<urn:criterion> <https://atomic.dev/ns/workflow#status> "met" .
<urn:criterion> <https://atomic.dev/ns/workflow#evidenceText> "test artifact" .
<urn:criterion> <https://atomic.dev/ns/workflow#verifiedByText> "independent fixture" .
<urn:criterion> <https://atomic.dev/ns/workflow#requiredKind> "manual" .
<urn:criterion> <https://atomic.dev/ns/workflow#hasVerification> <urn:execution> .
<urn:execution> <https://atomic.dev/ns/workflow#verificationKind> "manual" .
<urn:execution> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#VerificationRecord> .
<urn:execution> <https://atomic.dev/ns/workflow#status> "pass" .
<urn:execution> <https://atomic.dev/ns/workflow#observedState> "pinned-state" .
<urn:execution> <https://atomic.dev/ns/workflow#verificationReference> "urn:atomic:view-set:pinned-set" .
<urn:atomic:change:blake3-256:{change}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#Change> .
<urn:atomic:change:blake3-256:{change}> <https://atomic.dev/ns/workflow#status> "managed" .
<https://atomic.dev/ns/workflow#Verified> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#VerificationStatus> .
"#
    )
}

fn report(data: &str) -> atomic_shacl::ValidationOutput {
    validate_ntriples(
        data,
        "fixture",
        &ValidationRequest {
            profile: Profile::Review,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn empty_or_partial_review_dataset_cannot_pass_vacuously() {
    assert!(!report("").conforms);
    assert!(
        !report(
            &dataset()
                .lines()
                .filter(|line| !line.contains("evaluatesReview"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .conforms
    );
}

#[test]
fn review_policy_is_executable_shacl_not_a_rust_status_check() {
    let data = dataset();
    let good = report(&data);
    assert!(good.conforms, "{:#?}", good.results);
    for bad in [
        data.replace("\"done\"", "\"in_progress\""),
        data.replace("\"met\"", "\"unmet\""),
        data.replace("\"pass\"", "\"fail\""),
        data.replace("<urn:reviewer-key>", "<urn:author-key>"),
        data.replace(
            "#observedState> \"pinned-state\"",
            "#observedState> \"unrelated-state\"",
        ),
        data.lines()
            .filter(|line| !line.contains("declaresImplementation"))
            .collect::<Vec<_>>()
            .join("\n"),
    ] {
        assert!(
            !report(&bad).conforms,
            "accepted incomplete or unauthorized review"
        );
    }
}

#[test]
fn review_evidence_paths_replace_plan_shortcuts_and_reject_ambiguity() {
    let change = Hash::of(b"reviewed change").to_base32();
    let provenance = Hash::of(b"retained provenance").to_base32();
    let data = dataset()
        .lines()
        .filter(|line| !line.contains("declaresImplementation"))
        .collect::<Vec<_>>()
        .join("\n");
    let evidence = format!(
        r#"
<urn:atomic:change:blake3-256:{change}> <https://atomic.dev/ns/workflow#hasReviewEvidence> <urn:evidence> .
<urn:atomic:change:blake3-256:{change}> <https://atomic.dev/ns/workflow#changeExplainedBy> <urn:atomic:provenance:blake3-256:{provenance}> .
<urn:evidence> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#IntentObservation> .
<urn:evidence> <https://atomic.dev/ns/workflow#observedIntent> <urn:work> .
<urn:evidence> <https://atomic.dev/ns/workflow#sourceNode> "node-4" .
<urn:evidence> <https://atomic.dev/ns/workflow#sourceReference> "REAC::lee::22" .
<urn:evidence> <https://atomic.dev/ns/workflow#observationKind> "attested-receipt" .
<urn:evidence> <https://atomic.dev/ns/workflow#observationRole> "work" .
<urn:evidence> <https://atomic.dev/ns/workflow#status> "resolved" .
<urn:evidence> <https://atomic.dev/ns/workflow#freshnessResult> <https://atomic.dev/ns/workflow#Verified> .
<urn:evidence> <https://atomic.dev/ns/workflow#evidenceSource> <urn:atomic:provenance:blake3-256:{provenance}> .
<urn:evidence> <https://atomic.dev/ns/workflow#evidenceTurn> <urn:turn> .
<urn:turn> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#SessionTurn> .
<urn:other-turn> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#SessionTurn> .
<urn:turn> <https://atomic.dev/ns/workflow#turnExplainedBy> <urn:atomic:provenance:blake3-256:{provenance}> .
<urn:atomic:provenance:blake3-256:{provenance}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://atomic.dev/ns/workflow#ProvenanceGraph> .
"#
    );
    let data = format!("{data}\n{evidence}");
    let good = report(&data);
    assert!(good.conforms, "{:#?}", good.results);
    for bad in [
        data.replace("\"resolved\"", "\"ambiguous\""),
        data.replace("#observationRole> \"work\"", "#observationRole> \"review-context\""),
        data.replace("#evidenceTurn> <urn:turn>", "#evidenceTurn> <urn:other-turn>"),
        data.lines().filter(|line| !line.contains("#sourceNode>")).collect::<Vec<_>>().join("\n"),
        format!("{data}\n<urn:evidence> <https://atomic.dev/ns/workflow#evidenceTurn> <urn:other-turn> ."),
    ] {
        assert!(!report(&bad).conforms);
    }
}
