//! `atomic memory list` — an ATTESTATION-AWARE listing of the vault's memories.
//!
//! The canonical-family analogue of `atomic vault memory list` /
//! `atomic vault list --type memory`: those render a plain path/size table,
//! while this one adds the columns only the canonical engine can answer — the
//! memory's kind/status/about count (lifted, never graded), whether it carries a
//! *fresh* attestation, and whether that attestation *verifies* against the
//! resolving identity.
//!
//! # Columns
//!
//! ```text
//! id           kind        status   about   attested   verifies
//! 01j8zc4r8t   constraint  active   2       fresh      ✓
//! 01j9aa11bb   lesson      active   0       –          –
//! ```
//!
//! - `id`       — the filename stem of `memory/<id>.md`.
//! - `kind`     — `memory_kind` (from the attested node when Fresh/Stale, else a
//!   lift of the raw memory; NEVER a closed-vocab grade of the prose).
//! - `status`   — the memory's status (same source as `kind`).
//! - `about`    — the number of `about` input edges.
//! - `attested` — `fresh` / `stale` / `–` via [`bridge::load_attestation`].
//! - `verifies` — `✓` / `✗` / `–` via the DID-match-then-verify rule (identical
//!   to the intent list; see [`compute_verifies`]).
//!
//! Enumeration reuses `repo.vault_list("memory/", None)` — the exact call
//! `atomic vault memory list` makes; the `memory/` prefix already excludes
//! attestation entries (which live under `attestations/memory/…`), and a
//! belt-and-suspenders `!starts_with("attestations/")` filter guards it.

use clap::Parser;

use atomic_canonical::did::did_for_public_key;
use atomic_canonical::MemoryNode;
use atomic_identity::keypair::PublicKey;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// The EN DASH used everywhere for a "not applicable" cell.
const NA: &str = "–";

/// List the vault's memories with attestation + verification status.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct MemoryList {
    /// Identity whose public key to verify attestations against. Defaults to the
    /// current default identity. If no identity can be resolved at all, every
    /// `verifies` cell is `–` rather than the whole command erroring.
    #[arg(long)]
    pub identity: Option<String>,

    /// Maximum number of memories to show (most recent first). Shows all when omitted.
    #[arg(short = 'n', long)]
    pub limit: Option<usize>,

    /// Output as JSON (an array of {id,kind,status,about,attested,verifies}).
    #[arg(long)]
    pub json: bool,
}

/// The classification of one memory's attestation, for a table/JSON row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attested {
    Fresh,
    Stale,
    None,
}

impl Attested {
    fn table(self) -> &'static str {
        match self {
            Attested::Fresh => "fresh",
            Attested::Stale => "stale",
            Attested::None => NA,
        }
    }

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
    Yes,
    No,
    Na,
}

impl Verifies {
    fn table(self) -> &'static str {
        match self {
            Verifies::Yes => "✓",
            Verifies::No => "✗",
            Verifies::Na => NA,
        }
    }

    fn json(self) -> &'static str {
        match self {
            Verifies::Yes => "yes",
            Verifies::No => "no",
            Verifies::Na => "na",
        }
    }
}

/// A fully-computed memory row.
struct Row {
    id: String,
    kind: String,
    status: String,
    about: usize,
    attested: Attested,
    verifies: Verifies,
}

/// The verifying identity resolved ONCE for the whole list.
struct Verifier {
    public_key: PublicKey,
    did: String,
}

/// Resolve the verifying identity exactly as `memory verify` does, but SOFT: a
/// missing default (or no store) yields `Ok(None)` (all verifies `–`); a
/// `--identity <name>` that does not exist IS a hard error.
fn resolve_verifier(name: &Option<String>) -> CliResult<Option<Verifier>> {
    let store = match IdentityStore::open_default() {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let identity = if let Some(name) = name {
        Some(
            store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?,
        )
    } else {
        store.get_default().ok().flatten()
    };
    Ok(identity.map(|id| Verifier {
        did: did_for_public_key(&id.public_key),
        public_key: id.public_key,
    }))
}

/// The DID-match-then-verify rule for a memory. Pure over its inputs so it can be
/// unit-tested without a global `IdentityStore`. See the intent list's
/// `compute_verifies` for the full rationale — identical rule, `verify_memory`.
fn compute_verifies(attested: &bridge::Attestation, verifier: Option<&Verifier>) -> Verifies {
    let node = match attested {
        bridge::Attestation::Fresh(node) => node,
        bridge::Attestation::Stale(_) | bridge::Attestation::None => return Verifies::Na,
    };
    let verifier = match verifier {
        Some(v) => v,
        None => return Verifies::Na,
    };
    match node.attributed_to.as_deref() {
        Some(signer) if signer == verifier.did => {}
        _ => return Verifies::Na,
    }
    match atomic_canonical::verify_memory(node, &verifier.public_key) {
        Ok(()) => Verifies::Yes,
        Err(_) => Verifies::No,
    }
}

/// The `(kind, status, about)` display columns for a memory. Prefers the attested
/// node when Fresh/Stale; falls back to a lenient lift of the raw memory when
/// there is no attestation. NEVER runs closed-vocab directive parsing over the
/// prose — `lift_memory` does the lenient `:::memory` scan. A lift failure
/// degrades to the raw frontmatter `memoryKind`/`status` (or `–`) and about 0.
fn kind_status_about(
    node: Option<&MemoryNode>,
    inputs: &bridge::MemLiftInputs,
) -> (String, String, usize) {
    if let Some(node) = node {
        return (
            node.memory_kind.clone(),
            node.status.clone(),
            node.about.len(),
        );
    }
    match bridge::lift(inputs) {
        Ok(node) => (node.memory_kind, node.status, node.about.len()),
        Err(_) => {
            let fm = |k: &str| {
                inputs
                    .frontmatter
                    .get(k)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| NA.to_string())
            };
            (fm("memoryKind"), fm("status"), 0)
        }
    }
}

/// The node whose fields drive the display columns (kind/status/about) — only a
/// FRESH attestation. A Stale attestation is treated like None here so those
/// columns come from the CURRENT source (via `bridge::lift`), matching
/// `memory show`; the drift is still surfaced by the `attested` column ("stale").
/// (The `verifies` column reads the full `Attestation` separately.)
fn attested_node(attested: &bridge::Attestation) -> Option<&MemoryNode> {
    match attested {
        bridge::Attestation::Fresh(n) => Some(n),
        bridge::Attestation::Stale(_) | bridge::Attestation::None => None,
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

/// Compute one memory's row. A read/lift/attestation failure for a SINGLE memory
/// degrades that row's computed columns to `–`/0 rather than aborting the list.
fn compute_row(repo: &Repository, id: &str, verifier: Option<&Verifier>) -> Row {
    let inputs = match bridge::read_memory(repo, id) {
        Ok(i) => i,
        Err(_) => {
            return Row {
                id: id.to_string(),
                kind: NA.to_string(),
                status: NA.to_string(),
                about: 0,
                attested: Attested::None,
                verifies: Verifies::Na,
            }
        }
    };
    let attestation = bridge::load_attestation(repo, id, &inputs);
    let (attested, verifies, node_owned) = match attestation {
        Ok(a) => {
            let verifies = compute_verifies(&a, verifier);
            (classify(&a), verifies, Some(a))
        }
        Err(_) => (Attested::None, Verifies::Na, None),
    };
    let node = node_owned.as_ref().and_then(attested_node);
    let (kind, status, about) = kind_status_about(node, &inputs);
    Row {
        id: id.to_string(),
        kind,
        status,
        about,
        attested,
        verifies,
    }
}

impl Command for MemoryList {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let verifier = resolve_verifier(&self.identity)?;

        // Enumerate via the SAME call `atomic vault memory list` makes; the
        // `memory/` prefix already excludes attestation entries, and the filter
        // is a belt-and-suspenders guard (mirrors memory/mod.rs' invariant test).
        let entries = repo
            .vault_list("memory/", None)
            .map_err(CliError::Repository)?;
        let mut items: Vec<(String, String)> = entries
            .iter()
            .filter(|e| !e.path.starts_with("attestations/"))
            // The default `memory/MEMORY.md` index scaffold is NOT a canonical
            // memory (no `uid`/`memoryKind`; its frontmatter is `type:index`).
            // Skip it so the attestation-aware list only shows real memories.
            .filter(|e| !is_index_scaffold(&repo, &e.path))
            .map(|e| (bridge::memory_id(&e.path), e.updated_at.clone()))
            .collect();
        // Most-recent first (`updated_at` is RFC 3339, so lexical == chronological);
        // id as a stable tiebreaker.
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if let Some(limit) = self.limit {
            items.truncate(limit);
        }

        let rows: Vec<Row> = items
            .iter()
            .map(|(id, _)| compute_row(&repo, id, verifier.as_ref()))
            .collect();

        if self.json {
            let json: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "kind": r.kind,
                        "status": r.status,
                        "about": r.about,
                        "attested": r.attested.json(),
                        "verifies": r.verifies.json(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
            return Ok(());
        }

        if rows.is_empty() {
            println!("No memories found.");
            return Ok(());
        }

        // Fixed-width, left-aligned columns with a header row.
        let id_w = col_width(rows.iter().map(|r| r.id.chars().count()), "id");
        let kind_w = col_width(rows.iter().map(|r| r.kind.chars().count()), "kind");
        let status_w = col_width(rows.iter().map(|r| r.status.chars().count()), "status");

        println!(
            "  {:<id_w$}  {:<kind_w$}  {:<status_w$}  {:>5}  {:<8}  verifies",
            "id", "kind", "status", "about", "attested",
        );
        for r in &rows {
            println!(
                "  {:<id_w$}  {:<kind_w$}  {:<status_w$}  {:>5}  {:<8}  {}",
                r.id,
                r.kind,
                r.status,
                r.about,
                r.attested.table(),
                r.verifies.table(),
            );
        }

        Ok(())
    }
}

/// The width of a column: the widest row cell, but at least the header width.
fn col_width(cell_lens: impl Iterator<Item = usize>, header: &str) -> usize {
    let header_w = header.chars().count();
    cell_lens
        .chain(std::iter::once(header_w))
        .max()
        .unwrap_or(header_w)
}

/// Is this `memory/…` entry the vault's index scaffold rather than a real
/// canonical memory? The default `memory/MEMORY.md` installed by `init_vault`
/// carries frontmatter `{"name":"MEMORY","type":"index"}` — it has no
/// `uid`/`memoryKind` and is not attestable, so it must not appear as a row in
/// the canonical, attestation-aware list. Detection reads the entry's declared
/// `type == "index"` frontmatter (fail-open: a read error is treated as "not an
/// index" so a genuine memory is never silently hidden).
fn is_index_scaffold(repo: &Repository, path: &str) -> bool {
    let Ok(Some(entry)) = repo.vault_retrieve(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .as_deref()
        == Some("index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::lift_and_attest_memory;
    use atomic_core::pristine::VaultEntryType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    /// A fresh repo+vault.
    fn repo() -> (Repository, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        (repo, dir)
    }

    /// Store a memory via the GENERIC vault path (as `memory write`/attest do).
    fn store_memory(repo: &Repository, id: &str, kind: &str, about: &[&str], body: &str) {
        let mut spine = Map::new();
        spine.insert("uid".into(), Value::String(id.to_string()));
        spine.insert("memoryKind".into(), Value::String(kind.to_string()));
        spine.insert("status".into(), Value::String("active".into()));
        spine.insert(
            "createdAt".into(),
            Value::String("2026-07-01T00:00:00+00:00".into()),
        );
        if !about.is_empty() {
            spine.insert(
                "about".into(),
                Value::Array(about.iter().map(|a| Value::String(a.to_string())).collect()),
            );
        }
        let path = bridge::normalize_memory_path(id);
        repo.vault_store(
            &path,
            VaultEntryType::Memory,
            body.as_bytes().to_vec(),
            serde_json::to_string(&spine).unwrap(),
        )
        .unwrap();
    }

    /// Lift+attest a stored memory with the given keypair.
    fn attest_with(repo: &Repository, id: &str, kp: &KeyPair) -> MemoryNode {
        let inputs = bridge::read_memory(repo, id).unwrap();
        let identity = Identity::new("tester", kp);
        lift_and_attest_memory(&inputs.frontmatter, &inputs.body, &identity, kp).unwrap()
    }

    /// Mirror `attest`'s tracked-vault write (body = pretty JSON + '\n').
    fn write_tracked(repo: &Repository, id: &str, node: &MemoryNode) {
        let inputs = bridge::read_memory(repo, id).unwrap();
        let mut body = serde_json::to_string_pretty(&node.to_value()).unwrap();
        body.push('\n');
        let mut fm = Map::new();
        fm.insert("memoryId".into(), Value::String(bridge::memory_id(id)));
        fm.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        let vpath = bridge::attestation_vault_path(id);
        repo.vault_store(
            &vpath,
            VaultEntryType::Attestation,
            body.into_bytes(),
            serde_json::to_string(&fm).unwrap(),
        )
        .unwrap();
    }

    /// Compute a row with an explicit verifier keypair (no global store).
    fn row_for(repo: &Repository, id: &str, kp: Option<&KeyPair>) -> Row {
        let verifier = kp.map(|kp| Verifier {
            did: did_for_public_key(&kp.public),
            public_key: kp.public.clone(),
        });
        compute_row(repo, id, verifier.as_ref())
    }

    #[test]
    fn unattested_memory_lists_columns_from_lift() {
        let (repo, _dir) = repo();
        store_memory(&repo, "m1", "constraint", &[], ":::memory\nA fact.\n:::");
        let row = row_for(&repo, "m1", Some(&KeyPair::generate()));
        assert_eq!(row.kind, "constraint");
        assert_eq!(row.status, "active");
        assert_eq!(row.about, 0);
        assert_eq!(row.attested, Attested::None);
        assert_eq!(row.verifies, Verifies::Na);
    }

    #[test]
    fn attested_fresh_populates_about_and_verifies() {
        let (repo, _dir) = repo();
        store_memory(
            &repo,
            "m2",
            "lesson",
            &["urn:atomic:module:a", "urn:atomic:module:b"],
            ":::memory\nThe durable lesson.\n:::",
        );
        let kp = KeyPair::generate();
        let node = attest_with(&repo, "m2", &kp);
        write_tracked(&repo, "m2", &node);

        let row = row_for(&repo, "m2", Some(&kp));
        assert_eq!(row.kind, "lesson");
        assert_eq!(row.about, 2, "about count = node.about.len()");
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(row.verifies, Verifies::Yes);
    }

    #[test]
    fn other_signer_is_na_not_x() {
        let (repo, _dir) = repo();
        store_memory(&repo, "m3", "context", &[], ":::memory\nX.\n:::");
        let k1 = KeyPair::generate();
        let k2 = KeyPair::generate();
        let node = attest_with(&repo, "m3", &k1);
        write_tracked(&repo, "m3", &node);

        let row = row_for(&repo, "m3", Some(&k2));
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(row.verifies, Verifies::Na, "other-signer ⇒ '–', not '✗'");
    }

    #[test]
    fn no_identity_soft_fails_to_na() {
        let (repo, _dir) = repo();
        store_memory(&repo, "m4", "context", &[], ":::memory\nX.\n:::");
        let kp = KeyPair::generate();
        let node = attest_with(&repo, "m4", &kp);
        write_tracked(&repo, "m4", &node);

        let row = row_for(&repo, "m4", None);
        assert_eq!(row.attested, Attested::Fresh);
        assert_eq!(row.verifies, Verifies::Na);
    }

    #[test]
    fn never_grades_prose() {
        // A memory whose body legitimately contains ':::why' text or a Rust
        // turbofish '::<T>' still lists cleanly because lift_memory does the
        // lenient scan (no closed-vocab grading of prose).
        let (repo, _dir) = repo();
        let body = ":::memory\nUse ::<T> and note the :::why here is prose.\n:::";
        store_memory(&repo, "m5", "preference", &[], body);

        let row = row_for(&repo, "m5", Some(&KeyPair::generate()));
        assert_eq!(row.kind, "preference");
        assert_eq!(row.status, "active");
        assert_eq!(row.about, 0);
        assert_eq!(row.attested, Attested::None);
    }

    #[test]
    fn default_memory_index_scaffold_is_excluded() {
        // A fresh vault installs `memory/MEMORY.md` (type:index). It must NOT
        // appear as a memory row in the canonical list; only real memories do.
        let (repo, _dir) = repo();
        assert!(
            is_index_scaffold(&repo, "memory/MEMORY.md"),
            "the default index scaffold is detected"
        );
        store_memory(&repo, "real1", "context", &[], ":::memory\nX.\n:::");
        let ids: Vec<String> = repo
            .vault_list("memory/", None)
            .unwrap()
            .iter()
            .filter(|e| !e.path.starts_with("attestations/"))
            .filter(|e| !is_index_scaffold(&repo, &e.path))
            .map(|e| bridge::memory_id(&e.path))
            .collect();
        assert_eq!(ids, vec!["real1".to_string()], "only the real memory lists");
    }

    #[test]
    fn attestation_entry_never_a_row() {
        // Mirror memory/mod.rs: after attesting, vault_list("memory/") contains the
        // memory entry but no path under attestations/.
        let (repo, _dir) = repo();
        store_memory(&repo, "m6", "context", &[], ":::memory\nX.\n:::");
        let kp = KeyPair::generate();
        let node = attest_with(&repo, "m6", &kp);
        write_tracked(&repo, "m6", &node);

        let listed = repo.vault_list("memory/", None).unwrap();
        assert!(
            listed.iter().all(|e| !e.path.starts_with("attestations/")),
            "attestation entries must not appear under the memory/ prefix"
        );
        assert!(
            listed.iter().any(|e| e.path == "memory/m6.md"),
            "the memory entry itself is listed"
        );
        // And it never becomes its own row id (applying the same production
        // filters: no attestations/, no index scaffold).
        let ids: Vec<String> = listed
            .iter()
            .filter(|e| !e.path.starts_with("attestations/"))
            .filter(|e| !is_index_scaffold(&repo, &e.path))
            .map(|e| bridge::memory_id(&e.path))
            .collect();
        assert_eq!(ids, vec!["m6".to_string()]);
    }
}
