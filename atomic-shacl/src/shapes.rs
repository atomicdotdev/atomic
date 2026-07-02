//! The embedded Turtle shape graphs.
//!
//! The `.ttl` files under `src/shapes/` are the team-editable, standard-SHACL
//! source of truth for Intent/Memory policy. They are compiled into the binary
//! here via `include_str!` — the compiled bytes are the trusted artifact that
//! actually runs, while the on-disk file stays the reviewable/diffable source
//! (and remains loadable by any external SHACL validator).

/// IntentShape + AcceptanceCriterionShape (Core rules + the two SHACL-SPARQL conditionals).
pub const INTENT_SHAPES_TTL: &str = include_str!("shapes/intent.ttl");

/// MemoryShape (Core rules; the directionality absence is structural — no forward-edge rule).
pub const MEMORY_SHAPES_TTL: &str = include_str!("shapes/memory.ttl");
