//! The terminal line format `env_logger` used: `[time LEVEL target] message`.

use std::fmt;

use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// `[2026-09-28T10:00:00.000000Z WARN  atomic::commands::git] message`, with
/// the level colored when stderr takes colors. Spans are left out: this line
/// is for the person at the terminal, and the log file carries them.
pub(super) struct EnvLoggerFormat;

impl<S, N> FormatEvent<S, N> for EnvLoggerFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Records from the `log` macros carry their real level and target
        // in the normalized metadata.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        write!(writer, "[")?;
        SystemTime.format_time(&mut writer)?;
        write!(
            writer,
            " {} {}] ",
            level_label(metadata.level()),
            metadata.target()
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// The level padded to five columns, in `env_logger`'s colors.
fn level_label(level: &Level) -> console::StyledObject<String> {
    let style = match *level {
        Level::ERROR => console::Style::new().red(),
        Level::WARN => console::Style::new().yellow(),
        Level::INFO => console::Style::new().green(),
        Level::DEBUG => console::Style::new().blue(),
        Level::TRACE => console::Style::new().cyan(),
    };
    style
        .for_stderr()
        .apply_to(format!("{:<5}", level.as_str()))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lines_keep_env_loggers_shape_without_spans() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .event_format(EnvLoggerFormat),
        );
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!("atomic", cmd = "git import").entered();
            tracing::warn!(target: "atomic::commands::git", paths = 3, "disk is full");
        });

        let bytes = captured.0.lock().unwrap().clone();
        let line = console::strip_ansi_codes(std::str::from_utf8(&bytes).unwrap()).into_owned();
        assert!(line.starts_with('['), "{line}");
        assert!(
            line.ends_with(" WARN  atomic::commands::git] disk is full paths=3\n"),
            "{line}"
        );
        assert!(!line.contains("git import"), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
    }
}
