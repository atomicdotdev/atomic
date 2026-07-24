//! Vault types for the shared project knowledge store.
//!
//! The vault is a project-scoped knowledge store that lives in `.vault/`.
//! It stores goal transcripts, memory, skills, intents, and scratch notes
//! as versioned markdown files with cryptographic provenance.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Type of vault entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultEntryType {
    Session,    // 0
    ToolResult, // 1
    Memory,     // 2
    Intent,     // 3
    Skill,      // 4
    Scratch,    // 5
    /// Signed Recording-the-Why attestation (JSON-LD body). Index 6 — APPEND-ONLY.
    /// postcard encodes enum discriminants positionally, so this MUST stay last and
    /// MUST NOT be reordered/inserted/deleted. Mixed-version sync caveat: an older
    /// binary that predates this variant cannot postcard-decode an Attestation row
    /// and will error on that entry; newer readers are unaffected.
    Attestation, // 6
}

impl VaultEntryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::ToolResult => "tool_result",
            Self::Memory => "memory",
            Self::Intent => "intent",
            Self::Skill => "skill",
            Self::Scratch => "scratch",
            Self::Attestation => "attestation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for VaultEntryType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session" => Ok(Self::Session),
            "tool_result" => Ok(Self::ToolResult),
            "memory" => Ok(Self::Memory),
            "intent" => Ok(Self::Intent),
            "skill" => Ok(Self::Skill),
            "scratch" => Ok(Self::Scratch),
            "attestation" => Ok(Self::Attestation),
            _ => Err(format!("unknown vault entry type: {s}")),
        }
    }
}

impl std::fmt::Display for VaultEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single entry in the vault.
///
/// Stored in `VAULT_ENTRIES` keyed by vault-relative path (e.g. `sessions/abc123/_session.md`).
/// The entry contains both metadata and content bytes, enabling full reconstruction
/// of the markdown file on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultEntry {
    /// Type of this entry.
    pub entry_type: VaultEntryType,

    /// Blake3 hash of `content_bytes`.
    pub content_hash: [u8; 32],

    /// The raw content (markdown body, without frontmatter).
    pub content_bytes: Vec<u8>,

    /// YAML frontmatter serialized as JSON for structured querying.
    /// Empty object `{}` if no frontmatter.
    pub frontmatter_json: String,

    /// When this entry was first created (RFC 3339).
    pub created_at: String,

    /// When this entry was last modified (RFC 3339).
    pub updated_at: String,

    /// The NodeId of the change that introduced this entry (0 if not yet recorded).
    pub introduced_by: u64,
}

impl VaultEntry {
    /// Create a new vault entry. Computes the content hash automatically.
    pub fn new(
        entry_type: VaultEntryType,
        content: Vec<u8>,
        frontmatter_json: String,
        now: String,
    ) -> Self {
        let content_hash = blake3::hash(&content);
        Self {
            entry_type,
            content_hash: *content_hash.as_bytes(),
            content_bytes: content,
            frontmatter_json,
            created_at: now.clone(),
            updated_at: now,
            introduced_by: 0,
        }
    }

    /// Size in bytes (content only, not metadata).
    pub fn content_size(&self) -> usize {
        self.content_bytes.len()
    }
}

// ── Vault Manifest ──────────────────────────────────────────────

/// Summary of a goal in the manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalSummary {
    pub developer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub status: String,
    pub started_at: String,
    pub turns: u32,
    pub tokens: TokenCounts,
    pub tool_results: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings_hash: Option<String>,
    pub bytes: u64,
}

/// Token usage counts.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenCounts {
    pub fresh: u64,
    pub cached: u64,
    pub output: u64,
}

/// Summary of a memory file in the manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySummary {
    pub hash: String,
    pub bytes: u64,
    pub updated_at: String,
}

/// Summary of an intent in the manifest.
///
/// Keyed in the manifest by the intent's stable [`ULID`](https://github.com/ulid/spec)
/// `uid`. The `human_key` (`PROJECT::author::seq`) is a display alias composed
/// from `project`/`author`/`seq`; it is never the primary identity, so two
/// teammates minting the same sequence never collide on the real key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IntentSummary {
    pub status: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, alias = "sessions")]
    pub goals: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// Intent title (stored for list display without path lookup).
    #[serde(default)]
    pub title: String,
    /// The vault path to the intent file.
    #[serde(default)]
    pub vault_path: String,
    /// Stable primary identity (ULID). Empty on legacy pre-ULID entries.
    #[serde(default)]
    pub uid: String,
    /// Rendered human display key, e.g. `PIMO::lee-faus::3`.
    #[serde(default)]
    pub human_key: String,
    /// Project code component of the human key (e.g. `PIMO`).
    #[serde(default)]
    pub project: String,
    /// Author handle component of the human key (e.g. `lee-faus`).
    #[serde(default)]
    pub author: String,
    /// Per-author sequence component of the human key.
    #[serde(default)]
    pub seq: u32,
    /// Agent session that created this intent (provenance, not identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Turn number within the session at creation (1-indexed, as `turn_count+1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

/// Summary of a skill file in the manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSummary {
    pub hash: String,
    pub bytes: u64,
}

/// Index statistics in the manifest.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub embeddings_count: u64,
    pub triples_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_embed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
}

/// The vault manifest — a single JSON document indexing the entire vault.
///
/// Stored in `VAULT_MANIFEST` under the singleton key `"manifest"`.
/// Enables cold-start, sync decisions, and selective fetch without scanning files.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultManifest {
    pub version: u32,
    pub updated_at: String,
    /// Incremental merkle hash: `Hash(prev_merkle || change_hash)`.
    pub merkle: String,

    #[serde(default, alias = "sessions")]
    pub goals: HashMap<String, GoalSummary>,

    #[serde(default)]
    pub memory: HashMap<String, MemorySummary>,

    #[serde(default)]
    pub intents: HashMap<String, IntentSummary>,

    #[serde(default)]
    pub skills: HashMap<String, SkillSummary>,

    #[serde(default)]
    pub index: IndexStats,

    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub file_count: u32,

    /// Auto-incrementing counter for intent IDs. Next ID to assign.
    ///
    /// Legacy (pre-ULID) global counter. Retained for backward compatibility;
    /// the ULID identity model uses per-author counters in `intent_seq`.
    #[serde(default = "default_next_intent_id")]
    pub next_intent_id: u32,

    /// Project prefix / code for intent human keys (e.g., "PIMO", "ATOM").
    /// Derived from the project directory name on vault init.
    #[serde(default)]
    pub intent_prefix: String,

    /// Per-author sequence counters for human keys (`author` -> next `seq`).
    ///
    /// Each author increments only their own counter, so offline teammates
    /// never collide on a human key (`PROJECT::author::seq`) even though the
    /// primary identity is the ULID.
    #[serde(default)]
    pub intent_seq: HashMap<String, u32>,
}

fn default_next_intent_id() -> u32 {
    1
}

impl Default for VaultManifest {
    fn default() -> Self {
        Self {
            version: 1,
            updated_at: String::new(),
            merkle: String::new(),
            goals: HashMap::new(),
            memory: HashMap::new(),
            intents: HashMap::new(),
            skills: HashMap::new(),
            index: IndexStats::default(),
            total_bytes: 0,
            file_count: 0,
            next_intent_id: 1,
            intent_prefix: String::new(),
            intent_seq: HashMap::new(),
        }
    }
}

impl VaultManifest {
    /// Create a fresh manifest with the given timestamp.
    pub fn new(now: String) -> Self {
        Self {
            updated_at: now,
            ..Default::default()
        }
    }

    /// Derive an intent prefix from a project directory name.
    ///
    /// Takes the first 4 alphanumeric characters and uppercases them.
    /// Examples: "pi-mono-rs" → "PIMO", "atomic" → "ATOM", "my-app" → "MYAP"
    pub fn derive_intent_prefix(project_name: &str) -> String {
        project_name
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(4)
            .collect::<String>()
            .to_uppercase()
    }

    /// Get the next intent ID and advance the counter.
    /// Returns a string like "PIMO-1", "ATOM-42".
    ///
    /// Legacy (pre-ULID) allocation. New intents use
    /// [`allocate_author_seq`](Self::allocate_author_seq) +
    /// [`compose_human_key`](Self::compose_human_key) and a ULID primary id.
    pub fn allocate_intent_id(&mut self) -> String {
        let id = self.next_intent_id;
        self.next_intent_id = id.saturating_add(1);
        let prefix = if self.intent_prefix.is_empty() {
            "VAULT"
        } else {
            &self.intent_prefix
        };
        format!("{}-{}", prefix, id)
    }

    /// The project code used in human keys (falls back to `VAULT`).
    pub fn project_code(&self) -> &str {
        if self.intent_prefix.is_empty() {
            "VAULT"
        } else {
            &self.intent_prefix
        }
    }

    /// Allocate the next per-author sequence number, advancing that author's
    /// counter. Each author has an independent counter so offline teammates
    /// never collide on a human key.
    pub fn allocate_author_seq(&mut self, author: &str) -> u32 {
        let next = self.intent_seq.entry(author.to_string()).or_insert(1);
        let seq = *next;
        *next = seq.saturating_add(1);
        seq
    }

    /// Compose the human display key `PROJECT::author::seq`.
    ///
    /// The project is uppercased (it is a code); the author handle is kept
    /// verbatim (it may contain `-`), which is unambiguous because the fields
    /// are separated by [`HUMAN_KEY_SEP`] (`::`), not `-`.
    pub fn compose_human_key(project: &str, author: &str, seq: u32) -> String {
        format!(
            "{}{sep}{}{sep}{}",
            project.to_uppercase(),
            author,
            seq,
            sep = HUMAN_KEY_SEP
        )
    }
}

/// The field separator for human intent keys. `::` (not `-`) so an author
/// handle containing a hyphen (e.g. `lee-faus`) stays unambiguous.
pub const HUMAN_KEY_SEP: &str = "::";

/// A parsed intent reference: either a stable id (ULID / prefix) or a fully
/// composed human key to match against `IntentSummary::human_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentRef {
    /// A ULID or ULID prefix — matched against intent `uid`s.
    Uid(String),
    /// A fully composed human key (`PROJECT::author::seq`).
    HumanKey(String),
}

/// Resolve a user-supplied intent reference into a [`IntentRef`], filling in
/// the current project and author so the common case is just a number.
///
/// Rules (the project is never required from the user):
/// - `3`            -> `HumanKey("<PROJECT>::<author>::3")`
/// - `alice::3`     -> `HumanKey("<PROJECT>::alice::3")` (other teammate)
/// - `PIMO::lee::3` -> `HumanKey("PIMO::lee::3")` (exact, project uppercased)
/// - otherwise      -> `Uid(<input uppercased>)` (a ULID or its prefix)
pub fn parse_intent_reference(
    input: &str,
    default_project: &str,
    default_author: &str,
) -> IntentRef {
    let t = input.trim();

    if t.contains(HUMAN_KEY_SEP) {
        let parts: Vec<&str> = t.split(HUMAN_KEY_SEP).collect();
        return match parts.len() {
            3 => IntentRef::HumanKey(VaultManifest::compose_human_key(
                parts[0],
                parts[1],
                parts[2].parse().unwrap_or(0),
            )),
            2 => IntentRef::HumanKey(VaultManifest::compose_human_key(
                default_project,
                parts[0],
                parts[1].parse().unwrap_or(0),
            )),
            _ => IntentRef::HumanKey(t.to_string()),
        };
    }

    // A bare sequence number resolves to the current project + author.
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
        return IntentRef::HumanKey(VaultManifest::compose_human_key(
            default_project,
            default_author,
            t.parse().unwrap_or(0),
        ));
    }

    // Anything else is treated as a ULID (or a ULID prefix). ULIDs are
    // Crockford base32 and canonically uppercase.
    IntentRef::Uid(t.to_uppercase())
}

// ── Knowledge Graph ─────────────────────────────────────────────

/// A node in the knowledge graph.
///
/// Nodes represent repository artifacts: changes, files, entities, views,
/// goals, intents. The `id` is a structured string like `"change:abc123"`,
/// `"file:src/auth.rs"`, `"entity:src/auth.rs:authenticate:42"`, `"view:main"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgNode {
    /// Structured identifier (e.g., `"change:{hash}"`, `"file:{path}"`).
    pub id: String,
    /// Node category: `"change"`, `"file"`, `"entity"`, `"view"`, `"goal"`, `"intent"`, `"memory"`.
    pub kind: String,
    /// Human-readable label (short hash, filename, symbol name).
    pub label: String,
    /// Optional longer description (commit message, entity signature, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Where this node was derived from (e.g., `"change_graph"`, `"ast_extraction"`, `"vault"`).
    pub source: String,
    /// Arbitrary metadata as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl KgNode {
    /// Create a new KG node.
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        label: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            summary: None,
            source: source.into(),
            metadata: None,
        }
    }

    /// Builder: set summary.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Builder: set metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A directed edge in the knowledge graph.
///
/// Edges encode relationships: `MODIFIES`, `DEFINES`, `DEPENDS_ON`,
/// `ON_VIEW`, `CHILD_OF`, `LINKED_INTENT`, etc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgEdge {
    /// Source node ID.
    pub from_id: String,
    /// Target node ID.
    pub to_id: String,
    /// Relationship type (e.g., `"MODIFIES"`, `"DEFINES"`, `"DEPENDS_ON"`).
    pub kind: String,
    /// Optional edge metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl KgEdge {
    pub fn new(
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            from_id: from_id.into(),
            to_id: to_id.into(),
            kind: kind.into(),
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A subgraph returned by neighborhood queries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgSubgraph {
    pub nodes: Vec<KgNode>,
    pub edges: Vec<KgEdge>,
}

impl KgSubgraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Response from a KG query (hybrid search).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KgQueryResponse {
    /// The query as received.
    pub query: String,
    /// Matched KG nodes.
    pub nodes: Vec<KgNode>,
    /// Edges connecting the matched nodes.
    pub edges: Vec<KgEdge>,
    /// Vector similarity scores for matched nodes (node_id → score).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<std::collections::HashMap<String, f32>>,
    /// Time taken in milliseconds.
    pub elapsed_ms: u64,
}

// ── Embedding Storage ───────────────────────────────────────────

/// A stored embedding record for vector similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    /// The embedding vector (f32 × dims).
    pub vector: Vec<f32>,
    /// Blake3 hash of the source content (for staleness detection).
    pub content_hash: [u8; 32],
    /// The change that produced this content.
    pub introduced_by: u64,
    /// Which chunk of the source document.
    pub chunk_idx: u32,
    /// Preview text for display without reading the full file.
    pub preview: String,
}

/// Result of a vector similarity search.
#[derive(Clone, Debug)]
pub struct SearchResult {
    /// Vault-relative path to the source entry.
    pub path: String,
    /// Chunk index within the entry.
    pub chunk_idx: u32,
    /// Cosine similarity score (0.0 to 1.0).
    pub score: f32,
    /// Preview text.
    pub preview: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_human_key_uppercases_project_and_keeps_author() {
        assert_eq!(
            VaultManifest::compose_human_key("pimo", "lee-faus", 3),
            "PIMO::lee-faus::3"
        );
    }

    #[test]
    fn per_author_counters_are_independent() {
        let mut m = VaultManifest::default();
        assert_eq!(m.allocate_author_seq("lee"), 1);
        assert_eq!(m.allocate_author_seq("lee"), 2);
        // A different author starts from 1 — no collision with lee's sequence.
        assert_eq!(m.allocate_author_seq("alice"), 1);
        assert_eq!(m.allocate_author_seq("lee"), 3);
        assert_eq!(m.allocate_author_seq("alice"), 2);
    }

    #[test]
    fn parse_reference_bare_number_uses_project_and_author() {
        assert_eq!(
            parse_intent_reference("3", "PIMO", "lee-faus"),
            IntentRef::HumanKey("PIMO::lee-faus::3".to_string())
        );
    }

    #[test]
    fn parse_reference_author_scoped_overrides_only_author() {
        assert_eq!(
            parse_intent_reference("alice::3", "PIMO", "lee-faus"),
            IntentRef::HumanKey("PIMO::alice::3".to_string())
        );
    }

    #[test]
    fn parse_reference_full_key_is_exact_with_project_uppercased() {
        assert_eq!(
            parse_intent_reference("pimo::lee-faus::7", "OTHER", "someone"),
            IntentRef::HumanKey("PIMO::lee-faus::7".to_string())
        );
    }

    #[test]
    fn parse_reference_ulid_is_treated_as_uid() {
        // A ULID (26 Crockford base32 chars) is not a number and has no '::'.
        let ulid = "01J8ZE7G2WABCDEFGHJKMNPQRS";
        assert_eq!(
            parse_intent_reference(ulid, "PIMO", "lee"),
            IntentRef::Uid(ulid.to_string())
        );
        // A short prefix is also treated as a uid (prefix match happens later).
        assert_eq!(
            parse_intent_reference("01j8ze", "PIMO", "lee"),
            IntentRef::Uid("01J8ZE".to_string())
        );
    }

    #[test]
    fn vault_entry_roundtrip() {
        let entry = VaultEntry::new(
            VaultEntryType::Session,
            b"# Session transcript\nHello world".to_vec(),
            r#"{"developer":"alice"}"#.to_string(),
            "2025-01-15T12:00:00Z".to_string(),
        );

        // Verify content hash is populated
        assert_ne!(entry.content_hash, [0u8; 32]);

        // Postcard round-trip
        let bytes = postcard::to_allocvec(&entry).expect("serialize");
        let decoded: VaultEntry = postcard::from_bytes(&bytes).expect("deserialize");

        assert_eq!(decoded.entry_type, VaultEntryType::Session);
        assert_eq!(decoded.content_bytes, entry.content_bytes);
        assert_eq!(decoded.content_hash, entry.content_hash);
        assert_eq!(decoded.frontmatter_json, entry.frontmatter_json);
        assert_eq!(decoded.created_at, "2025-01-15T12:00:00Z");
        assert_eq!(decoded.updated_at, "2025-01-15T12:00:00Z");
        assert_eq!(decoded.introduced_by, 0);
    }

    #[test]
    fn vault_entry_content_size() {
        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"some content".to_vec(),
            "{}".to_string(),
            "2025-01-15T12:00:00Z".to_string(),
        );
        assert_eq!(entry.content_size(), 12);
    }

    #[test]
    fn vault_entry_type_display_and_parse() {
        let types = [
            (VaultEntryType::Session, "session"),
            (VaultEntryType::ToolResult, "tool_result"),
            (VaultEntryType::Memory, "memory"),
            (VaultEntryType::Intent, "intent"),
            (VaultEntryType::Skill, "skill"),
            (VaultEntryType::Scratch, "scratch"),
            (VaultEntryType::Attestation, "attestation"),
        ];

        for (ty, expected_str) in &types {
            assert_eq!(ty.as_str(), *expected_str);
            assert_eq!(ty.to_string(), *expected_str);
            assert_eq!(VaultEntryType::parse(expected_str), Some(*ty));
        }

        assert_eq!(VaultEntryType::parse("unknown"), None);
    }

    #[test]
    fn test_vault_entry_type_roundtrip() {
        // as_str / from_str / Display round-trip for the appended Attestation
        // variant.
        assert_eq!(VaultEntryType::Attestation.as_str(), "attestation");
        assert_eq!(VaultEntryType::Attestation.to_string(), "attestation");
        assert_eq!(
            "attestation".parse::<VaultEntryType>().unwrap(),
            VaultEntryType::Attestation
        );

        // postcard encodes the enum discriminant positionally: Attestation is
        // index 6 (last). Round-trip a full VaultEntry through postcard and
        // confirm the discriminant decodes back to Attestation.
        let entry = VaultEntry::new(
            VaultEntryType::Attestation,
            b"{\"@id\":\"x\"}\n".to_vec(),
            "{}".to_string(),
            "2025-01-15T12:00:00Z".to_string(),
        );
        let bytes = postcard::to_allocvec(&entry).expect("serialize");
        let decoded: VaultEntry = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.entry_type, VaultEntryType::Attestation);
        // Positional discriminant sanity: the first byte encodes the variant
        // index (6) for this small enum.
        let ty_bytes = postcard::to_allocvec(&VaultEntryType::Attestation).expect("serialize ty");
        assert_eq!(ty_bytes, vec![6]);
    }

    #[test]
    fn vault_manifest_default() {
        let manifest = VaultManifest::default();
        assert_eq!(manifest.version, 1);
        assert!(manifest.goals.is_empty());
        assert!(manifest.memory.is_empty());
        assert!(manifest.intents.is_empty());
        assert!(manifest.skills.is_empty());
        assert_eq!(manifest.total_bytes, 0);
        assert_eq!(manifest.file_count, 0);
    }

    #[test]
    fn vault_manifest_new_sets_timestamp() {
        let manifest = VaultManifest::new("2025-01-15T12:00:00Z".to_string());
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.updated_at, "2025-01-15T12:00:00Z");
    }

    #[test]
    fn vault_manifest_default_has_intent_fields() {
        let manifest = VaultManifest::default();
        assert_eq!(manifest.next_intent_id, 1);
        assert_eq!(manifest.intent_prefix, "");
    }

    #[test]
    fn vault_manifest_derive_intent_prefix() {
        assert_eq!(VaultManifest::derive_intent_prefix("pi-mono-rs"), "PIMO");
        assert_eq!(VaultManifest::derive_intent_prefix("atomic"), "ATOM");
        assert_eq!(VaultManifest::derive_intent_prefix("my-app"), "MYAP");
        assert_eq!(VaultManifest::derive_intent_prefix("a"), "A");
        assert_eq!(VaultManifest::derive_intent_prefix(""), "");
        assert_eq!(
            VaultManifest::derive_intent_prefix("hello_world_project"),
            "HELL"
        );
    }

    #[test]
    fn vault_manifest_allocate_intent_id() {
        let mut manifest = VaultManifest {
            intent_prefix: "PIMO".to_string(),
            ..Default::default()
        };

        assert_eq!(manifest.allocate_intent_id(), "PIMO-1");
        assert_eq!(manifest.allocate_intent_id(), "PIMO-2");
        assert_eq!(manifest.allocate_intent_id(), "PIMO-3");
        assert_eq!(manifest.next_intent_id, 4);
    }

    #[test]
    fn vault_manifest_allocate_intent_id_default_prefix() {
        let mut manifest = VaultManifest::default();
        assert_eq!(manifest.allocate_intent_id(), "VAULT-1");
    }

    #[test]
    fn vault_manifest_json_roundtrip() {
        let mut manifest = VaultManifest::new("2025-01-15T12:00:00Z".to_string());
        manifest.goals.insert(
            "abc123".to_string(),
            GoalSummary {
                developer: "alice".to_string(),
                intent: Some("fix bug".to_string()),
                status: "complete".to_string(),
                started_at: "2025-01-15T12:00:00Z".to_string(),
                turns: 5,
                tokens: TokenCounts {
                    fresh: 1000,
                    cached: 500,
                    output: 200,
                },
                tool_results: vec!["result1.md".to_string()],
                findings_hash: None,
                bytes: 4096,
            },
        );
        manifest.memory.insert(
            "project.md".to_string(),
            MemorySummary {
                hash: "deadbeef".to_string(),
                bytes: 1024,
                updated_at: "2025-01-15T12:00:00Z".to_string(),
            },
        );
        manifest.total_bytes = 5120;
        manifest.file_count = 3;

        let json = serde_json::to_string(&manifest).expect("serialize");
        let decoded: VaultManifest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.updated_at, "2025-01-15T12:00:00Z");
        assert_eq!(decoded.goals.len(), 1);
        assert_eq!(decoded.memory.len(), 1);
        assert_eq!(decoded.total_bytes, 5120);
        assert_eq!(decoded.file_count, 3);

        let goal = decoded.goals.get("abc123").unwrap();
        assert_eq!(goal.developer, "alice");
        assert_eq!(goal.intent.as_deref(), Some("fix bug"));
        assert_eq!(goal.turns, 5);
        assert_eq!(goal.tokens.fresh, 1000);
    }

    #[test]
    fn vault_manifest_stored_as_json_roundtrip() {
        // VAULT_MANIFEST stores JSON (not postcard), so verify that path.
        let manifest = VaultManifest::new("2025-01-15T12:00:00Z".to_string());
        let json_bytes = serde_json::to_vec(&manifest).expect("serialize");
        let decoded: VaultManifest = serde_json::from_slice(&json_bytes).expect("deserialize");
        assert_eq!(decoded.version, manifest.version);
        assert_eq!(decoded.updated_at, manifest.updated_at);
        assert!(decoded.goals.is_empty());
    }

    #[test]
    fn vault_manifest_deserializes_legacy_sessions_key() {
        // Existing JSON on disk may use "sessions" — the alias ensures it still works.
        let json = r#"{
            "version": 1,
            "updated_at": "2025-01-15T12:00:00Z",
            "merkle": "",
            "sessions": {
                "abc": {
                    "developer": "bob",
                    "status": "complete",
                    "started_at": "2025-01-15T12:00:00Z",
                    "turns": 3,
                    "tokens": {"fresh": 10, "cached": 0, "output": 5},
                    "tool_results": [],
                    "bytes": 100
                }
            }
        }"#;
        let decoded: VaultManifest = serde_json::from_str(json).expect("deserialize legacy");
        assert_eq!(decoded.goals.len(), 1);
        assert_eq!(decoded.goals["abc"].developer, "bob");
    }

    #[test]
    fn intent_summary_deserializes_legacy_sessions_field() {
        // Existing JSON may use "sessions" for the linked-goals count.
        let json = r#"{
            "status": "open",
            "priority": "high",
            "sessions": 5
        }"#;
        let decoded: IntentSummary = serde_json::from_str(json).expect("deserialize legacy");
        assert_eq!(decoded.goals, 5);
    }

    #[test]
    fn vault_entry_type_serde_roundtrip() {
        let ty = VaultEntryType::ToolResult;
        let json = serde_json::to_string(&ty).expect("serialize");
        assert_eq!(json, r#""tool_result""#);
        let decoded: VaultEntryType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, ty);
    }

    #[test]
    fn kg_node_new_and_builder() {
        let node = KgNode::new("change:abc123", "change", "abc123", "change_graph")
            .with_summary("Fix auth bug")
            .with_metadata(serde_json::json!({"view": "main"}));
        assert_eq!(node.id, "change:abc123");
        assert_eq!(node.kind, "change");
        assert_eq!(node.summary, Some("Fix auth bug".to_string()));
        assert!(node.metadata.is_some());
    }

    #[test]
    fn kg_edge_new() {
        let edge = KgEdge::new("change:abc", "file:src/auth.rs", "MODIFIES");
        assert_eq!(edge.from_id, "change:abc");
        assert_eq!(edge.to_id, "file:src/auth.rs");
        assert_eq!(edge.kind, "MODIFIES");
        assert!(edge.metadata.is_none());
    }

    #[test]
    fn kg_subgraph_empty() {
        let sg = KgSubgraph::new();
        assert!(sg.is_empty());
    }

    #[test]
    fn kg_node_json_roundtrip() {
        let node = KgNode::new("file:src/main.rs", "file", "src/main.rs", "change_graph");
        let json = serde_json::to_string(&node).unwrap();
        let decoded: KgNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn kg_node_skips_none_fields() {
        let node = KgNode::new("x", "y", "z", "w");
        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("summary"));
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn embedding_record_roundtrip() {
        let record = EmbeddingRecord {
            vector: vec![0.1, 0.2, 0.3],
            content_hash: [0u8; 32],
            introduced_by: 1,
            chunk_idx: 0,
            preview: "hello world".to_string(),
        };
        let bytes = postcard::to_allocvec(&record).unwrap();
        let decoded: EmbeddingRecord = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.vector, record.vector);
        assert_eq!(decoded.preview, "hello world");
    }
}
