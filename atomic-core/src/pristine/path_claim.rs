//! Durable path-claim event types and fixed-width storage codec.
//!
//! `PATH_CLAIMS` is an additive event index. It deliberately stores no
//! view-specific winner: callers filter events by visible changes and perform
//! causal reduction outside the pristine layer.

use std::io::{Error, ErrorKind};

use crate::types::{ChangePosition, GraphNode, NodeId, Position};

use super::{PristineError, PristineResult};

/// Current schema version for the durable path-claim index.
pub const PATH_CLAIM_SCHEMA_VERSION: u32 = 1;

/// Key used in `PRISTINE_META` to mark a completed path-claim backfill.
pub const PATH_CLAIM_SCHEMA_KEY: &str = "path_claims_schema";

/// Version byte embedded in every encoded path-claim event.
pub const PATH_CLAIM_EVENT_VERSION: u8 = 1;

/// Fixed encoded width of [`PathClaimEvent`].
pub const PATH_CLAIM_EVENT_SIZE: usize = 87;

/// Filesystem kind asserted by a path claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum PathClaimKind {
    File = 0,
    Directory = 1,
}

impl TryFrom<u8> for PathClaimKind {
    type Error = PristineError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::File),
            1 => Ok(Self::Directory),
            other => Err(invalid_path_claim_data(format!(
                "unknown path-claim kind {other}"
            ))),
        }
    }
}

/// State contributed by one path-claim transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum PathClaimState {
    Alive = 0,
    Dead = 1,
}

impl TryFrom<u8> for PathClaimState {
    type Error = PristineError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Alive),
            1 => Ok(Self::Dead),
            other => Err(invalid_path_claim_data(format!(
                "unknown path-claim state {other}"
            ))),
        }
    }
}

/// Stable identity of the structural graph claim affected by an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathClaimId {
    /// Stable inode/root position claimed by the path.
    pub claimant: Position<NodeId>,
    /// Exact parent graph node from which the name edge originates.
    pub parent: GraphNode<NodeId>,
    /// Exact graph node containing the claimed basename.
    pub name: GraphNode<NodeId>,
    /// Change that introduced the structural parent-to-name edge.
    pub introduced_by: NodeId,
}

impl PathClaimId {
    pub const fn new(
        claimant: Position<NodeId>,
        parent: GraphNode<NodeId>,
        name: GraphNode<NodeId>,
        introduced_by: NodeId,
    ) -> Self {
        Self {
            claimant,
            parent,
            name,
            introduced_by,
        }
    }
}

/// One additive transition for a stable path claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathClaimEvent {
    /// Change whose visibility activates this transition.
    pub event_change: NodeId,
    /// Operation order within `event_change`.
    pub operation_index: u32,
    /// File/directory kind carried independently of TREE projection.
    pub kind: PathClaimKind,
    /// Alive/dead state contributed by this transition.
    pub state: PathClaimState,
    /// Exact structural claim affected by this transition.
    pub claim: PathClaimId,
}

impl PathClaimEvent {
    pub const fn new(
        event_change: NodeId,
        operation_index: u32,
        kind: PathClaimKind,
        state: PathClaimState,
        claim: PathClaimId,
    ) -> Self {
        Self {
            event_change,
            operation_index,
            kind,
            state,
            claim,
        }
    }
}

/// A decoded path key and its additive claim event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathClaimEntry {
    pub path: String,
    pub event: PathClaimEvent,
}

impl PathClaimEntry {
    pub fn new(path: impl Into<String>, event: PathClaimEvent) -> Self {
        Self {
            path: path.into(),
            event,
        }
    }
}

/// Encode a path-claim event using a stable, fixed-width big-endian layout.
pub fn encode_path_claim_event(event: &PathClaimEvent) -> [u8; PATH_CLAIM_EVENT_SIZE] {
    let mut bytes = [0u8; PATH_CLAIM_EVENT_SIZE];
    bytes[0] = PATH_CLAIM_EVENT_VERSION;
    put_u64(&mut bytes, 1, event.event_change.get());
    bytes[9..13].copy_from_slice(&event.operation_index.to_be_bytes());
    put_u64(&mut bytes, 13, event.claim.claimant.change.get());
    put_u64(&mut bytes, 21, event.claim.claimant.pos.get());
    bytes[29] = event.kind as u8;
    bytes[30] = event.state as u8;
    put_u64(&mut bytes, 31, event.claim.parent.change.get());
    put_u64(&mut bytes, 39, event.claim.parent.start.get());
    put_u64(&mut bytes, 47, event.claim.parent.end.get());
    put_u64(&mut bytes, 55, event.claim.name.change.get());
    put_u64(&mut bytes, 63, event.claim.name.start.get());
    put_u64(&mut bytes, 71, event.claim.name.end.get());
    put_u64(&mut bytes, 79, event.claim.introduced_by.get());
    bytes
}

/// Decode and validate one fixed-width path-claim event.
pub fn decode_path_claim_event(
    bytes: &[u8; PATH_CLAIM_EVENT_SIZE],
) -> PristineResult<PathClaimEvent> {
    if bytes[0] != PATH_CLAIM_EVENT_VERSION {
        return Err(invalid_path_claim_data(format!(
            "unsupported path-claim event version {} (expected {})",
            bytes[0], PATH_CLAIM_EVENT_VERSION
        )));
    }

    let operation_index =
        u32::from_be_bytes(bytes[9..13].try_into().map_err(|_| {
            invalid_path_claim_data("invalid path-claim operation index".to_string())
        })?);
    let claimant = Position::new(
        NodeId::new(get_u64(bytes, 13)),
        ChangePosition::new(get_u64(bytes, 21)),
    );
    let parent = GraphNode::new(
        NodeId::new(get_u64(bytes, 31)),
        ChangePosition::new(get_u64(bytes, 39)),
        ChangePosition::new(get_u64(bytes, 47)),
    );
    let name = GraphNode::new(
        NodeId::new(get_u64(bytes, 55)),
        ChangePosition::new(get_u64(bytes, 63)),
        ChangePosition::new(get_u64(bytes, 71)),
    );

    Ok(PathClaimEvent {
        event_change: NodeId::new(get_u64(bytes, 1)),
        operation_index,
        kind: PathClaimKind::try_from(bytes[29])?,
        state: PathClaimState::try_from(bytes[30])?,
        claim: PathClaimId {
            claimant,
            parent,
            name,
            introduced_by: NodeId::new(get_u64(bytes, 79)),
        },
    })
}

pub(crate) fn invalid_path_claim_data(message: String) -> PristineError {
    PristineError::Io(Error::new(ErrorKind::InvalidData, message))
}

pub(crate) fn path_claim_schema_error(found: Option<u32>) -> PristineError {
    let message = match found {
        Some(version) if version > PATH_CLAIM_SCHEMA_VERSION => format!(
            "PATH_CLAIMS schema version {version} is newer than supported version {PATH_CLAIM_SCHEMA_VERSION}; use a compatible newer Atomic binary"
        ),
        Some(version) => format!(
            "PATH_CLAIMS migration required (found schema version {version}; expected version {PATH_CLAIM_SCHEMA_VERSION}); reopen the repository writable and run the path-claim backfill before read-only or open-existing access"
        ),
        None => format!(
            "PATH_CLAIMS migration required (missing schema completion marker; expected version {PATH_CLAIM_SCHEMA_VERSION}); reopen the repository writable and run the path-claim backfill before read-only or open-existing access"
        ),
    };
    invalid_path_claim_data(message)
}

pub(crate) fn tree_bijection_error(message: impl Into<String>) -> PristineError {
    invalid_path_claim_data(format!(
        "TREE/REV_TREE invariant violation: {}",
        message.into()
    ))
}

fn put_u64(bytes: &mut [u8; PATH_CLAIM_EVENT_SIZE], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn get_u64(bytes: &[u8; PATH_CLAIM_EVENT_SIZE], offset: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_be_bytes(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> PathClaimEvent {
        PathClaimEvent::new(
            NodeId::new(9),
            17,
            PathClaimKind::Directory,
            PathClaimState::Dead,
            PathClaimId::new(
                Position::new(NodeId::new(3), ChangePosition::new(41)),
                GraphNode::new(
                    NodeId::new(4),
                    ChangePosition::new(5),
                    ChangePosition::new(11),
                ),
                GraphNode::new(
                    NodeId::new(7),
                    ChangePosition::new(13),
                    ChangePosition::new(21),
                ),
                NodeId::new(7),
            ),
        )
    }

    #[test]
    fn path_claim_codec_is_fixed_width_and_roundtrips() {
        let encoded = encode_path_claim_event(&event());
        assert_eq!(encoded.len(), PATH_CLAIM_EVENT_SIZE);
        assert_eq!(decode_path_claim_event(&encoded).unwrap(), event());
    }

    #[test]
    fn path_claim_codec_uses_big_endian_event_order_fields() {
        let encoded = encode_path_claim_event(&event());
        assert_eq!(&encoded[1..9], &9u64.to_be_bytes());
        assert_eq!(&encoded[9..13], &17u32.to_be_bytes());
    }

    #[test]
    fn path_claim_codec_rejects_unknown_discriminants() {
        let mut encoded = encode_path_claim_event(&event());
        encoded[0] = PATH_CLAIM_EVENT_VERSION + 1;
        assert!(decode_path_claim_event(&encoded).is_err());

        let mut encoded = encode_path_claim_event(&event());
        encoded[29] = 0xff;
        assert!(decode_path_claim_event(&encoded).is_err());

        let mut encoded = encode_path_claim_event(&event());
        encoded[30] = 0xff;
        assert!(decode_path_claim_event(&encoded).is_err());
    }
}
