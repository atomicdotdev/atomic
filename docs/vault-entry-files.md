# Complete Vault entry files

Vault Markdown is an editable projection. Use a complete entry file when rebuilding stored Vault entries: it retains binary body bytes, the exact `frontmatter_json` string, entry type, body hash, timestamps and the introducing-change reference.

The repository API is explicit:

```rust,ignore
let file = source.vault_export_entry_file("memory/example.md", "stable-source-origin")?;
let revision = file.revision()?;
std::fs::write("example.vault-entry", file.to_bytes()?)?;

// Transfer and import the introducing change first, if there is one.
receiver.vault_restore_entry_file(&std::fs::read("example.vault-entry")?)?;
let original = receiver.vault_retained_entry_file(&revision)?;
```

Use the same stable source namespace for exports from the same origin. The API does not invent a global repository identity. A file keeps the source's `introduced_by` number and its namespace, plus the Change hash used to resolve that reference in the receiver. An unrecorded entry keeps `introduced_by = 0` and has no introducing hash. Unresolved recorded references fail instead of becoming unrecorded entries.

`atomic.vault.entry.file.v1` uses the existing `VaultEntry` serialization inside a strict JSON record. Body bytes are byte arrays; frontmatter remains an opaque string, so source formatting and large numbers inside it are preserved. The complete revision hash covers all fields. A title or timestamp edit changes that hash even if the body hash stays the same. Unknown schemas, unknown fields, noncanonical encoding and invalid body hashes are refused.

Restore commits the mapped entry, its manifest listing and the exact original file together. The new `VAULT_ENTRY_REVISIONS` table retains that original file, including its source-local ID. Repeating a restore is safe. Restore refuses a different existing entry to protect local work; restore into a fresh receiver for reconstruction. Deleting a current entry does not delete retained files.

This API covers individual Vault entries. It does not export the whole Vault manifest, authored knowledge graph, embeddings, attestations or every historical revision. It does not add automatic push/pull discovery, an outbox, deletion propagation or remote conflict resolution. Those belong to the broader replication work. Existing Markdown and libatomic/RPC bundle formats are unchanged.
