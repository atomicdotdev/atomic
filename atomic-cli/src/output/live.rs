//! Scoped terminal-output capture for an import visual. Full diagnostic events
//! still go to the existing logger; important console messages are replayed.

use std::collections::VecDeque;
use std::sync::Mutex;

static ACTIVE: Mutex<Option<State>> = Mutex::new(None);
const MAX_DIAGNOSTICS: usize = 64;

#[derive(Clone, Copy)]
pub(crate) enum Kind {
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

pub(crate) fn is_active() -> bool {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub(crate) fn latest() -> String {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|state| state.latest.clone())
        .unwrap_or_default()
}

pub(crate) fn capture(kind: Kind, message: &str) -> bool {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = active.as_mut() {
        state.capture(kind, message);
        true
    } else {
        false
    }
}

pub(crate) struct LiveOutput;

impl LiveOutput {
    pub(crate) fn start() -> Option<Self> {
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
