//! The single place that decides what a `key: value` line looks like.
//!
//! Atomic's machine-facing output speaks one dialect: one datum per line,
//! `key: value`, no color, no emoji, no prose. Three call sites emit it — the
//! removed-command shim ([`crate::commands::vault::removed`]), the injected
//! `--help` block ([`crate::agent_doc`]), and the parse-failure renderer
//! ([`crate::agent_error`]) — and the benefit evaporates the moment they drift.
//! So there is exactly one function that renders it, and every one of them
//! calls this.
//!
//! Deliberately NOT part of [`crate::output`]: that module is the human
//! surface (color, tables, progress bars, terminal detection). This is the
//! agent surface, and its whole value is that it is styling-free.

/// Render `key: value` data.
///
/// One datum per line, `": "` separator, trailing newline on every line.
/// Pairs whose value is empty are dropped rather than emitted as a bare key —
/// an empty value carries no information and a parser would have to special-case
/// it.
pub fn emit(kv: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (key, value) in kv {
        if value.is_empty() {
            continue;
        }
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// [`emit`] straight to stderr.
///
/// stderr, not stdout: a command's stdout is its result (often `--json`), and a
/// diagnostic must never pollute it. Mirrors the same rule
/// [`crate::main`]'s error handler follows for hints.
pub fn emit_stderr(kv: &[(&str, String)]) {
    eprint!("{}", emit(kv));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_one_datum_per_line() {
        let out = emit(&[("error", "missing-required-arg".into()), ("cmd", "atomic memory new".into())]);
        assert_eq!(out, "error: missing-required-arg\ncmd: atomic memory new\n");
    }

    #[test]
    fn drops_empty_values() {
        let out = emit(&[("a", "1".into()), ("b", String::new()), ("c", "3".into())]);
        assert_eq!(out, "a: 1\nc: 3\n");
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert!(emit(&[]).is_empty());
    }

    /// The dialect has no styling. If an ANSI escape ever appears here, some
    /// caller has started routing colored text through the agent surface.
    #[test]
    fn output_is_never_styled() {
        let out = emit(&[("run", "atomic memory attest <id>".into())]);
        assert!(!out.contains('\u{1b}'), "emit must never style: {out:?}");
    }
}
