//! `atomic kind init <k>` — write a starter definition for a new kind.

use clap::Parser;

use atomic_canonical::kind_spec::{is_reserved_kind_name, is_valid_kind_name};
use atomic_repository::{kind_definition_path, KindDefinition, KindStatus, Repository};

use super::{display_path, open_repository, record_hint};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{hint, success};

/// Write a starter definition at `.vault/kinds/<k>/_kind.md`.
///
/// The starter declares one `status` enum with a default, so it registers
/// as written. Edit its fields, check it with `atomic kind validate <k>`,
/// then record it as its own change before creating instances.
#[derive(Parser, Debug)]
#[command(name = "init")]
pub struct KindInit {
    /// The kind name: lowercase letters, digits, `_` and `-`, starting with a
    /// letter, and not a built-in vault name such as `intent` or `memory`.
    pub kind: String,

    /// Emit `{"kind","file","registered"}` instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

impl Command for KindInit {
    fn run(&self) -> CliResult<()> {
        let kind = self.kind.as_str();
        check_new_kind_name(kind)?;

        let repo = open_repository()?;
        let rel = kind_definition_path(kind);
        let file = repo.vault_dir().join(&rel);
        if std::fs::symlink_metadata(&file).is_ok() {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "{} already exists; edit it instead of re-initializing",
                    display_path(&rel)
                ),
            });
        }

        let content = starter_definition(kind);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_new_file(&file, &content)?;
        ingest(&repo, &rel, &content)?;

        let registered = matches!(
            KindDefinition::parse(kind, content.as_bytes()).status,
            KindStatus::Registered(_)
        );
        if self.json {
            let out = serde_json::json!({
                "kind": kind,
                "file": display_path(&rel),
                "registered": registered,
            });
            println!("{out}");
            return Ok(());
        }

        println!("{} {}", success("Created"), display_path(&rel));
        println!();
        println!("{}", hint("Edit the fields, then check the definition:"));
        println!("  atomic kind validate {kind}");
        println!(
            "{}",
            hint("Record it as its own change before creating instances:")
        );
        for line in record_hint(&rel, &format!("feat: add {kind} kind")) {
            println!("  {line}");
        }
        Ok(())
    }
}

/// Reject a name that is malformed or reserved for a built-in vault kind.
fn check_new_kind_name(kind: &str) -> CliResult<()> {
    if !is_valid_kind_name(kind) {
        return Err(CliError::InvalidArgument {
            message: format!(
                "invalid kind name {kind:?}: use 1-63 lowercase letters, digits, `_` or `-`, \
                 starting with a letter"
            ),
        });
    }
    if is_reserved_kind_name(kind) {
        return Err(CliError::InvalidArgument {
            message: format!("kind name {kind:?} is reserved for a built-in vault kind"),
        });
    }
    Ok(())
}

/// Create `path` with `content`, failing if anything is already there.
pub(crate) fn write_new_file(path: &std::path::Path, content: &str) -> CliResult<()> {
    use std::io::Write;
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => CliError::InvalidArgument {
                message: format!("{} already exists", path.display()),
            },
            _ => CliError::Io(e),
        })?;
    handle.write_all(content.as_bytes())?;
    Ok(())
}

/// Deflate one just-written vault file into the vault index, through the
/// same path `atomic vault sync` uses for files on disk. A repository
/// without a vault has no index to update.
pub(crate) fn ingest(repo: &Repository, vault_relative: &str, content: &str) -> CliResult<()> {
    if !repo.has_vault().map_err(CliError::Repository)? {
        return Ok(());
    }
    repo.vault_record_files(&[(vault_relative.to_string(), Some(content.to_string()))])
        .map_err(CliError::Repository)?;
    Ok(())
}

/// A definition that registers as written: one `status` enum with a
/// default, and a guidance body explaining the file's two halves.
fn starter_definition(kind: &str) -> String {
    // A kind name like `true` or `null` would read back as a boolean or
    // null if written bare, so quote it.
    let quoted = serde_json::to_string(kind).expect("a string serializes");
    format!(
        "---\n\
name: {quoted}\n\
specVersion: 1\n\
title: {quoted}\n\
fields:\n\
\x20 status:\n\
\x20   type: enum\n\
\x20   values: [draft, active, retired]\n\
\x20   default: draft\n\
\x20   required: true\n\
\x20   description: Where this {kind} is in its lifecycle.\n\
body:\n\
\x20 required: false\n\
embed: false\n\
context: false\n\
---\n\
# {kind}\n\
\n\
This body is guidance for the people and agents who write `{kind}` entries:\n\
when to create one, what a good one contains, and how it relates to other\n\
vault nodes. It is shown by `atomic kind show {kind} --guidance` and is never\n\
validated.\n\
\n\
The frontmatter above is the schema. Each field has a `type` (`string`,\n\
`integer`, `boolean`, `enum` or `ref`, each with a `[]` suffix for a list)\n\
and may set `required`, `default` and `description`. An `enum` lists its\n\
`values`; a `ref` names its `target` kinds (`intent`, `memory`, or another\n\
custom kind) and may declare an `edge` (UPPER_SNAKE_CASE), projected into the\n\
knowledge graph as `{kind}.<EDGE>`. Instances may only use declared fields.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_definition_registers() {
        for kind in ["adr", "true", "null", "run-book_2"] {
            let def = KindDefinition::parse(kind, starter_definition(kind).as_bytes());
            let KindStatus::Registered(spec) = &def.status else {
                panic!("{kind}: {:?}", def.status);
            };
            assert_eq!(spec.name, kind);
            let status = spec.field("status").expect("status field");
            assert_eq!(status.default, Some(serde_json::json!("draft")));
            assert!(def.guidance.contains("is never\nvalidated"));
        }
    }

    #[test]
    fn reserved_and_malformed_names_are_rejected() {
        assert!(check_new_kind_name("adr").is_ok());
        assert!(check_new_kind_name("intent").is_err());
        assert!(check_new_kind_name("kind").is_err());
        assert!(check_new_kind_name("Adr").is_err());
        assert!(check_new_kind_name("1adr").is_err());
    }
}
