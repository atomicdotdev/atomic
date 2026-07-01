//! The gate — a SHACL-style validator that decides whether a node may enter
//! the graph. M0 hand-codes the shapes in Rust; a real SHACL evaluator over
//! Turtle shapes arrives in a later milestone. The *semantics* are what matter
//! here and they match the doc:
//!
//!   - **Presence is enforced, content is left honest.** The gate requires the
//!     reason *exists* (a `why`), authorship is present, and the proof is
//!     present. It never grades the reason's prose.
//!   - **`status: done` is granted, not written.** A caller advancing an intent
//!     to `done` must pass the gate first.
//!   - **A met acceptance criterion must carry evidence.** `acStatus = met`
//!     without `verifiedBy` + `evidence` is rejected (a checked box with no
//!     proof fails).
//!   - **Closed world.** Unknown status values are rejected.
//!
//! The gate never auto-fixes a load-bearing fact — it only reports.

use crate::node::CanonicalNode;
use crate::vocab;

/// A single shape violation.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// The `@id` of the focus node (or sub-node) that failed.
    pub focus_node: String,
    /// The shape that was violated.
    pub shape: String,
    /// The property path at fault (if any).
    pub path: Option<String>,
    /// Human-readable message.
    pub message: String,
}

/// The result of validating a node.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    pub conforms: bool,
    pub results: Vec<Violation>,
}

impl ValidationReport {
    fn from(results: Vec<Violation>) -> Self {
        ValidationReport {
            conforms: results.is_empty(),
            results,
        }
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.conforms {
            return write!(f, "conforms: yes");
        }
        writeln!(f, "conforms: no ({} violation(s))", self.results.len())?;
        for v in &self.results {
            let path = v.path.as_deref().unwrap_or("-");
            writeln!(f, "  ✗ [{}] {} ({}): {}", v.shape, v.focus_node, path, v.message)?;
        }
        Ok(())
    }
}

/// Validate a canonical Intent node against `IntentShape` (+ sub-shapes).
pub fn validate_intent(node: &CanonicalNode) -> ValidationReport {
    let mut out = Vec::new();
    let focus = &node.id;

    // status ∈ closed set, exactly one (it is a single field, so cardinality holds).
    if !vocab::is_known_intent_status(&node.status) {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("status".into()),
            message: format!(
                "status '{}' is not one of {:?}",
                node.status,
                vocab::INTENT_STATUS
            ),
        });
    }

    // attributedTo must be present (sh:class prov:Agent deferred — see M0 notes).
    if node.attributed_to.as_deref().unwrap_or("").is_empty() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("attributedTo".into()),
            message: "author (attributedTo) must be present as a DID".into(),
        });
    }

    // proof must be present.
    if node.proof.is_none() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("proof".into()),
            message: "intent must carry a Data Integrity proof".into(),
        });
    }

    // presence-enforced: a reason must exist (content left honest).
    match &node.why {
        Some(w) if !w.trim().is_empty() => {}
        _ => out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("why".into()),
            message: "a reason (why) must be present — its content is not graded".into(),
        }),
    }

    // Each acceptance criterion satisfies AcceptanceCriterionShape.
    for ac in &node.has_acceptance_criterion {
        if !vocab::is_known_ac_status(&ac.ac_status) {
            out.push(Violation {
                focus_node: ac.id.clone(),
                shape: "AcceptanceCriterionShape".into(),
                path: Some("acStatus".into()),
                message: format!(
                    "acStatus '{}' is not one of {:?}",
                    ac.ac_status,
                    vocab::AC_STATUS
                ),
            });
        }
        if ac.ac_status == "met" {
            let has_verifier = !ac.verified_by.as_deref().unwrap_or("").is_empty();
            let has_evidence = !ac.evidence.as_deref().unwrap_or("").is_empty();
            if !has_verifier || !has_evidence {
                out.push(Violation {
                    focus_node: ac.id.clone(),
                    shape: "AcceptanceCriterionShape".into(),
                    path: Some("acStatus".into()),
                    message: "a met acceptance criterion must carry verifiedBy and evidence".into(),
                });
            }
        }
    }

    ValidationReport::from(out)
}
