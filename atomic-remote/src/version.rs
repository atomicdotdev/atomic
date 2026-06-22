//! Version comparison utilities for CLI upgrade warnings.
//!
//! When the server sends an `X-Atomic-Min-Version` header, the CLI
//! checks whether the running version satisfies the minimum requirement
//! and warns the user if it does not.

use reqwest::header::HeaderMap;

/// Check if `current` version is less than `required` version.
///
/// Performs a simple major.minor.patch numeric comparison. Missing parts
/// default to 0 (e.g. `"1.2"` is treated as `"1.2.0"`).
///
/// Returns `true` if `current < required` (upgrade needed).
/// Returns `false` if `current >= required` or if either version string
/// cannot be parsed — this avoids spurious warnings on dev or pre-release
/// version strings.
pub fn needs_upgrade(current: &str, required: &str) -> bool {
    fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
        let mut parts = v.trim().splitn(4, '.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next().unwrap_or("0").parse::<u32>().ok()?;
        let patch = parts.next().unwrap_or("0").parse::<u32>().ok()?;
        Some((major, minor, patch))
    }

    match (parse_version(current), parse_version(required)) {
        (Some(cur), Some(req)) => cur < req,
        _ => false,
    }
}

/// Check the `X-Atomic-Min-Version` response header and warn if the
/// current CLI version is older than the server's minimum requirement.
///
/// Prints a warning to stderr when an upgrade is needed. Silently does
/// nothing if the header is absent, unparseable, or already satisfied.
pub fn check_min_version_header(headers: &HeaderMap) {
    let header_name =
        reqwest::header::HeaderName::from_static("x-atomic-min-version");

    if let Some(val) = headers.get(&header_name) {
        if let Ok(min_ver) = val.to_str() {
            let current = crate::VERSION;
            if needs_upgrade(current, min_ver) {
                eprintln!(
                    "warning: this server requires Atomic CLI >= {} (you have {}). \
                     Please upgrade: https://atomic.dev/install",
                    min_ver, current
                );
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- needs_upgrade ---

    #[test]
    fn older_major_needs_upgrade() {
        assert!(needs_upgrade("0.9.0", "1.0.0"));
    }

    #[test]
    fn same_version_no_upgrade() {
        assert!(!needs_upgrade("1.2.3", "1.2.3"));
    }

    #[test]
    fn newer_version_no_upgrade() {
        assert!(!needs_upgrade("2.0.0", "1.9.9"));
    }

    #[test]
    fn older_minor_needs_upgrade() {
        assert!(needs_upgrade("1.1.0", "1.2.0"));
    }

    #[test]
    fn older_patch_needs_upgrade() {
        assert!(needs_upgrade("1.2.2", "1.2.3"));
    }

    #[test]
    fn newer_patch_no_upgrade() {
        assert!(!needs_upgrade("1.2.4", "1.2.3"));
    }

    #[test]
    fn missing_patch_treated_as_zero() {
        // "1.2" == "1.2.0", so "1.2" vs "1.2.0" → equal → no upgrade
        assert!(!needs_upgrade("1.2", "1.2.0"));
    }

    #[test]
    fn unparseable_current_no_upgrade() {
        // Don't warn on dev builds or dirty version strings
        assert!(!needs_upgrade("dev", "1.0.0"));
    }

    #[test]
    fn unparseable_required_no_upgrade() {
        assert!(!needs_upgrade("1.0.0", "not-a-version"));
    }

    // --- check_min_version_header ---

    #[test]
    fn no_header_no_panic() {
        let headers = HeaderMap::new();
        // Should not panic or print anything
        check_min_version_header(&headers);
    }

    #[test]
    fn header_already_satisfied_no_warn() {
        // When required <= current, no warning (the function just returns)
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-atomic-min-version"),
            reqwest::header::HeaderValue::from_static("0.0.1"),
        );
        // crate::VERSION is the actual crate version, which will be >= 0.0.1
        check_min_version_header(&headers);
    }
}
