//! Timestamp encodings declared by hook adapters, never inferred from magnitude.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::HookType;

/// Supported wire encodings for the top-level hook `timestamp` field.
#[derive(Clone, Copy, Debug)]
pub(super) enum TimestampFormat {
    Rfc3339,
    /// Cline's hook factory emits `Date.now().toString()`.
    UnixMillisString,
    /// Copilot's camelCase hooks emit epoch milliseconds; its VS Code-compatible
    /// hooks emit ISO timestamps. The JSON type distinguishes the wire formats.
    Rfc3339OrUnixMillis,
}

impl TimestampFormat {
    /// Capture receipt time only when the source omits its timestamp. Call this
    /// before adapter-specific normalization so user/tool data cannot supply it.
    pub(super) fn parse(
        self,
        agent: &str,
        hook_type: HookType,
        raw: &Value,
    ) -> AgentResult<DateTime<Utc>> {
        let Some(value) = raw.get("timestamp").filter(|value| !value.is_null()) else {
            return Ok(Utc::now());
        };

        let timestamp = match self {
            Self::Rfc3339 => parse_rfc3339(value),
            Self::UnixMillisString => value
                .as_str()
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(DateTime::from_timestamp_millis),
            Self::Rfc3339OrUnixMillis => match value {
                Value::String(_) => parse_rfc3339(value),
                Value::Number(_) => value.as_i64().and_then(DateTime::from_timestamp_millis),
                _ => None,
            },
        };

        timestamp.ok_or_else(|| AgentError::HookParseFailed {
            agent: agent.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: match self {
                Self::Rfc3339 => "timestamp must be an RFC-3339 string",
                Self::UnixMillisString => {
                    "timestamp must be a decimal epoch-millisecond string within the supported UTC range"
                }
                Self::Rfc3339OrUnixMillis => {
                    "timestamp must be an RFC-3339 string or integer epoch milliseconds within the supported UTC range"
                }
            }.to_string(),
        })
    }
}

fn parse_rfc3339(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}
