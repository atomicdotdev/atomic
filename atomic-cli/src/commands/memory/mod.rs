//! `atomic memory` — the canonical "record the why" engine surface for MEMORIES.
//!
//! This is a NEW top-level command, a sibling of the raw `atomic vault memory`
//! tree ([`crate::commands::vault::memory`]): that tree manages the raw/legacy
//! vault memory entries (the agent stdin `write` flow, plus `list`/`show`),
//! while this one drives the `atomic-canonical` engine — lifting a stored
//! memory into a canonical JSON-LD [`MemoryNode`](atomic_canonical::MemoryNode),
//! gating it, attesting it, and rendering it. It is the memory analogue of the
//! `atomic intent` family.
//!
//! # Verbs
//!
//! ```text
//! atomic memory new --kind <k>       Scaffold a directive/frontmatter memory
//! atomic memory show <id>            Render a memory as a read-time projection
//! atomic memory validate <id|path>   Gate a memory against the canonical shapes
//! atomic memory attest <id>          Sign a memory into a tracked attestation
//! atomic memory verify <id>          Verify a signed memory's attestation
//! ```
//!
//! # Persistence — additive by construction
//!
//! A memory is a single flat tracked vault entry at `memory/<id>.md` (created
//! via the GENERIC `vault_store` path, exactly like the raw `vault memory
//! write`). Attestation NEVER mutates the stored memory entry, its
//! `content_hash`, or the manifest merkle: the attested node + proof are written
//! as a first-class tracked `attestation` entry at
//! `attestations/memory/<id>/attested.md` (dual-written to a legacy
//! `.atomic/canonical/memory/<id>/` sidecar during the transition). See
//! [`bridge`].

use clap::Subcommand;

use crate::commands::Command;
use crate::error::CliResult;

pub mod attest;
pub mod bridge;
pub mod list;
pub mod new;
pub mod show;
pub mod validate;
pub mod verify;

pub use attest::MemoryAttest;
pub use list::MemoryList;
pub use new::MemoryNew;
pub use show::MemoryShow;
pub use validate::MemoryValidate;
pub use verify::MemoryVerify;

/// Subcommands for the canonical memory engine.
#[derive(Subcommand, Debug)]
pub enum MemoryCommands {
    /// Scaffold a new directive-based memory into the vault.
    ///
    /// Writes a `memory/<id>.md` entry with a frontmatter spine
    /// (`uid`/`memoryKind`/`status`/`createdAt`/`about`) and a `:::memory`
    /// container body so it round-trips cleanly through
    /// lift → validate → attest.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory new --kind constraint
    /// atomic memory new --kind lesson --about urn:atomic:module:storage
    /// ```
    New(MemoryNew),

    /// Show a memory as a rendered read-time projection.
    ///
    /// Lifts the memory and renders it via the canonical projection. This is a
    /// pure read: it does not gate and does not require a proof, so it works on
    /// a plain (un-attested) memory.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory show 01j8zc4r8t
    /// atomic memory show 01j8zc4r8t --json
    /// ```
    Show(MemoryShow),

    /// Validate a memory against the canonical shapes (the authoring gate).
    ///
    /// Lifts the memory (by id from the vault, or from a markdown file path)
    /// into a canonical node and runs the SHACL-style gate. An un-attested
    /// memory reports violations for the missing proof and author
    /// (`attributedTo`) — this is correct: `validate` is the authoring check,
    /// `attest` is what makes it conform. Exits nonzero if the report does not
    /// conform.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory validate 01j8zc4r8t
    /// atomic memory validate ./note.md
    /// atomic memory validate 01j8zc4r8t --json
    /// ```
    Validate(MemoryValidate),

    /// Attest a memory: sign it into a tracked attestation entry.
    ///
    /// Gates the memory first (refusing to sign a node that fails for a reason
    /// signing cannot fix), fills `attributedTo` from the signing identity,
    /// signs the canonical node, re-gates the attested result, and writes the
    /// node (with embedded contentHash + proof) to a tracked vault entry at
    /// `attestations/memory/<id>/attested.md` (dual-written to a legacy sidecar).
    /// The source memory is unchanged and the vault's hashing scheme is
    /// untouched — a new attestation entry is simply added.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory attest 01j8zc4r8t
    /// atomic memory attest 01j8zc4r8t --identity alice-work
    /// ```
    Attest(MemoryAttest),

    /// Verify a signed memory's attestation.
    ///
    /// Reads the attestation written by `attest`, confirms it is still fresh
    /// (the memory hasn't changed since it was signed), and verifies its content
    /// hash + Ed25519 Data Integrity proof. Exits nonzero if there is no
    /// attestation, it is stale, or the signature does not verify.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory verify 01j8zc4r8t
    /// atomic memory verify 01j8zc4r8t --identity alice-work
    /// ```
    Verify(MemoryVerify),

    /// List the vault's memories, attestation-aware.
    ///
    /// The canonical-family analogue of `atomic vault memory list`: alongside the
    /// id it adds the memory's kind/status/about-count (lifted, never graded), an
    /// `attested` column (fresh / stale / –) and a `verifies` column (✓ / ✗ / –)
    /// with the same DID-match-then-verify rule as `atomic intent list`.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic memory list
    /// atomic memory list --identity alice-work
    /// atomic memory list --json
    /// ```
    List(MemoryList),
}

/// Record the "why" for durable context: lift, validate, attest, and render
/// canonical memories.
///
/// A sibling of the raw `atomic vault memory` tree. The verbs operate on real
/// vault memories through the `atomic-canonical` engine, persisting attestations
/// additively as tracked vault entries — the stored memory entries are never
/// mutated.
#[derive(Debug, clap::Args)]
#[command(name = "memory")]
pub struct Memory {
    #[command(subcommand)]
    pub command: MemoryCommands,
}

impl Command for Memory {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            MemoryCommands::New(cmd) => cmd.run(),
            MemoryCommands::Show(cmd) => cmd.run(),
            MemoryCommands::Validate(cmd) => cmd.run(),
            MemoryCommands::Attest(cmd) => cmd.run(),
            MemoryCommands::Verify(cmd) => cmd.run(),
            MemoryCommands::List(cmd) => cmd.run(),
        }
    }
}

/// A validation-failure error: the canonical gate reported a non-conforming
/// node. Kept as a `Parser`-free helper so `main.rs`'s `err.exit_code()` fires
/// with a distinct, user-fixable exit code (2, a usage/authoring error) rather
/// than the internal-bug code. Copied verbatim from `intent/mod.rs`.
pub(crate) fn validation_failed(message: impl Into<String>) -> crate::error::CliError {
    crate::error::CliError::InvalidArgument {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::memory::lift_memory;
    use atomic_canonical::{validate_memory, verify_memory};
    use atomic_core::pristine::VaultEntryType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use atomic_repository::Repository;
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    /// END-TO-END at the persistence + engine layer, exercising the exact writes
    /// and reads the CLI verbs perform (the CLI resolves identity via a global
    /// IdentityStore, which we cannot mutate in a unit test, so here we sign with
    /// an in-test keypair; the tracked/sidecar plumbing is otherwise identical).
    ///
    /// Chain: new -> validate (2 fillable violations, nonzero) -> attest (tracked
    /// entry + legacy sidecar, self-verify) -> validate again (Fresh CONFORMS) ->
    /// verify (Fresh, OK) -> show (attested node). Asserts createdAt stability so
    /// the re-lift signing bytes stay identical across the whole chain.
    #[test]
    fn end_to_end_new_validate_attest_verify_show() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // --- new: write memory/<id>.md with a stable createdAt spine ----------
        let id = "01j8zc4r8t".to_string();
        let created_at = "2026-07-01T12:00:00+00:00";
        let vault_path = bridge::normalize_memory_path(&id);
        let mut spine = Map::new();
        spine.insert("uid".into(), Value::String(id.clone()));
        spine.insert("memoryKind".into(), Value::String("constraint".into()));
        spine.insert("status".into(), Value::String("active".into()));
        spine.insert("createdAt".into(), Value::String(created_at.into()));
        let scaffold_body = ":::memory\nThe durable fact.\n:::\n";
        repo.vault_store(
            &vault_path,
            VaultEntryType::Memory,
            scaffold_body.as_bytes().to_vec(),
            serde_json::to_string(&spine).unwrap(),
        )
        .unwrap();
        repo.vault_materialize(&vault_path).unwrap();

        // --- validate (unattested): EXACTLY the two fillable violations -------
        let inputs = bridge::read_memory(&repo, &id).unwrap();
        assert!(
            matches!(
                bridge::load_attestation(&repo, &id, &inputs).unwrap(),
                bridge::Attestation::None
            ),
            "no attestation yet"
        );
        let unattested = bridge::lift(&inputs).unwrap();
        // createdAt survived the round-trip and is present on the lifted node.
        assert_eq!(unattested.created_at, created_at);
        let report = validate_memory(&unattested);
        assert!(!report.conforms, "un-attested memory does not conform");
        let mut paths: Vec<_> = report
            .results
            .iter()
            .map(|v| v.path.clone().unwrap_or_default())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["attributedTo".to_string(), "proof".to_string()]);

        // --- attest: sign + write tracked entry + legacy sidecar -------------
        let kp = KeyPair::generate();
        let identity = Identity::new("tester", &kp);
        let node = atomic_canonical::lift_and_attest_memory(
            &inputs.frontmatter,
            &inputs.body,
            &identity,
            &kp,
        )
        .unwrap();
        assert!(validate_memory(&node).conforms, "attested node conforms");
        verify_memory(&node, &kp.public).expect("self-verify");
        assert_eq!(
            node.created_at, created_at,
            "createdAt stable through attest"
        );

        // legacy sidecar
        let sidecar_path = bridge::attested_sidecar_path(&repo, &id);
        std::fs::create_dir_all(sidecar_path.parent().unwrap()).unwrap();
        let mut artifact = Map::new();
        artifact.insert("node".into(), node.to_value());
        let mut source = Map::new();
        source.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        artifact.insert("source".into(), Value::Object(source));
        std::fs::write(
            &sidecar_path,
            serde_json::to_string_pretty(&Value::Object(artifact)).unwrap(),
        )
        .unwrap();

        // tracked entry
        let mut body = serde_json::to_string_pretty(&node.to_value()).unwrap();
        body.push('\n');
        let mut fm = Map::new();
        fm.insert("memoryId".into(), Value::String(bridge::memory_id(&id)));
        fm.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        let apath = bridge::attestation_vault_path(&id);
        assert_eq!(apath, "attestations/memory/01j8zc4r8t/attested.md");
        repo.vault_store(
            &apath,
            VaultEntryType::Attestation,
            body.into_bytes(),
            serde_json::to_string(&fm).unwrap(),
        )
        .unwrap();

        // --- validate again: Fresh attested node CONFORMS --------------------
        let inputs2 = bridge::read_memory(&repo, &id).unwrap();
        let fresh = match bridge::load_attestation(&repo, &id, &inputs2).unwrap() {
            bridge::Attestation::Fresh(n) => *n,
            other => panic!("expected Fresh, got {}", attestation_label(&other)),
        };
        assert!(validate_memory(&fresh).conforms, "attested memory conforms");
        assert_eq!(
            fresh.created_at, created_at,
            "createdAt stable through re-read"
        );

        // --- verify: Fresh, verify_memory OK ---------------------------------
        verify_memory(&fresh, &kp.public).expect("verify Fresh attested node");

        // --- show: renders the attested node with author + proof -------------
        let rendered = atomic_canonical::render_memory(&fresh, atomic_canonical::Target::Cli);
        assert!(rendered.contains("Memory (constraint)"));
        assert!(
            rendered.contains("The durable fact."),
            "rendered text present"
        );
        assert!(
            fresh.attributed_to.is_some(),
            "author present on attested node"
        );
        assert!(fresh.proof.is_some(), "proof present on attested node");

        // --- regression: `vault memory list` (prefix memory/) does NOT surface
        //     the attestation entry (it lives under attestations/memory/). ------
        let listed = repo.vault_list("memory/", None).unwrap();
        assert!(
            listed.iter().all(|e| !e.path.starts_with("attestations/")),
            "attestation entries must not appear under the memory/ prefix"
        );
        assert!(
            listed.iter().any(|e| e.path == vault_path),
            "the memory entry itself is listed under memory/"
        );
    }

    fn attestation_label(a: &bridge::Attestation) -> &'static str {
        match a {
            bridge::Attestation::None => "None",
            bridge::Attestation::Fresh(_) => "Fresh",
            bridge::Attestation::Stale(_) => "Stale",
        }
    }

    /// Guard: the scaffold body lifts to non-empty text (attest-ready).
    #[test]
    fn scaffold_lifts_to_nonempty_text() {
        let fm: Map<String, Value> = serde_json::json!({
            "uid": "x",
            "memoryKind": "context",
            "createdAt": "2026-07-01T00:00:00+00:00",
        })
        .as_object()
        .unwrap()
        .clone();
        let node =
            lift_memory(&fm, ":::memory\n<!-- stub comment counts as text -->\n:::").unwrap();
        assert!(!node.text.trim().is_empty());
    }
}
