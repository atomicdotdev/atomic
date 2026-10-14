//! CB-11A Git-authoritative ignore policy and managed `.git/info/exclude`
//! mirroring (RFC §9.4).
//!
//! In colocated mode Git ignore sources are authoritative for untracked
//! status parity. `.atomicignore` patterns that are not already represented
//! in Git's ignore sources are mirrored into a managed block inside
//! `.git/info/exclude` — but only with explicit consent. Without consent the
//! divergence is *reported* (never silently resolved): Atomic refuses to hide
//! files that Git reports.
//!
//! Tracked (or indexed) paths are never excluded by ignore rules: mirroring
//! only ever governs discovery of untracked paths.

use std::fs;
use std::path::Path;

/// Managed-block begin sentinel written into `.git/info/exclude`.
pub const MANAGED_BLOCK_BEGIN: &str = "# >>> atomic managed ignore (consented; do not edit) >>>";
/// Managed-block end sentinel written into `.git/info/exclude`.
pub const MANAGED_BLOCK_END: &str = "# <<< atomic managed ignore <<<";

/// Result of checking the ignore policy across Atomic and Git sources.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IgnorePolicyReport {
    /// Non-comment, non-empty patterns from `.atomicignore`.
    pub atomic_patterns: Vec<String>,
    /// `.atomicignore` patterns not represented in any Git ignore source.
    pub unmirrored: Vec<String>,
    /// Patterns currently inside the managed `.git/info/exclude` block.
    pub managed_block_patterns: Vec<String>,
    /// Whether the managed block exists in `.git/info/exclude`.
    pub managed_block_present: bool,
}

impl IgnorePolicyReport {
    /// True when `.atomicignore` patterns are not covered by Git ignore
    /// sources and mirroring has not been consented to.
    pub fn diverges(&self) -> bool {
        !self.unmirrored.is_empty()
    }
}

/// Errors from writing the managed exclude block.
#[derive(Debug, thiserror::Error)]
pub enum IgnoreMirrorError {
    #[error("cannot read Git administrative directory: {0}")]
    GitDirectory(String),
    #[error("cannot read `.git/info/exclude`: {0}")]
    ReadExclude(String),
    #[error("cannot write `.git/info/exclude`: {0}")]
    WriteExclude(String),
    #[error("cannot read `.atomicignore`: {0}")]
    ReadAtomicIgnore(String),
}

/// Parse pattern lines from `.atomicignore` at the repository root.
pub fn atomicignore_patterns(root: &Path) -> Result<Vec<String>, IgnoreMirrorError> {
    let path = root.join(".atomicignore");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(IgnoreMirrorError::ReadAtomicIgnore(error.to_string())),
    };
    Ok(parse_pattern_lines(&bytes))
}

fn parse_pattern_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            // Strip a single trailing carriage return for CRLF files.
            line.strip_suffix('\r').unwrap_or(line)
        })
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn split_managed_block(contents: &str) -> (Option<String>, Vec<String>, bool) {
    let mut managed_lines = Vec::new();
    let mut outside = String::new();
    let mut in_block = false;
    let mut block_present = false;
    for line in contents.lines() {
        if line.trim() == MANAGED_BLOCK_BEGIN {
            in_block = true;
            block_present = true;
            continue;
        }
        if line.trim() == MANAGED_BLOCK_END {
            in_block = false;
            continue;
        }
        if in_block {
            managed_lines.push(line.to_string());
        } else {
            outside.push_str(line);
            outside.push('\n');
        }
    }
    (
        if block_present { Some(outside) } else { None },
        managed_lines
            .into_iter()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .collect(),
        block_present,
    )
}

fn exclude_path(root: &Path) -> Result<std::path::PathBuf, IgnoreMirrorError> {
    let repository = git2::Repository::discover(root)
        .map_err(|error| IgnoreMirrorError::GitDirectory(format!("{}: {error}", root.display())))?;
    // `.git/info/exclude` lives in the common administrative directory so
    // linked worktrees share one managed block. git2 0.19 does not expose
    // `commondir()`, so resolve the `commondir` pointer file explicitly.
    let git_dir = repository.path();
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(pointer) => {
            let pointer = pointer.trim_end_matches(['\n', '\r']);
            let absolute = if Path::new(pointer).is_absolute() {
                std::path::PathBuf::from(pointer)
            } else {
                git_dir.join(pointer)
            };
            fs::canonicalize(&absolute).unwrap_or(absolute)
        }
        Err(_) => git_dir.to_path_buf(),
    };
    Ok(common_dir.join("info").join("exclude"))
}

fn read_exclude_patterns(path: &Path) -> Result<Vec<String>, IgnoreMirrorError> {
    match fs::read(path) {
        Ok(bytes) => Ok(parse_pattern_lines(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(IgnoreMirrorError::ReadExclude(error.to_string())),
    }
}

/// Normalize a pattern for representation comparison: strip a leading and a
/// trailing slash so `.atomic` matches the anchored exclude line `/.atomic/`.
fn normalize_pattern(pattern: &str) -> &str {
    pattern
        .strip_prefix('/')
        .unwrap_or(pattern)
        .strip_suffix('/')
        .unwrap_or(pattern)
}

/// Check ignore-policy parity without writing anything.
///
/// A pattern counts as represented when it appears verbatim (after slash
/// normalization) in `.gitignore`, `.git/info/exclude`, or the managed block.
/// The Git administrative directory `.git` is ignored by Git itself and is
/// never reported as divergent or mirrored.
pub fn check_ignore_policy(root: &Path) -> Result<IgnorePolicyReport, IgnoreMirrorError> {
    let atomic_patterns = atomicignore_patterns(root)?;
    let exclude_path = exclude_path(root)?;
    let git_patterns = {
        let mut patterns = read_exclude_patterns(&exclude_path)?;
        match fs::read_to_string(root.join(".gitignore")) {
            Ok(contents) => patterns.extend(parse_pattern_lines(contents.as_bytes())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(IgnoreMirrorError::ReadExclude(error.to_string())),
        }
        patterns
    };
    let (managed_block_patterns, block_present) = if exclude_path.is_file() {
        let contents = fs::read_to_string(&exclude_path)
            .map_err(|error| IgnoreMirrorError::ReadExclude(error.to_string()))?;
        let (_, patterns, present) = split_managed_block(&contents);
        (patterns, present)
    } else {
        (Vec::new(), false)
    };

    let represented: std::collections::BTreeSet<String> = git_patterns
        .iter()
        .chain(managed_block_patterns.iter())
        .map(|pattern| normalize_pattern(pattern).to_string())
        .collect();
    let unmirrored = atomic_patterns
        .iter()
        .filter(|pattern| {
            let normalized = normalize_pattern(pattern);
            // `.git` is ignored by Git itself; it can never diverge.
            normalized != ".git" && !represented.contains(normalized)
        })
        .cloned()
        .collect();

    Ok(IgnorePolicyReport {
        atomic_patterns,
        unmirrored,
        managed_block_patterns,
        managed_block_present: block_present,
    })
}

/// Mirror unrepresented `.atomicignore` patterns into the managed
/// `.git/info/exclude` block. This is the explicit-consent operation; it
/// writes only between the managed sentinels and never touches user content
/// in the exclude file.
pub fn mirror_ignores(root: &Path) -> Result<IgnorePolicyReport, IgnoreMirrorError> {
    let report = check_ignore_policy(root)?;
    let exclude_path = exclude_path(root)?;

    let existing = match fs::read_to_string(&exclude_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(IgnoreMirrorError::WriteExclude(error.to_string())),
    };
    let (outside, mut managed, _) = split_managed_block(&existing);

    for pattern in &report.unmirrored {
        if !managed.iter().any(|line| line == pattern) {
            managed.push(pattern.clone());
        }
    }

    let mut output = String::new();
    if let Some(outside) = outside {
        output.push_str(outside.trim_end_matches('\n'));
        if !output.is_empty() {
            output.push('\n');
        }
    } else if !existing.trim_end_matches('\n').is_empty() {
        // No managed block yet: preserve all existing user lines verbatim.
        output.push_str(existing.trim_end_matches('\n'));
        output.push('\n');
    }
    output.push_str(MANAGED_BLOCK_BEGIN);
    output.push('\n');
    output.push_str("# Mirrored from .atomicignore with explicit consent.\n");
    for line in &managed {
        output.push_str(line);
        output.push('\n');
    }
    output.push_str(MANAGED_BLOCK_END);
    output.push('\n');

    if let Some(parent) = exclude_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| IgnoreMirrorError::WriteExclude(error.to_string()))?;
    }
    fs::write(&exclude_path, output)
        .map_err(|error| IgnoreMirrorError::WriteExclude(error.to_string()))?;

    check_ignore_policy(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> TempDir {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        temp
    }

    #[test]
    fn empty_atomicignore_never_diverges() {
        let temp = fixture();
        let report = check_ignore_policy(temp.path()).unwrap();
        assert!(report.atomic_patterns.is_empty());
        assert!(!report.diverges());
    }

    #[test]
    fn unrepresented_patterns_diverge_until_mirrored() {
        let temp = fixture();
        std::fs::write(temp.path().join(".atomicignore"), b"target/\n*.log\n").unwrap();
        let report = check_ignore_policy(temp.path()).unwrap();
        assert_eq!(report.atomic_patterns, vec!["target/", "*.log"]);
        assert_eq!(report.unmirrored, vec!["target/", "*.log"]);
        assert!(report.diverges());

        let mirrored = mirror_ignores(temp.path()).unwrap();
        assert!(!mirrored.diverges());
        assert!(mirrored.managed_block_present);
        let contents = std::fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap();
        assert!(contents.contains(MANAGED_BLOCK_BEGIN));
        assert!(contents.contains("target/"));
    }

    #[test]
    fn mirroring_preserves_user_exclude_lines() {
        let temp = fixture();
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"# user comment\n*.tmp\n").unwrap();
        std::fs::write(temp.path().join(".atomicignore"), b"target/\n").unwrap();

        mirror_ignores(temp.path()).unwrap();
        let contents = std::fs::read_to_string(&exclude).unwrap();
        assert!(contents.contains("# user comment"));
        assert!(contents.contains("*.tmp"));
        assert!(contents.contains(MANAGED_BLOCK_BEGIN));
        assert!(contents.contains("target/"));

        // Mirroring twice is idempotent.
        mirror_ignores(temp.path()).unwrap();
        let again = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(again.matches(MANAGED_BLOCK_BEGIN).count(), 1);
        assert_eq!(again.matches("target/").count(), 1);
    }

    #[test]
    fn patterns_already_in_gitignore_do_not_diverge() {
        let temp = fixture();
        std::fs::write(temp.path().join(".gitignore"), b"target/\n").unwrap();
        std::fs::write(temp.path().join(".atomicignore"), b"target/\n*.log\n").unwrap();
        let report = check_ignore_policy(temp.path()).unwrap();
        assert_eq!(report.unmirrored, vec!["*.log"]);
    }
}
