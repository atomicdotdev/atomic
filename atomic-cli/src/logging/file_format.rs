//! The log file's line: `time pid=N LEVEL ThreadId(N) spans: target: message`.

use std::borrow::Cow;
use std::fmt;

use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{Format, Full, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// `tracing`'s full format with the process id after the time, since many
/// processes append to the same file and some threads (tokio workers,
/// watchers) run outside the command's span. Every event is one line.
pub(super) struct FileFormat {
    pid: u32,
    inner: Format<Full, ()>,
}

impl FileFormat {
    pub(super) fn new() -> Self {
        Self {
            pid: std::process::id(),
            inner: tracing_subscriber::fmt::format()
                .without_time()
                .with_thread_ids(true),
        }
    }
}

impl<S, N> FormatEvent<S, N> for FileFormat
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
        let mut line = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut line), event)?;
        SystemTime.format_time(&mut writer)?;
        write!(writer, " pid={} ", self.pid)?;
        writeln!(writer, "{}", one_line(line.trim_end_matches('\n')))
    }
}

/// Newlines and carriage returns inside an event become `\n` and `\r`, so
/// every event is exactly one line: text from Git, such as a tag annotation,
/// cannot forge log lines, and a line is appended with a single write.
fn one_line(text: &str) -> Cow<'_, str> {
    if text.contains(['\n', '\r']) {
        Cow::Owned(text.replace('\n', "\\n").replace('\r', "\\r"))
    } else {
        Cow::Borrowed(text)
    }
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
    fn each_event_is_one_line_with_the_pid_and_its_spans() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .event_format(FileFormat::new()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!("atomic", cmd = %"git import").entered();
            tracing::info!(
                target: "atomic::git::import",
                "tag 'v1' annotation: fixed\n2026-01-01T00:00:00Z  INFO forged"
            );
        });

        let bytes = captured.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let pid = format!(" pid={}  INFO ThreadId(", std::process::id());
        assert!(text.contains(&pid), "{text}");
        assert!(
            text.ends_with(
                "atomic{cmd=git import}: atomic::git::import: tag 'v1' annotation: \
                 fixed\\n2026-01-01T00:00:00Z  INFO forged\n"
            ),
            "{text}"
        );
    }
}
