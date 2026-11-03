//! Canonical resource IRIs. Hash parsers intentionally do not use the CLI's
//! lenient prefix parser. Percent encoding is canonical and round-trip checked.

use super::project_id::{strict_base32, ProjectId};
use crate::types::{Base32, Hash};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("non-canonical Atomic resource IRI: {0}")]
pub struct IriError(pub String);

fn encode(value: &str, path: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || (path && byte == b'/') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("write to string");
        }
    }
    encoded
}

fn decode(value: &str, path: bool) -> Result<String, IriError> {
    let mut decoded = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value
                .get(i + 1..i + 3)
                .ok_or_else(|| IriError(value.into()))?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| IriError(value.into()))?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| IriError(value.into()))?;
    if encode(&decoded, path) != value {
        return Err(IriError(value.into()));
    }
    Ok(decoded)
}

pub fn mint_project(project: &ProjectId) -> String {
    project.to_urn()
}
pub fn parse_project(iri: &str) -> Result<ProjectId, IriError> {
    ProjectId::from_base32(
        iri.strip_prefix("urn:atomic:project:")
            .ok_or_else(|| IriError(iri.into()))?,
    )
    .map_err(|_| IriError(iri.into()))
}

macro_rules! global {
    ($mint:ident, $parse:ident, $kind:literal) => {
        pub fn $mint(hash: &Hash) -> String {
            format!(
                concat!("urn:atomic:", $kind, ":blake3-256:{}"),
                hash.to_base32()
            )
        }
        pub fn $parse(iri: &str) -> Result<Hash, IriError> {
            let hash = iri
                .strip_prefix(concat!("urn:atomic:", $kind, ":blake3-256:"))
                .ok_or_else(|| IriError(iri.into()))?;
            strict_base32(hash)
                .map(crate::types::Merkle)
                .map_err(|_| IriError(iri.into()))
        }
    };
}
global!(mint_change, parse_change, "change");
global!(mint_provenance, parse_provenance, "provenance");
global!(mint_attestation, parse_attestation, "attestation");
global!(
    mint_project_identity,
    parse_project_identity,
    "project-identity"
);

fn qualified(project: &ProjectId, kind: &str, value: &str, path: bool) -> String {
    format!("{}:{kind}:{}", project.to_urn(), encode(value, path))
}

fn parse_qualified<'a>(iri: &'a str, kind: &str) -> Result<(ProjectId, &'a str), IriError> {
    let rest = iri
        .strip_prefix("urn:atomic:project:")
        .ok_or_else(|| IriError(iri.into()))?;
    let (project, rest) = rest.split_once(':').ok_or_else(|| IriError(iri.into()))?;
    let project = ProjectId::from_base32(project).map_err(|_| IriError(iri.into()))?;
    let rest = rest
        .strip_prefix(kind)
        .and_then(|s| s.strip_prefix(':'))
        .ok_or_else(|| IriError(iri.into()))?;
    Ok((project, rest))
}

macro_rules! named {
    ($mint:ident, $parse:ident, $kind:literal, $path:literal) => {
        pub fn $mint(project: &ProjectId, name: &str) -> String {
            qualified(project, $kind, name, $path)
        }
        pub fn $parse(iri: &str) -> Result<(ProjectId, String), IriError> {
            let (project, value) = parse_qualified(iri, $kind)?;
            Ok((project, decode(value, $path)?))
        }
    };
}
named!(mint_goal, parse_goal, "goal", false);
named!(mint_memory, parse_memory, "memory", false);
named!(mint_skill, parse_skill, "skill", false);
named!(mint_tool_result, parse_tool_result, "tool-result", false);
named!(mint_concept, parse_concept, "concept", false);
named!(mint_view, parse_view, "view", false);
named!(mint_file, parse_file, "file", true);
named!(mint_session, parse_session, "session", false);
named!(mint_named_identity, parse_named_identity, "identity", false);

pub fn mint_identity(project: &ProjectId, name_or_did: &str) -> String {
    if name_or_did.starts_with("did:atomic:") {
        name_or_did.to_owned()
    } else {
        mint_named_identity(project, name_or_did)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityIri {
    Did(String),
    Named(ProjectId, String),
}

pub fn parse_identity(iri: &str) -> Result<IdentityIri, IriError> {
    if let Some(fingerprint) = iri.strip_prefix("did:atomic:") {
        strict_base32(fingerprint).map_err(|_| IriError(iri.into()))?;
        Ok(IdentityIri::Did(iri.to_owned()))
    } else {
        let (project, name) = parse_named_identity(iri)?;
        Ok(IdentityIri::Named(project, name))
    }
}

fn canonical_uid(uid: &str) -> Result<(), IriError> {
    let parsed = ulid::Ulid::from_string(uid).map_err(|_| IriError(uid.into()))?;
    if parsed.to_string() != uid {
        return Err(IriError(uid.into()));
    }
    Ok(())
}

pub fn mint_intent(project: &ProjectId, uid: &str) -> Result<String, IriError> {
    canonical_uid(uid)?;
    Ok(qualified(project, "intent", uid, false))
}
pub fn parse_intent(iri: &str) -> Result<(ProjectId, String), IriError> {
    let (project, uid) = parse_qualified(iri, "intent")?;
    canonical_uid(uid)?;
    Ok((project, uid.to_owned()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    AcceptanceCriterion,
    Task,
    Scope,
    Constraint,
}
impl ChildKind {
    pub const fn segment(self) -> &'static str {
        match self {
            Self::AcceptanceCriterion => "ac",
            Self::Task => "task",
            Self::Scope => "scope",
            Self::Constraint => "constraint",
        }
    }
}
pub fn mint_child(
    project: &ProjectId,
    uid: &str,
    kind: ChildKind,
    child: &str,
) -> Result<String, IriError> {
    Ok(format!(
        "{}:{}:{}",
        mint_intent(project, uid)?,
        kind.segment(),
        encode(&child.to_uppercase(), false)
    ))
}
pub fn parse_child(iri: &str) -> Result<(ProjectId, String, ChildKind, String), IriError> {
    let (project, rest) = parse_qualified(iri, "intent")?;
    let mut parts = rest.split(':');
    let uid = parts.next().ok_or_else(|| IriError(iri.into()))?;
    canonical_uid(uid)?;
    let kind = match parts.next() {
        Some("ac") => ChildKind::AcceptanceCriterion,
        Some("task") => ChildKind::Task,
        Some("scope") => ChildKind::Scope,
        Some("constraint") => ChildKind::Constraint,
        _ => return Err(IriError(iri.into())),
    };
    let child = decode(parts.next().ok_or_else(|| IriError(iri.into()))?, false)?;
    if parts.next().is_some() || child != child.to_uppercase() {
        return Err(IriError(iri.into()));
    }
    Ok((project, uid.to_owned(), kind, child))
}

pub fn mint_turn(project: &ProjectId, session: &str, number: u32) -> String {
    format!("{}:turn:{number}", mint_session(project, session))
}
pub fn parse_turn(iri: &str) -> Result<(ProjectId, String, u32), IriError> {
    let (session, number) = iri
        .rsplit_once(":turn:")
        .ok_or_else(|| IriError(iri.into()))?;
    let (project, session) = parse_session(session)?;
    let n: u32 = number.parse().map_err(|_| IriError(iri.into()))?;
    if n.to_string() != number {
        return Err(IriError(iri.into()));
    }
    Ok((project, session, n))
}
pub fn mint_todo(project: &ProjectId, session: &str, id: &str) -> String {
    format!(
        "{}:todo:{}",
        mint_session(project, session),
        encode(id, false)
    )
}
pub fn parse_todo(iri: &str) -> Result<(ProjectId, String, String), IriError> {
    let (session, id) = iri
        .rsplit_once(":todo:")
        .ok_or_else(|| IriError(iri.into()))?;
    let (project, session) = parse_session(session)?;
    Ok((project, session, decode(id, false)?))
}
pub fn mint_finding(project: &ProjectId, code: &str, subject: &str) -> String {
    format!(
        "{}:finding:{}:{}",
        project.to_urn(),
        encode(code, false),
        Hash::of(subject.as_bytes()).to_base32()
    )
}
pub fn parse_finding(iri: &str) -> Result<(ProjectId, String, Hash), IriError> {
    let (project, rest) = parse_qualified(iri, "finding")?;
    let (code, hash) = rest.split_once(':').ok_or_else(|| IriError(iri.into()))?;
    let hash = strict_base32(hash)
        .map(crate::types::Merkle)
        .map_err(|_| IriError(iri.into()))?;
    Ok((project, decode(code, false)?, hash))
}
