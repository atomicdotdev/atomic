//! The JSON-LD `@context` document — the real term→IRI table served at
//! [`crate::node::CONTEXT_URL`].
//!
//! Every canonical node (Intent, Memory) and the PROV projection name this one
//! context by URL. Shipping the actual document (rather than a bare URL) makes
//! the emitted JSON genuine, offline-processable RDF: each short key maps to a
//! `prov:`/`atom:`/`sec:`/`rdfs:`/`xsd:` IRI, object properties are `@type:@id`,
//! and timestamps are `xsd:dateTime`. The compiled bytes are the trusted
//! artifact; the file under `ns/` is the reviewable source.

use std::collections::BTreeSet;

use serde_json::Value;

/// The embedded `@context` document (the bytes served at `CONTEXT_URL`).
pub const CONTEXT_JSONLD: &str = include_str!("../ns/ctx.jsonld");

/// The set of term keys defined by the context (terms + namespace prefixes).
/// Parsed from the embedded document.
pub fn defined_terms() -> BTreeSet<String> {
    let doc: Value = serde_json::from_str(CONTEXT_JSONLD).expect("ctx.jsonld is valid JSON");
    doc.get("@context")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Collect every object key appearing anywhere in `value`.
pub fn collect_keys(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                out.insert(k.clone());
                collect_keys(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_keys(v, out)),
        _ => {}
    }
}

/// The keys in `value` that the context must define: bare terms only —
/// JSON-LD keywords (`@id`, `@type`, …) need no term, and CURIEs (`prov:Activity`)
/// resolve through their namespace prefix.
pub fn undefined_terms(value: &Value) -> BTreeSet<String> {
    let defined = defined_terms();
    let mut keys = BTreeSet::new();
    collect_keys(value, &mut keys);
    keys.into_iter()
        .filter(|k| !k.starts_with('@') && !k.contains(':'))
        .filter(|k| !defined.contains(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_document_is_valid_and_nonempty() {
        assert!(defined_terms().len() > 20, "context should define the vocabulary");
        assert!(defined_terms().contains("prov"));
        assert!(defined_terms().contains("used"));
        assert!(defined_terms().contains("associatedWith"));
        assert!(defined_terms().contains("label"));
    }
}
