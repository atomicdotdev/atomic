//! Diff algorithm selection.
//!
//! This module provides the [`Algorithm`] enum for selecting which diff
//! algorithm to use when computing differences between sequences.
//!
//! # Available Algorithms
//!
//! ## Myers (Default)
//!
//! The Myers diff algorithm, published by Eugene Myers in 1986, is the default
//! choice for most diff operations. It finds the **shortest edit script** (SES) -
//! the minimum number of insertions and deletions needed to transform one sequence
//! into another.
//!
//! **Characteristics:**
//! - Optimal: Always finds the minimum edit distance
//! - Time complexity: O(ND) where N = |a| + |b| and D = edit distance
//! - Space complexity: O(N) with linear space refinement
//! - Best for: Small to medium changes in source code
//!
//! **Reference:**
//! Myers, E.W. "An O(ND) Difference Algorithm and Its Variations"
//! Algorithmica 1(2): 251-266, 1986
//!
//! ## Patience
//!
//! The Patience diff algorithm, created by Bram Cohen (of BitTorrent fame),
//! produces more human-readable diffs by using unique lines as anchor points.
//!
//! **How it works:**
//! 1. Find all lines that appear exactly once in both sequences
//! 2. Find the Longest Increasing Subsequence (LIS) of these unique matches
//! 3. Use these as anchors and recursively diff the regions between them
//! 4. Fall back to Myers for regions with no unique matches
//!
//! **Characteristics:**
//! - Not always minimal edit distance
//! - Often produces more "intuitive" diffs
//! - Better at handling moved code blocks
//! - Best for: Large structural changes, code with repeated patterns
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::Algorithm;
//!
//! // Default is Myers
//! let algo = Algorithm::default();
//! assert_eq!(algo, Algorithm::Myers);
//!
//! // Can be selected explicitly
//! let myers = Algorithm::Myers;
//! let patience = Algorithm::Patience;
//! ```
//!
//! # Choosing an Algorithm
//!
//! | Scenario | Recommended |
//! |----------|-------------|
//! | Small, localized changes | Myers |
//! | Large structural refactors | Patience |
//! | Code with repeated patterns (e.g., closing braces) | Patience |
//! | Binary or non-text data | Myers |
//! | Performance-critical diffing | Myers |
//! | Human-readable patch review | Patience |

use std::fmt;

/// The diff algorithm to use for computing differences.
///
/// Different algorithms have different trade-offs between:
/// - **Optimality**: Does it find the minimum edit distance?
/// - **Readability**: Are the diffs easy for humans to understand?
/// - **Performance**: How fast is it for various input sizes?
///
/// See the module documentation for detailed comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// Myers diff algorithm - finds the shortest edit script.
    ///
    /// This is the default algorithm, suitable for most use cases.
    /// It guarantees finding the minimum number of changes but may
    /// sometimes produce diffs that are harder to read when there
    /// are many repeated lines.
    Myers,

    /// Patience diff algorithm - uses unique lines as anchors.
    ///
    /// This algorithm often produces more human-readable diffs,
    /// especially for code with repeated patterns. It may not
    /// find the absolute minimum edit distance but typically
    /// produces more intuitive results.
    Patience,
}

impl Default for Algorithm {
    /// Returns the default algorithm (Myers).
    ///
    /// Myers is the default because:
    /// - It's well-studied and widely used (git default)
    /// - It guarantees optimal (minimum) edit distance
    /// - It performs well on typical source code changes
    fn default() -> Self {
        Algorithm::Myers
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Algorithm::Myers => write!(f, "myers"),
            Algorithm::Patience => write!(f, "patience"),
        }
    }
}

impl std::str::FromStr for Algorithm {
    type Err = AlgorithmParseError;

    /// Parse an algorithm name from a string.
    ///
    /// # Accepted Values
    ///
    /// - "myers" or "Myers" → `Algorithm::Myers`
    /// - "patience" or "Patience" → `Algorithm::Patience`
    ///
    /// # Errors
    ///
    /// Returns `AlgorithmParseError` if the string doesn't match
    /// any known algorithm name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "myers" => Ok(Algorithm::Myers),
            "patience" => Ok(Algorithm::Patience),
            _ => Err(AlgorithmParseError(s.to_string())),
        }
    }
}

/// Error returned when parsing an unknown algorithm name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlgorithmParseError(pub String);

impl fmt::Display for AlgorithmParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown diff algorithm '{}'. Valid options: myers, patience",
            self.0
        )
    }
}

impl std::error::Error for AlgorithmParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_myers() {
        assert_eq!(Algorithm::default(), Algorithm::Myers);
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", Algorithm::Myers), "myers");
        assert_eq!(format!("{}", Algorithm::Patience), "patience");
    }

    #[test]
    fn test_parse_myers() {
        assert_eq!("myers".parse::<Algorithm>().unwrap(), Algorithm::Myers);
        assert_eq!("Myers".parse::<Algorithm>().unwrap(), Algorithm::Myers);
        assert_eq!("MYERS".parse::<Algorithm>().unwrap(), Algorithm::Myers);
    }

    #[test]
    fn test_parse_patience() {
        assert_eq!(
            "patience".parse::<Algorithm>().unwrap(),
            Algorithm::Patience
        );
        assert_eq!(
            "Patience".parse::<Algorithm>().unwrap(),
            Algorithm::Patience
        );
        assert_eq!(
            "PATIENCE".parse::<Algorithm>().unwrap(),
            Algorithm::Patience
        );
    }

    #[test]
    fn test_parse_error() {
        let err = "unknown".parse::<Algorithm>().unwrap_err();
        assert!(err.to_string().contains("unknown"));
        assert!(err.to_string().contains("myers"));
        assert!(err.to_string().contains("patience"));
    }

    #[test]
    fn test_debug() {
        assert_eq!(format!("{:?}", Algorithm::Myers), "Myers");
        assert_eq!(format!("{:?}", Algorithm::Patience), "Patience");
    }

    #[test]
    fn test_clone_copy() {
        let algo = Algorithm::Myers;
        let cloned = algo.clone();
        let copied = algo;
        assert_eq!(algo, cloned);
        assert_eq!(algo, copied);
    }

    #[test]
    fn test_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Algorithm::Myers);
        set.insert(Algorithm::Patience);
        assert!(set.contains(&Algorithm::Myers));
        assert!(set.contains(&Algorithm::Patience));
        assert_eq!(set.len(), 2);
    }
}
