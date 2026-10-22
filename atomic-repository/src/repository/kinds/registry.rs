//! The kind registry: every kind a source declares, built from its files.

use std::collections::BTreeMap;

use atomic_canonical::kind_spec::{has_conflict_markers, is_valid_kind_name, KindSpec};

use super::{kind_definition_path, KindFiles, KindPath};
use crate::repository::vault_frontmatter::{parse_frontmatter_strict, split_frontmatter};
use crate::RepositoryError;

/// Whether a definition registered its kind.
#[derive(Debug, Clone, PartialEq)]
pub enum KindStatus {
    Registered(KindSpec),
    /// The definition did not parse cleanly, so nothing about the kind is
    /// registered. Each string is one readable problem.
    Invalid(Vec<String>),
}

/// One kind's definition file, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct KindDefinition {
    pub name: String,
    /// Vault-relative: `kinds/<name>/_kind.md`.
    pub def_path: String,
    pub status: KindStatus,
    /// The definition's markdown body: guidance prose for authors and
    /// agents. It is never validated, and is empty when the file has no
    /// frontmatter block to split it from.
    pub guidance: String,
}

impl KindDefinition {
    /// The spec, when the kind is registered.
    pub fn spec(&self) -> Option<&KindSpec> {
        match &self.status {
            KindStatus::Registered(spec) => Some(spec),
            KindStatus::Invalid(_) => None,
        }
    }

    /// Parse the definition of kind `name` from its file's bytes.
    ///
    /// A missing frontmatter block, a YAML error, a conflict marker anywhere
    /// in the file, or any spec error makes the kind
    /// [`KindStatus::Invalid`]; a kind never registers partially.
    pub fn parse(name: &str, bytes: &[u8]) -> KindDefinition {
        let def_path = kind_definition_path(name);
        let invalid = |problems: Vec<String>, guidance: &str| KindDefinition {
            name: name.to_string(),
            def_path: def_path.clone(),
            status: KindStatus::Invalid(problems),
            guidance: guidance.to_string(),
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return invalid(vec!["the file is not valid UTF-8".into()], "");
        };
        let split = split_frontmatter(text);
        let guidance = split.map_or("", |(_, body)| body);
        if has_conflict_markers(text) {
            return invalid(
                vec!["the file contains unresolved merge-conflict markers".into()],
                guidance,
            );
        }
        let Some((yaml, body)) = split else {
            return invalid(
                vec![
                    "no frontmatter block: the file must open with a `---` line and close the \
                     block with another"
                        .into(),
                ],
                "",
            );
        };
        let fm = match parse_frontmatter_strict(yaml) {
            Ok(fm) => fm,
            Err(e) => return invalid(vec![format!("frontmatter: {e}")], body),
        };
        match KindSpec::from_frontmatter(name, &fm) {
            Ok(spec) => KindDefinition {
                name: name.to_string(),
                def_path,
                status: KindStatus::Registered(spec),
                guidance: body.to_string(),
            },
            Err(e) => invalid(e.problems, body),
        }
    }
}

/// A kind in a [`KindRegistry`]: its definition plus the instance files
/// found beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct KindEntry {
    pub definition: KindDefinition,
    /// Vault-relative instance paths (`kinds/<k>/<ULID>.md`), sorted.
    pub instances: Vec<String>,
}

impl KindEntry {
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    pub fn spec(&self) -> Option<&KindSpec> {
        self.definition.spec()
    }
}

/// Every kind declared in one source, plus the files under `kinds/` that
/// belong to no declared kind.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KindRegistry {
    kinds: BTreeMap<String, KindEntry>,
    unregistered: BTreeMap<String, Vec<String>>,
    stray: Vec<String>,
}

impl KindRegistry {
    /// Walk `kinds/**` in `files` and parse every definition.
    ///
    /// Prefer [`Repository::kind_registry`](crate::Repository::kind_registry),
    /// which caches view registries.
    pub fn load(files: &KindFiles<'_>) -> Result<KindRegistry, RepositoryError> {
        let mut registry = KindRegistry::default();
        let mut instances: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut definitions = Vec::new();
        for path in files.list_kind_files()? {
            match KindPath::classify(&path) {
                Some(KindPath::Definition { kind }) => definitions.push(kind),
                Some(KindPath::Instance { kind, .. }) => {
                    instances.entry(kind).or_default().push(path)
                }
                Some(KindPath::Stray) | None => registry.stray.push(path),
            }
        }
        for kind in definitions {
            if let Some(definition) = Self::load_kind(files, &kind)? {
                registry.kinds.insert(
                    kind.clone(),
                    KindEntry {
                        definition,
                        instances: instances.remove(&kind).unwrap_or_default(),
                    },
                );
            }
        }
        registry.unregistered = instances;
        Ok(registry)
    }

    /// Read and parse only `kinds/<name>/_kind.md`, without walking any
    /// other kind or listing instances. Returns `None` when the definition
    /// file does not exist or `name` is not a valid kind name. A definition
    /// with an unresolved name conflict in a view is [`KindStatus::Invalid`].
    ///
    /// This is the per-instance lookup for KG projection.
    pub fn load_kind(
        files: &KindFiles<'_>,
        name: &str,
    ) -> Result<Option<KindDefinition>, RepositoryError> {
        if !is_valid_kind_name(name) {
            return Ok(None);
        }
        let def_path = kind_definition_path(name);
        if let Some(conflict) = files.name_conflict(&def_path) {
            // Neither side can be read, so nothing registers: fail closed.
            return Ok(Some(KindDefinition {
                name: name.to_string(),
                def_path,
                status: KindStatus::Invalid(vec![conflict.describe()]),
                guidance: String::new(),
            }));
        }
        Ok(files
            .read(&kind_definition_path(name))?
            .map(|bytes| KindDefinition::parse(name, &bytes)))
    }

    pub fn get(&self, name: &str) -> Option<&KindEntry> {
        self.kinds.get(name)
    }

    /// The spec of `name`, when that kind is declared and registered.
    pub fn spec(&self, name: &str) -> Option<&KindSpec> {
        self.get(name).and_then(KindEntry::spec)
    }

    /// Every declared kind (registered or invalid), by name.
    pub fn kinds(&self) -> impl Iterator<Item = &KindEntry> {
        self.kinds.values()
    }

    /// Instance files whose kind directory has no `_kind.md`, by kind name.
    pub fn unregistered(&self) -> &BTreeMap<String, Vec<String>> {
        &self.unregistered
    }

    /// Files under `kinds/` that are neither a definition nor an instance
    /// (see [`KindPath::Stray`]), sorted.
    pub fn stray(&self) -> &[String] {
        &self.stray
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.unregistered.is_empty() && self.stray.is_empty()
    }
}
