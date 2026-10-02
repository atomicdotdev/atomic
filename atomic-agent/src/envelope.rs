//! Session envelope for embedding turn/session data inside Atomic changes.
//!
//! The canonical `SessionEnvelope` codec lives in
//! [`atomic_core::change::envelope`] so that receiving-side verification
//! (repository publication gates, remotes) can decode envelopes without
//! depending on the agent runtime. This module re-exports it for agent-side
//! callers and bridges the codec error into [`AgentError`].

pub use atomic_core::change::envelope::{
    EnvelopeResult, SessionEnvelope, SessionEnvelopeBuilder, SessionEnvelopeError,
};

use crate::error::AgentError;

impl From<SessionEnvelopeError> for AgentError {
    fn from(error: SessionEnvelopeError) -> Self {
        AgentError::EnvelopeCodecError {
            reason: error.reason,
        }
    }
}
