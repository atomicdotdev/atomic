//! Presentation adapter for the existing command-facing Git API.
//!
//! Embedders can route notices through their own UI and redacting logger.
//! Direct command output (tables, progress, JSON) retains the CLI contract.
pub mod live;

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub enum Notice {
    Success,
    Info,
    Warning,
    Hint,
}

static REPORTER: OnceLock<fn(Notice, &str)> = OnceLock::new();

/// Install the process-wide notice renderer before invoking commands.
/// Returns false if a renderer has already been installed.
pub fn set_reporter(reporter: fn(Notice, &str)) -> bool {
    REPORTER.set(reporter).is_ok()
}

fn report(kind: Notice, message: &str) {
    if let Some(reporter) = REPORTER.get() {
        reporter(kind, message);
        return;
    }
    // A reporter owns logging and capture. Only use the fallback capture
    // when no reporter is installed, so logging still sees every notice.
    let capture_kind = match kind {
        Notice::Success => live::Kind::Success,
        Notice::Info | Notice::Hint => live::Kind::Info,
        Notice::Warning => live::Kind::Warning,
    };
    if live::capture(capture_kind, message) {
        return;
    }
    match kind {
        Notice::Success => println!(
            "{} {}",
            console::style("✓").green(),
            console::style(message).green()
        ),
        Notice::Info => println!(
            "{} {}",
            console::style("ℹ").cyan(),
            console::style(message).cyan()
        ),
        Notice::Warning => eprintln!(
            "{} {}",
            console::style("⚠").yellow(),
            console::style(message).yellow()
        ),
        Notice::Hint => println!("{}", console::style(message).dim()),
    }
}

pub(crate) fn print_success(message: &str) {
    report(Notice::Success, message);
}
pub(crate) fn print_info(message: &str) {
    report(Notice::Info, message);
}
pub(crate) fn print_warning(message: &str) {
    report(Notice::Warning, message);
}
pub(crate) fn print_hint(message: &str) {
    report(Notice::Hint, message);
}
pub(crate) fn emphasis<D: std::fmt::Display>(text: D) -> console::StyledObject<D> {
    console::style(text).bold()
}
