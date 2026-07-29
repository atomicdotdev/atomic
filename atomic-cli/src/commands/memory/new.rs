//! `atomic memory new --kind <k> [--about m1,m2] [--derived-from id1,id2]` —
//! scaffold a directive-based memory into the vault, or write supplied text
//! directly for non-interactive workflows.
//!
//! Unlike `atomic intent new` (which delegates to `vault_intent_create`), there
//! is NO dedicated repository create method for memories. This writes directly
//! through the GENERIC vault path (`vault_store` + `vault_materialize`) — the
//! same path the raw `atomic vault memory write` uses — so the memory enters
//! redb, the manifest, and the merkle exactly as a first-class tracked entry.

use clap::Parser;
use serde_json::{Map, Value};

use atomic_canonical::vocab::{MEMORY_KIND, MEMORY_STATUS};
use atomic_core::pristine::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// The directive scaffold emitted for a new memory.
///
/// A single `:::memory` container (the closed, lift-recognized directive from
/// `LIFTED_MEMORY_DIRECTIVES`) wrapping a stub comment. `lift_memory` takes the
/// container's body as the authoritative single authoring site; the comment
/// counts as text, so a freshly-scaffolded memory lifts to non-empty text and
/// its ONLY validate violations are the fillable `attributedTo` + `proof`
/// (attest-ready), mirroring intent-new behavior.
const MEMORY_SCAFFOLD: &str = "\
:::memory
<!-- The durable fact. State the constraint / preference / lesson / context.
     Its content is never graded — but it must be present. Replace this stub. -->
:::
";

/// Scaffold a new directive-based memory into the vault.
#[derive(Parser, Debug)]
#[command(name = "new")]
pub struct MemoryNew {
    /// Memory kind: one of `constraint`, `preference`, `lesson`, `context`,
    /// `decision`. Use `decision` for a durable decision→outcome record
    /// (chosen approach + why), typically emitted at turn end.
    #[arg(long)]
    pub kind: String,

    /// Module/domain urns this memory is about (comma-separated, optional).
    #[arg(long, value_delimiter = ',')]
    pub about: Vec<String>,

    /// Intent, change, or source-memory RDF ids this learning was derived from
    /// (comma-separated, optional).
    #[arg(long, value_delimiter = ',')]
    pub derived_from: Vec<String>,

    /// Durable memory text for a non-interactive workflow. When omitted, write
    /// the editable `:::memory` scaffold.
    #[arg(long)]
    pub text: Option<String>,

    /// Explicit memory id (the filename stem). Defaults to a freshly generated
    /// lowercased ULID.
    #[arg(long)]
    pub id: Option<String>,

    /// Lifecycle status. One of `active`, `superseded`, `retracted`.
    #[arg(long, default_value = "active")]
    pub status: String,

    /// Emit the created memory as JSON (`{"id":…,"file":…,"kind":…}`) instead of
    /// the human summary — for scripting, e.g. `… --json | jq -r .id`.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryNew {
    fn run(&self) -> CliResult<()> {
        // Reject a bad --kind EARLY (before touching the vault) so `new` never
        // writes a non-liftable / non-conforming spine.
        if !MEMORY_KIND.contains(&self.kind.as_str()) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown memory kind '{}' (one of {:?})",
                    self.kind, MEMORY_KIND
                ),
            });
        }
        // status is an equally-closed vocabulary — reject a bad one early too,
        // so `new` never persists a non-conforming spine (the gate would only
        // catch it later at validate/attest).
        if !MEMORY_STATUS.contains(&self.status.as_str()) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown memory status '{}' (one of {:?})",
                    self.status, MEMORY_STATUS
                ),
            });
        }
        if self
            .text
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(CliError::InvalidArgument {
                message: "memory text cannot be empty".to_string(),
            });
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let id = self.id.clone().unwrap_or_else(generate_ulid_lowercased);
        let vault_path = bridge::normalize_memory_path(&id);

        // Build the frontmatter SPINE as FLAT SCALAR/array fields only (so it
        // survives yaml_frontmatter_to_json round-trip). Do NOT write
        // attributedTo/proof/contentHash — attest fills those. createdAt is
        // STABLE (load-bearing: it is signed, so every re-lift must reproduce
        // byte-identical signing bytes).
        let mut spine = Map::new();
        spine.insert("uid".into(), Value::String(id.clone()));
        spine.insert("memoryKind".into(), Value::String(self.kind.clone()));
        spine.insert("status".into(), Value::String(self.status.clone()));
        spine.insert(
            "createdAt".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        if !self.about.is_empty() {
            spine.insert(
                "about".into(),
                Value::Array(self.about.iter().cloned().map(Value::String).collect()),
            );
        }
        if !self.derived_from.is_empty() {
            spine.insert(
                "derivedFrom".into(),
                Value::Array(
                    self.derived_from
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        let frontmatter_json =
            serde_json::to_string(&spine).expect("frontmatter spine serialization is infallible");

        let body = self
            .text
            .as_deref()
            .map(|text| format!("{}\n", text.trim()))
            .unwrap_or_else(|| MEMORY_SCAFFOLD.to_string());

        // Store the body WITHOUT a frontmatter block; the spine goes through the
        // frontmatter_json arg. Enters redb + manifest.memory + merkle exactly
        // like the raw `vault memory write` path.
        repo.vault_store(
            &vault_path,
            VaultEntryType::Memory,
            body.into_bytes(),
            frontmatter_json,
        )
        .map_err(CliError::Repository)?;
        repo.vault_materialize(&vault_path)
            .map_err(CliError::Repository)?;

        if self.json {
            let out = serde_json::json!({
                "id": id,
                "file": format!(".vault/{vault_path}"),
                "kind": self.kind,
            });
            println!(
                "{}",
                serde_json::to_string(&out).expect("json is infallible")
            );
            return Ok(());
        }

        println!("Created memory: {id}");
        println!("  file: .vault/{vault_path}");
        println!("  kind: {}", self.kind);
        if !self.derived_from.is_empty() {
            println!("  derived from: {} source(s)", self.derived_from.len());
        }
        println!();
        if self.text.is_none() {
            println!("Edit the :::memory stub, then:");
        }
        println!("  atomic memory validate {id}");
        println!("  atomic memory attest {id}");

        Ok(())
    }
}

/// Generate a fresh lowercased ULID-style stem: a 48-bit millisecond timestamp
/// followed by 80 bits of randomness, encoded in Crockford base32 (26 chars),
/// lowercased. No ULID crate is a workspace dependency, so this is a
/// self-contained generator using the existing `chrono` + `uuid` deps —
/// deliberately NOT a `PREFIX-N` scheme (memories have no prefix allocator).
fn generate_ulid_lowercased() -> String {
    const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u128;
    // 80 bits of randomness from a v4 UUID.
    let rand = uuid::Uuid::new_v4().as_u128();

    // A ULID is a 128-bit value: 48-bit time (high) + 80-bit randomness (low).
    let value: u128 = (now_ms << 80) | (rand & ((1u128 << 80) - 1));

    // Encode the 128-bit value as 26 Crockford base32 chars (130 bits, so the
    // top 2 bits of the first symbol are always 0 — standard ULID layout).
    let mut out = [0u8; 26];
    let mut v = value;
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(v & 0x1f) as usize];
        v >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("crockford alphabet is valid ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::memory::lift_memory;
    use tempfile::tempdir;

    #[test]
    fn test_generate_ulid_shape() {
        let id = generate_ulid_lowercased();
        assert_eq!(id.len(), 26, "ULID stem is 26 chars");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "ULID stem is lowercased Crockford base32: {id}"
        );
        // Two consecutive generations differ (randomness in the low bits).
        assert_ne!(generate_ulid_lowercased(), generate_ulid_lowercased());
    }

    #[test]
    fn test_new_writes_liftable_memory() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Drive `new`'s storage logic directly (the CLI resolves the repo via
        // find_repository_root; here we exercise the same writes on a known repo).
        let id = "memtest01".to_string();
        let vault_path = bridge::normalize_memory_path(&id);
        let mut spine = Map::new();
        spine.insert("uid".into(), Value::String(id.clone()));
        spine.insert("memoryKind".into(), Value::String("constraint".into()));
        spine.insert("status".into(), Value::String("active".into()));
        spine.insert(
            "createdAt".into(),
            Value::String("2026-07-01T00:00:00+00:00".into()),
        );
        spine.insert(
            "about".into(),
            Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
        );
        let fm_json = serde_json::to_string(&spine).unwrap();
        repo.vault_store(
            &vault_path,
            VaultEntryType::Memory,
            MEMORY_SCAFFOLD.as_bytes().to_vec(),
            fm_json,
        )
        .unwrap();
        repo.vault_materialize(&vault_path).unwrap();

        // The stored entry is a Memory entry keyed on the path, and lifts cleanly.
        let entry = repo.vault_retrieve(&vault_path).unwrap().expect("entry");
        assert_eq!(entry.entry_type, VaultEntryType::Memory);
        let fm: Map<String, Value> = serde_json::from_str(&entry.frontmatter_json).unwrap();
        assert_eq!(
            fm.get("createdAt").and_then(Value::as_str),
            Some("2026-07-01T00:00:00+00:00")
        );
        assert_eq!(
            fm.get("about").unwrap(),
            &Value::Array(vec![Value::String("a".into()), Value::String("b".into())])
        );
        let body = String::from_utf8_lossy(&entry.content_bytes);
        assert!(
            body.contains(":::memory"),
            "body carries the :::memory container"
        );
        let node = lift_memory(&fm, &body).expect("scaffold lifts cleanly");
        assert_eq!(node.memory_kind, "constraint");
        assert_eq!(node.about, vec!["a".to_string(), "b".to_string()]);
        assert!(!node.text.trim().is_empty(), "stub comment counts as text");

        // Materialized to disk.
        let on_disk = repo.vault_dir().join(&vault_path);
        assert!(on_disk.exists(), "materialized at {}", on_disk.display());
    }

    #[test]
    fn test_learning_inputs_lift_as_provenance() {
        let mut spine = Map::new();
        spine.insert("uid".into(), Value::String("learning-run-1".into()));
        spine.insert("memoryKind".into(), Value::String("lesson".into()));
        spine.insert("status".into(), Value::String("active".into()));
        spine.insert(
            "derivedFrom".into(),
            Value::Array(vec![
                Value::String("urn:atomic:intent:demo-1".into()),
                Value::String("urn:atomic:change:abc".into()),
                Value::String("urn:atomic:memory:source-1".into()),
            ]),
        );

        let node = lift_memory(
            &spine,
            "Use the health-check timeout from project memory, not a new default.\n",
        )
        .expect("non-interactive learning lifts");
        assert_eq!(node.memory_kind, "lesson");
        assert_eq!(node.derived_from.len(), 3);
        assert_eq!(node.derived_from[0], "urn:atomic:intent:demo-1");
        assert!(node.text.contains("health-check timeout"));
    }
}
