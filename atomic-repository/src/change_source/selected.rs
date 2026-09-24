use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use atomic_config::GitWatch;

use super::{
    ChangeSource, ChangeSourceError, ChangeSourceFallbackReason, ChangeSourceKind,
    ChangeSourceRequest, ChangeSourceResult, ChangeSourceToken, FsmonitorChangeSource,
    ScanChangeSource, WatchmanChangeSource,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeSourceEnvironment {
    pub ci: bool,
    pub container: bool,
}

impl ChangeSourceEnvironment {
    pub fn detect() -> Self {
        Self {
            ci: std::env::var_os("CI").is_some(),
            container: std::env::var_os("container").is_some()
                || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
                || Path::new("/.dockerenv").exists(),
        }
    }
}

/// Transaction-local source selection. Failures fall back to a complete scan
/// and are never cached as repository state.
pub struct SelectedChangeSource {
    configured: GitWatch,
    environment: ChangeSourceEnvironment,
    fsmonitor: Arc<dyn ChangeSource>,
    watchman: Arc<dyn ChangeSource>,
}

impl SelectedChangeSource {
    pub fn new(configured: GitWatch) -> Self {
        Self::with_sources(
            configured,
            ChangeSourceEnvironment::detect(),
            Arc::new(FsmonitorChangeSource::new(
                Duration::from_secs(5),
                1_000_000,
            )),
            Arc::new(WatchmanChangeSource::new(Duration::from_secs(5), 1_000_000)),
        )
    }

    pub fn with_sources(
        configured: GitWatch,
        environment: ChangeSourceEnvironment,
        fsmonitor: Arc<dyn ChangeSource>,
        watchman: Arc<dyn ChangeSource>,
    ) -> Self {
        Self {
            configured,
            environment,
            fsmonitor,
            watchman,
        }
    }

    pub fn changes_with_tokens(
        &self,
        request: ChangeSourceRequest<'_>,
        fsmonitor_token: Option<&ChangeSourceToken>,
        watchman_token: Option<&ChangeSourceToken>,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        let fsmonitor_request = ChangeSourceRequest {
            previous_token: fsmonitor_token,
            ..request
        };
        let watchman_request = ChangeSourceRequest {
            previous_token: watchman_token,
            ..request
        };
        match self.configured {
            GitWatch::Off => Self::scan(
                request,
                ChangeSourceKind::Scan,
                ChangeSourceFallbackReason::ConfiguredOff,
            ),
            GitWatch::Auto if self.environment.ci => Self::scan(
                request,
                ChangeSourceKind::Scan,
                ChangeSourceFallbackReason::DisabledInCi,
            ),
            GitWatch::Auto if self.environment.container => Self::scan(
                request,
                ChangeSourceKind::Scan,
                ChangeSourceFallbackReason::DisabledInContainer,
            ),
            GitWatch::Fsmonitor => {
                match Self::try_source(self.fsmonitor.as_ref(), fsmonitor_request) {
                    Ok(result) => Ok(result),
                    Err(reason) => Self::scan(request, ChangeSourceKind::Fsmonitor, reason),
                }
            }
            GitWatch::Watchman => {
                match Self::try_source(self.watchman.as_ref(), watchman_request) {
                    Ok(result) => Ok(result),
                    Err(reason) => Self::scan(request, ChangeSourceKind::Watchman, reason),
                }
            }
            GitWatch::Auto => match Self::try_source(self.fsmonitor.as_ref(), fsmonitor_request) {
                Ok(result) => Ok(result),
                Err(fsmonitor) => {
                    match Self::try_source(self.watchman.as_ref(), watchman_request) {
                        Ok(result) => Ok(result),
                        Err(watchman) => Self::scan(
                            request,
                            ChangeSourceKind::Custom,
                            ChangeSourceFallbackReason::SourceError(format!(
                                "fsmonitor: {fsmonitor:?}; watchman: {watchman:?}"
                            )),
                        ),
                    }
                }
            },
        }
    }

    fn scan(
        request: ChangeSourceRequest<'_>,
        source: ChangeSourceKind,
        reason: ChangeSourceFallbackReason,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        let mut result = ScanChangeSource.changes(request)?;
        result.fallback_source = Some(source);
        result.fallback = Some(reason);
        Ok(result)
    }

    fn try_source(
        source: &dyn ChangeSource,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceFallbackReason> {
        source.changes(request).map_err(fallback_reason)
    }
}

impl ChangeSource for SelectedChangeSource {
    fn changes(
        &self,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        self.changes_with_tokens(request, request.previous_token, request.previous_token)
    }
}

fn fallback_reason(error: ChangeSourceError) -> ChangeSourceFallbackReason {
    match error {
        ChangeSourceError::Unavailable(message) => ChangeSourceFallbackReason::Unavailable(message),
        ChangeSourceError::UnsupportedVersion { found, minimum } => {
            ChangeSourceFallbackReason::UnsupportedVersion { found, minimum }
        }
        ChangeSourceError::UnknownToken => ChangeSourceFallbackReason::UnknownToken,
        ChangeSourceError::Overflow { .. } => ChangeSourceFallbackReason::Overflow,
        ChangeSourceError::MalformedResponse(message) => {
            ChangeSourceFallbackReason::MalformedResponse(message)
        }
        ChangeSourceError::Timeout { milliseconds } => {
            ChangeSourceFallbackReason::Timeout { milliseconds }
        }
        other => ChangeSourceFallbackReason::SourceError(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::RepoPath;

    struct FakeSource(Result<ChangeSourceResult, ChangeSourceError>);
    struct PanicSource;

    impl ChangeSource for FakeSource {
        fn changes(
            &self,
            _: ChangeSourceRequest<'_>,
        ) -> Result<ChangeSourceResult, ChangeSourceError> {
            self.0.clone()
        }
    }

    impl ChangeSource for PanicSource {
        fn changes(
            &self,
            _: ChangeSourceRequest<'_>,
        ) -> Result<ChangeSourceResult, ChangeSourceError> {
            panic!("lower-priority source must not be queried")
        }
    }

    fn request<'a>(root: &'a Path, tracked: &'a [RepoPath]) -> ChangeSourceRequest<'a> {
        ChangeSourceRequest {
            root,
            tracked_paths: tracked,
            previous_token: None,
        }
    }

    fn error(error: ChangeSourceError) -> Arc<dyn ChangeSource> {
        Arc::new(FakeSource(Err(error)))
    }

    #[test]
    fn auto_selects_fsmonitor_then_watchman_then_scan() {
        let dir = tempfile::tempdir().unwrap();
        let selected = SelectedChangeSource::with_sources(
            GitWatch::Auto,
            ChangeSourceEnvironment::default(),
            Arc::new(ScanChangeSource),
            Arc::new(PanicSource),
        );
        assert!(selected.changes(request(dir.path(), &[])).unwrap().complete);

        let selected = SelectedChangeSource::with_sources(
            GitWatch::Auto,
            ChangeSourceEnvironment::default(),
            error(ChangeSourceError::Unavailable("fsmonitor".into())),
            Arc::new(ScanChangeSource),
        );
        let watchman = selected.changes(request(dir.path(), &[])).unwrap();
        assert_eq!(watchman.fallback, None);

        let selected = SelectedChangeSource::with_sources(
            GitWatch::Auto,
            ChangeSourceEnvironment::default(),
            error(ChangeSourceError::Unavailable("fsmonitor".into())),
            error(ChangeSourceError::Unavailable("watchman".into())),
        );
        let scan = selected.changes(request(dir.path(), &[])).unwrap();
        assert!(scan.complete);
        assert!(matches!(
            scan.fallback,
            Some(ChangeSourceFallbackReason::SourceError(_))
        ));
    }

    #[test]
    fn ci_auto_and_explicit_failures_become_typed_complete_scans() {
        let dir = tempfile::tempdir().unwrap();
        let selected = SelectedChangeSource::with_sources(
            GitWatch::Auto,
            ChangeSourceEnvironment {
                ci: true,
                container: false,
            },
            error(ChangeSourceError::Unavailable("unused".into())),
            error(ChangeSourceError::Unavailable("unused".into())),
        );
        let result = selected.changes(request(dir.path(), &[])).unwrap();
        assert!(result.complete);
        assert_eq!(
            result.fallback,
            Some(ChangeSourceFallbackReason::DisabledInCi)
        );

        let selected = SelectedChangeSource::with_sources(
            GitWatch::Fsmonitor,
            ChangeSourceEnvironment::default(),
            error(ChangeSourceError::Timeout { milliseconds: 5 }),
            error(ChangeSourceError::Unavailable("unused".into())),
        );
        let result = selected.changes(request(dir.path(), &[])).unwrap();
        assert_eq!(
            result.fallback,
            Some(ChangeSourceFallbackReason::Timeout { milliseconds: 5 })
        );
    }
}
