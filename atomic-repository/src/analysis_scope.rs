//! Source-selection policy shared by review and static code relationship adapters.

use std::path::{Component, Path};

/// Vault artifacts remain repository/review evidence, but are never application
/// code inputs merely because an artifact happens to have a source extension.
/// This accepts canonical repository-relative paths and native path components.
pub fn is_vault_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == ".vault"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_components_are_metadata_but_lookalikes_are_not() {
        for path in [
            ".vault",
            ".vault/memory/a.md",
            "./.vault/attestations/a.md",
            ".vault/generated.ts",
            "nested/.vault/generated.rs",
        ] {
            assert!(is_vault_path(path), "{path}");
        }
        for path in [
            "src/vault.ts",
            ".vaultish/code.ts",
            "src/.vault-backup/code.rs",
            "vault/code.ts",
            "README.md",
        ] {
            assert!(!is_vault_path(path), "{path}");
        }
    }
}
