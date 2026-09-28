//! Logging: warnings on the terminal, `info` and above in a daily file.
//!
//! The terminal shows what it showed under `env_logger`: warnings by default,
//! more with `--verbose` or `RUST_LOG`, one `[time LEVEL target] message` line
//! each. Every command also appends `info` and above to `atomic.YYYY-MM-DD.log`
//! (UTC date) in `~/.atomic/logs`, or in `logs/` under `ATOMIC_CONFIG_DIR`
//! when that is set. Each event is one line, appended with a single write, so
//! it is on disk when the call returns, and processes sharing the file (agent
//! hooks run concurrently) never interleave within a line. Each line names its
//! process and the spans it ran in.
//!
//! - `ATOMIC_LOG` sets the file's filter, in `RUST_LOG` syntax; `off` turns
//!   the file off.
//! - `ATOMIC_LOG_DIR` moves the file; it must be an absolute path.
//!
//! The `log` macros the crates already use reach both outputs unchanged.

mod file_format;
mod redact;
mod retention;
mod terminal;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

use crate::error::CliError;

/// The terminal filter `--verbose` turns on.
///
/// Scoped to the Atomic crates on purpose: a bare `debug` also unleashes
/// `reqwest`/`hyper` wire logging, which buries the one line the user wanted.
///
/// `atomic` is a prefix match, so it covers this binary (whose module paths
/// are `atomic::…`, after the `[[bin]]` name rather than the `atomic-cli`
/// package) along with every `atomic_*` library crate. `atomic_core` is pegged
/// back to `info`: its per-vertex graph logging is far below the level anyone
/// reaching for `--verbose` is asking about.
const VERBOSE_FILTER: &str = "atomic=debug,atomic_core=info";

/// The terminal filter without `--verbose` or `RUST_LOG`.
const TERMINAL_FILTER: &str = "warn";

/// Targets written for the log file. `--verbose` keeps them off the
/// terminal, where a large import's lines would bury the ones it is for.
/// (Targets match by string prefix, so neither may prefix a module path.)
const FILE_ONLY_TARGETS: &str = "atomic::logging::command=warn,atomic::git::import=warn";

/// Crates that log through `tracing` itself. Nothing collected their events
/// under `env_logger`, so they stay off the terminal unless `RUST_LOG` asks.
const TRACING_NATIVE_DEPENDENCIES: &str = "h2=off,hyper=off,hyper_util=off";

/// The file filter without `ATOMIC_LOG`: `info` from Atomic's crates and
/// warnings from everything else, so dependencies' own chatter stays out.
const FILE_FILTER: &str = "warn,atomic=info";

/// A command's start and finish.
const COMMAND_TARGET: &str = "atomic::logging::command";

/// Panics are logged under this target for the file only: the panic hook
/// already prints them on the terminal.
const PANIC_TARGET: &str = "atomic::panic";

const LOG_FILE_PREFIX: &str = "atomic";
const LOG_FILE_SUFFIX: &str = "log";

/// Daily files kept, today's included.
const KEPT_LOG_FILES: usize = 7;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Install logging for this process. Call once, before anything logs.
pub(crate) fn init() {
    let mut layers = vec![terminal_layer()];
    let mut unusable = None;
    let mut opened = None;
    if let Some((directives, dir)) = file_settings() {
        match open_file(dir.path()) {
            Ok(appender) => {
                layers.push(file_layer(appender, &directives));
                opened = Some(dir);
            }
            Err(error) => unusable = Some((dir, error)),
        }
    }
    if tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .is_err()
    {
        return;
    }
    log_panics();
    if let Some((dir, error)) = unusable {
        report_unusable(&dir, &error);
    }
    if let Some(dir) = opened {
        prune_old_files(dir.path());
    }
}

/// A command's run in the log: every line inside it carries
/// `atomic{cmd=git import}`, and its start and end are logged.
pub(crate) struct CommandLog {
    _span: tracing::span::EnteredSpan,
}

impl CommandLog {
    pub(crate) fn start(matches: &clap::ArgMatches) -> Self {
        let span = tracing::info_span!("atomic", cmd = %command_path(matches)).entered();
        let cwd = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        tracing::info!(
            target: COMMAND_TARGET,
            version = env!("CARGO_PKG_VERSION"),
            cwd = %cwd,
            "command started"
        );
        Self { _span: span }
    }

    /// Log the outcome. The span closes when `self` drops here, logging how
    /// long the command took. URLs in the error lose their credentials.
    pub(crate) fn finish(self, error: Option<&CliError>) {
        match error {
            None => tracing::info!(target: COMMAND_TARGET, outcome = %"ok", "command finished"),
            Some(error) => tracing::info!(
                target: COMMAND_TARGET,
                outcome = %"error",
                exit_code = error.exit_code(),
                error = %redact::urls(&error.to_string()),
                "command finished"
            ),
        }
    }
}

/// Detect the global `--verbose` flag straight from `argv`.
///
/// Logging has to be live before clap runs, because argument parsing itself
/// can fail and we want the debug trail for that too. `--verbose` is a global
/// flag, so its position is unconstrained — scanning argv is both simpler and
/// more faithful than trying to parse twice.
fn verbose_requested() -> bool {
    std::env::args_os().any(|a| a == "-v" || a == "--verbose")
}

/// The terminal's filter: `RUST_LOG` wins, then `--verbose`, then warnings.
/// Every command advertises `-v, --verbose`, and the debug lines that explain
/// which identity a request authenticated as are only reachable through it.
/// Like `env_logger`, a `RUST_LOG` with no usable directive shows errors
/// rather than nothing.
fn terminal_directives(rust_log: Option<String>, verbose: bool) -> String {
    let directives = match rust_log.filter(|value| !value.trim().is_empty()) {
        Some(value) if parses(&value) => value,
        Some(_) => "error".to_string(),
        None if verbose => {
            format!("{VERBOSE_FILTER},{FILE_ONLY_TARGETS},{TRACING_NATIVE_DEPENDENCIES}")
        }
        None => format!("{TERMINAL_FILTER},{TRACING_NATIVE_DEPENDENCIES}"),
    };
    format!("{directives},{PANIC_TARGET}=off")
}

fn parses(directives: &str) -> bool {
    directives
        .split(',')
        .any(|directive| !directive.trim().is_empty())
        && EnvFilter::builder().parse(directives).is_ok()
}

/// The file's filter from `ATOMIC_LOG`, or `None` when it is `off`.
fn file_directives(atomic_log: Option<String>) -> Option<String> {
    match atomic_log.map(|value| value.trim().to_string()) {
        Some(value) if value.eq_ignore_ascii_case("off") => None,
        Some(value) if !value.is_empty() => Some(value),
        _ => Some(FILE_FILTER.to_string()),
    }
}

/// Where the log file goes, and whether the user chose it.
#[derive(Debug, PartialEq, Eq)]
enum LogDir {
    /// `ATOMIC_LOG_DIR`: failing to open it is worth a warning.
    Chosen(PathBuf),
    /// Under the global config dir. Failing quietly keeps a read-only home
    /// from putting a warning on every command.
    Default(PathBuf),
}

impl LogDir {
    fn path(&self) -> &Path {
        match self {
            Self::Chosen(path) | Self::Default(path) => path,
        }
    }
}

fn log_dir(chosen: Option<OsString>, config_dir: Option<PathBuf>) -> Option<LogDir> {
    match chosen.filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(LogDir::Chosen(PathBuf::from(dir))),
        None => config_dir
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| LogDir::Default(dir.join("logs"))),
    }
}

fn file_settings() -> Option<(String, LogDir)> {
    let directives = file_directives(std::env::var("ATOMIC_LOG").ok())?;
    let dir = log_dir(
        std::env::var_os("ATOMIC_LOG_DIR"),
        atomic_config::global_config_dir(),
    )?;
    Some((directives, dir))
}

/// The command path clap matched, such as `git import`.
fn command_path(matches: &clap::ArgMatches) -> String {
    let mut names = Vec::new();
    let mut current = matches;
    while let Some((name, sub)) = current.subcommand() {
        names.push(name);
        current = sub;
    }
    names.join(" ")
}

fn terminal_layer() -> BoxedLayer {
    let directives = terminal_directives(std::env::var("RUST_LOG").ok(), verbose_requested());
    tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .event_format(terminal::EnvLoggerFormat)
        .with_filter(EnvFilter::builder().parse_lossy(directives))
        .boxed()
}

/// Why the log file could not be opened.
#[derive(Debug)]
enum OpenError {
    /// A relative directory would land in whatever directory the command
    /// runs in, usually a repository.
    Relative,
    Create(std::io::Error),
    Init(InitError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relative => f.write_str("not an absolute path"),
            Self::Create(error) => write!(f, "{error}"),
            Self::Init(error) => write!(f, "{error}"),
        }
    }
}

fn open_file(dir: &Path) -> Result<RollingFileAppender, OpenError> {
    if !dir.is_absolute() {
        return Err(OpenError::Relative);
    }
    create_private_dir(dir).map_err(OpenError::Create)?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .build(dir)
        .map_err(OpenError::Init)
}

/// Create the log directory readable by its owner only: the file records
/// working directories, paths and error text. An existing directory keeps
/// its permissions.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The file layer, with a line for each span's duration when it closes.
fn file_layer(appender: RollingFileAppender, directives: &str) -> BoxedLayer {
    tracing_subscriber::fmt::layer()
        .with_writer(appender)
        .with_span_events(FmtSpan::CLOSE)
        .event_format(file_format::FileFormat::new())
        .with_filter(EnvFilter::builder().parse_lossy(directives))
        .boxed()
}

fn report_unusable(dir: &LogDir, error: &OpenError) {
    match dir {
        LogDir::Chosen(path) => tracing::warn!(
            "ATOMIC_LOG_DIR {} is unusable ({error}); file logging is off for this command",
            path.display()
        ),
        LogDir::Default(path) => tracing::debug!(
            "log directory {} is unusable ({error}); file logging is off for this command",
            path.display()
        ),
    }
}

fn prune_old_files(dir: &Path) {
    let failures = retention::prune(dir, LOG_FILE_PREFIX, LOG_FILE_SUFFIX, KEPT_LOG_FILES);
    for (path, error) in failures {
        tracing::debug!("could not remove old log file {}: {error}", path.display());
    }
}

/// Log panics to the file; the previous hook still prints them.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: PANIC_TARGET, "{info}");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_wins_over_verbose_on_the_terminal() {
        assert_eq!(
            terminal_directives(Some("atomic_remote=trace".into()), true),
            "atomic_remote=trace,atomic::panic=off"
        );
        assert_eq!(
            terminal_directives(None, true),
            "atomic=debug,atomic_core=info,\
             atomic::logging::command=warn,atomic::git::import=warn,\
             h2=off,hyper=off,hyper_util=off,atomic::panic=off"
        );
        assert_eq!(
            terminal_directives(None, false),
            "warn,h2=off,hyper=off,hyper_util=off,atomic::panic=off"
        );
        assert_eq!(
            terminal_directives(Some("  ".into()), false),
            terminal_directives(None, false)
        );
    }

    #[test]
    fn file_only_targets_do_not_prefix_real_modules() {
        let modules = [
            "atomic::commands::git::parallel",
            "atomic::logging",
            "atomic_core::output",
        ];
        for directive in FILE_ONLY_TARGETS.split(',') {
            let target = directive.split('=').next().unwrap();
            for module in modules {
                assert!(!module.starts_with(target), "{target} prefixes {module}");
            }
        }
    }

    #[test]
    fn an_unusable_rust_log_still_shows_errors() {
        for rust_log in ["atomic=loud", ",", " , "] {
            assert_eq!(
                terminal_directives(Some(rust_log.into()), true),
                "error,atomic::panic=off",
                "{rust_log:?}"
            );
        }
    }

    #[test]
    fn atomic_log_sets_or_disables_the_file_filter() {
        assert_eq!(file_directives(None).as_deref(), Some(FILE_FILTER));
        assert_eq!(
            file_directives(Some("".into())).as_deref(),
            Some(FILE_FILTER)
        );
        assert_eq!(file_directives(Some("off".into())), None);
        assert_eq!(file_directives(Some(" OFF ".into())), None);
        assert_eq!(
            file_directives(Some("atomic::git::import=debug".into())).as_deref(),
            Some("atomic::git::import=debug")
        );
    }

    #[test]
    fn atomic_log_dir_wins_over_the_config_dir() {
        let config = Some(PathBuf::from("/home/me/.atomic"));
        assert_eq!(
            log_dir(Some("/tmp/logs".into()), config.clone()),
            Some(LogDir::Chosen(PathBuf::from("/tmp/logs")))
        );
        assert_eq!(
            log_dir(None, config.clone()),
            Some(LogDir::Default(PathBuf::from("/home/me/.atomic/logs")))
        );
        assert_eq!(
            log_dir(Some("".into()), config),
            Some(LogDir::Default(PathBuf::from("/home/me/.atomic/logs")))
        );
        assert_eq!(log_dir(None, Some(PathBuf::new())), None);
        assert_eq!(log_dir(None, None), None);
    }

    #[test]
    fn a_relative_log_dir_is_refused() {
        let error = open_file(Path::new("logs")).unwrap_err();
        assert!(matches!(error, OpenError::Relative), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_log_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("new").join("logs");
        open_file(&dir).unwrap();
        for created in [home.path().join("new"), dir] {
            let mode = std::fs::metadata(&created).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", created.display());
        }
    }

    #[test]
    fn command_path_names_every_subcommand_level() {
        let cli = clap::Command::new("atomic").subcommand(
            clap::Command::new("git")
                .subcommand(clap::Command::new("import").arg(clap::Arg::new("branch"))),
        );
        let matches = cli
            .clone()
            .try_get_matches_from(["atomic", "git", "import", "main"])
            .unwrap();
        assert_eq!(command_path(&matches), "git import");
        let bare = cli.try_get_matches_from(["atomic"]).unwrap();
        assert_eq!(command_path(&bare), "");
    }
}
