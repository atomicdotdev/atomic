//! `atomic intent list` — an ATTESTATION-AWARE listing of the vault's intents.
//!
//! This is the canonical-family analogue of `atomic vault intent list` /
//! `atomic vault list --type intent`: those render a plain path/size/status
//! table, while this one adds the two columns that only the canonical engine can
//! answer — whether each intent carries a *fresh* attestation, and whether that
//! attestation cryptographically *verifies* against the resolving identity.
//!
//! # Columns
//!
//! ```text
//! humanKey   status   attested   verifies
//! PIMO-1     todo     fresh      ✓
//! PIMO-2     doing    –          –
//! PIMO-3     done     stale      –
//! ```
//!
//! - `humanKey` — the `IntentInfo.id` (e.g. `PIMO-1`), the manifest key.
//! - `status`   — the raw status column from the intent manifest.
//! - `attested` — `fresh` / `stale` / `–` via [`bridge::load_attestation`].
//! - `verifies` — `✓` / `✗` / `–` via the DID-match-then-verify rule (see
//!   [`compute_verifies`]): `–` when there is no attestation, when it is stale,
//!   when there is no resolvable identity, or when the attestation was signed by
//!   a *different* identity (a merely-unresolvable signer is NOT a failure);
//!   `✗` only when a same-signer node fails its hash/signature check.
//!
//! Enumeration reuses [`Repository::vault_intent_list`] — the exact source
//! `atomic vault intent list` uses. Attestation entries live under
//! `attestations/…` and are never in `manifest.intents`, so they can never
//! surface as a row.

use clap::Parser;

use atomic_canonical::did::did_for_public_key;
use atomic_identity::keypair::PublicKey;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// The EN DASH used everywhere for a "not applicable" cell.
const NA: &str = "–";

/// List the vault's intents with attestation + verification status.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct IntentList {
    /// Identity whose public key to verify attestations against. Defaults to the
    /// current default identity. If no identity can be resolved at all, every
    /// `verifies` cell is `–` rather than the whole command erroring.
    #[arg(long)]
    pub identity: Option<String>,

    /// Output as JSON (an array of {id,status,attested,verifies}).
    #[arg(long)]
    pub json: bool,
}

/// Agent guidance for `atomic intent list`.
///
/// Note the asymmetry encoded in the first `fails:` row: with no vault this
/// prints `No intents found.` and exits 0, while `intent new` in the same repo
/// exits 3.
pub const DOC: Doc = Doc {
    when: "orienting: what intents exist and are they signed",
    run: "intent list --json",
    then: &[
        Ref {
            cmd: "intent show <ID> --json",
            note: "read one row's contents",
        },
        Ref {
            cmd: "intent attest <ID>",
            note: "for rows showing stale or -",
        },
    ],
    instead: &[
        Ref {
            cmd: "vault list --type intent",
            note: "paths and sizes, no attestation columns",
        },
    ],
    fails: &[
        Fail {
            cond: "no vault: prints 'No intents found', not an error",
            exit: 0,
            fix: Ref {
                cmd: "vault init",
                note: "",
            },
        },
        Fail {
            cond: "--identity unknown (a missing DEFAULT is soft, this is not)",
            exit: 3,
            fix: Ref {
                cmd: "identity list",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

/// The classification of one intent's attestation, for a table/JSON row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attested {
    Fresh,
    Stale,
    None,
}

impl Attested {
    /// The lowercase table token.
    fn table(self) -> &'static str {
        match self {
            Attested::Fresh => "fresh",
            Attested::Stale => "stale",
            Attested::None => NA,
        }
    }

    /// The machine-consumable JSON token.
    fn json(self) -> &'static str {
        match self {
            Attested::Fresh => "fresh",
            Attested::Stale => "stale",
            Attested::None => "none",
        }
    }
}

/// The result of the DID-match-then-verify rule for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verifies {
    /// Same-signer, fresh attestation that cryptographically verified.
    Yes,
    /// Same-signer, fresh attestation whose hash/signature FAILED.
    No,
    /// No attestation, stale, no resolvable identity, or a different signer.
    Na,
}

impl Verifies {
    /// The human-table glyph.
    fn table(self) -> &'static str {
        match self {
            Verifies::Yes => "✓",
            Verifies::No => "✗",
            Verifies::Na => NA,
        }
    }

    /// The machine-consumable JSON token.
    fn json(self) -> &'static str {
        match self {
            Verifies::Yes => "yes",
            Verifies::No => "no",
            Verifies::Na => "na",
        }
    }
}

/// A fully-computed intent row.
struct Row {
    human_key: String,
    status: String,
    attested: Attested,
    verifies: Verifies,
}

/// The verifying identity resolved ONCE for the whole list: its public key and
/// its `did:atomic`. `None` means no identity could be resolved (soft-fail), in
/// which case every `verifies` cell is `–`.
struct Verifier {
    public_key: PublicKey,
    did: String,
}

/// Resolve the verifying identity exactly as the verify verb does, but SOFT: a
/// missing default (or no store) yields `Ok(None)` so the list still renders
/// with all-`–` verifies. A `--identity <name>` that does not exist IS a hard
/// error (`IdentityNotFound`), matching the verify verb.
fn resolve_verifier(name: &Option<String>) -> CliResult<Option<Verifier>> {
    let store = match IdentityStore::open_default() {
        Ok(s) => s,
        // No store at all ⇒ soft-fail to "no identity" (all verifies '–').
        Err(_) => return Ok(None),
    };
    let identity = if let Some(name) = name {
        // A named-but-missing identity is a hard error (parity with verify).
        Some(
            store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?,
        )
    } else {
        // No default resolvable ⇒ soft-fail, don't abort the list.
        store.get_default().ok().flatten()
    };
    Ok(identity.map(|id| Verifier {
        did: did_for_public_key(&id.public_key),
        public_key: id.public_key,
    }))
}

/// The DID-match-then-verify rule for an intent. Pure over its inputs so it can
/// be unit-tested without a global `IdentityStore`.
///
/// - `attested` None ⇒ `Na`.
/// - `attested` Stale ⇒ `Na` (staleness is surfaced by the attested column;
///   there is nothing fresh to cryptographically verify — do NOT feed a Stale
///   node to `verify`, which would report `✗` on a changed-but-validly-signed
///   node).
/// - `attested` Fresh:
///     - no resolvable identity ⇒ `Na`;
///     - the attestation's signer DID does not match the resolving key ⇒ `Na`
///       (a legitimately other-signed node, NOT a failure — showing `✗` here
///       would be a false negative);
///     - same signer ⇒ run `verify`: `Ok` ⇒ `Yes`, `Err` ⇒ `No`.
fn compute_verifies(attested: &bridge::Attestation, verifier: Option<&Verifier>) -> Verifies {
    let node = match attested {
        bridge::Attestation::Fresh(node) => node,
        bridge::Attestation::Stale(_) | bridge::Attestation::None => return Verifies::Na,
    };
    let verifier = match verifier {
        Some(v) => v,
        None => return Verifies::Na,
    };
    // DID pre-check FIRST: a different (or absent) signer is `–`, not `✗`.
    match node.attributed_to.as_deref() {
        Some(signer) if signer == verifier.did => {}
        _ => return Verifies::Na,
    }
    // Same signer ⇒ the only path that can yield `✗` is a real hash/sig failure.
    match atomic_canonical::verify(node, &verifier.public_key) {
        Ok(()) => Verifies::Yes,
        Err(_) => Verifies::No,
    }
}

/// Classify a loaded attestation into the `attested` column token.
fn classify(attested: &bridge::Attestation) -> Attested {
    match attested {
        bridge::Attestation::Fresh(_) => Attested::Fresh,
        bridge::Attestation::Stale(_) => Attested::Stale,
        bridge::Attestation::None => Attested::None,
    }
}

/// Compute one intent's row. A read/lift/attestation failure for a SINGLE intent
/// degrades that row's attested/verifies to `–` rather than aborting the whole
/// list.
fn compute_row(
    repo: &Repository,
    info: &atomic_repository::IntentInfo,
    verifier: Option<&Verifier>,
) -> Row {
    // read_intent → inputs → load_attestation. Any failure ⇒ '–' columns.
    let attestation = bridge::read_intent(repo, &info.id)
        .and_then(|inputs| bridge::load_attestation(repo, &info.id, &inputs));
    let (attested, verifies) = match attestation {
        Ok(a) => (classify(&a), compute_verifies(&a, verifier)),
        Err(_) => (Attested::None, Verifies::Na),
    };
    Row {
        human_key: info.id.clone(),
        status: info.status.clone(),
        attested,
        verifies,
    }
}

impl Command for IntentList {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Resolve the verifying identity ONCE (soft-fail to "no identity").
        let verifier = resolve_verifier(&self.identity)?;

        // Enumerate via the SAME source `atomic vault intent list` uses; already
        // sorted by id. Attestation entries are never in `manifest.intents`.
        let intents = repo.vault_intent_list(None).map_err(CliError::Repository)?;

        let rows: Vec<Row> = intents
            .iter()
            .map(|info| compute_row(&repo, info, verifier.as_ref()))
            .collect();

        if self.json {
            let json: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.human_key,
                        "status": r.status,
                        "attested": r.attested.json(),
                        "verifies": r.verifies.json(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
            return Ok(());
        }

        if rows.is_empty() {
            println!("No intents found.");
            return Ok(());
        }

        // Fixed-width, left-aligned columns with a header row.
        let key_w = rows
            .iter()
            .map(|r| r.human_key.chars().count())
            .chain(std::iter::once("humanKey".chars().count()))
            .max()
            .unwrap_or(8);
        let status_w = rows
            .iter()
            .map(|r| r.status.chars().count())
            .chain(std::iter::once("status".chars().count()))
            .max()
            .unwrap_or(6);

        println!(
            "  {:<key_w$}  {:<status_w$}  {:<8}  verifies",
            "humanKey", "status", "attested",
        );
        for r in &rows {
            println!(
                "  {:<key_w$}  {:<status_w$}  {:<8}  {}",
                r.human_key,
                r.status,
                r.attested.table(),
                r.verifies.table(),
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::{lift_and_attest, CanonicalNode};
    use atomic_core::pristine::VaultEntryType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use atomic_repository::IntentCreateOptions;
    use serde_json::Value;
    use tempfile::tempdir;

    /// Create a repo+vault and N intents; return (repo, sorted ids, tempdir).
    fn repo_with_intents(n: usize) -> (Repository, Vec<String>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            let result = repo
                .vault_intent_create(IntentCreateOptions {
                    title: format!("Intent {i}"),
                    priority: Some("medium".to_string()),
                    assignee: None,
                    labels: Vec::new(),
                    session_id: None,
                    turn_id: None,
                })
                .unwrap();
            ids.push(result.id);
        }
        ids.sort();
        (repo, ids, dir)
    }

    /// Lift+attest a stored intent with a given keypair, returning the node.
    fn attest_with(repo: &Repository, id: &str, kp: &KeyPair) -> CanonicalNode {
        let inputs = bridge::read_intent(repo, id).unwrap();
        let identity = Identity::new("tester", kp);
        lift_and_attest(&inputs.frontmatter, &inputs.body, &identity, kp).unwrap()
    }

    /// Mirror `attest`'s tracked-vault write (body = pretty JSON + '\n').
    fn write_tracked(repo: &Repository, id: &str, node: &CanonicalNode) {
        let inputs = bridge::read_intent(repo, id).unwrap();
        let mut body = serde_json::to_string_pretty(&node.to_value()).unwrap();
        body.push('\n');
        let mut fm = serde_json::Map::new();
        fm.insert(
            "intentId".into(),
            Value::String(bridge::normalized_id(repo, id).unwrap()),
        );
        fm.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        let vpath = bridge::attestation_vault_path(repo, id).unwrap();
        repo.vault_store(
            &vpath,
            VaultEntryType::Attestation,
            body.into_bytes(),
            serde_json::to_string(&fm).unwrap(),
        )
        .unwrap();
    }

    /// Compute a single row using an explicit verifier keypair (no global store).
    fn row_for(repo: &Repository, id: &str, kp: Option<&KeyPair>) -> Row {
        let verifier = kp.map(|kp| Verifier {
            did: did_for_public_key(&kp.public),
            public_key: kp.public.clone(),
        });
        let info = repo
            .vault_intent_list(None)
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        compute_row(repo, &info, verifier.as_ref())
    }

    #[test]
    fn unattested_row_is_na_na() {
        let (repo, ids, _dir) = repo_with_intents(1);
        let kp = KeyPair::generate();
        let row = row_for(&repo, &ids[0], Some(&kp));
        assert_eq!(row.attested, Attested::None);
        assert_eq!(row.verifies, Verifies::Na);
        assert_eq!(row.attested.table(), "–");
        assert_eq!(row.verifies.table(), "–");
    }

    #[test]
    fn fresh_same_signer_verifies() {
        let (repo, ids, _dir) = repo_with_intents(1);
        let kp = KeyPair::generate();
        let node = attest_with(&repo, &ids[0], &kp);
        write_tracked(&repo, &ids[0], &node);

        let row = row_for(&repo, &ids[0], Some(&kp));
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(row.verifies, Verifies::Yes);
        assert_eq!(row.verifies.table(), "✓");
    }

    #[test]
    fn stale_is_stale_and_na() {
        let (repo, ids, _dir) = repo_with_intents(1);
        let kp = KeyPair::generate();
        let node = attest_with(&repo, &ids[0], &kp);
        write_tracked(&repo, &ids[0], &node);

        // Edit the stored intent body so the recorded source hash no longer
        // matches ⇒ load_attestation returns Stale.
        let path = bridge::vault_path_for(&repo, &ids[0]).unwrap().unwrap();
        let entry = repo.vault_retrieve(&path).unwrap().unwrap();
        let mut body = String::from_utf8_lossy(&entry.content_bytes).into_owned();
        body.push_str("\n<!-- edited after attest -->\n");
        repo.vault_store(
            &path,
            VaultEntryType::Intent,
            body.into_bytes(),
            entry.frontmatter_json.clone(),
        )
        .unwrap();

        let row = row_for(&repo, &ids[0], Some(&kp));
        assert_eq!(row.attested, Attested::Stale);
        assert_eq!(row.verifies, Verifies::Na, "stale ⇒ verifies '–', not '✗'");
        assert_eq!(row.attested.table(), "stale");
    }

    #[test]
    fn tampered_same_signer_is_x() {
        // A fresh attestation whose stored body was mutated after signing, with a
        // DID that MATCHES the resolving key ⇒ DID-match passes, verify() returns
        // a hash mismatch ⇒ '✗'. Distinguishes ✗ (real failure) from '–'.
        let (repo, ids, _dir) = repo_with_intents(1);
        let kp = KeyPair::generate();
        let mut node = attest_with(&repo, &ids[0], &kp);
        write_tracked(&repo, &ids[0], &node);
        // Confirm untampered verifies.
        assert_eq!(row_for(&repo, &ids[0], Some(&kp)).verifies, Verifies::Yes);

        // Tamper a signed field (the title/text) AFTER signing but keep the same
        // attributedTo DID and the same recorded sourceContentHash (so it stays
        // Fresh). The content hash / signature no longer match the body.
        node.title = "tampered title".to_string();
        write_tracked(&repo, &ids[0], &node);

        let row = row_for(&repo, &ids[0], Some(&kp));
        assert_eq!(
            row.attested,
            Attested::Fresh,
            "still fresh (hash anchor kept)"
        );
        assert_eq!(row.verifies, Verifies::No, "tampered same-signer ⇒ '✗'");
        assert_eq!(row.verifies.table(), "✗");
    }

    #[test]
    fn other_signer_is_na_not_x() {
        // Attest with K1 but resolve the list identity as K2 (different DID).
        // attested='fresh' but node.attributed_to != did(K2) ⇒ verifies='–'
        // (the core false-negative guard), NOT '✗'.
        let (repo, ids, _dir) = repo_with_intents(1);
        let k1 = KeyPair::generate();
        let k2 = KeyPair::generate();
        let node = attest_with(&repo, &ids[0], &k1);
        write_tracked(&repo, &ids[0], &node);

        let row = row_for(&repo, &ids[0], Some(&k2));
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(
            row.verifies,
            Verifies::Na,
            "other-signer ⇒ '–' (false-negative guard), never '✗'"
        );
    }

    #[test]
    fn no_identity_soft_fails_to_na() {
        // A fresh attested row still lists with verifies='–' when NO identity is
        // resolvable (verifier = None), rather than erroring.
        let (repo, ids, _dir) = repo_with_intents(1);
        let kp = KeyPair::generate();
        let node = attest_with(&repo, &ids[0], &kp);
        write_tracked(&repo, &ids[0], &node);

        let row = row_for(&repo, &ids[0], None);
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(row.verifies, Verifies::Na);
    }

    #[test]
    fn attestation_entry_never_a_row() {
        // After attesting, the attestation entry (attestations/…) NEVER appears as
        // its own intent row: vault_intent_list yields exactly the N intents.
        let (repo, ids, _dir) = repo_with_intents(3);
        let kp = KeyPair::generate();
        let node = attest_with(&repo, &ids[0], &kp);
        write_tracked(&repo, &ids[0], &node);

        let listed = repo.vault_intent_list(None).unwrap();
        assert_eq!(listed.len(), 3, "exactly the 3 intents, no attestation row");
        assert!(
            listed.iter().all(|i| !i.id.starts_with("attestations/")),
            "no attestations/ path leaks into the intent listing"
        );
    }
}
