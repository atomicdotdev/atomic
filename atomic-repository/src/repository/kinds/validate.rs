//! Reference resolution and the read-only kind validation report.

use atomic_canonical::kind_spec::{
    has_conflict_markers, instance_kg_id, is_valid_kind_name, is_valid_ulid, validate_instance,
    BaseType, FieldSpec, InstanceReport, KindSpec, KindViolation, KindViolationCode, RefResolver,
    BUILTIN_REF_TARGETS,
};
use atomic_core::pristine::VaultEntryType;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use std::cell::RefCell;
use std::collections::HashMap;

use super::{
    kind_instance_path, KindFiles, KindPath, KindRegistry, KindSource, KindStatus, KINDS_DIR,
};
use crate::repository::vault_frontmatter::{parse_frontmatter_strict, split_frontmatter};
use crate::repository::vault_triples::entry_subject;
use crate::repository::Repository;
use crate::RepositoryError;

/// Resolves kind ref values, against one [`KindFiles`] source
/// ([`KindRefResolver::new`]) or against the stored vault entries in redb
/// ([`KindRefResolver::stored`]).
///
/// - A custom kind `k`: `raw` must be a ULID, `k` must be **registered** in
///   the same source (its `_kind.md` parses into a valid spec), and
///   `kinds/k/<raw>.md` must exist there; resolves to `k:<raw>`. An instance
///   of an unregistered or invalid kind is not a node, so a ref to it does
///   not resolve.
/// - `memory`: `memory/<raw>.md` must exist in the source; resolves to
///   `memory:<raw>`.
/// - `intent`: `raw` must be the intent's full ULID (any case) or its full
///   human key exactly as stored in the manifest (`PIMO::alice::3`);
///   resolves to the intent's KG subject. Short forms that depend on the
///   current identity (`3`, `alice::3`) or on uniqueness (a ULID prefix) do
///   not resolve: [`Repository::canonicalize_kind_refs`] rewrites them
///   before an instance is written.
///
/// Intent resolution always reads the redb vault index, because intents are
/// addressed by keys only the manifest maps to paths. Against a view source
/// an intent ref therefore resolves when the intent exists anywhere in the
/// repository, even if the view does not track it.
pub struct KindRefResolver<'a, 'r> {
    backend: Backend<'a, 'r>,
    /// Whether each custom target kind is registered, looked up once.
    registered: RefCell<HashMap<String, bool>>,
}

enum Backend<'a, 'r> {
    Files(&'a KindFiles<'r>),
    Stored(&'a Repository),
}

impl<'a, 'r> KindRefResolver<'a, 'r> {
    /// Resolve against the files of `files`' source.
    pub fn new(files: &'a KindFiles<'r>) -> Self {
        Self {
            backend: Backend::Files(files),
            registered: RefCell::default(),
        }
    }

    fn repository(&self) -> &Repository {
        match &self.backend {
            Backend::Files(files) => files.repository(),
            Backend::Stored(repo) => repo,
        }
    }

    fn exists(&self, path: &str) -> bool {
        match &self.backend {
            Backend::Files(files) => files.exists(path),
            Backend::Stored(repo) => matches!(repo.vault_retrieve(path), Ok(Some(_))),
        }
    }

    fn is_registered(&self, kind: &str) -> bool {
        if let Some(&known) = self.registered.borrow().get(kind) {
            return known;
        }
        let definition = match &self.backend {
            Backend::Files(files) => KindRegistry::load_kind(files, kind),
            Backend::Stored(repo) => repo.stored_kind_definition(kind),
        };
        let registered = definition
            .unwrap_or_else(|e| {
                log::debug!("kind ref target {kind}: {e}");
                None
            })
            .is_some_and(|d| d.spec().is_some());
        self.registered
            .borrow_mut()
            .insert(kind.to_string(), registered);
        registered
    }
}

impl<'a> KindRefResolver<'a, 'a> {
    /// Resolve against the stored vault entries: a custom-kind target must
    /// have a stored instance entry and a stored, registering definition,
    /// and a memory target a stored entry. This is what KG projection uses,
    /// so an instance and everything it refers to come from the same layer.
    pub fn stored(repo: &'a Repository) -> Self {
        Self {
            backend: Backend::Stored(repo),
            registered: RefCell::default(),
        }
    }
}

impl RefResolver for KindRefResolver<'_, '_> {
    fn resolve(&self, target_kind: &str, raw: &str) -> Option<String> {
        match target_kind {
            "intent" => {
                let path = self.repository().intent_path_strict(raw)?;
                Some(entry_subject(&path, VaultEntryType::Intent))
            }
            "memory" => {
                let name_ok = !raw.is_empty()
                    && !raw.contains('\\')
                    && raw
                        .split('/')
                        .all(|c| !c.is_empty() && c != "." && c != "..");
                let path = format!("memory/{raw}.md");
                (name_ok && self.exists(&path))
                    .then(|| entry_subject(&path, VaultEntryType::Memory))
            }
            kind if is_valid_kind_name(kind) && is_valid_ulid(raw) => (self.is_registered(kind)
                && self.exists(&kind_instance_path(kind, raw)))
            .then(|| instance_kg_id(kind, raw)),
            _ => None,
        }
    }
}

/// The outcome of [`check_instance`].
#[derive(Debug, Clone, PartialEq)]
pub enum InstanceCheck {
    /// The file parsed and was validated against the spec.
    Checked {
        /// The strictly parsed frontmatter (system keys removed).
        frontmatter: Map<String, Value>,
        body: String,
        report: InstanceReport,
    },
    /// The file could not be validated at all: `code` is
    /// [`KindProblemCode::ConflictMarker`] or
    /// [`KindProblemCode::FrontmatterInvalid`].
    Problem {
        code: KindProblemCode,
        message: String,
    },
}

impl InstanceCheck {
    /// True only for a checked instance with no violations.
    pub fn conforms(&self) -> bool {
        matches!(self, InstanceCheck::Checked { report, .. } if report.conforms)
    }
}

/// Validate one instance file (`kinds/<k>/<uid>.md`) of a registered kind.
///
/// `uid` is the filename stem; `bytes` the whole raw file. A conflict marker
/// anywhere in the file (frontmatter included) or frontmatter that fails
/// strict parsing stops the check before any field is examined.
pub fn check_instance(
    spec: &KindSpec,
    uid: &str,
    bytes: &[u8],
    resolver: &dyn RefResolver,
) -> InstanceCheck {
    let problem = |code, message: &str| InstanceCheck::Problem {
        code,
        message: message.to_string(),
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return problem(
            KindProblemCode::FrontmatterInvalid,
            "the file is not valid UTF-8",
        );
    };
    if has_conflict_markers(text) {
        return problem(
            KindProblemCode::ConflictMarker,
            "the file contains unresolved merge-conflict markers",
        );
    }
    let Some((yaml, body)) = split_frontmatter(text) else {
        return problem(
            KindProblemCode::FrontmatterInvalid,
            "no frontmatter block: the file must open with a `---` line and close the block \
             with another",
        );
    };
    let frontmatter = match parse_frontmatter_strict(yaml) {
        Ok(fm) => fm,
        Err(e) => {
            return problem(
                KindProblemCode::FrontmatterInvalid,
                &format!("frontmatter: {e}"),
            )
        }
    };
    let report = validate_instance(spec, uid, &frontmatter, body, resolver);
    InstanceCheck::Checked {
        frontmatter,
        body: body.to_string(),
        report,
    }
}

/// Stable codes for problems found by [`Repository::kind_validate`] above
/// the level of a single field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KindProblemCode {
    /// Instances under `kinds/<k>/` with no `kinds/<k>/_kind.md`.
    UnregisteredKind,
    /// A definition that does not register, and each of its instances
    /// (whose fields are then not validated).
    KindInvalid,
    /// A file under `kinds/` that is neither `<k>/_kind.md` nor
    /// `<k>/<ULID>.md`.
    StrayFile,
    /// An instance whose frontmatter is missing or fails strict parsing.
    FrontmatterInvalid,
    /// An instance with merge-conflict markers anywhere in the file.
    ConflictMarker,
    /// A `kinds/**` instance or directory that two or more files or
    /// directories claim in a view (an unresolved name conflict). The view
    /// renders none of them, so the instance cannot be read. Reported only
    /// for a view source: in the working copy the same conflict is a file
    /// holding both sides between markers, which is `CONFLICT_MARKER`. A
    /// name-conflicted *definition* makes its kind invalid (`KIND_INVALID`).
    NameConflict,
    /// An instance path that could not be read as a regular file: a
    /// symlink (never followed), something other than a file, a read error,
    /// or a file removed while validating. Its fields were not validated.
    Unreadable,
}

impl KindProblemCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            KindProblemCode::UnregisteredKind => "UNREGISTERED_KIND",
            KindProblemCode::KindInvalid => "KIND_INVALID",
            KindProblemCode::StrayFile => "STRAY_FILE",
            KindProblemCode::FrontmatterInvalid => "FRONTMATTER_INVALID",
            KindProblemCode::ConflictMarker => "CONFLICT_MARKER",
            KindProblemCode::NameConflict => "NAME_CONFLICT",
            KindProblemCode::Unreadable => "UNREADABLE",
        }
    }
}

impl std::fmt::Display for KindProblemCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A repository-level problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindProblem {
    pub code: KindProblemCode,
    /// Vault-relative path of the file at fault.
    pub path: String,
    /// The kind directory the file is in, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KindReportStatus {
    Registered,
    Invalid,
}

/// One instance's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceResult {
    /// Vault-relative: `kinds/<k>/<uid>.md`.
    pub path: String,
    /// The filename stem.
    pub uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub conforms: bool,
    pub violations: Vec<KindViolation>,
    /// Set when the instance was not validated field by field; the matching
    /// entry is in [`KindValidationReport::problems`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<KindProblemCode>,
}

/// One declared kind's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindReport {
    pub name: String,
    pub def_path: String,
    pub status: KindReportStatus,
    /// Why the definition did not register; empty when it did.
    pub spec_problems: Vec<String>,
    /// The namespaced edges (`<kind>.<EDGE>`) the kind can project.
    pub edges: Vec<String>,
    pub instance_count: usize,
    pub instances: Vec<InstanceResult>,
}

/// The result of [`Repository::kind_validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindValidationReport {
    pub source: KindSource,
    /// No problems, and every instance conforms.
    pub ok: bool,
    pub kinds: Vec<KindReport>,
    pub problems: Vec<KindProblem>,
}

/// The `<k>` of `kinds/<k>/...`, when the path has a kind directory.
fn kind_dir(path: &str) -> Option<&str> {
    path.strip_prefix(KINDS_DIR)?
        .strip_prefix('/')?
        .split_once('/')
        .map(|(k, _)| k)
}

impl Repository {
    /// Validate every kind in `source`, or only `kind_filter`.
    ///
    /// Checks each definition, every instance against its kind's spec
    /// (including that refs resolve, see [`KindRefResolver`]), and that
    /// `kinds/` holds nothing but definitions and instances. Nothing is
    /// written.
    ///
    /// # Errors
    ///
    /// [`RepositoryError::ViewNotFound`] for a missing view, and
    /// [`RepositoryError::InvalidOperation`] when `kind_filter` names a kind
    /// with no files at all in `source`.
    pub fn kind_validate(
        &self,
        source: &KindSource,
        kind_filter: Option<&str>,
    ) -> Result<KindValidationReport, RepositoryError> {
        let files = self.kind_files(source)?;
        let registry = self.kind_registry_for(&files)?;
        let resolver = KindRefResolver::new(&files);
        let wanted = |kind: Option<&str>| kind_filter.is_none() || kind == kind_filter;

        if let Some(k) = kind_filter {
            let known = registry.get(k).is_some()
                || registry.unregistered().contains_key(k)
                || registry.stray().iter().any(|p| kind_dir(p) == Some(k));
            if !known {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("no kind named '{k}' in the {source}"),
                });
            }
        }

        let mut problems = Vec::new();
        let mut kinds = Vec::new();
        for entry in registry.kinds().filter(|e| wanted(Some(e.name()))) {
            let def = &entry.definition;
            let mut report = KindReport {
                name: def.name.clone(),
                def_path: def.def_path.clone(),
                status: KindReportStatus::Registered,
                spec_problems: Vec::new(),
                edges: Vec::new(),
                instance_count: entry.instances.len(),
                instances: Vec::new(),
            };
            match &def.status {
                KindStatus::Registered(spec) => {
                    report.edges = spec.edges();
                    for path in &entry.instances {
                        let result =
                            self.validate_one(&files, spec, path, &resolver, &mut problems)?;
                        report.instances.push(result);
                    }
                }
                KindStatus::Invalid(spec_problems) => {
                    report.status = KindReportStatus::Invalid;
                    report.spec_problems = spec_problems.clone();
                    problems.push(KindProblem {
                        code: KindProblemCode::KindInvalid,
                        path: def.def_path.clone(),
                        kind: Some(def.name.clone()),
                        message: format!(
                            "kind '{}' is not registered: {}",
                            def.name,
                            spec_problems.join("; ")
                        ),
                    });
                    for path in &entry.instances {
                        problems.push(KindProblem {
                            code: KindProblemCode::KindInvalid,
                            path: path.clone(),
                            kind: Some(def.name.clone()),
                            message: format!(
                                "instance of invalid kind '{}'; its fields were not validated",
                                def.name
                            ),
                        });
                        report.instances.push(InstanceResult {
                            path: path.clone(),
                            uid: instance_uid(path),
                            title: None,
                            conforms: false,
                            violations: Vec::new(),
                            problem: Some(KindProblemCode::KindInvalid),
                        });
                    }
                }
            }
            kinds.push(report);
        }

        for (kind, paths) in registry.unregistered() {
            if !wanted(Some(kind)) {
                continue;
            }
            for path in paths {
                problems.push(KindProblem {
                    code: KindProblemCode::UnregisteredKind,
                    path: path.clone(),
                    kind: Some(kind.clone()),
                    message: format!(
                        "instance of undeclared kind '{kind}': {} does not exist",
                        super::kind_definition_path(kind)
                    ),
                });
            }
        }

        for path in registry.stray() {
            let kind = kind_dir(path);
            if !wanted(kind) {
                continue;
            }
            problems.push(KindProblem {
                code: KindProblemCode::StrayFile,
                path: path.clone(),
                kind: kind.map(str::to_string),
                message: "kinds/ may only hold <kind>/_kind.md definitions and <kind>/<ULID>.md \
                          instances (an uppercase ULID filename, a valid kind directory name, \
                          no nested directories)"
                    .to_string(),
            });
        }

        // A conflicted directory can hide the files under it from the view,
        // so it is a problem in its own right. (Conflicted files were listed
        // by the walk and reported above.)
        for (path, conflict) in files.name_conflicts().filter(|(_, c)| c.directory) {
            // `kinds/<k>` itself names its kind too.
            let kind = kind_dir(path).or_else(|| path.strip_prefix(&format!("{KINDS_DIR}/")));
            if !wanted(kind) {
                continue;
            }
            problems.push(KindProblem {
                code: KindProblemCode::NameConflict,
                path: path.to_string(),
                kind: kind.map(str::to_string),
                message: conflict.describe(),
            });
        }

        let ok = problems.is_empty() && kinds.iter().flat_map(|k| &k.instances).all(|i| i.conforms);
        Ok(KindValidationReport {
            source: source.clone(),
            ok,
            kinds,
            problems,
        })
    }

    /// Rewrite every user-typed intent reference in `fm` to the form a
    /// stored instance must use (see [`KindRefResolver`]): the intent's
    /// full human key when it has one, else its full ULID.
    ///
    /// Only values of `ref` fields that target `intent` are touched, and
    /// only when the value does not already resolve as written (against the
    /// working copy, trying the field's targets in order) — so a full key, a
    /// full ULID, or a ref to another target kind stays as it is. A value is
    /// looked up the way `atomic intent show` does: `3` and `alice::3` are
    /// composed with the current project and identity, and a ULID prefix
    /// must be unique. An `intent:`-qualified value keeps its qualifier.
    ///
    /// A value no intent matches is left alone for validation to report as
    /// `UNRESOLVED_REF`.
    ///
    /// # Errors
    ///
    /// One `UNRESOLVED_REF` violation per value that is ambiguous (a ULID
    /// prefix shared by several intents); `fm` is still updated for every
    /// other value.
    pub fn canonicalize_kind_refs(
        &self,
        spec: &KindSpec,
        fm: &mut Map<String, Value>,
    ) -> Result<(), Vec<KindViolation>> {
        let files = self.kind_files(&KindSource::WorkingCopy).map_err(|e| {
            vec![KindViolation {
                code: KindViolationCode::UnresolvedRef,
                field: None,
                message: e.to_string(),
            }]
        })?;
        let resolver = KindRefResolver::new(&files);
        let mut violations = Vec::new();
        for (name, field) in &spec.fields {
            if field.field_type.base != BaseType::Ref || !field.target.iter().any(|t| t == "intent")
            {
                continue;
            }
            let Some(value) = fm.get_mut(name) else {
                continue;
            };
            let items: Vec<&mut Value> = match value {
                Value::Array(items) => items.iter_mut().collect(),
                other => vec![other],
            };
            for item in items {
                let Value::String(raw) = item else { continue };
                match self.canonical_kind_intent_ref(field, raw.trim(), &resolver) {
                    Ok(Some(canonical)) => *raw = canonical,
                    Ok(None) => {}
                    Err(message) => violations.push(KindViolation {
                        code: KindViolationCode::UnresolvedRef,
                        field: Some(name.clone()),
                        message,
                    }),
                }
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// The canonical form of one ref value, or `None` to leave it as it is.
    fn canonical_kind_intent_ref(
        &self,
        field: &FieldSpec,
        raw: &str,
        resolver: &KindRefResolver<'_, '_>,
    ) -> Result<Option<String>, String> {
        // `<kind>:<id>` with a single colon is qualified, as in validation;
        // `alice::3` is an intent short form, not kind `alice`.
        let qualified = raw.split_once(':').filter(|(k, id)| {
            !id.starts_with(':') && (BUILTIN_REF_TARGETS.contains(k) || is_valid_kind_name(k))
        });
        let (prefix, id) = match qualified {
            Some(("intent", id)) => ("intent:", id),
            Some(_) => return Ok(None),
            None => {
                if field
                    .target
                    .iter()
                    .any(|t| resolver.resolve(t, raw).is_some())
                {
                    return Ok(None);
                }
                ("", raw)
            }
        };
        if resolver.resolve("intent", id).is_some() {
            return Ok(None);
        }
        match self.canonical_intent_reference(id) {
            Ok(found) => Ok(found.map(|canonical| format!("{prefix}{canonical}"))),
            Err(e) => Err(format!("{raw:?}: {e}")),
        }
    }

    fn validate_one(
        &self,
        files: &KindFiles<'_>,
        spec: &KindSpec,
        path: &str,
        resolver: &KindRefResolver<'_, '_>,
        problems: &mut Vec<KindProblem>,
    ) -> Result<InstanceResult, RepositoryError> {
        let uid = instance_uid(path);
        let mut result = InstanceResult {
            path: path.to_string(),
            uid: uid.clone(),
            title: None,
            conforms: false,
            violations: Vec::new(),
            problem: None,
        };
        if let Some(conflict) = files.name_conflict(path) {
            result.problem = Some(KindProblemCode::NameConflict);
            problems.push(KindProblem {
                code: KindProblemCode::NameConflict,
                path: path.to_string(),
                kind: Some(spec.name.clone()),
                message: conflict.describe(),
            });
            return Ok(result);
        }
        let read = match files.read(path) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(
                "not a regular file (a symlink, which is never followed, or \
                             something else), or removed while validating"
                    .to_string(),
            ),
            Err(e) => Err(format!("could not be read: {e}")),
        };
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(message) => {
                result.problem = Some(KindProblemCode::Unreadable);
                problems.push(KindProblem {
                    code: KindProblemCode::Unreadable,
                    path: path.to_string(),
                    kind: Some(spec.name.clone()),
                    message,
                });
                return Ok(result);
            }
        };
        match check_instance(spec, &uid, &bytes, resolver) {
            InstanceCheck::Checked {
                frontmatter,
                report,
                ..
            } => {
                result.title = frontmatter
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                result.conforms = report.conforms;
                result.violations = report.violations;
            }
            InstanceCheck::Problem { code, message } => {
                result.problem = Some(code);
                problems.push(KindProblem {
                    code,
                    path: path.to_string(),
                    kind: Some(spec.name.clone()),
                    message,
                });
            }
        }
        Ok(result)
    }
}

fn instance_uid(path: &str) -> String {
    match KindPath::classify(path) {
        Some(KindPath::Instance { uid, .. }) => uid,
        _ => String::new(),
    }
}
