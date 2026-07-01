//! Content hashing over the canonical node.
//!
//! The hash is `blake3(JCS(node))`, not `blake3(markdown body)`. This is the
//! load-bearing change from atomic's current vault (which hashes the raw body
//! and so attests an unstructured surface). Hashing the canonical typed node
//! is what makes the content hash a hash *of the meaning*.
//!
//! Goes through the single `jcs::canonicalize` entry point so it can never
//! drift from what `proof.rs` signs.

use serde_json::Value;

use crate::jcs;

/// The `blake3:<hex>` content hash of a JSON value's canonical form.
pub fn content_hash(value: &Value) -> String {
    let canonical = jcs::canonicalize(value);
    let digest = blake3::hash(canonical.as_bytes());
    format!("blake3:{}", digest.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_is_deterministic_and_key_order_independent() {
        let a = json!({ "b": 1, "a": 2 });
        let b = json!({ "a": 2, "b": 1 });
        assert_eq!(content_hash(&a), content_hash(&b));
        assert!(content_hash(&a).starts_with("blake3:"));
    }

    #[test]
    fn hash_changes_with_content() {
        let a = json!({ "status": "backlog" });
        let b = json!({ "status": "done" });
        assert_ne!(content_hash(&a), content_hash(&b));
    }
}
