//! Canonical workflow vocabulary and portable resource identity.

pub mod iri;
pub mod legacy;
pub mod ontology;
pub mod project_id;

/// Storage generation of the derived canonical workflow graph.
pub const KG_GENERATION: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outgoing,
    Incoming,
    #[default]
    Both,
}
