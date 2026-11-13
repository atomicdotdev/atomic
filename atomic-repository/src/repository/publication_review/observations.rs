//! Conservative receipt extraction from hash-verified retained provenance.
//! This recognizes observations, never successful execution or governing intent.
use super::*;
use atomic_canonical::publication_review::IntentObservationPin;
use atomic_core::change::{provenance_graph::ProvenanceNodeKind, ProvenanceGraph};
use std::collections::BTreeSet;

pub(super) fn resolve(
    repo: &Repository,
    hash: &Hash,
    graph: &ProvenanceGraph,
    keys: DidKeyProvider<'_>,
) -> Result<Vec<IntentObservationPin>, RepositoryError> {
    let ledger = repo.get_session_ledger(&graph.session_id)?;
    let mut turns: Vec<_> = ledger
        .iter()
        .flat_map(|(_, turns)| turns)
        .filter(|turn| turn.provenance_hash == *hash)
        .map(|turn| turn.turn_number)
        .collect();
    turns.sort_unstable();
    turns.dedup();
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for node in &graph.nodes {
        if !ids.insert(&node.id) {
            return Err(refused("duplicate provenance node identity"));
        }
        if node.kind != ProvenanceNodeKind::Execution {
            continue;
        }
        let Some(detail) = node.detail.as_deref() else {
            continue;
        };
        // Duplicate JSON keys are uncertainty, not a last-key-wins authority.
        let detail = atomic_canonical::jcs::admit_document(detail.as_bytes()).map_err(refused)?;
        let command = detail.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let output = detail
            .get("output_summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for (kind, reference) in receipts(command, output) {
            let (target, retained_revision, diagnostic) = match crate::intent_bridge::read_intent(
                repo,
                reference
                    .strip_prefix("urn:atomic:intent:")
                    .unwrap_or(&reference),
            )
            .and_then(|inputs| {
                // The ordinary authoring CLI accepts prefixes and local
                // shorthand. Historical evidence must name an exact key or
                // UID, independent of the inspecting user's defaults.
                let bare = reference
                    .strip_prefix("urn:atomic:intent:")
                    .unwrap_or(&reference);
                if !["id", "uid"].iter().any(|field| {
                    inputs
                        .frontmatter
                        .get(*field)
                        .and_then(|value| value.as_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case(bare))
                }) {
                    return Err(refused(
                        "receipt reference is not an exact intent key or UID",
                    ));
                }
                let current = crate::intent_bridge::lift(&inputs)?;
                let revision = retained_revision(repo, &reference, &inputs, &current.id, keys);
                Ok((current, revision))
            }) {
                Ok((intent, Ok(revision))) => (Some(intent.id), revision, None),
                Ok((intent, Err(error))) => (Some(intent.id), None, Some(error.to_string())),
                Err(error) => (None, None, Some(error.to_string())),
            };
            result.push(IntentObservationPin {
                role: Default::default(),
                provenance: hash.to_base32(),
                node: node.id.clone(),
                session: graph.session_id.clone(),
                turns: turns.clone(),
                kind: kind.into(),
                reference,
                resolution: match (target.is_some(), turns.len()) {
                    (true, 1) => "resolved",
                    (_, n) if n > 1 => "ambiguous",
                    _ => "unresolved",
                }
                .into(),
                intent: target,
                retained_revision,
                diagnostic: diagnostic.or_else(|| match turns.len() {
                    1 => None,
                    0 => Some("No ledger turn pins this provenance hash".into()),
                    _ => Some("Multiple ledger turns pin this provenance hash".into()),
                }),
            });
            if result.len() > 4096 {
                return Err(refused("intent observations exceed review budget"));
            }
        }
    }
    Ok(result)
}

fn retained_revision(
    repo: &Repository,
    reference: &str,
    inputs: &crate::intent_bridge::LiftInputs,
    expected_id: &str,
    keys: DidKeyProvider<'_>,
) -> Result<Option<atomic_canonical::publication_review::SignedRevisionPin>, RepositoryError> {
    use crate::intent_bridge::Attestation;
    let (node, relation) = match crate::intent_bridge::load_attestation(repo, reference, inputs)? {
        Attestation::None => return Ok(None),
        Attestation::Fresh(node) | Attestation::Stale(node) => {
            // Mutable freshness sidecars never authenticate the source relation.
            let did = node
                .attributed_to
                .as_deref()
                .ok_or_else(|| refused("retained revision has no author"))?;
            let key = atomic_identity::keypair::PublicKey::from_bytes(
                &keys(did).ok_or_else(|| refused("retained revision key unavailable"))?,
            )
            .map_err(refused)?;
            if node.id != expected_id || !atomic_canonical::did::did_matches_public_key(did, &key) {
                return Err(refused("retained signed revision identity mismatch"));
            }
            atomic_canonical::verify(&node, &key).map_err(refused)?;
            let expected = atomic_canonical::proof::prepare_attestation(
                crate::intent_bridge::lift(inputs)?.to_value(),
                &key,
            )
            .value;
            let relation = if atomic_canonical::proof::signing_view(&node.to_value()) == expected {
                "current-source"
            } else {
                "retained-not-current"
            };
            (node, (key, relation))
        }
    };
    Ok(Some(
        atomic_canonical::publication_review::SignedRevisionPin {
            signed_hash: atomic_canonical::hash::content_hash(&node.to_value()),
            public_key: relation.0.to_base32(),
            source_relation: relation.1.into(),
        },
    ))
}

/// Deliberately not a shell interpreter. Only exact lifecycle receipt lines
/// paired with the corresponding CLI command are recognized. Quoted examples,
/// arbitrary mentions, filename matches and timestamps do not resolve evidence.
fn receipts(command: &str, output: &str) -> Vec<(&'static str, String)> {
    let mut result = BTreeSet::new();
    for (verb, prefix, kind) in [
        ("new", "Created intent: ", "created"),
        ("update", "Updated intent: ", "updated"),
        ("attest", "Attested intent: ", "attested-receipt"),
        ("verify", "Verified intent: ", "verified-receipt"),
    ] {
        // Segments permit retained multi-command invocations, but do not infer
        // success of any segment or execute anything in the transcript.
        let commands: Vec<_> = command
            .split([';', '\n'])
            .filter_map(|segment| {
                let words: Vec<_> = segment.split_whitespace().collect();
                (words.len() >= 4
                    && words[0].rsplit('/').next() == Some("atomic")
                    && words[1] == "intent"
                    && words[2] == verb)
                    .then_some(words)
            })
            .collect();
        for line in output.lines() {
            if let Some(reference) = line.trim().strip_prefix(prefix) {
                let reference = reference.trim();
                if reference.is_empty() || reference.contains(char::is_whitespace) {
                    continue;
                }
                if commands
                    .iter()
                    .any(|words| verb == "new" || words[3] == reference)
                {
                    result.insert((kind, reference.to_owned()));
                }
            }
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_extraction_does_not_promote_mentions_or_failure_to_truth() {
        assert_eq!(
            receipts(
                "atomic intent new \"work\"; atomic vault sync",
                "Created intent: REAC::lee::22\n  file: .vault/intents/uid/intent.md"
            ),
            vec![("created", "REAC::lee::22".into())]
        );
        assert!(receipts(
            "echo 'atomic intent attest REAC::lee::22'",
            "Attested intent: REAC::lee::22"
        )
        .is_empty());
        assert!(receipts(
            "atomic intent attest REAC::lee::23",
            "Attested intent: REAC::lee::22"
        )
        .is_empty());
        assert!(receipts("atomic intent verify REAC::lee::22", "attestation is stale").is_empty());
        assert_eq!(
            receipts(
                "atomic intent attest REAC::lee::22",
                "Attested intent: REAC::lee::22"
            )[0]
            .0,
            "attested-receipt"
        );
    }
}
