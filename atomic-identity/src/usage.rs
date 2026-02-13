//! Identity usage contexts for multi-identity support
//!
//! This module defines the different contexts in which an identity
//! might be used. A user typically has multiple identities for
//! different purposes:
//!
//! - **Personal**: Side projects, open source contributions
//! - **Work**: Professional/employer-related work
//! - **Community**: Open source maintainer, organization member
//! - **Custom**: User-defined usage contexts
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::IdentityUsage;
//!
//! let personal = IdentityUsage::Personal;
//! let work = IdentityUsage::Work;
//! let community = IdentityUsage::Community;
//! let custom = IdentityUsage::Custom("consulting".to_string());
//!
//! assert!(personal.is_personal());
//! assert!(work.is_work());
//! assert!(!personal.is_work());
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

/// The usage context for an identity.
///
/// Identities can be categorized by how they're used, allowing users
/// to maintain separate identities for different contexts while
/// keeping their work organized.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdentityUsage {
    /// Personal projects and contributions.
    ///
    /// Typically used for:
    /// - Side projects
    /// - Personal experiments
    /// - Learning/educational work
    #[default]
    Personal,

    /// Work/employer-related usage.
    ///
    /// Typically used for:
    /// - Professional work
    /// - Employer-owned repositories
    /// - Client projects
    Work,

    /// Community/organization usage.
    ///
    /// Typically used for:
    /// - Open source maintainer work
    /// - Organization contributions
    /// - Volunteer projects
    Community,

    /// Bot or automated system usage.
    ///
    /// Typically used for:
    /// - CI/CD systems
    /// - Automated tooling
    /// - Service accounts
    Bot,

    /// Custom usage context.
    ///
    /// Allows users to define their own categories.
    Custom(String),
}

impl IdentityUsage {
    /// Get a human-readable description.
    pub fn description(&self) -> String {
        match self {
            IdentityUsage::Personal => "Personal".to_string(),
            IdentityUsage::Work => "Work".to_string(),
            IdentityUsage::Community => "Community".to_string(),
            IdentityUsage::Bot => "Bot".to_string(),
            IdentityUsage::Custom(name) => format!("Custom: {}", name),
        }
    }

    /// Get a short code for compact display.
    pub fn short_code(&self) -> &str {
        match self {
            IdentityUsage::Personal => "P",
            IdentityUsage::Work => "W",
            IdentityUsage::Community => "C",
            IdentityUsage::Bot => "B",
            IdentityUsage::Custom(_) => "X",
        }
    }

    /// Check if this is a personal identity.
    #[inline]
    pub fn is_personal(&self) -> bool {
        matches!(self, IdentityUsage::Personal)
    }

    /// Check if this is a work identity.
    #[inline]
    pub fn is_work(&self) -> bool {
        matches!(self, IdentityUsage::Work)
    }

    /// Check if this is a community identity.
    #[inline]
    pub fn is_community(&self) -> bool {
        matches!(self, IdentityUsage::Community)
    }

    /// Check if this is a bot identity.
    #[inline]
    pub fn is_bot(&self) -> bool {
        matches!(self, IdentityUsage::Bot)
    }

    /// Check if this is a custom identity.
    #[inline]
    pub fn is_custom(&self) -> bool {
        matches!(self, IdentityUsage::Custom(_))
    }

    /// Get the custom name if this is a custom usage.
    pub fn custom_name(&self) -> Option<&str> {
        match self {
            IdentityUsage::Custom(name) => Some(name),
            _ => None,
        }
    }

    /// Create a custom usage context.
    pub fn custom(name: impl Into<String>) -> Self {
        IdentityUsage::Custom(name.into())
    }

    /// Parse from a string (case-insensitive).
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "personal" | "p" => IdentityUsage::Personal,
            "work" | "w" => IdentityUsage::Work,
            "community" | "c" => IdentityUsage::Community,
            "bot" | "b" => IdentityUsage::Bot,
            other => IdentityUsage::Custom(other.to_string()),
        }
    }

    /// Get all standard usage types (excluding Custom).
    pub fn standard_types() -> &'static [IdentityUsage] {
        &[
            IdentityUsage::Personal,
            IdentityUsage::Work,
            IdentityUsage::Community,
            IdentityUsage::Bot,
        ]
    }
}

impl fmt::Display for IdentityUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityUsage::Personal => write!(f, "personal"),
            IdentityUsage::Work => write!(f, "work"),
            IdentityUsage::Community => write!(f, "community"),
            IdentityUsage::Bot => write!(f, "bot"),
            IdentityUsage::Custom(name) => write!(f, "{}", name),
        }
    }
}

impl From<&str> for IdentityUsage {
    fn from(s: &str) -> Self {
        IdentityUsage::parse(s)
    }
}

impl From<String> for IdentityUsage {
    fn from(s: String) -> Self {
        IdentityUsage::parse(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_usage_default() {
        let usage: IdentityUsage = Default::default();
        assert!(usage.is_personal());
    }

    #[test]
    fn test_identity_usage_is_methods() {
        assert!(IdentityUsage::Personal.is_personal());
        assert!(!IdentityUsage::Personal.is_work());

        assert!(IdentityUsage::Work.is_work());
        assert!(!IdentityUsage::Work.is_personal());

        assert!(IdentityUsage::Community.is_community());
        assert!(IdentityUsage::Bot.is_bot());

        let custom = IdentityUsage::Custom("test".to_string());
        assert!(custom.is_custom());
        assert!(!custom.is_personal());
    }

    #[test]
    fn test_identity_usage_custom_name() {
        let custom = IdentityUsage::Custom("consulting".to_string());
        assert_eq!(custom.custom_name(), Some("consulting"));

        assert_eq!(IdentityUsage::Personal.custom_name(), None);
    }

    #[test]
    fn test_identity_usage_description() {
        assert_eq!(IdentityUsage::Personal.description(), "Personal");
        assert_eq!(IdentityUsage::Work.description(), "Work");
        assert_eq!(IdentityUsage::Community.description(), "Community");
        assert_eq!(IdentityUsage::Bot.description(), "Bot");
        assert_eq!(
            IdentityUsage::Custom("test".to_string()).description(),
            "Custom: test"
        );
    }

    #[test]
    fn test_identity_usage_short_code() {
        assert_eq!(IdentityUsage::Personal.short_code(), "P");
        assert_eq!(IdentityUsage::Work.short_code(), "W");
        assert_eq!(IdentityUsage::Community.short_code(), "C");
        assert_eq!(IdentityUsage::Bot.short_code(), "B");
        assert_eq!(IdentityUsage::Custom("test".to_string()).short_code(), "X");
    }

    #[test]
    fn test_identity_usage_parse() {
        assert_eq!(IdentityUsage::parse("personal"), IdentityUsage::Personal);
        assert_eq!(IdentityUsage::parse("PERSONAL"), IdentityUsage::Personal);
        assert_eq!(IdentityUsage::parse("P"), IdentityUsage::Personal);

        assert_eq!(IdentityUsage::parse("work"), IdentityUsage::Work);
        assert_eq!(IdentityUsage::parse("W"), IdentityUsage::Work);

        assert_eq!(IdentityUsage::parse("community"), IdentityUsage::Community);
        assert_eq!(IdentityUsage::parse("C"), IdentityUsage::Community);

        assert_eq!(IdentityUsage::parse("bot"), IdentityUsage::Bot);
        assert_eq!(IdentityUsage::parse("B"), IdentityUsage::Bot);

        assert_eq!(
            IdentityUsage::parse("custom-thing"),
            IdentityUsage::Custom("custom-thing".to_string())
        );
    }

    #[test]
    fn test_identity_usage_display() {
        assert_eq!(format!("{}", IdentityUsage::Personal), "personal");
        assert_eq!(format!("{}", IdentityUsage::Work), "work");
        assert_eq!(format!("{}", IdentityUsage::Community), "community");
        assert_eq!(format!("{}", IdentityUsage::Bot), "bot");
        assert_eq!(
            format!("{}", IdentityUsage::Custom("consulting".to_string())),
            "consulting"
        );
    }

    #[test]
    fn test_identity_usage_from_str() {
        let usage: IdentityUsage = "work".into();
        assert!(usage.is_work());

        let custom: IdentityUsage = "my-custom".into();
        assert!(custom.is_custom());
        assert_eq!(custom.custom_name(), Some("my-custom"));
    }

    #[test]
    fn test_identity_usage_custom_constructor() {
        let custom = IdentityUsage::custom("freelance");
        assert!(custom.is_custom());
        assert_eq!(custom.custom_name(), Some("freelance"));
    }

    #[test]
    fn test_identity_usage_standard_types() {
        let types = IdentityUsage::standard_types();
        assert_eq!(types.len(), 4);
        assert!(types.contains(&IdentityUsage::Personal));
        assert!(types.contains(&IdentityUsage::Work));
        assert!(types.contains(&IdentityUsage::Community));
        assert!(types.contains(&IdentityUsage::Bot));
    }

    #[test]
    fn test_identity_usage_json_roundtrip() {
        let usages = vec![
            IdentityUsage::Personal,
            IdentityUsage::Work,
            IdentityUsage::Community,
            IdentityUsage::Bot,
            IdentityUsage::Custom("consulting".to_string()),
        ];

        for usage in usages {
            let json = serde_json::to_string(&usage).unwrap();
            let recovered: IdentityUsage = serde_json::from_str(&json).unwrap();
            assert_eq!(usage, recovered);
        }
    }

    #[test]
    fn test_identity_usage_equality() {
        assert_eq!(IdentityUsage::Personal, IdentityUsage::Personal);
        assert_ne!(IdentityUsage::Personal, IdentityUsage::Work);

        assert_eq!(
            IdentityUsage::Custom("a".to_string()),
            IdentityUsage::Custom("a".to_string())
        );
        assert_ne!(
            IdentityUsage::Custom("a".to_string()),
            IdentityUsage::Custom("b".to_string())
        );
    }

    #[test]
    fn test_identity_usage_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(IdentityUsage::Personal);
        set.insert(IdentityUsage::Work);
        set.insert(IdentityUsage::Personal); // Duplicate

        assert_eq!(set.len(), 2);
    }
}
