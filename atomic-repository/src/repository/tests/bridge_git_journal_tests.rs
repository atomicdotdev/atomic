//! CB-5C bridge-owned Git write journaling: operation before visibility.

use atomic_core::operation::{
    EffectTarget, EffectValue, GitHashAlgorithm, GitRefTarget, OperationKind,
};
use atomic_core::Hash;

use super::*;

fn sha1(hex: &str) -> GitRefTarget {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex byte"))
        .collect();
    GitRefTarget::Direct(
        atomic_core::GitObjectId::new(GitHashAlgorithm::Sha1, bytes).expect("valid oid"),
    )
}

fn sole_head(
    repository: &Repository,
    working_copy: WorkingCopyId,
) -> (atomic_core::OperationId, OperationKind) {
    let scope = atomic_core::operation::OperationScope::WorkingCopy(working_copy);
    let head = match repository
        .operation_log(scope, Some(1), false)
        .expect("load operation head")
        .head_state
    {
        crate::OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repository.operation_details(head).expect("operation details");
    (head, details.operation.payload().kind)
}

#[test]
fn bridge_git_ref_write_journals_operation_before_visibility_and_records_receipt() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    // The bridge observed the old target and intends the new one. The journal
    // entry must exist with its operation ID and intended effect BEFORE the
    // caller performs the Git write (here modeled by the returned value).
    let prepared = repository
        .prepare_bridge_git_ref_write(
            working_copy,
            "refs/heads/main",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"bridge write evidence"),
        )
        .expect("journal the bridge write");
    let (head, kind) = sole_head(&repository, working_copy);
    assert_eq!(head, prepared.operation_id);
    assert_eq!(kind, OperationKind::ExportGitRefs);
    let details = repository
        .operation_details(prepared.operation_id)
        .expect("details");
    assert_eq!(details.verification, OperationVerificationState::Prepared);
    assert_eq!(details.operation.payload().delta.effects.len(), 1);
    let effect = &details.operation.payload().delta.effects[0];
    assert!(matches!(
        effect.target,
        EffectTarget::GitRef { ref name }
            if name == "refs/heads/main"
    ));
    assert_eq!(effect.expected_old, EffectValue::GitRef(sha1(old_oid)));
    assert_eq!(effect.expected_new, EffectValue::GitRef(sha1(new_oid)));

    // The write happens here in the caller; the receipt is leased afterwards.
    let receipt = repository
        .record_bridge_git_ref_receipt(&prepared, Some(sha1(new_oid)))
        .expect("record the applied receipt");
    assert_eq!(
        receipt.payload().kind,
        atomic_core::operation::EffectReceiptKind::Applied
    );
    let operation_id = prepared.operation_id;
    repository
        .finalize_bridge_git_write(prepared, Some(sha1(new_oid)))
        .expect("verify the completed write");
    let details = repository
        .operation_details(operation_id)
        .expect("details");
    assert_eq!(details.verification, OperationVerificationState::Verified);
}

#[test]
fn bridge_git_write_with_unexpected_ref_target_fails_and_does_not_verify() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    let prepared = repository
        .prepare_bridge_git_ref_write(
            working_copy,
            "refs/heads/main",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"divergence evidence"),
        )
        .expect("journal the bridge write");

    // The caller observes a third value: someone else moved the ref between
    // observation and mutation. The receipt must be a durable rejection and
    // the operation must not verify.
    let observed =
        repository.record_bridge_git_ref_receipt(&prepared, Some(sha1("3333333333333333333333333333333333333333")));
    assert!(observed.is_err(), "a third ref value must fail closed");
    let details = repository
        .operation_details(prepared.operation_id)
        .expect("details");
    assert_eq!(
        details.verification,
        OperationVerificationState::LeaseRejected,
        "a rejected lease must leave the operation unverified"
    );
}

// ============================================================================
// Review C2: capture anchors are the only captured-operation authentication
// ============================================================================

/// Encode a durable post-rewrite JSON evidence record exactly the way the
/// hook journals one — including the operation capture token (review E2).
fn token_event_json(
    old_oid: &str,
    new_oid: &str,
    event_id: &str,
    capture_token: Option<&str>,
) -> Vec<u8> {
    let mut event = serde_json::json!({
        "version": 1,
        "record_type": "post-rewrite",
        "event_id": event_id,
        "recorded_at": "2026-09-12T00:00:00+00:00",
        "advisory": true,
        "rewritten": [{"old_oid": old_oid, "new_oid": new_oid}],
    });
    if let Some(token) = capture_token {
        event["capture_token"] = serde_json::Value::String(token.to_string());
    }
    serde_json::to_vec(&event).expect("serialize event")
}

fn token_hex(token: &[u8; 32]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn anchored_capture_authenticates_only_through_a_verified_active_operation() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    // An unknown operation can never anchor a capture: there is no active
    // captured operation for the event to belong to.
    let unknown = atomic_core::types::OperationId::from_bytes([0x5Au8; 32]);
    assert!(
        repository
            .capture_bridge_event_anchored(b"unanchored attempt", unknown)
            .is_err(),
        "an unknown operation must not accept an anchor"
    );
    assert_eq!(
        repository
            .bridge_event_capture_anchor(b"unanchored attempt")
            .expect("read anchor"),
        None,
        "a refused anchor leaves no row"
    );

    // The genuine flow: a real operation is PREPARED (it holds the active
    // head of its scope and has no verified receipt yet), the hook capture
    // happens DURING that window carrying the operation's minted capture
    // token (review E2), and the operation then completes with a verified
    // receipt. Only that sequence authenticates.
    let prepared = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/main",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"rewrite operation evidence"),
        )
        .expect("journal the active rewrite operation");

    // The preparation mints an immutable capture context for the operation.
    let minted = repository
        .bridge_ref_capture_token(prepared.operation_id)
        .expect("read the minted capture context")
        .expect("a prepared bridge write carries a minted capture token");
    let event_bytes = token_event_json(
        old_oid,
        new_oid,
        "0d3c0b31-86d1-4d44-9d35-8f0e0a41c001",
        Some(&token_hex(&minted)),
    );

    let _anchored = repository
        .capture_bridge_event_anchored(&event_bytes, prepared.operation_id)
        .expect("anchor the capture to the active operation");
    assert_eq!(
        repository
            .bridge_event_capture_anchor(&event_bytes)
            .expect("read anchor"),
        Some(prepared.operation_id),
        "the anchored capture must record its operation"
    );

    // Before the operation verifies, the evidence is not yet operation
    // linkage: the anchor names an operation that exists but has no verified
    // receipt.
    assert!(
        !repository
            .operation_has_verified_receipt(prepared.operation_id)
            .expect("read verification"),
        "a prepared-but-unverified operation does not authenticate its captures"
    );

    // An unrelated event captured by the plain advisory path stays
    // unanchored forever.
    repository
        .capture_bridge_event(b"direct hook stdin, no operation")
        .expect("advisory capture");
    assert_eq!(
        repository
            .bridge_event_capture_anchor(b"direct hook stdin, no operation")
            .expect("read anchor"),
        None,
        "plain captures carry no anchor"
    );

    // The operation completes and verifies; the anchored capture then
    // authenticates through the verified receipt.
    let receipt = repository
        .record_bridge_git_ref_receipt(&prepared, Some(sha1(new_oid)))
        .expect("record the applied receipt");
    let _ = receipt;
    let operation_id = prepared.operation_id;
    repository
        .finalize_bridge_git_write(prepared, Some(sha1(new_oid)))
        .expect("verify the completed write");
    assert!(
        repository
            .operation_has_verified_receipt(operation_id)
            .expect("read verification"),
        "the finalized operation must carry a verified receipt"
    );

    // Anchoring to the now-completed operation is refused: the capture would
    // be written AFTER the active window, not during it.
    assert!(
        repository
            .capture_bridge_event_anchored(b"late capture", operation_id)
            .is_err(),
        "an already-verified operation can no longer anchor captures"
    );
}

#[test]
fn anchored_capture_refuses_an_operation_that_is_not_the_active_head() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();

    // Prepare a genuine operation, then replace its head with another one:
    // the superseded operation no longer holds the active head, so it cannot
    // anchor captures even before finalization.
    let prepared = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/main",
            Some(sha1("1111111111111111111111111111111111111111")),
            sha1("2222222222222222222222222222222222222222"),
            Hash::of(b"evidence"),
        )
        .expect("journal the first operation");
    // Finalize it so the head moves on (the finalization CAS advances the
    // scope past the prepared operation).
    let operation_id = prepared.operation_id;
    let _ = repository.record_bridge_git_ref_receipt(&prepared, Some(sha1("2222222222222222222222222222222222222222")));
    let _ = repository.finalize_bridge_git_write(prepared, Some(sha1("2222222222222222222222222222222222222222")));

    assert!(
        repository
            .capture_bridge_event_anchored(b"stale capture", operation_id)
            .is_err(),
        "a non-head operation must not accept capture anchors"
    );
}

// ============================================================================
// Review D3: anchors are immutable, create-only, and bound to the exact
// active operation context (before/after leases)
// ============================================================================

#[test]
fn anchored_capture_refuses_conflicting_reanchoring() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    // A first prepared operation whose leases describe exactly this rewrite.
    let prepared = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/main",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"rewrite evidence"),
        )
        .expect("journal the active rewrite operation");
    // The capture is produced inside the first operation's capture context:
    // its bytes carry the token minted at preparation time (review E2).
    let minted = repository
        .bridge_ref_capture_token(prepared.operation_id)
        .expect("read the minted capture context")
        .expect("a prepared bridge write carries a minted capture token");
    let event = token_event_json(
        old_oid,
        new_oid,
        "6a05d2ec-6bd8-4d5b-8db9-1ef5c8d9b002",
        Some(&token_hex(&minted)),
    );
    let anchored = repository
        .capture_bridge_event_anchored(&event, prepared.operation_id)
        .expect("the matching operation accepts the anchor");

    // Idempotent: the SAME bytes anchored to the SAME operation succeed and
    // keep the anchor.
    let repeat = repository
        .capture_bridge_event_anchored(&event, prepared.operation_id)
        .expect("re-anchoring to the same operation is idempotent");
    assert_eq!(repeat, anchored, "the anchor is unchanged");
    assert_eq!(
        repository
            .bridge_event_capture_anchor(&event)
            .expect("read anchor"),
        Some(prepared.operation_id),
    );

    // Finalize the first operation so a second ref-write can be prepared
    // (the ordered operation lock is exclusive). The second operation
    // carries the SAME leases — only its identity differs — so a fresh
    // anchor attempt reaches the immutability check instead of a lease
    // refusal. The historical unconditional insert silently REPLACED the
    // anchor (review D3 fixture reanchored-sibling re-anchored the same
    // digest to refs/heads/UNRELATED operations).
    let first_operation_id = prepared.operation_id;
    repository
        .record_bridge_git_ref_receipt(&prepared, Some(sha1(new_oid)))
        .expect("record the applied receipt");
    repository
        .finalize_bridge_git_write(prepared, Some(sha1(new_oid)))
        .expect("verify the completed write");
    let second = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/next",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"second evidence"),
        )
        .expect("journal the second operation");
    let error = repository
        .capture_bridge_event_anchored(&event, second.operation_id)
        .expect_err("an anchor must never be re-anchored to another operation");
    assert!(
        error.to_string().contains("cannot be re-anchored"),
        "the refusal must name the immutability contract: {error}"
    );
    assert_eq!(
        repository
            .bridge_event_capture_anchor(&event)
            .expect("read anchor"),
        Some(first_operation_id),
        "the conflicting re-anchor must not replace the original anchor"
    );

    // The reader-side re-validation agrees: the anchor binds the event to
    // the first operation, not the second.
    assert!(
        repository
            .bridge_anchor_binds_operation(&event, first_operation_id)
            .expect("read binding"),
        "the anchor genuinely binds the matching operation"
    );
    assert!(
        !repository
            .bridge_anchor_binds_operation(&event, second.operation_id)
            .expect("read binding"),
        "an unrelated operation never binds the event"
    );
}

#[test]
fn anchored_capture_refuses_events_the_operation_does_not_describe() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();

    // An active ExportGitRefs operation whose leases name 3333 -> 4444.
    let prepared = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/UNRELATED",
            Some(sha1("3333333333333333333333333333333333333333")),
            sha1("4444444444444444444444444444444444444444"),
            Hash::of(b"unrelated ref write"),
        )
        .expect("journal the active operation");

    // The ordinary sibling pair from the D3 fixture: a durably captured
    // advisory event naming two ordinary sibling commits. No lease of the
    // active operation describes this rewrite, so the anchor must refuse —
    // a retrospective advisory capture may never be promoted by anchoring
    // it to an unrelated ref-write.
    let sibling_event = format!(
        "{} {}\n",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
    repository
        .capture_bridge_event(sibling_event.as_bytes())
        .expect("capture the advisory event");
    let error = repository
        .capture_bridge_event_anchored(sibling_event.as_bytes(), prepared.operation_id)
        .expect_err("an unrelated active ref-write must not adopt a foreign event");
    assert!(
        error
            .to_string()
            .contains("leases do not describe exactly this rewrite"),
        "the refusal must name the lease binding: {error}"
    );
    assert_eq!(
        repository
            .bridge_event_capture_anchor(sibling_event.as_bytes())
            .expect("read anchor"),
        None,
        "the refused promotion leaves no anchor row"
    );

    // The exact-match positive still works — but only through the
    // operation's capture context: the durable JSON evidence shape binds
    // exactly like the wire pair once it carries the minted capture token
    // (review E2).
    let old_oid = "3333333333333333333333333333333333333333";
    let new_oid = "4444444444444444444444444444444444444444";
    let minted = repository
        .bridge_ref_capture_token(prepared.operation_id)
        .expect("read the minted capture context")
        .expect("a prepared bridge write carries a minted capture token");
    let bytes = token_event_json(
        old_oid,
        new_oid,
        "d9f2b53c-0c46-4a30-9a2f-4c0b4e7f11aa",
        Some(&token_hex(&minted)),
    );
    repository
        .capture_bridge_event_anchored(&bytes, prepared.operation_id)
        .expect("the durable JSON evidence shape binds through the capture token");
    assert!(
        repository
            .bridge_anchor_binds_operation(&bytes, prepared.operation_id)
            .expect("read binding"),
        "the JSON-record anchor binds its operation"
    );

    // Review E2: the SAME OIDs in an advisory capture that carries NO
    // capture token — the shape an old advisory hook capture produced before
    // the operation existed — must refuse. Matching ref movement is not a
    // rewrite, and a retrospective advisory capture may never be promoted.
    let tokenless = token_event_json(
        old_oid,
        new_oid,
        "d9f2b53c-0c46-4a30-9a2f-4c0b4e7f11ab",
        None,
    );
    let error = repository
        .capture_bridge_event_anchored(&tokenless, prepared.operation_id)
        .expect_err("a matching-OID capture with no capture token must refuse");
    assert!(
        error.to_string().contains("carry no operation capture token"),
        "the refusal must name the missing capture context: {error}"
    );

    // A capture carrying a FOREIGN token is refused the same way.
    let foreign = token_event_json(
        old_oid,
        new_oid,
        "d9f2b53c-0c46-4a30-9a2f-4c0b4e7f11ac",
        Some(&token_hex(&[0x11u8; 32])),
    );
    let error = repository
        .capture_bridge_event_anchored(&foreign, prepared.operation_id)
        .expect_err("a foreign capture token must refuse");
    assert!(
        error
            .to_string()
            .contains("capture token does not match the operation's minted capture context"),
        "the refusal must name the token binding: {error}"
    );
}

// Review E2: temporal capture-context binding. An advisory capture that
// predates the prepared operation — even with exactly matching OIDs and a
// real later leased ref write through the public API — can never be
// retroactively promoted into operation linkage.
#[test]
fn anchored_capture_refuses_an_advisory_capture_that_predates_the_operation() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    // Step 1: the direct hook captures an unanchored advisory pair (no
    // active operation exists yet — no capture context to carry).
    let advisory = token_event_json(
        old_oid,
        new_oid,
        "3f5c1a97-0f0e-4d63-8f0a-5f1e2d3c4b5d",
        None,
    );
    repository
        .capture_bridge_event(&advisory)
        .expect("capture the advisory event");
    assert_eq!(
        repository
            .bridge_event_capture_anchor(&advisory)
            .expect("read anchor"),
        None,
        "an advisory capture carries no anchor"
    );

    // Step 2: only afterwards the public API prepares a ref-only write whose
    // leases match those exact OIDs — the E2 forgery shape.
    let prepared = repository
        .prepare_bridge_git_rewrite(
            working_copy,
            "refs/heads/ref-only-review",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"ref-only movement; no Git rewrite"),
        )
        .expect("journal the later ref-only write");

    // Step 3: the retrospective promotion is refused. The event predates the
    // operation and carries no capture token, so matching OIDs authenticate
    // nothing.
    let error = repository
        .capture_bridge_event_anchored(&advisory, prepared.operation_id)
        .expect_err("a pre-operation advisory capture must never anchor");
    assert!(
        error.to_string().contains("carry no operation capture token"),
        "the refusal must name the missing capture context: {error}"
    );
    assert_eq!(
        repository
            .bridge_event_capture_anchor(&advisory)
            .expect("read anchor"),
        None,
        "the refused promotion leaves no anchor row"
    );

    // The same event captured INSIDE the operation's capture context (the
    // hook ran with ATOMIC_BRIDGE_CAPTURE_TOKEN exported) binds and, after a
    // verified receipt, authenticates end to end.
    let minted = repository
        .bridge_ref_capture_token(prepared.operation_id)
        .expect("read the minted capture context")
        .expect("a prepared bridge write carries a minted capture token");
    let in_context = token_event_json(
        old_oid,
        new_oid,
        "8f4e2d6a-9b3c-4e7f-a1d2-3c4b5a6f7e8d",
        Some(&token_hex(&minted)),
    );
    repository
        .capture_bridge_event_anchored(&in_context, prepared.operation_id)
        .expect("an in-context capture binds to its operation");
    repository
        .record_bridge_git_ref_receipt(&prepared, Some(sha1(new_oid)))
        .expect("record the applied receipt");
    let operation_id = prepared.operation_id;
    repository
        .finalize_bridge_git_write(prepared, Some(sha1(new_oid)))
        .expect("verify the completed write");
    assert!(
        repository
            .bridge_anchor_binds_operation(&in_context, operation_id)
            .expect("read binding"),
        "the in-context capture authenticates through the verified operation"
    );
    assert!(
        !repository
            .bridge_anchor_binds_operation(&advisory, operation_id)
            .expect("read binding"),
        "the advisory capture never authenticates"
    );
}


// Review F2: an ordinary ExportGitRefs ref movement is NOT a rewrite
// execution, so it mints no capture context and can never become rewrite
// authority. This reproduces the exact TOKEN_REF_FORGERY_ACCEPTED probe
// shape at the public-API boundary: prepare a ref-only write, obtain a
// token-shaped capture, anchor it, and verify the movement — the anchor
// refuses because the operation has no minted rewrite context.
#[test]
fn ordinary_ref_movement_is_not_rewrite_authority() {
    let (_directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let old_oid = "1111111111111111111111111111111111111111";
    let new_oid = "2222222222222222222222222222222222222222";

    let prepared = repository
        .prepare_bridge_git_ref_write(
            working_copy,
            "refs/heads/ordinary-review",
            Some(sha1(old_oid)),
            sha1(new_oid),
            Hash::of(b"ordinary ref movement, no rewrite"),
        )
        .expect("journal the ordinary ref movement");

    // The ordinary movement mints no capture token: it is not a rewrite.
    assert_eq!(
        repository
            .bridge_ref_capture_token(prepared.operation_id)
            .expect("read capture context"),
        None,
        "an ordinary ref movement must not mint a rewrite capture token"
    );

    // A token-shaped capture cannot anchor: with no minted context there is
    // nothing for it to bind, so the ref-only operation must refuse.
    let forged = token_event_json(
        old_oid,
        new_oid,
        "forged-ref-only-movement",
        Some(&token_hex(&[0x11u8; 32])),
    );
    let error = repository
        .capture_bridge_event_anchored(&forged, prepared.operation_id)
        .expect_err("a ref-only operation must not accept a rewrite anchor");
    assert!(
        error.to_string().contains("no minted capture context"),
        "the refusal must name the missing rewrite context: {error}"
    );

    // The ordinary movement still verifies as a ref movement.
    repository
        .record_bridge_git_ref_receipt(&prepared, Some(sha1(new_oid)))
        .expect("record the applied receipt");
    let operation_id = prepared.operation_id;
    repository
        .finalize_bridge_git_write(prepared, Some(sha1(new_oid)))
        .expect("verify the completed ref movement");
    assert!(
        repository
            .operation_has_verified_receipt(operation_id)
            .expect("read verification"),
        "the ordinary ref movement is a verified operation"
    );
    assert!(
        !repository
            .bridge_anchor_binds_operation(&forged, operation_id)
            .expect("read binding"),
        "a token-bearing ordinary ref movement is never rewrite authority"
    );
}
