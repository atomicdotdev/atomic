//! Reference resolution and the read-only kind validation report.

use atomic_canonical::kind_spec::{
    has_conflict_markers, instance_kg_id, is_valid_kind_name, is_valid_ulid, validate_instance,
    InstanceReport, KindSpec, KindViolation, RefResolver,
};
use atomic_core::pristine::VaultEntryType;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{kind_instance_path, KindFiles, KindPath, KindSource, KindStatus, KINDS_DIR};
use crate::repository::vault_frontmatter::{parse_frontmatter_strict, split_frontmatter};
use crate::repository::vault_triples::entry_subject;
use crate::repository::Repository;
use crate::RepositoryError;

/// Resolves kind ref values against one [`KindFiles`] source.
///
/// - A custom kind `k`: `raw` must be a ULID and `kinds/k/<raw>.md` must
///   exist in the source; resolves to `k:<raw>`.
/// - `memory`: `memory/<raw>.md` must exist in the source; resolves to
///   `memory:<raw>`.
/// - `intent`: resolved through [`Repository::vault_intent_path`], which
///   accepts human keys (`PIMO-12`) and ULID prefixes; resolves to the
///   intent's KG subject.
///
/// Intent resolution reads the redb vault index, not the source's files,
/// because intents are addressed by keys that only the manifest maps to
/// paths. Against a view source an intent ref therefore resolves when the
/// intent exists anywhere in the repository, even if the view does not
/// track it.
pub struct KindRefResolver<'a, 'r> {
    files: &'a KindFiles<'r>,
}

impl<'a, 'r> KindRefResolver<'a, 'r> {
    pub fn new(files: &'a KindFiles<'r>) -> Self {
        Self { files }
    }
}

impl RefResolver for KindRefResolver<'_, '_> {
    fn resolve(&self, target_kind: &str, raw: &str) -> Option<String> {
        match target_kind {
            "intent" => {
                let path = self
                    .files
                    .repository()
                    .vault_intent_path(raw)
                    .ok()
                    .flatten()?;
                Some(entry_subject(&path, VaultEntryType::Intent))
            }
            "memory" => {
                let name_ok = !raw.is_empty()
                    && !raw.contains('\\')
                    && raw
                        .split('/')
                        .all(|c| !c.is_empty() && c != "." && c != "..");
                let path = format!("memory/{raw}.md");
                (name_ok && self.files.exists(&path))
                    .then(|| entry_subject(&path, VaultEntryType::Memory))
            }
            kind if is_valid_kind_name(kind) && is_valid_ulid(raw) => self
                .files
                .exists(&kind_instance_path(kind, raw))
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
        // The walk just listed it; a concurrent delete reads as absent.
        let Some(bytes) = files.read(path)? else {
            return Ok(result);
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
