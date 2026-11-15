//! Experimental sandbox patch operations. No production opcode is finalized.
use crate::types::Hash;
use serde::{Deserialize, Serialize};

/// An exact operation identity. External completion carriers use ordinal zero;
/// an own-name declaration is part of its content patch at its actual ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContextEffectRef {
    pub change: Hash,
    pub ordinal: u32,
}

/// Add a context-name relation or reverse one exact observed effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrototypeContextEffect {
    Bind {
        consumer: Hash,
        symbol: u64,
        provider: Hash,
    },
    Reverse {
        observed: ContextEffectRef,
    },
    /// Names captured in this newly authored patch's own encoding frame.
    /// Unlike Bind, this does not claim to recover an old author's context.
    /// The ordered pairs cover every foreign semantic owner exactly once.
    DeclareOwnNames {
        owners: Vec<(u64, Hash)>,
    },
}
