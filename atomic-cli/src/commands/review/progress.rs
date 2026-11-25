use indicatif::ProgressBar;
use std::io::IsTerminal;

pub(super) struct Thinking(Option<ProgressBar>);

fn enabled(json: bool, stdout_terminal: bool, stderr_terminal: bool, dumb: bool) -> bool {
    !json && stdout_terminal && stderr_terminal && !dumb
}

impl Thinking {
    pub(super) fn start(json: bool) -> Self {
        Self(
            enabled(
                json,
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
                std::env::var("TERM").is_ok_and(|term| term == "dumb"),
            )
            .then(|| {
                crate::output::progress::create_spinner(
                    "Analyzing recorded syntax, semantic relationships and metrics...",
                )
            }),
        )
    }
}

impl Drop for Thinking {
    fn drop(&mut self) {
        if let Some(spinner) = &self.0 {
            spinner.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_is_only_for_interactive_human_output() {
        assert!(enabled(false, true, true, false));
        for args in [
            (true, true, true, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
        ] {
            assert!(!enabled(args.0, args.1, args.2, args.3));
        }
    }

    #[test]
    fn guard_clears_the_spinner_on_success_and_error_paths() {
        for fail in [false, true] {
            let spinner = ProgressBar::hidden();
            let result = (|| -> Result<(), ()> {
                let _thinking = Thinking(Some(spinner.clone()));
                if fail {
                    return Err(());
                }
                Ok(())
            })();
            assert_eq!(result.is_err(), fail);
            assert!(spinner.is_finished());
        }
    }
}
