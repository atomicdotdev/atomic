use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use atomic_core::types::Hash;

use super::process::{CommandOutput, CommandRunner, CommandSpec, ProcessCommandRunner};
use super::scan::observe_candidate;
use super::{
    ChangeCandidateReason, ChangeSource, ChangeSourceError, ChangeSourceRequest,
    ChangeSourceResult, ChangeSourceStats, ChangeSourceToken,
};
use crate::repository::RepoPath;

const TOKEN_PREFIX: &[u8] = b"git-fsmonitor-v1:";
const MINIMUM_GIT_VERSION: (u64, u64, u64) = (2, 37, 0);

/// Read-only adapter for Git's builtin fsmonitor-backed porcelain query.
pub struct FsmonitorChangeSource {
    runner: Arc<dyn CommandRunner>,
    timeout: Duration,
    max_candidates: usize,
}

impl FsmonitorChangeSource {
    pub fn new(timeout: Duration, max_candidates: usize) -> Self {
        Self::with_runner(Arc::new(ProcessCommandRunner), timeout, max_candidates)
    }

    pub fn with_runner(
        runner: Arc<dyn CommandRunner>,
        timeout: Duration,
        max_candidates: usize,
    ) -> Self {
        Self {
            runner,
            timeout,
            max_candidates,
        }
    }

    fn git(&self, root: &Path, args: &[&str]) -> Result<CommandOutput, ChangeSourceError> {
        let mut spec = CommandSpec::new("git", root);
        spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        spec.env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
        spec.timeout = self.timeout;
        spec.max_output_bytes = self.max_candidates.saturating_mul(4096).max(64 * 1024);
        self.runner.run(&spec)
    }

    fn ensure_available(&self, root: &Path) -> Result<(), ChangeSourceError> {
        let output = self.git(root, &["--version"])?;
        if output.status != 0 {
            return Err(unavailable("git --version", &output));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| ChangeSourceError::MalformedResponse("non-UTF-8 Git version".into()))?;
        let version = text
            .trim()
            .strip_prefix("git version ")
            .ok_or_else(|| ChangeSourceError::MalformedResponse(text.trim().into()))?;
        let parsed = parse_version(version).ok_or_else(|| {
            ChangeSourceError::MalformedResponse(format!("invalid Git version {version}"))
        })?;
        if parsed < MINIMUM_GIT_VERSION {
            return Err(ChangeSourceError::UnsupportedVersion {
                found: version.to_owned(),
                minimum: "2.37.0".into(),
            });
        }

        let daemon = self.git(root, &["fsmonitor--daemon", "status"])?;
        if daemon.status != 0 {
            return Err(unavailable("Git builtin fsmonitor", &daemon));
        }
        let configured = self.git(root, &["config", "--type=bool", "--get", "core.fsmonitor"])?;
        if configured.status != 0 || configured.stdout.as_slice() != b"true\n" {
            return Err(ChangeSourceError::Unavailable(
                "Git builtin fsmonitor is not enabled by core.fsmonitor=true".into(),
            ));
        }
        Ok(())
    }
}

impl ChangeSource for FsmonitorChangeSource {
    fn changes(
        &self,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        validate_token(request.previous_token)?;
        self.ensure_available(request.root)?;
        let output = self.git(
            request.root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )?;
        if output.status != 0 {
            return Err(unavailable("Git status query", &output));
        }

        let tracked: BTreeSet<_> = request.tracked_paths.iter().cloned().collect();
        let raw = parse_porcelain_v2(&output.stdout, self.max_candidates)?;
        let mut paths = BTreeSet::new();
        for bytes in raw {
            let path =
                RepoPath::new(bytes).map_err(|error| ChangeSourceError::Path(error.to_string()))?;
            if tracked.contains(&path) {
                paths.insert(path);
            }
        }
        let mut candidates = Vec::with_capacity(paths.len());
        for path in paths {
            candidates.push(observe_candidate(
                request.root,
                path,
                ChangeCandidateReason::Explicit,
            )?);
        }
        Ok(ChangeSourceResult {
            root: request.root.to_path_buf(),
            source: super::ChangeSourceKind::Fsmonitor,
            fallback_source: None,
            token: output_token(&output.stdout),
            complete: false,
            fallback: None,
            stats: ChangeSourceStats {
                tracked_paths: request.tracked_paths.len(),
                candidates: candidates.len(),
                metadata_reads: candidates.len(),
                ..ChangeSourceStats::default()
            },
            candidates,
        })
    }
}

fn validate_token(token: Option<&ChangeSourceToken>) -> Result<(), ChangeSourceError> {
    if token.is_some_and(|token| !token.0.starts_with(TOKEN_PREFIX)) {
        return Err(ChangeSourceError::UnknownToken);
    }
    Ok(())
}

fn output_token(payload: &[u8]) -> ChangeSourceToken {
    let mut bytes = TOKEN_PREFIX.to_vec();
    bytes.extend_from_slice(Hash::of(payload).as_bytes());
    ChangeSourceToken(bytes)
}

fn unavailable(operation: &str, output: &CommandOutput) -> ChangeSourceError {
    let detail = String::from_utf8_lossy(&output.stderr);
    ChangeSourceError::Unavailable(format!(
        "{operation} exited with {}{}",
        output.status,
        if detail.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", detail.trim())
        }
    ))
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split_whitespace().next()?.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()?
            .split(|character: char| !character.is_ascii_digit())
            .next()?
            .parse()
            .ok()?,
    ))
}

fn parse_porcelain_v2(bytes: &[u8], limit: usize) -> Result<Vec<Vec<u8>>, ChangeSourceError> {
    let records: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut paths = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let (spaces, renamed) = match record[0] {
            b'1' => (8, false),
            b'2' => (9, true),
            b'u' => (10, false),
            b'?' => (1, false),
            b'!' | b'#' => continue,
            _ => {
                return Err(ChangeSourceError::MalformedResponse(
                    "unknown Git porcelain-v2 record".into(),
                ))
            }
        };
        let path = field_after_spaces(record, spaces).ok_or_else(|| {
            ChangeSourceError::MalformedResponse("truncated Git porcelain-v2 record".into())
        })?;
        if path.is_empty() {
            return Err(ChangeSourceError::MalformedResponse(
                "empty Git candidate path".into(),
            ));
        }
        paths.push(path.to_vec());
        if paths.len() > limit {
            return Err(ChangeSourceError::Overflow { limit });
        }
        if renamed {
            if index >= records.len() || records[index].is_empty() {
                return Err(ChangeSourceError::MalformedResponse(
                    "rename record has no original path".into(),
                ));
            }
            index += 1;
        }
    }
    Ok(paths)
}

fn field_after_spaces(record: &[u8], count: usize) -> Option<&[u8]> {
    let mut remaining = record;
    for _ in 0..count {
        let separator = remaining.iter().position(|byte| *byte == b' ')?;
        remaining = &remaining[separator + 1..];
    }
    Some(remaining)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use tempfile::tempdir;

    struct FakeRunner(Mutex<VecDeque<Result<CommandOutput, ChangeSourceError>>>);

    impl CommandRunner for FakeRunner {
        fn run(&self, _: &CommandSpec) -> Result<CommandOutput, ChangeSourceError> {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    fn output(stdout: &[u8]) -> Result<CommandOutput, ChangeSourceError> {
        Ok(CommandOutput {
            status: 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn source(
        outputs: Vec<Result<CommandOutput, ChangeSourceError>>,
        limit: usize,
    ) -> FsmonitorChangeSource {
        FsmonitorChangeSource::with_runner(
            Arc::new(FakeRunner(Mutex::new(outputs.into()))),
            Duration::from_secs(1),
            limit,
        )
    }

    #[test]
    fn implements_canonical_candidate_contract() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("tracked"), b"x").unwrap();
        let tracked = vec![RepoPath::from_bytes(b"tracked").unwrap()];
        let source = source(
            vec![
                output(b"git version 2.45.1\n"),
                output(b"watching\n"),
                output(b"true\n"),
                output(b"? ignored\0? tracked\0? tracked\0"),
            ],
            8,
        );
        let result = source
            .changes(ChangeSourceRequest {
                root: dir.path(),
                tracked_paths: &tracked,
                previous_token: None,
            })
            .unwrap();
        assert!(!result.complete);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].path, tracked[0]);
    }

    #[test]
    fn preserves_typed_failures() {
        let old = source(vec![output(b"git version 2.36.4\n")], 8);
        assert!(matches!(
            old.changes(ChangeSourceRequest {
                root: Path::new("."),
                tracked_paths: &[],
                previous_token: None,
            }),
            Err(ChangeSourceError::UnsupportedVersion { .. })
        ));
        let source = source(Vec::new(), 8);
        assert_eq!(
            source
                .changes(ChangeSourceRequest {
                    root: Path::new("."),
                    tracked_paths: &[],
                    previous_token: Some(&ChangeSourceToken(b"other".to_vec())),
                })
                .unwrap_err(),
            ChangeSourceError::UnknownToken
        );
        assert!(matches!(
            parse_porcelain_v2(b"? a\0? b\0", 1),
            Err(ChangeSourceError::Overflow { limit: 1 })
        ));
        assert!(matches!(
            parse_porcelain_v2(b"bad\0", 8),
            Err(ChangeSourceError::MalformedResponse(_))
        ));
    }
}
