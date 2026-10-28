//! Scoped terminal-output capture for an import visual. Full diagnostic events
//! still go to the existing logger; important console messages are replayed.
//!
//! The shim visuals and an embedding CLI must share this module's state.
//! Reporters should log each event before calling [`capture`].

use std::collections::VecDeque;
use std::sync::Mutex;

static ACTIVE: Mutex<Option<State>> = Mutex::new(None);
const MAX_DIAGNOSTICS: usize = 64;

#[derive(Clone, Copy)]
/// How a notice is retained and replayed by the active visual.
pub enum Kind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Default)]
struct State {
    latest: String,
    success: Option<String>,
    diagnostics: VecDeque<(Kind, String)>,
    omitted: usize,
}

impl State {
    fn capture(&mut self, kind: Kind, message: &str) {
        self.latest = console::strip_ansi_codes(message)
            .chars()
            .take(200)
            .collect();
        match kind {
            Kind::Info => {}
            Kind::Success => self.success = Some(message.chars().take(4096).collect()),
            Kind::Warning | Kind::Error => {
                if self.diagnostics.len() == MAX_DIAGNOSTICS {
                    self.diagnostics.pop_front();
                    self.omitted += 1;
                }
                self.diagnostics
                    .push_back((kind, message.chars().take(4096).collect()));
            }
        }
    }
}

pub fn is_active() -> bool {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Latest notice for the visual, stripped of ANSI codes and bounded in length.
pub fn latest() -> String {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|state| state.latest.clone())
        .unwrap_or_default()
}

/// Capture a notice when a visual is active. A true result tells the caller
/// to suppress immediate console output, after recording its diagnostic log.
pub fn capture(kind: Kind, message: &str) -> bool {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = active.as_mut() {
        state.capture(kind, message);
        true
    } else {
        false
    }
}

/// Process-wide capture scope. Drop after restoring the terminal to replay
/// retained warnings/errors and the final success message.
pub struct LiveOutput;

impl LiveOutput {
    /// Start capture, or return `None` if another visual already owns it.
    pub fn start() -> Option<Self> {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if active.is_some() {
            return None;
        }
        *active = Some(State::default());
        Some(Self)
    }
}

impl Drop for LiveOutput {
    fn drop(&mut self) {
        let state = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(state) = state {
            for (kind, message) in state.diagnostics {
                let label = match kind {
                    Kind::Error => "Error",
                    _ => "Warning",
                };
                eprintln!("{label}: {message}");
            }
            if state.omitted > 0 {
                eprintln!("{} earlier diagnostics omitted from the live view; consult the Atomic log for full diagnostics.", state.omitted);
            }
            if let Some(message) = state.success {
                println!("{message}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_bounded_and_keeps_diagnostics_and_last_success() {
        let mut state = State::default();
        for _ in 0..100 {
            state.capture(Kind::Warning, &"x".repeat(5000));
        }
        state.capture(Kind::Success, "Imported 100 changes");
        state.capture(Kind::Info, "Finalizing...");
        assert_eq!(state.diagnostics.len(), MAX_DIAGNOSTICS);
        assert_eq!(state.omitted, 36);
        assert_eq!(state.diagnostics[0].1.len(), 4096);
        assert_eq!(state.success.as_deref(), Some("Imported 100 changes"));
        assert_eq!(state.latest, "Finalizing...");
    }
}
