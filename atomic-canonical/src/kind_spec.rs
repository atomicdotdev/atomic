//! User-defined vault kinds — the spec language and the instance validator.
//!
//! The built-in node types (Intent, Memory, …) live in the closed [`vocab`]
//! tier: their names, status sets and edges are fixed in this crate, and an
//! unknown member is a gate error. This module is the **open-world extension
//! tier** beside it. A repository may declare its own kinds — an ADR, an RFC, a
//! runbook — by committing a definition at `.vault/kinds/<k>/_kind.md`, whose
//! YAML frontmatter *is* the schema and whose body is guidance prose that is
//! never validated. Instances live next to it as `.vault/kinds/<k>/<ULID>.md`.
//!
//! "Open" describes the set of kinds, not the instances. Relative to the specs
//! registered in a view, the tier is closed again: an instance key the spec
//! does not declare is a violation, an enum value outside `values` is a
//! violation, a ref must resolve to an existing node of a declared target kind,
//! and a spec that does not parse cleanly registers nothing (fail closed,
//! never partial). Built-in kinds are untouched: their names are reserved
//! ([`RESERVED_KIND_NAMES`]), so a custom kind can neither shadow nor extend
//! them. A ref may *point at* the built-in `intent` and `memory` kinds.
//!
//! The constructs are a small, data-driven subset of SHACL-Core
//! ([`KindSpec::summary_rows`] lists the mapping): `required` is
//! `sh:minCount 1`, a scalar field is `sh:maxCount 1`, the datatypes are
//! `sh:datatype`, `enum` is `sh:in`, `ref` is `sh:class`, and the whole shape
//! is `sh:closed true`. There is deliberately no regex (`pattern`) or other
//! content constraint: like the built-in gate, presence and shape are enforced
//! and content is left honest.
//!
//! This module is pure. It receives already-parsed frontmatter as a JSON map
//! and resolves refs through a caller-supplied [`RefResolver`], so it has no
//! YAML, repository or graph dependency.
//!
//! [`vocab`]: crate::vocab

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;

/// The only `specVersion` this module understands.
pub const SPEC_VERSION: u64 = 1;

/// Kind names a definition may not claim: the built-in node types, the vault's
/// own directories and entry types, and their plurals.
pub const RESERVED_KIND_NAMES: &[&str] = &[
    "goal",
    "session",
    "intent",
    "memory",
    "skill",
    "tool",
    "tool_result",
    "scratch",
    "attestation",
    "vault",
    "concept",
    "identity",
    "todo",
    "ac",
    "acceptance_criterion",
    "task",
    "scope",
    "constraint",
    "ref",
    "change",
    "provgraph",
    "activity",
    "agent",
    "triage",
    "delegation",
    "file",
    "module",
    "entity",
    "view",
    "embedding",
    "test",
    "prompts",
    "goals",
    "intents",
    "skills",
    "attestations",
    "sessions",
    "kinds",
    "kind",
];

/// Field names a definition may not declare. `uid` and `title` have fixed
/// meaning on every instance; the rest are owned by the vault index.
pub const RESERVED_FIELD_NAMES: &[&str] = &[
    "uid",
    "title",
    "entry_type",
    "content_hash",
    "created_at",
    "updated_at",
];

/// Built-in kinds a `ref` field may target in addition to custom kinds.
pub const BUILTIN_REF_TARGETS: &[&str] = &["intent", "memory"];

/// Keys the vault index owns. They are stripped from frontmatter before it
/// reaches this module; if one slips through it is ignored, not reported.
const SYSTEM_KEYS: &[&str] = &["entry_type", "content_hash", "created_at", "updated_at"];

const TOP_LEVEL_KEYS: &[&str] = &[
    "name",
    "specVersion",
    "title",
    "fields",
    "body",
    "embed",
    "context",
];

const FIELD_KEYS: &[&str] = &["type", "required", "values", "target", "edge", "default"];

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

// ---------------------------------------------------------------------------
// Name rules
// ---------------------------------------------------------------------------

/// `^[a-z][a-z0-9_-]{0,62}$` — syntax only; see [`is_reserved_kind_name`].
pub fn is_valid_kind_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

pub fn is_reserved_kind_name(name: &str) -> bool {
    RESERVED_KIND_NAMES.contains(&name)
}

/// `^[a-z][a-zA-Z0-9_]*$` — syntax only; see [`RESERVED_FIELD_NAMES`].
pub fn is_valid_field_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

/// `^[A-Z][A-Z0-9_]*$`.
pub fn is_valid_edge_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_uppercase()
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// A 26-character uppercase Crockford base32 ULID whose value fits 128 bits
/// (so the first character is at most `7`).
pub fn is_valid_ulid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 26 && b[0] <= b'7' && b.iter().all(|c| CROCKFORD.contains(c))
}

fn is_ref_target_name(name: &str) -> bool {
    BUILTIN_REF_TARGETS.contains(&name)
        || (is_valid_kind_name(name) && !is_reserved_kind_name(name))
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// The projected KG edge kind for a custom edge: `"<kind>.<EDGE>"`.
pub fn namespaced_edge(kind: &str, edge: &str) -> String {
    format!("{kind}.{edge}")
}

/// The KG node id of an instance: `"<kind>:<uid>"`.
pub fn instance_kg_id(kind: &str, uid: &str) -> String {
    format!("{kind}:{uid}")
}

/// The JSON-LD `@id` of an instance: `urn:atomic:<kind>:<uid>`, the same
/// shape the intent lift uses.
pub fn instance_urn(kind: &str, uid: &str) -> String {
    crate::lift::as_urn(kind, uid)
}

// ---------------------------------------------------------------------------
// Spec types
// ---------------------------------------------------------------------------

/// The scalar type underlying a [`FieldType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseType {
    String,
    /// A JSON integer; a float such as `1.0` is not an integer.
    Integer,
    Boolean,
    /// A string drawn from the field's `values`.
    Enum,
    /// A reference to an existing node of one of the field's `target` kinds.
    Ref,
}

impl BaseType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BaseType::String => "string",
            BaseType::Integer => "integer",
            BaseType::Boolean => "boolean",
            BaseType::Enum => "enum",
            BaseType::Ref => "ref",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "string" => BaseType::String,
            "integer" => BaseType::Integer,
            "boolean" => BaseType::Boolean,
            "enum" => BaseType::Enum,
            "ref" => BaseType::Ref,
            _ => return None,
        })
    }
}

/// A field's declared type: a [`BaseType`], optionally as a list (`enum[]`).
///
/// Serializes as its spelling in the definition, e.g. `"ref[]"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldType {
    pub base: BaseType,
    /// `true` for the `[]` form: the value is a list of `base`.
    pub list: bool,
}

impl FieldType {
    /// Parse `string integer boolean enum ref`, each optionally suffixed `[]`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.strip_suffix("[]") {
            Some(base) => BaseType::parse(base).map(|base| FieldType { base, list: true }),
            None => BaseType::parse(s).map(|base| FieldType { base, list: false }),
        }
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.base.as_str())?;
        if self.list {
            f.write_str("[]")?;
        }
        Ok(())
    }
}

impl Serialize for FieldType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        FieldType::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown field type {s:?}")))
    }
}

/// One declared field of a kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    /// The allowed values of an `enum`/`enum[]` field; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// The kinds a `ref`/`ref[]` field may point at, tried in order for a bare
    /// value; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target: Vec<String>,
    /// The un-namespaced edge a resolved ref projects (refs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    /// Written into new instances by [`apply_defaults`]; never applied when
    /// validating, so a missing required field is still reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// The `body:` section of a definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodySpec {
    /// An instance must carry a non-blank body.
    #[serde(default)]
    pub required: bool,
}

/// A validated kind definition (the frontmatter of `kinds/<k>/_kind.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KindSpec {
    pub name: String,
    pub spec_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldSpec>,
    #[serde(default)]
    pub body: BodySpec,
    /// Instances are embedded for semantic search.
    #[serde(default)]
    pub embed: bool,
    /// Instances are offered as agent context.
    #[serde(default)]
    pub context: bool,
}

/// Every problem found in a definition. A kind with any problem is not
/// registered.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("invalid kind spec {kind:?}: {}", .problems.join("; "))]
pub struct KindSpecError {
    /// The directory name the definition was loaded from.
    pub kind: String,
    /// Path-qualified problems, e.g. `fields.title: unknown key "pattern"`.
    pub problems: Vec<String>,
}

/// A row of [`KindSpec::summary_rows`]: one construct and its SHACL-Core
/// equivalent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaclRow {
    /// A field name, `body`, or `(kind)` for shape-level constructs.
    pub subject: String,
    /// The construct as written in the definition.
    pub construct: String,
    pub shacl: String,
}

#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn at(&mut self, path: &str, msg: impl fmt::Display) {
        if path.is_empty() {
            self.0.push(msg.to_string());
        } else {
            self.0.push(format!("{path}: {msg}"));
        }
    }
}

impl KindSpec {
    /// Validate a definition's frontmatter and build the spec. `dir_name` is
    /// the `<k>` of `kinds/<k>/_kind.md`; the spec's `name` must equal it.
    ///
    /// Every problem is collected rather than stopping at the first, so an
    /// author sees the whole list at once.
    pub fn from_frontmatter(
        dir_name: &str,
        fm: &Map<String, Value>,
    ) -> Result<KindSpec, KindSpecError> {
        let mut p = Problems::default();
        check_spec(dir_name, fm, &mut p);
        let fail = |problems| KindSpecError {
            kind: dir_name.to_string(),
            problems,
        };
        if !p.0.is_empty() {
            return Err(fail(p.0));
        }
        // The manual pass above is a superset of what serde enforces; this is
        // the typed conversion plus a second, independent closed-key check.
        serde_json::from_value(Value::Object(fm.clone())).map_err(|e| fail(vec![e.to_string()]))
    }

    pub fn field(&self, name: &str) -> Option<&FieldSpec> {
        self.fields.get(name)
    }

    /// Every namespaced edge (`<kind>.<EDGE>`) this kind can project, sorted
    /// and de-duplicated.
    pub fn edges(&self) -> Vec<String> {
        self.fields
            .values()
            .filter_map(|f| f.edge.as_deref())
            .map(|e| namespaced_edge(&self.name, e))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Each construct of the spec beside its SHACL-Core equivalent, for
    /// display (`atomic kind show`).
    pub fn summary_rows(&self) -> Vec<ShaclRow> {
        let row = |subject: &str, construct: String, shacl: String| ShaclRow {
            subject: subject.to_string(),
            construct,
            shacl,
        };
        let mut rows = vec![row(
            "(kind)",
            "closed: undeclared keys are violations".into(),
            "sh:closed true ; sh:ignoredProperties ( uid title )".into(),
        )];
        for (name, f) in &self.fields {
            let ft = f.field_type;
            rows.push(row(
                name,
                format!("type: {ft}"),
                match ft.base {
                    BaseType::String => "sh:datatype xsd:string".into(),
                    BaseType::Integer => "sh:datatype xsd:integer".into(),
                    BaseType::Boolean => "sh:datatype xsd:boolean".into(),
                    BaseType::Enum => format!(
                        "sh:in ( {} )",
                        f.values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                    BaseType::Ref => match f.target.as_slice() {
                        [one] => format!("sh:class {one}"),
                        many => format!(
                            "sh:or ( {} )",
                            many.iter()
                                .map(|t| format!("[ sh:class {t} ]"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                    },
                },
            ));
            if f.required {
                rows.push(row(name, "required: true".into(), "sh:minCount 1".into()));
            }
            if !ft.list {
                rows.push(row(name, "single value".into(), "sh:maxCount 1".into()));
            }
            if let Some(edge) = &f.edge {
                rows.push(row(
                    name,
                    format!("edge: {edge}"),
                    format!("sh:path {}", namespaced_edge(&self.name, edge)),
                ));
            }
            if let Some(default) = &f.default {
                rows.push(row(
                    name,
                    format!("default: {default}"),
                    format!("sh:defaultValue {default}"),
                ));
            }
        }
        if self.body.required {
            rows.push(row(
                "body",
                "required: true".into(),
                "(none; atomic extension: non-blank body)".into(),
            ));
        }
        rows
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "mapping",
    }
}

fn check_spec(dir_name: &str, fm: &Map<String, Value>, p: &mut Problems) {
    for key in fm.keys() {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            p.at("", format_args!("unknown key {key:?}"));
        }
    }

    match fm.get("specVersion") {
        None => p.at("specVersion", "required"),
        Some(v) => match v.as_u64() {
            Some(SPEC_VERSION) => {}
            Some(n) => p.at(
                "specVersion",
                format_args!("unsupported version {n} (only {SPEC_VERSION} is supported)"),
            ),
            None => p.at(
                "specVersion",
                format_args!("expected an integer, got {}", type_name(v)),
            ),
        },
    }

    match fm.get("name") {
        None => p.at("name", "required"),
        Some(Value::String(name)) => {
            if !is_valid_kind_name(name) {
                p.at(
                    "name",
                    format_args!("{name:?} is not a valid kind name (^[a-z][a-z0-9_-]{{0,62}}$)"),
                );
            } else if is_reserved_kind_name(name) {
                p.at("name", format_args!("{name:?} is a reserved kind name"));
            }
            if name != dir_name {
                p.at(
                    "name",
                    format_args!("{name:?} does not match its directory {dir_name:?}"),
                );
            }
        }
        Some(v) => p.at(
            "name",
            format_args!("expected a string, got {}", type_name(v)),
        ),
    }

    if let Some(v) = fm.get("title") {
        if !v.is_string() {
            p.at(
                "title",
                format_args!("expected a string, got {}", type_name(v)),
            );
        }
    }
    for key in ["embed", "context"] {
        if let Some(v) = fm.get(key) {
            if !v.is_boolean() {
                p.at(
                    key,
                    format_args!("expected a boolean, got {}", type_name(v)),
                );
            }
        }
    }

    match fm.get("body") {
        None => {}
        Some(Value::Object(body)) => {
            for key in body.keys() {
                if key != "required" {
                    p.at("body", format_args!("unknown key {key:?}"));
                }
            }
            if let Some(v) = body.get("required") {
                if !v.is_boolean() {
                    p.at(
                        "body.required",
                        format_args!("expected a boolean, got {}", type_name(v)),
                    );
                }
            }
        }
        Some(v) => p.at(
            "body",
            format_args!("expected a mapping, got {}", type_name(v)),
        ),
    }

    match fm.get("fields") {
        None => {}
        Some(Value::Object(fields)) => {
            for (name, spec) in fields {
                check_field(name, spec, p);
            }
        }
        Some(v) => p.at(
            "fields",
            format_args!("expected a mapping, got {}", type_name(v)),
        ),
    }
}

/// Check a list of non-empty, unique strings at `path`.
fn check_string_list(path: &str, v: &Value, p: &mut Problems) {
    let Value::Array(items) = v else {
        p.at(
            path,
            format_args!("expected a list of strings, got {}", type_name(v)),
        );
        return;
    };
    if items.is_empty() {
        p.at(path, "must not be empty");
    }
    let mut seen = BTreeSet::new();
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::String(s) if s.is_empty() => p.at(&format!("{path}[{i}]"), "must not be empty"),
            Value::String(s) => {
                if !seen.insert(s.as_str()) {
                    p.at(path, format_args!("duplicate value {s:?}"));
                }
            }
            other => p.at(
                &format!("{path}[{i}]"),
                format_args!("expected a string, got {}", type_name(other)),
            ),
        }
    }
}

fn check_field(name: &str, spec: &Value, p: &mut Problems) {
    let path = format!("fields.{name}");
    if !is_valid_field_name(name) {
        p.at(
            &path,
            format_args!("{name:?} is not a valid field name (^[a-z][a-zA-Z0-9_]*$)"),
        );
    } else if RESERVED_FIELD_NAMES.contains(&name) {
        p.at(&path, format_args!("{name:?} is a reserved field name"));
    }
    let Value::Object(spec) = spec else {
        p.at(
            &path,
            format_args!("expected a mapping, got {}", type_name(spec)),
        );
        return;
    };
    for key in spec.keys() {
        if !FIELD_KEYS.contains(&key.as_str()) {
            p.at(&path, format_args!("unknown key {key:?}"));
        }
    }

    let ft = match spec.get("type") {
        None => {
            p.at(&format!("{path}.type"), "required");
            None
        }
        Some(Value::String(s)) => {
            let ft = FieldType::parse(s);
            if ft.is_none() {
                p.at(
                    &format!("{path}.type"),
                    format_args!(
                        "unknown type {s:?} (expected string, integer, boolean, enum or ref, \
                         optionally suffixed with [])"
                    ),
                );
            }
            ft
        }
        Some(v) => {
            p.at(
                &format!("{path}.type"),
                format_args!("expected a string, got {}", type_name(v)),
            );
            None
        }
    };
    let base = ft.map(|t| t.base);

    if let Some(v) = spec.get("required") {
        if !v.is_boolean() {
            p.at(
                &format!("{path}.required"),
                format_args!("expected a boolean, got {}", type_name(v)),
            );
        }
    }

    // values: enums only, required there.
    let mut values_ok = false;
    match (base, spec.get("values")) {
        (Some(BaseType::Enum), None) => p.at(&format!("{path}.values"), "required for enum fields"),
        (Some(BaseType::Enum), Some(v)) => {
            let before = p.0.len();
            check_string_list(&format!("{path}.values"), v, p);
            values_ok = p.0.len() == before;
        }
        (Some(_), Some(_)) => p.at(&path, "\"values\" is only allowed on enum fields"),
        _ => {}
    }

    // target and edge: refs only; target required there.
    match (base, spec.get("target")) {
        (Some(BaseType::Ref), None) => p.at(&format!("{path}.target"), "required for ref fields"),
        (Some(BaseType::Ref), Some(v)) => {
            check_string_list(&format!("{path}.target"), v, p);
            if let Value::Array(items) = v {
                for (i, t) in items.iter().enumerate() {
                    let Some(t) = t.as_str().filter(|t| !t.is_empty()) else {
                        continue;
                    };
                    let at = format!("{path}.target[{i}]");
                    if !is_valid_kind_name(t) {
                        p.at(&at, format_args!("{t:?} is not a valid kind name"));
                    } else if !is_ref_target_name(t) {
                        p.at(
                            &at,
                            format_args!(
                                "{t:?} is a reserved kind name (of the built-ins only \
                                 intent and memory may be targeted)"
                            ),
                        );
                    }
                }
            }
        }
        (Some(_), Some(_)) => p.at(&path, "\"target\" is only allowed on ref fields"),
        _ => {}
    }
    match (base, spec.get("edge")) {
        (_, None) => {}
        (Some(BaseType::Ref), Some(Value::String(e))) => {
            if !is_valid_edge_name(e) {
                p.at(
                    &format!("{path}.edge"),
                    format_args!("{e:?} is not a valid edge name (^[A-Z][A-Z0-9_]*$)"),
                );
            }
        }
        (Some(BaseType::Ref), Some(v)) => p.at(
            &format!("{path}.edge"),
            format_args!("expected a string, got {}", type_name(v)),
        ),
        (Some(_), Some(_)) => p.at(&path, "\"edge\" is only allowed on ref fields"),
        (None, Some(_)) => {}
    }

    // default: must itself validate; never on refs.
    if let Some(d) = spec.get("default") {
        let at = format!("{path}.default");
        match ft {
            _ if d.is_null() => p.at(&at, "must not be null"),
            Some(FieldType {
                base: BaseType::Ref,
                ..
            }) => p.at(&at, "defaults are not allowed on ref fields"),
            Some(ft) if ft.base != BaseType::Enum || values_ok => {
                let values = enum_values(spec);
                for (_, msg) in check_value(ft, &values, d) {
                    p.at(&at, msg);
                }
            }
            _ => {}
        }
    }
}

fn enum_values(spec: &Map<String, Value>) -> Vec<String> {
    spec.get("values")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Instance validation
// ---------------------------------------------------------------------------

/// Stable machine-readable violation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KindViolationCode {
    /// A frontmatter key the spec does not declare.
    UndeclaredField,
    /// A required field is absent, null, or an empty list.
    MissingRequired,
    /// A value of the wrong JSON type.
    WrongType,
    /// An enum value outside the declared `values`.
    NotInEnum,
    /// A list where one value is expected, or a single value where a list is.
    Cardinality,
    /// A ref that does not resolve to an existing node.
    UnresolvedRef,
    /// A qualified ref (`<kind>:<id>`) naming a kind outside `target`.
    RefKindMismatch,
    /// Frontmatter `uid` differs from the filename.
    UidMismatch,
    /// Frontmatter has no `uid`.
    MissingUid,
    /// `body.required` and the body is blank.
    BodyRequired,
    /// An unresolved merge-conflict marker.
    ConflictMarker,
    /// The filename is not a valid ULID.
    InvalidUlid,
}

impl KindViolationCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            KindViolationCode::UndeclaredField => "UNDECLARED_FIELD",
            KindViolationCode::MissingRequired => "MISSING_REQUIRED",
            KindViolationCode::WrongType => "WRONG_TYPE",
            KindViolationCode::NotInEnum => "NOT_IN_ENUM",
            KindViolationCode::Cardinality => "CARDINALITY",
            KindViolationCode::UnresolvedRef => "UNRESOLVED_REF",
            KindViolationCode::RefKindMismatch => "REF_KIND_MISMATCH",
            KindViolationCode::UidMismatch => "UID_MISMATCH",
            KindViolationCode::MissingUid => "MISSING_UID",
            KindViolationCode::BodyRequired => "BODY_REQUIRED",
            KindViolationCode::ConflictMarker => "CONFLICT_MARKER",
            KindViolationCode::InvalidUlid => "INVALID_ULID",
        }
    }
}

impl fmt::Display for KindViolationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One way an instance fails its kind's spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindViolation {
    pub code: KindViolationCode,
    /// The frontmatter key at fault; `None` for body- and file-level problems.
    pub field: Option<String>,
    pub message: String,
}

impl fmt::Display for KindViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "[{}] {field}: {}", self.code, self.message),
            None => write!(f, "[{}] {}", self.code, self.message),
        }
    }
}

/// A ref value that resolved to an existing node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRef {
    pub field: String,
    /// The namespaced edge (`<kind>.<EDGE>`) to project, or `None` when the
    /// field declares no `edge` (the ref was existence-checked only).
    pub edge: Option<String>,
    /// The KG node id the resolver returned.
    pub target_id: String,
}

/// The outcome of [`validate_instance`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceReport {
    pub conforms: bool,
    pub violations: Vec<KindViolation>,
    /// Refs that resolved, in field order, de-duplicated per field. Populated
    /// even when the instance does not conform.
    pub resolved_refs: Vec<ResolvedRef>,
}

impl fmt::Display for InstanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.conforms {
            return write!(f, "conforms: yes");
        }
        writeln!(f, "conforms: no ({} violation(s))", self.violations.len())?;
        for v in &self.violations {
            writeln!(f, "  ✗ {v}")?;
        }
        Ok(())
    }
}

/// Looks up the node a ref value points at.
pub trait RefResolver {
    /// Return the KG node id of the `target_kind` node that `raw` names (a
    /// ULID or a human key such as `PIMO-12`), or `None` if there is none.
    /// `raw` is never qualified: [`validate_instance`] strips a `<kind>:`
    /// prefix before calling this.
    fn resolve(&self, target_kind: &str, raw: &str) -> Option<String>;
}

type Problem = (KindViolationCode, String);

/// Check the items of `v` against `ft`. Returns `(item index, problem)` pairs;
/// the index is `None` for a scalar or a whole-value problem. Refs are only
/// type-checked here (a string); resolution is separate.
fn check_value(ft: FieldType, values: &[String], v: &Value) -> Vec<(Option<usize>, String)> {
    check_value_coded(ft, values, v)
        .into_iter()
        .map(|(i, (_, msg))| match i {
            Some(i) => (Some(i), format!("[{i}]: {msg}")),
            None => (None, msg),
        })
        .collect()
}

fn check_value_coded(ft: FieldType, values: &[String], v: &Value) -> Vec<(Option<usize>, Problem)> {
    match (ft.list, v) {
        (true, Value::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                check_scalar(ft.base, values, item)
                    .err()
                    .map(|e| (Some(i), e))
            })
            .collect(),
        (true, _) => vec![(
            None,
            (
                KindViolationCode::Cardinality,
                format!(
                    "expected a list of {}, got a single value",
                    ft.base.as_str()
                ),
            ),
        )],
        (false, Value::Array(_)) => vec![(
            None,
            (
                KindViolationCode::Cardinality,
                format!("expected a single {}, got a list", ft.base.as_str()),
            ),
        )],
        (false, _) => check_scalar(ft.base, values, v)
            .err()
            .map(|e| (None, e))
            .into_iter()
            .collect(),
    }
}

fn check_scalar(base: BaseType, values: &[String], v: &Value) -> Result<(), Problem> {
    let ok = match (base, v) {
        (BaseType::String | BaseType::Ref, Value::String(_)) => true,
        (BaseType::Integer, Value::Number(n)) => n.is_i64() || n.is_u64(),
        (BaseType::Boolean, Value::Bool(_)) => true,
        (BaseType::Enum, Value::String(s)) => {
            if values.iter().any(|x| x == s) {
                true
            } else {
                return Err((KindViolationCode::NotInEnum, not_in_enum(s, values)));
            }
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err((
            KindViolationCode::WrongType,
            format!("expected {}, got {}", base.as_str(), type_name(v)),
        ))
    }
}

fn not_in_enum(s: &str, values: &[String]) -> String {
    format!("{s:?} is not one of: {}", values.join(", "))
}

/// Resolve one ref value against a field's targets.
fn resolve_ref(
    field: &FieldSpec,
    raw: &str,
    resolver: &dyn RefResolver,
) -> Result<String, Problem> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err((KindViolationCode::UnresolvedRef, "empty reference".into()));
    }
    let qualified = raw
        .split_once(':')
        .filter(|(k, _)| BUILTIN_REF_TARGETS.contains(k) || is_valid_kind_name(k));
    if let Some((kind, id)) = qualified {
        if !field.target.iter().any(|t| t == kind) {
            return Err((
                KindViolationCode::RefKindMismatch,
                format!(
                    "{raw:?} names kind {kind:?}, but this field targets: {}",
                    field.target.join(", ")
                ),
            ));
        }
        return resolver.resolve(kind, id).ok_or_else(|| {
            (
                KindViolationCode::UnresolvedRef,
                format!("{raw:?} does not resolve to an existing {kind}"),
            )
        });
    }
    field
        .target
        .iter()
        .find_map(|t| resolver.resolve(t, raw))
        .ok_or_else(|| {
            (
                KindViolationCode::UnresolvedRef,
                format!(
                    "{raw:?} does not resolve to an existing {}",
                    field.target.join(" or ")
                ),
            )
        })
}

/// Validate one instance (`kinds/<k>/<uid>.md`) against its kind's spec.
///
/// `fm` is the parsed frontmatter, `body` the markdown after it. A `null`
/// value counts as absent. Defaults are not applied: they are written into
/// new files by [`apply_defaults`], so the file on disk is what is checked.
pub fn validate_instance(
    spec: &KindSpec,
    uid_from_filename: &str,
    fm: &Map<String, Value>,
    body: &str,
    resolver: &dyn RefResolver,
) -> InstanceReport {
    use KindViolationCode as C;
    let mut violations = Vec::new();
    let mut resolved_refs = Vec::new();
    let mut push = |code, field: Option<&str>, message: String| {
        violations.push(KindViolation {
            code,
            field: field.map(str::to_string),
            message,
        })
    };
    let present = |key: &str| fm.get(key).filter(|v| !v.is_null());

    if !is_valid_ulid(uid_from_filename) {
        push(
            C::InvalidUlid,
            Some("uid"),
            format!(
                "filename {uid_from_filename:?} is not a ULID (26 uppercase Crockford base32 \
                 characters)"
            ),
        );
    }
    match present("uid") {
        None => push(C::MissingUid, Some("uid"), "uid is required".into()),
        Some(Value::String(uid)) if uid != uid_from_filename => push(
            C::UidMismatch,
            Some("uid"),
            format!("uid {uid:?} does not match the filename {uid_from_filename:?}"),
        ),
        Some(Value::String(_)) => {}
        Some(v) => push(
            C::WrongType,
            Some("uid"),
            format!("expected string, got {}", type_name(v)),
        ),
    }
    if let Some(v) = present("title") {
        if !v.is_string() {
            push(
                C::WrongType,
                Some("title"),
                format!("expected string, got {}", type_name(v)),
            );
        }
    }

    for key in fm.keys() {
        let k = key.as_str();
        if k == "uid" || k == "title" || SYSTEM_KEYS.contains(&k) || spec.fields.contains_key(k) {
            continue;
        }
        push(
            C::UndeclaredField,
            Some(k),
            format!("{k:?} is not declared by kind {:?}", spec.name),
        );
    }

    for (name, field) in &spec.fields {
        let Some(value) = present(name) else {
            if field.required {
                push(
                    C::MissingRequired,
                    Some(name),
                    "required field is missing".into(),
                );
            }
            continue;
        };
        if field.required && value.as_array().is_some_and(Vec::is_empty) {
            push(
                C::MissingRequired,
                Some(name),
                "required field is an empty list".into(),
            );
        }
        let problems = check_value_coded(field.field_type, &field.values, value);
        let bad_items: BTreeSet<Option<usize>> = problems.iter().map(|(i, _)| *i).collect();
        for (i, (code, msg)) in problems {
            let msg = match i {
                Some(i) => format!("[{i}]: {msg}"),
                None => msg,
            };
            push(code, Some(name), msg);
        }
        if field.field_type.base != BaseType::Ref || bad_items.contains(&None) {
            continue;
        }
        let items: Vec<(Option<usize>, &str)> = match value {
            Value::Array(items) => items
                .iter()
                .enumerate()
                .filter(|(i, _)| !bad_items.contains(&Some(*i)))
                .filter_map(|(i, v)| v.as_str().map(|s| (Some(i), s)))
                .collect(),
            Value::String(s) => vec![(None, s.as_str())],
            _ => Vec::new(),
        };
        let mut seen = BTreeSet::new();
        for (i, raw) in items {
            match resolve_ref(field, raw, resolver) {
                Ok(target_id) => {
                    if seen.insert(target_id.clone()) {
                        resolved_refs.push(ResolvedRef {
                            field: name.clone(),
                            edge: field
                                .edge
                                .as_deref()
                                .map(|e| namespaced_edge(&spec.name, e)),
                            target_id,
                        });
                    }
                }
                Err((code, msg)) => {
                    let msg = match i {
                        Some(i) => format!("[{i}]: {msg}"),
                        None => msg,
                    };
                    push(code, Some(name), msg);
                }
            }
        }
    }

    if spec.body.required && body.trim().is_empty() {
        push(C::BodyRequired, None, "body is required".into());
    }
    if let Some(line) = first_conflict_marker_line(body) {
        push(
            C::ConflictMarker,
            None,
            format!("unresolved conflict marker at body line {line}"),
        );
    }

    InstanceReport {
        conforms: violations.is_empty(),
        violations,
        resolved_refs,
    }
}

fn is_conflict_marker_line(line: &str) -> bool {
    ["<<<<<<<", "=======", ">>>>>>>"].iter().any(|m| {
        line.strip_prefix(m)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

fn first_conflict_marker_line(text: &str) -> Option<usize> {
    text.lines()
        .position(is_conflict_marker_line)
        .map(|i| i + 1)
}

/// Does `text` contain a merge-conflict marker line? A marker is seven `<`,
/// `=` or `>` at the start of a line, alone or followed by a space. That
/// covers Git's `<<<<<<< ours` / `=======` / `>>>>>>> theirs` and Atomic's
/// own `>>>>>>> 1 [HASH]` / `======= 1` / `<<<<<<< 1`.
pub fn has_conflict_markers(text: &str) -> bool {
    first_conflict_marker_line(text).is_some()
}

// ---------------------------------------------------------------------------
// Write-time helpers
// ---------------------------------------------------------------------------

/// Insert each field's `default` where `fm` has no value (absent or null).
/// Used when creating an instance, never when validating one.
pub fn apply_defaults(spec: &KindSpec, fm: &mut Map<String, Value>) {
    for (name, field) in &spec.fields {
        let Some(default) = &field.default else {
            continue;
        };
        if fm.get(name).is_none_or(Value::is_null) {
            fm.insert(name.clone(), default.clone());
        }
    }
}

/// A `--set field=value` string that cannot be coerced to the field's type.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct ParseValueError(pub String);

/// Coerce a command-line `--set field=value` string to the field's JSON type.
/// A `[]` type takes a comma-separated list (items trimmed, empty items
/// dropped, so an empty string is an empty list). Enum values are checked
/// against `values`; refs are passed through as strings and resolved later
/// by [`validate_instance`].
pub fn parse_set_value(field: &FieldSpec, raw: &str) -> Result<Value, ParseValueError> {
    let ft = field.field_type;
    if ft.list {
        return raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| parse_scalar(ft.base, &field.values, s))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array);
    }
    let raw = if ft.base == BaseType::String {
        raw
    } else {
        raw.trim()
    };
    parse_scalar(ft.base, &field.values, raw)
}

fn parse_scalar(base: BaseType, values: &[String], s: &str) -> Result<Value, ParseValueError> {
    match base {
        BaseType::String | BaseType::Ref => Ok(Value::String(s.to_string())),
        BaseType::Integer => s
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| ParseValueError(format!("{s:?} is not an integer"))),
        BaseType::Boolean => match s {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(ParseValueError(format!(
                "{s:?} is not a boolean (expected true or false)"
            ))),
        },
        BaseType::Enum => {
            if values.iter().any(|v| v == s) {
                Ok(Value::String(s.to_string()))
            } else {
                Err(ParseValueError(not_in_enum(s, values)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    const UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
    const UID2: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
    const UID3: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TT";

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    fn adr_fm() -> Map<String, Value> {
        obj(json!({
            "name": "adr",
            "specVersion": 1,
            "title": "Architecture Decision Record",
            "fields": {
                "status": {
                    "type": "enum",
                    "values": ["proposed", "accepted", "superseded"],
                    "required": true,
                    "default": "proposed"
                },
                "supersedes": {"type": "ref[]", "target": ["adr"], "edge": "SUPERSEDES"},
                "intent": {"type": "ref", "target": ["intent"], "edge": "IMPLEMENTS"}
            },
            "body": {"required": true},
            "embed": true,
            "context": true
        }))
    }

    fn adr() -> KindSpec {
        KindSpec::from_frontmatter("adr", &adr_fm()).expect("ADR example parses")
    }

    /// A spec with one field, for rule-by-rule tests.
    fn one_field(field: Value) -> Result<KindSpec, KindSpecError> {
        KindSpec::from_frontmatter(
            "thing",
            &obj(json!({"name": "thing", "specVersion": 1, "fields": {"f": field}})),
        )
    }

    fn problems(r: Result<KindSpec, KindSpecError>) -> Vec<String> {
        r.expect_err("spec should be rejected").problems
    }

    fn assert_rejected_with(r: Result<KindSpec, KindSpecError>, needle: &str) {
        let ps = problems(r);
        assert!(
            ps.iter().any(|p| p == needle),
            "expected problem {needle:?}, got {ps:?}"
        );
    }

    #[test]
    fn adr_example_parses() {
        let spec = adr();
        assert_eq!(spec.name, "adr");
        assert_eq!(spec.spec_version, 1);
        assert_eq!(spec.title.as_deref(), Some("Architecture Decision Record"));
        assert!(spec.body.required && spec.embed && spec.context);
        let status = spec.field("status").unwrap();
        assert_eq!(status.field_type, FieldType::parse("enum").unwrap());
        assert!(status.required);
        assert_eq!(status.values, ["proposed", "accepted", "superseded"]);
        assert_eq!(status.default, Some(json!("proposed")));
        let sup = spec.field("supersedes").unwrap();
        assert_eq!(
            sup.field_type,
            FieldType {
                base: BaseType::Ref,
                list: true
            }
        );
        assert!(!sup.required, "required defaults to false");
        assert_eq!(spec.edges(), ["adr.IMPLEMENTS", "adr.SUPERSEDES"]);
    }

    #[test]
    fn spec_defaults() {
        let spec =
            KindSpec::from_frontmatter("note", &obj(json!({"name": "note", "specVersion": 1})))
                .unwrap();
        assert_eq!(spec.body, BodySpec { required: false });
        assert!(!spec.embed && !spec.context);
        assert!(spec.title.is_none() && spec.fields.is_empty());
        assert!(spec.edges().is_empty());
    }

    #[test]
    fn spec_round_trips_through_serde() {
        let spec = adr();
        let Value::Object(m) = serde_json::to_value(&spec).unwrap() else {
            panic!()
        };
        assert_eq!(m["fields"]["supersedes"]["type"], json!("ref[]"));
        assert_eq!(KindSpec::from_frontmatter("adr", &m).unwrap(), spec);
    }

    #[test]
    fn pattern_on_a_field_is_rejected_with_a_path_qualified_message() {
        let mut fm = adr_fm();
        fm["fields"]["summary"] = json!({"type": "string", "pattern": "^[A-Z]"});
        let ps = problems(KindSpec::from_frontmatter("adr", &fm));
        assert_eq!(ps, [r#"fields.summary: unknown key "pattern""#]);

        // `title` is also a reserved field name; both problems are reported.
        let mut fm = adr_fm();
        fm["fields"]["title"] = json!({"type": "string", "pattern": "^[A-Z]"});
        let ps = problems(KindSpec::from_frontmatter("adr", &fm));
        assert!(ps.contains(&r#"fields.title: unknown key "pattern""#.to_string()));
        assert!(ps.contains(&r#"fields.title: "title" is a reserved field name"#.to_string()));
    }

    #[test]
    fn every_problem_is_collected() {
        let fm = obj(json!({
            "name": "Bad",
            "specVersion": 2,
            "extra": 1,
            "embed": "yes",
            "fields": {
                "a": {"type": "float"},
                "b": {"type": "enum"},
                "c": {"type": "ref", "edge": "lower"}
            }
        }));
        let err = KindSpec::from_frontmatter("bad", &fm).unwrap_err();
        assert_eq!(err.kind, "bad");
        for needle in [
            r#"unknown key "extra""#,
            "specVersion: unsupported version 2 (only 1 is supported)",
            r#"name: "Bad" is not a valid kind name (^[a-z][a-z0-9_-]{0,62}$)"#,
            r#"name: "Bad" does not match its directory "bad""#,
            "embed: expected a boolean, got string",
            "fields.b.values: required for enum fields",
            "fields.c.target: required for ref fields",
            r#"fields.c.edge: "lower" is not a valid edge name (^[A-Z][A-Z0-9_]*$)"#,
        ] {
            assert!(
                err.problems.iter().any(|p| p == needle),
                "{needle:?} in {err}"
            );
        }
        assert!(err
            .problems
            .iter()
            .any(|p| p.starts_with(r#"fields.a.type: unknown type "float""#)));
        assert_eq!(err.problems.len(), 9, "{err}");
        assert!(err.to_string().starts_with("invalid kind spec \"bad\": "));
    }

    #[test]
    fn spec_version_must_be_one() {
        let mut fm = adr_fm();
        fm["specVersion"] = json!(2);
        assert_rejected_with(
            KindSpec::from_frontmatter("adr", &fm),
            "specVersion: unsupported version 2 (only 1 is supported)",
        );
        fm["specVersion"] = json!("1");
        assert_rejected_with(
            KindSpec::from_frontmatter("adr", &fm),
            "specVersion: expected an integer, got string",
        );
        fm.remove("specVersion");
        assert_rejected_with(
            KindSpec::from_frontmatter("adr", &fm),
            "specVersion: required",
        );
    }

    #[test]
    fn kind_name_rules() {
        let spec = |name: &str| {
            KindSpec::from_frontmatter(name, &obj(json!({"name": name, "specVersion": 1})))
        };
        let max = format!("a{}", "b".repeat(62));
        for ok in ["a", "adr", "run-book", "rfc_2", max.as_str()] {
            assert!(spec(ok).is_ok(), "{ok:?} should be a valid kind name");
        }
        let too_long = format!("a{}", "b".repeat(63));
        for bad in ["", "Adr", "1adr", "_adr", "a.b", "a b", too_long.as_str()] {
            assert!(spec(bad).is_err(), "{bad:?} should be rejected");
        }
        for reserved in RESERVED_KIND_NAMES {
            assert_rejected_with(
                spec(reserved),
                &format!("name: {reserved:?} is a reserved kind name"),
            );
        }
        let mismatch =
            KindSpec::from_frontmatter("adrs", &obj(json!({"name": "adr", "specVersion": 1})));
        assert_rejected_with(
            mismatch,
            r#"name: "adr" does not match its directory "adrs""#,
        );
        assert_rejected_with(
            KindSpec::from_frontmatter("adr", &obj(json!({"specVersion": 1}))),
            "name: required",
        );
    }

    #[test]
    fn field_name_rules() {
        let spec = |name: &str| {
            KindSpec::from_frontmatter(
                "thing",
                &obj(json!({"name": "thing", "specVersion": 1,
                            "fields": {name: {"type": "string"}}})),
            )
        };
        for ok in ["a", "dueDate", "due_date", "x1"] {
            assert!(spec(ok).is_ok(), "{ok:?}");
        }
        for bad in ["Status", "1x", "a-b", "_a", "a.b"] {
            assert_rejected_with(
                spec(bad),
                &format!("fields.{bad}: {bad:?} is not a valid field name (^[a-z][a-zA-Z0-9_]*$)"),
            );
        }
        for reserved in RESERVED_FIELD_NAMES {
            assert_rejected_with(
                spec(reserved),
                &format!("fields.{reserved}: {reserved:?} is a reserved field name"),
            );
        }
    }

    #[test]
    fn all_ten_types_parse_and_others_do_not() {
        for base in ["string", "integer", "boolean", "enum", "ref"] {
            for ty in [base.to_string(), format!("{base}[]")] {
                let ft = FieldType::parse(&ty).unwrap();
                assert_eq!(ft.to_string(), ty);
                let mut f = json!({"type": ty});
                if base == "enum" {
                    f["values"] = json!(["a"]);
                }
                if base == "ref" {
                    f["target"] = json!(["intent"]);
                }
                assert!(one_field(f).is_ok(), "{ty}");
            }
        }
        for bad in ["float", "String", "string[][]", "[]", "list", "ref[ ]"] {
            assert!(FieldType::parse(bad).is_none(), "{bad}");
            assert!(problems(one_field(json!({"type": bad})))[0]
                .starts_with(&format!("fields.f.type: unknown type {bad:?}")));
        }
        assert_rejected_with(
            one_field(json!({"required": true})),
            "fields.f.type: required",
        );
        assert_rejected_with(
            one_field(json!({"type": 3})),
            "fields.f.type: expected a string, got integer",
        );
        assert_rejected_with(
            one_field(json!("string")),
            "fields.f: expected a mapping, got string",
        );
        assert_rejected_with(
            one_field(json!({"type": "string", "required": "yes"})),
            "fields.f.required: expected a boolean, got string",
        );
    }

    #[test]
    fn enum_values_rules() {
        assert_rejected_with(
            one_field(json!({"type": "enum", "values": []})),
            "fields.f.values: must not be empty",
        );
        assert_rejected_with(
            one_field(json!({"type": "enum[]", "values": ["a", "b", "a"]})),
            r#"fields.f.values: duplicate value "a""#,
        );
        assert_rejected_with(
            one_field(json!({"type": "enum", "values": ["a", 1]})),
            "fields.f.values[1]: expected a string, got integer",
        );
        assert_rejected_with(
            one_field(json!({"type": "enum", "values": "a"})),
            "fields.f.values: expected a list of strings, got string",
        );
        assert_rejected_with(
            one_field(json!({"type": "string", "values": ["a"]})),
            r#"fields.f: "values" is only allowed on enum fields"#,
        );
    }

    #[test]
    fn ref_target_and_edge_rules() {
        for ok in [
            json!(["intent"]),
            json!(["memory"]),
            json!(["thing"]),
            json!(["adr", "rfc"]),
        ] {
            assert!(
                one_field(json!({"type": "ref", "target": ok})).is_ok(),
                "{ok}"
            );
        }
        assert_rejected_with(
            one_field(json!({"type": "ref", "target": []})),
            "fields.f.target: must not be empty",
        );
        assert_rejected_with(
            one_field(json!({"type": "ref", "target": ["goal"]})),
            r#"fields.f.target[0]: "goal" is a reserved kind name (of the built-ins only intent and memory may be targeted)"#,
        );
        assert_rejected_with(
            one_field(json!({"type": "ref", "target": ["adr", "Bad Name"]})),
            r#"fields.f.target[1]: "Bad Name" is not a valid kind name"#,
        );
        assert_rejected_with(
            one_field(json!({"type": "string", "target": ["adr"]})),
            r#"fields.f: "target" is only allowed on ref fields"#,
        );
        assert_rejected_with(
            one_field(json!({"type": "enum", "values": ["a"], "edge": "X"})),
            r#"fields.f: "edge" is only allowed on ref fields"#,
        );
        for bad in ["supersedes", "1X", "X-Y", "", "Xy"] {
            assert!(
                one_field(json!({"type": "ref", "target": ["adr"], "edge": bad})).is_err(),
                "{bad:?}"
            );
        }
        let spec = one_field(json!({"type": "ref[]", "target": ["adr"], "edge": "HAS_TASK2"}));
        assert_eq!(spec.unwrap().edges(), ["thing.HAS_TASK2"]);
    }

    #[test]
    fn default_must_validate() {
        for (f, d) in [
            (json!({"type": "string"}), json!("x")),
            (json!({"type": "integer"}), json!(-3)),
            (json!({"type": "boolean"}), json!(false)),
            (json!({"type": "enum", "values": ["a", "b"]}), json!("b")),
            (
                json!({"type": "enum[]", "values": ["a", "b"]}),
                json!(["a", "b"]),
            ),
            (json!({"type": "string[]"}), json!([])),
        ] {
            let mut f = f;
            f["default"] = d;
            assert!(one_field(f.clone()).is_ok(), "{f}");
        }
        let with_default = |mut f: Value, d: Value| {
            f["default"] = d;
            one_field(f)
        };
        assert_rejected_with(
            with_default(json!({"type": "integer"}), json!("x")),
            "fields.f.default: expected integer, got string",
        );
        assert_rejected_with(
            with_default(json!({"type": "integer"}), json!(1.5)),
            "fields.f.default: expected integer, got number",
        );
        assert_rejected_with(
            with_default(json!({"type": "enum", "values": ["a", "b"]}), json!("c")),
            r#"fields.f.default: "c" is not one of: a, b"#,
        );
        assert_rejected_with(
            with_default(
                json!({"type": "enum[]", "values": ["a"]}),
                json!(["a", "z"]),
            ),
            r#"fields.f.default: [1]: "z" is not one of: a"#,
        );
        assert_rejected_with(
            with_default(json!({"type": "string[]"}), json!("x")),
            "fields.f.default: expected a list of string, got a single value",
        );
        assert_rejected_with(
            with_default(json!({"type": "string"}), json!(["x"])),
            "fields.f.default: expected a single string, got a list",
        );
        assert_rejected_with(
            with_default(json!({"type": "ref", "target": ["adr"]}), json!(UID)),
            "fields.f.default: defaults are not allowed on ref fields",
        );
        assert_rejected_with(
            with_default(json!({"type": "string"}), Value::Null),
            "fields.f.default: must not be null",
        );
    }

    #[test]
    fn top_level_shape_rules() {
        let base = || obj(json!({"name": "thing", "specVersion": 1}));
        let with = |k: &str, v: Value| {
            let mut fm = base();
            fm.insert(k.into(), v);
            KindSpec::from_frontmatter("thing", &fm)
        };
        assert_rejected_with(with("pattern", json!("x")), r#"unknown key "pattern""#);
        assert_rejected_with(
            with("title", json!(1)),
            "title: expected a string, got integer",
        );
        assert_rejected_with(
            with("context", json!(1)),
            "context: expected a boolean, got integer",
        );
        assert_rejected_with(
            with("fields", json!([])),
            "fields: expected a mapping, got list",
        );
        assert_rejected_with(
            with("body", json!(true)),
            "body: expected a mapping, got boolean",
        );
        assert_rejected_with(
            with("body", json!({"required": true, "minLength": 3})),
            r#"body: unknown key "minLength""#,
        );
        assert_rejected_with(
            with("body", json!({"required": "yes"})),
            "body.required: expected a boolean, got string",
        );
    }

    // -- instances ----------------------------------------------------------

    /// Resolves `(kind, raw)` pairs from a table.
    #[derive(Default)]
    struct FakeResolver(HashMap<(String, String), String>);

    impl FakeResolver {
        fn with(mut self, kind: &str, raw: &str) -> Self {
            self.0
                .insert((kind.into(), raw.into()), instance_kg_id(kind, raw));
            self
        }
    }

    impl RefResolver for FakeResolver {
        fn resolve(&self, target_kind: &str, raw: &str) -> Option<String> {
            self.0.get(&(target_kind.into(), raw.into())).cloned()
        }
    }

    fn resolver() -> FakeResolver {
        FakeResolver::default()
            .with("intent", "PIMO-12")
            .with("adr", UID2)
            .with("adr", UID3)
    }

    fn instance() -> Map<String, Value> {
        obj(json!({
            "uid": UID,
            "title": "Sign changes with Ed25519",
            "status": "proposed",
            "intent": "PIMO-12"
        }))
    }

    const BODY: &str = "We sign change headers with Ed25519 because ...\n";

    fn validate(fm: &Map<String, Value>, body: &str) -> InstanceReport {
        validate_instance(&adr(), UID, fm, body, &resolver())
    }

    fn codes(r: &InstanceReport) -> Vec<KindViolationCode> {
        r.violations.iter().map(|v| v.code).collect()
    }

    fn only(r: &InstanceReport) -> KindViolation {
        assert_eq!(r.violations.len(), 1, "{r}");
        assert!(!r.conforms);
        r.violations[0].clone()
    }

    #[test]
    fn example_instance_conforms() {
        let r = validate(&instance(), BODY);
        assert!(r.conforms, "{r}");
        assert_eq!(r.to_string(), "conforms: yes");
        assert_eq!(
            r.resolved_refs,
            [ResolvedRef {
                field: "intent".into(),
                edge: Some("adr.IMPLEMENTS".into()),
                target_id: "intent:PIMO-12".into(),
            }]
        );
    }

    #[test]
    fn status_maybe_is_not_in_enum() {
        let mut fm = instance();
        fm["status"] = json!("maybe");
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::NotInEnum);
        assert_eq!(v.field.as_deref(), Some("status"));
        assert_eq!(
            v.message,
            r#""maybe" is not one of: proposed, accepted, superseded"#
        );
        assert_eq!(
            v.to_string(),
            r#"[NOT_IN_ENUM] status: "maybe" is not one of: proposed, accepted, superseded"#
        );
    }

    #[test]
    fn undeclared_field() {
        let mut fm = instance();
        fm.insert("pattern".into(), json!("x"));
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::UndeclaredField);
        assert_eq!(v.message, r#""pattern" is not declared by kind "adr""#);
    }

    #[test]
    fn system_keys_are_ignored_and_null_is_absent() {
        let mut fm = instance();
        fm.insert("created_at".into(), json!("2026-01-01"));
        fm.insert("supersedes".into(), Value::Null);
        assert!(validate(&fm, BODY).conforms);
        fm["status"] = Value::Null;
        assert_eq!(
            only(&validate(&fm, BODY)).code,
            KindViolationCode::MissingRequired
        );
    }

    #[test]
    fn missing_required() {
        let mut fm = instance();
        fm.remove("status");
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::MissingRequired);
        assert_eq!(v.field.as_deref(), Some("status"));

        let spec = one_field(json!({"type": "string[]", "required": true})).unwrap();
        let fm = obj(json!({"uid": UID, "f": []}));
        let r = validate_instance(&spec, UID, &fm, "", &resolver());
        assert_eq!(codes(&r), [KindViolationCode::MissingRequired]);
    }

    #[test]
    fn wrong_type() {
        let mut fm = instance();
        fm["status"] = json!(3);
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::WrongType);
        assert_eq!(v.message, "expected enum, got integer");

        let mut fm = instance();
        fm["title"] = json!(["a"]);
        assert_eq!(only(&validate(&fm, BODY)).field.as_deref(), Some("title"));

        let spec = KindSpec::from_frontmatter(
            "thing",
            &obj(json!({"name": "thing", "specVersion": 1, "fields": {
                "n": {"type": "integer"}, "b": {"type": "boolean"}, "s": {"type": "string"}
            }})),
        )
        .unwrap();
        let ok = obj(json!({"uid": UID, "n": -7, "b": true, "s": "x"}));
        assert!(validate_instance(&spec, UID, &ok, "", &resolver()).conforms);
        let bad = obj(json!({"uid": UID, "n": 1.0, "b": "true", "s": 5}));
        let r = validate_instance(&spec, UID, &bad, "", &resolver());
        assert_eq!(codes(&r), [KindViolationCode::WrongType; 3]);
        assert_eq!(r.violations[1].message, "expected integer, got number");
    }

    #[test]
    fn cardinality() {
        let mut fm = instance();
        fm["status"] = json!(["proposed"]);
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::Cardinality);
        assert_eq!(v.message, "expected a single enum, got a list");

        let mut fm = instance();
        fm.insert("supersedes".into(), json!(UID2));
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::Cardinality);
        assert_eq!(v.message, "expected a list of ref, got a single value");
    }

    #[test]
    fn uid_rules() {
        let mut fm = instance();
        fm["uid"] = json!(UID2);
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::UidMismatch);

        let mut fm = instance();
        fm.remove("uid");
        assert_eq!(
            only(&validate(&fm, BODY)).code,
            KindViolationCode::MissingUid
        );

        let mut fm = instance();
        fm["uid"] = json!(42);
        assert_eq!(
            only(&validate(&fm, BODY)).code,
            KindViolationCode::WrongType
        );

        let lower = UID.to_lowercase();
        let mut fm = instance();
        fm["uid"] = json!(lower);
        let r = validate_instance(&adr(), &lower, &fm, BODY, &resolver());
        assert_eq!(codes(&r), [KindViolationCode::InvalidUlid]);
    }

    #[test]
    fn ulid_syntax() {
        assert!(is_valid_ulid(UID));
        assert!(is_valid_ulid("7ZZZZZZZZZZZZZZZZZZZZZZZZZ"));
        for bad in [
            "8ZZZZZZZZZZZZZZZZZZZZZZZZZ", // overflows 128 bits
            "01JD8ZK3M6W2Q4V7X9B5N1C3T",  // 25 chars
            "01JD8ZK3M6W2Q4V7X9B5N1C3TRX",
            "01JD8ZK3M6W2Q4V7X9B5N1C3TI", // I is not Crockford
            "01JD8ZK3M6W2Q4V7X9B5N1C3TL",
            "01JD8ZK3M6W2Q4V7X9B5N1C3TO",
            "01JD8ZK3M6W2Q4V7X9B5N1C3TU",
            "01jd8zk3m6w2q4v7x9b5n1c3tr",
            "",
        ] {
            assert!(!is_valid_ulid(bad), "{bad:?}");
        }
    }

    #[test]
    fn body_required() {
        let v = only(&validate(&instance(), "  \n\t\n"));
        assert_eq!(v.code, KindViolationCode::BodyRequired);
        assert_eq!(v.field, None);
        let spec = one_field(json!({"type": "string"})).unwrap();
        let fm = obj(json!({"uid": UID}));
        assert!(validate_instance(&spec, UID, &fm, "", &resolver()).conforms);
    }

    #[test]
    fn conflict_marker_in_body() {
        let body = "intro\n<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
        let v = only(&validate(&instance(), body));
        assert_eq!(v.code, KindViolationCode::ConflictMarker);
        assert_eq!(v.message, "unresolved conflict marker at body line 2");
    }

    #[test]
    fn conflict_marker_detection() {
        for yes in [
            "<<<<<<< HEAD\n",
            "x\n=======\ny",
            "x\r\n=======\r\ny",
            ">>>>>>> branch",
            ">>>>>>> 1 [ABCDEFGH]\n",
            "a\n======= 1\nb",
            "<<<<<<< 1",
            "<<<<<<<",
        ] {
            assert!(has_conflict_markers(yes), "{yes:?}");
        }
        for no in [
            "",
            "plain text",
            " <<<<<<< indented",
            "a ======= b",
            "Title\n========\n",
            "<<<<<<<< eight",
            ">>>>>>>x",
            "======-",
        ] {
            assert!(!has_conflict_markers(no), "{no:?}");
        }
    }

    #[test]
    fn qualified_and_bare_refs() {
        // Qualified, naming a target kind.
        let mut fm = instance();
        fm["intent"] = json!("intent:PIMO-12");
        let r = validate(&fm, BODY);
        assert!(r.conforms, "{r}");
        assert_eq!(r.resolved_refs[0].target_id, "intent:PIMO-12");

        // Qualified, naming a kind outside `target`.
        fm["intent"] = json!("memory:PIMO-12");
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::RefKindMismatch);
        assert_eq!(
            v.message,
            r#""memory:PIMO-12" names kind "memory", but this field targets: intent"#
        );

        // Qualified, right kind, missing node.
        fm["intent"] = json!("intent:PIMO-99");
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::UnresolvedRef);
        assert_eq!(
            v.message,
            r#""intent:PIMO-99" does not resolve to an existing intent"#
        );

        // Bare, missing node.
        fm["intent"] = json!("PIMO-99");
        let v = only(&validate(&fm, BODY));
        assert_eq!(v.code, KindViolationCode::UnresolvedRef);
        assert_eq!(
            v.message,
            r#""PIMO-99" does not resolve to an existing intent"#
        );

        // Not a string at all.
        fm["intent"] = json!(12);
        assert_eq!(
            only(&validate(&fm, BODY)).code,
            KindViolationCode::WrongType
        );
    }

    #[test]
    fn bare_ref_tries_targets_in_order() {
        let spec =
            one_field(json!({"type": "ref", "target": ["memory", "intent"], "edge": "ABOUT"}))
                .unwrap();
        let both = FakeResolver::default()
            .with("memory", "k")
            .with("intent", "k");
        let fm = obj(json!({"uid": UID, "f": "k"}));
        let r = validate_instance(&spec, UID, &fm, "", &both);
        assert_eq!(r.resolved_refs[0].target_id, "memory:k");
        let intent_only = FakeResolver::default().with("intent", "k");
        let r = validate_instance(&spec, UID, &fm, "", &intent_only);
        assert_eq!(r.resolved_refs[0].target_id, "intent:k");
        assert_eq!(r.resolved_refs[0].edge.as_deref(), Some("thing.ABOUT"));
        let r = validate_instance(&spec, UID, &fm, "", &FakeResolver::default());
        assert_eq!(
            only(&r).message,
            r#""k" does not resolve to an existing memory or intent"#
        );
    }

    #[test]
    fn ref_without_edge_resolves_but_projects_no_edge() {
        let spec = one_field(json!({"type": "ref", "target": ["intent"]})).unwrap();
        let fm = obj(json!({"uid": UID, "f": "PIMO-12"}));
        let r = validate_instance(&spec, UID, &fm, "", &resolver());
        assert!(r.conforms);
        assert_eq!(r.resolved_refs[0].edge, None);
        assert!(spec.edges().is_empty());
    }

    #[test]
    fn ref_list_produces_one_edge_per_target() {
        let mut fm = instance();
        fm.insert(
            "supersedes".into(),
            json!([UID2, format!("adr:{UID3}"), UID2, 7, "nope"]),
        );
        let r = validate(&fm, BODY);
        let sup: Vec<_> = r
            .resolved_refs
            .iter()
            .filter(|r| r.field == "supersedes")
            .collect();
        assert_eq!(sup.len(), 2, "duplicates collapse");
        assert!(sup
            .iter()
            .all(|r| r.edge.as_deref() == Some("adr.SUPERSEDES")));
        assert_eq!(sup[0].target_id, format!("adr:{UID2}"));
        assert_eq!(sup[1].target_id, format!("adr:{UID3}"));
        assert_eq!(
            codes(&r),
            [
                KindViolationCode::WrongType,
                KindViolationCode::UnresolvedRef
            ]
        );
        assert_eq!(r.violations[0].message, "[3]: expected ref, got integer");
        assert!(r.violations[1].message.starts_with("[4]: "));
    }

    #[test]
    fn self_reference_is_allowed() {
        let spec = KindSpec::from_frontmatter(
            "adr",
            &obj(json!({"name": "adr", "specVersion": 1,
                        "fields": {"parent": {"type": "ref", "target": ["adr"], "edge": "CHILD_OF"}}})),
        )
        .unwrap();
        let fm = obj(json!({"uid": UID, "parent": UID2}));
        let r = validate_instance(&spec, UID, &fm, "", &resolver());
        assert!(r.conforms, "{r}");
    }

    #[test]
    fn all_codes_have_stable_spellings() {
        use KindViolationCode as C;
        for c in [
            C::UndeclaredField,
            C::MissingRequired,
            C::WrongType,
            C::NotInEnum,
            C::Cardinality,
            C::UnresolvedRef,
            C::RefKindMismatch,
            C::UidMismatch,
            C::MissingUid,
            C::BodyRequired,
            C::ConflictMarker,
            C::InvalidUlid,
        ] {
            assert_eq!(serde_json::to_value(c).unwrap(), json!(c.as_str()));
        }
    }

    #[test]
    fn report_display_lists_violations() {
        let mut fm = instance();
        fm.remove("uid");
        let r = validate(&fm, "");
        assert_eq!(
            r.to_string(),
            "conforms: no (2 violation(s))\n  ✗ [MISSING_UID] uid: uid is required\n  ✗ \
             [BODY_REQUIRED] body is required\n"
        );
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn identifiers() {
        assert_eq!(namespaced_edge("adr", "SUPERSEDES"), "adr.SUPERSEDES");
        assert_eq!(instance_kg_id("adr", UID), format!("adr:{UID}"));
        assert_eq!(instance_urn("adr", UID), format!("urn:atomic:adr:{UID}"));
    }

    #[test]
    fn apply_defaults_fills_only_absent_fields() {
        let spec = adr();
        let mut fm = obj(json!({"uid": UID}));
        apply_defaults(&spec, &mut fm);
        assert_eq!(fm["status"], json!("proposed"));
        assert!(!fm.contains_key("supersedes") && !fm.contains_key("intent"));

        let mut fm = obj(json!({"uid": UID, "status": "accepted"}));
        apply_defaults(&spec, &mut fm);
        assert_eq!(fm["status"], json!("accepted"));

        let mut fm = obj(json!({"uid": UID, "status": null}));
        apply_defaults(&spec, &mut fm);
        assert_eq!(fm["status"], json!("proposed"));
    }

    #[test]
    fn parse_set_value_coerces_to_the_field_type() {
        let f = |v: Value| serde_json::from_value::<FieldSpec>(v).unwrap();
        let parse = |field: Value, raw: &str| parse_set_value(&f(field), raw);

        assert_eq!(
            parse(json!({"type": "string"}), " a, b "),
            Ok(json!(" a, b "))
        );
        assert_eq!(parse(json!({"type": "integer"}), " 42 "), Ok(json!(42)));
        assert_eq!(parse(json!({"type": "integer"}), "-1"), Ok(json!(-1)));
        assert!(parse(json!({"type": "integer"}), "1.5").is_err());
        assert_eq!(parse(json!({"type": "boolean"}), "true"), Ok(json!(true)));
        assert_eq!(
            parse(json!({"type": "boolean"}), "yes")
                .unwrap_err()
                .to_string(),
            r#""yes" is not a boolean (expected true or false)"#
        );
        let status = json!({"type": "enum", "values": ["proposed", "accepted", "superseded"]});
        assert_eq!(parse(status.clone(), "accepted"), Ok(json!("accepted")));
        assert_eq!(
            parse(status, "maybe").unwrap_err().to_string(),
            r#""maybe" is not one of: proposed, accepted, superseded"#
        );
        assert_eq!(
            parse(
                json!({"type": "ref", "target": ["intent"]}),
                "intent:PIMO-12"
            ),
            Ok(json!("intent:PIMO-12"))
        );
        assert_eq!(
            parse(
                json!({"type": "ref[]", "target": ["adr"]}),
                &format!("{UID2}, {UID3},")
            ),
            Ok(json!([UID2, UID3]))
        );
        assert_eq!(
            parse(json!({"type": "integer[]"}), "1,2, 3"),
            Ok(json!([1, 2, 3]))
        );
        assert_eq!(parse(json!({"type": "string[]"}), ""), Ok(json!([])));
        assert!(parse(json!({"type": "enum[]", "values": ["a"]}), "a,b").is_err());
        assert!(parse(json!({"type": "boolean[]"}), "true,nah").is_err());
    }

    #[test]
    fn summary_rows_map_constructs_to_shacl() {
        let rows = adr().summary_rows();
        let has = |subject: &str, shacl: &str| {
            rows.iter()
                .any(|r| r.subject == subject && r.shacl == shacl)
        };
        assert!(has(
            "(kind)",
            "sh:closed true ; sh:ignoredProperties ( uid title )"
        ));
        assert!(has("status", "sh:minCount 1"));
        assert!(has("status", "sh:maxCount 1"));
        assert!(has(
            "status",
            r#"sh:in ( "proposed" "accepted" "superseded" )"#
        ));
        assert!(has("status", r#"sh:defaultValue "proposed""#));
        assert!(has("intent", "sh:class intent"));
        assert!(has("intent", "sh:path adr.IMPLEMENTS"));
        assert!(has("supersedes", "sh:class adr"));
        assert!(!has("supersedes", "sh:maxCount 1"), "lists are unbounded");
        assert!(has("body", "(none; atomic extension: non-blank body)"));

        let spec = KindSpec::from_frontmatter(
            "thing",
            &obj(json!({"name": "thing", "specVersion": 1, "fields": {
                "s": {"type": "string"}, "n": {"type": "integer"}, "b": {"type": "boolean[]"},
                "r": {"type": "ref", "target": ["adr", "intent"]}
            }})),
        )
        .unwrap();
        let rows = spec.summary_rows();
        for shacl in [
            "sh:datatype xsd:string",
            "sh:datatype xsd:integer",
            "sh:datatype xsd:boolean",
            "sh:or ( [ sh:class adr ] [ sh:class intent ] )",
        ] {
            assert!(rows.iter().any(|r| r.shacl == shacl), "{shacl}");
        }
    }

    #[test]
    fn reserved_lists_are_self_consistent() {
        for k in RESERVED_KIND_NAMES {
            assert!(
                is_valid_kind_name(k),
                "{k} would be rejected on syntax alone"
            );
        }
        for f in RESERVED_FIELD_NAMES {
            assert!(is_valid_field_name(f), "{f}");
        }
        for t in BUILTIN_REF_TARGETS {
            assert!(is_reserved_kind_name(t) && is_ref_target_name(t));
        }
        assert_eq!(RESERVED_KIND_NAMES.len(), 39);
    }
}
