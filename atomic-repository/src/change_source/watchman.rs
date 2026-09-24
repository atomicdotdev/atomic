use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::process::{CommandOutput, CommandRunner, CommandSpec, ProcessCommandRunner};
use super::scan::observe_candidate;
use super::{
    ChangeCandidateReason, ChangeSource, ChangeSourceError, ChangeSourceRequest,
    ChangeSourceResult, ChangeSourceStats, ChangeSourceToken,
};
use crate::repository::RepoPath;

const TOKEN_PREFIX: &[u8] = b"watchman-clock-v1:";

/// Watchman JSON-protocol candidate adapter.
pub struct WatchmanChangeSource {
    runner: Arc<dyn CommandRunner>,
    timeout: Duration,
    max_candidates: usize,
}

impl WatchmanChangeSource {
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

    fn watchman(
        &self,
        root: &Path,
        args: &[&str],
        stdin: Vec<u8>,
    ) -> Result<CommandOutput, ChangeSourceError> {
        let mut spec = CommandSpec::new("watchman", root);
        spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        spec.stdin = stdin;
        spec.timeout = self.timeout;
        spec.max_output_bytes = self.max_candidates.saturating_mul(4096).max(64 * 1024);
        self.runner.run(&spec)
    }
}

impl ChangeSource for WatchmanChangeSource {
    fn changes(
        &self,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        validate_token(request.previous_token)?;
        let root = request.root.to_str().ok_or_else(|| {
            ChangeSourceError::Unavailable("Watchman requires a UTF-8 repository path".into())
        })?;
        let since = request
            .previous_token
            .map(|token| &token.0[TOKEN_PREFIX.len()..])
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| ChangeSourceError::UnknownToken)?;
        let mut query = json!({
            "expression": ["type", "f"],
            "fields": ["name"]
        });
        if let Some(since) = since {
            query["since"] = Value::String(since.to_owned());
        }
        let mut stdin = serde_json::to_vec(&json!(["query", root, query]))
            .map_err(|error| ChangeSourceError::MalformedResponse(error.to_string()))?;
        stdin.push(b'\n');
        let output = self.watchman(request.root, &["-j", "--no-pretty"], stdin)?;
        if output.status != 0 {
            return Err(unavailable("Watchman query", &output));
        }
        let (clock, raw_paths) = parse_response(
            &output.stdout,
            request.previous_token.is_some(),
            self.max_candidates,
        )?;
        let tracked: BTreeSet<_> = request.tracked_paths.iter().cloned().collect();
        let mut paths = BTreeSet::new();
        for bytes in raw_paths {
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
        let mut token = TOKEN_PREFIX.to_vec();
        token.extend_from_slice(clock.as_bytes());
        Ok(ChangeSourceResult {
            root: request.root.to_path_buf(),
            source: super::ChangeSourceKind::Watchman,
            fallback_source: None,
            token: ChangeSourceToken(token),
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

fn parse_response(
    bytes: &[u8],
    had_previous: bool,
    limit: usize,
) -> Result<(String, Vec<Vec<u8>>), ChangeSourceError> {
    let response: Value = serde_json::from_slice(bytes)
        .map_err(|error| ChangeSourceError::MalformedResponse(error.to_string()))?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        return Err(ChangeSourceError::Unavailable(error.to_owned()));
    }
    if had_previous
        && response
            .get("is_fresh_instance")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(ChangeSourceError::UnknownToken);
    }
    let clock = response
        .get("clock")
        .and_then(Value::as_str)
        .ok_or_else(|| ChangeSourceError::MalformedResponse("missing Watchman clock".into()))?;
    let files = response
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| ChangeSourceError::MalformedResponse("missing Watchman files".into()))?;
    if files.len() > limit {
        return Err(ChangeSourceError::Overflow { limit });
    }
    let paths = files
        .iter()
        .map(|file| {
            file.as_str()
                .map(|path| path.as_bytes().to_vec())
                .ok_or_else(|| {
                    ChangeSourceError::MalformedResponse(
                        "Watchman file entry is not a string".into(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((clock.to_owned(), paths))
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

    fn source(
        outputs: Vec<Result<CommandOutput, ChangeSourceError>>,
        limit: usize,
    ) -> WatchmanChangeSource {
        WatchmanChangeSource::with_runner(
            Arc::new(FakeRunner(Mutex::new(outputs.into()))),
            Duration::from_secs(1),
            limit,
        )
    }

    fn output(json: &str) -> Result<CommandOutput, ChangeSourceError> {
        Ok(CommandOutput {
            status: 0,
            stdout: json.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn implements_canonical_candidate_contract() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("tracked"), b"x").unwrap();
        let tracked = vec![RepoPath::from_bytes(b"tracked").unwrap()];
        let source = source(
            vec![output(
                r#"{"clock":"c:1:2","files":["tracked","tracked","ignored"]}"#,
            )],
            8,
        );
        let result = source
            .changes(ChangeSourceRequest {
                root: dir.path(),
                tracked_paths: &tracked,
                previous_token: None,
            })
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].path, tracked[0]);
        assert!(!result.complete);
    }

    #[test]
    fn preserves_unknown_overflow_malformed_and_timeout() {
        let foreign_source = source(Vec::new(), 8);
        assert_eq!(
            foreign_source
                .changes(ChangeSourceRequest {
                    root: Path::new("."),
                    tracked_paths: &[],
                    previous_token: Some(&ChangeSourceToken(b"foreign".to_vec())),
                })
                .unwrap_err(),
            ChangeSourceError::UnknownToken
        );
        assert!(matches!(
            parse_response(br#"{"clock":"c","files":["a","b"]}"#, false, 1),
            Err(ChangeSourceError::Overflow { .. })
        ));
        assert!(matches!(
            parse_response(br#"{"files":[]}"#, false, 8),
            Err(ChangeSourceError::MalformedResponse(_))
        ));
        let timeout = source(
            vec![Err(ChangeSourceError::Timeout { milliseconds: 10 })],
            8,
        );
        assert_eq!(
            timeout
                .changes(ChangeSourceRequest {
                    root: Path::new("."),
                    tracked_paths: &[],
                    previous_token: None,
                })
                .unwrap_err(),
            ChangeSourceError::Timeout { milliseconds: 10 }
        );
    }
}
