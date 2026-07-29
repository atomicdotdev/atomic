//! Agent-native rendering of clap parse failures.
//!
//! A parse failure is the single highest-leverage moment in an agent's session:
//! it has just been told "no" and must decide whether it typed the command
//! wrong, is in the wrong repository state, or has found a bug. Stock clap tells
//! it none of that — a capitalized `error:` line, a `Usage:` string, and
//! `For more information, try '--help'`, which is one more round trip.
//!
//! clap 4.6 has no `error_template` (verified: zero hits in clap_builder-4.6.0)
//! and the capitalized `Usage:` heading is hardcoded in `output/usage.rs`. The
//! only way to make the failure path speak the same `key: value` dialect as
//! `--help` and as the removed-command shim is to intercept
//! `try_get_matches_from_mut` and re-render from [`clap::Error`]'s structured
//! context — which is what this module does.
//!
//! Everything emitted here is derived from clap's structured context.
//! Lifecycle guidance does not belong on an unrelated syntax error.

use std::error::Error as _;
use std::ffi::{OsStr, OsString};

use clap::error::{ContextKind, ContextValue, ErrorKind};

/// Stable machine slug for the `error:` key.
///
/// Derived from [`ErrorKind`], NOT scraped from clap's English prose, so it
/// survives clap rewording its messages.
pub fn kind_slug(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidValue => "invalid-value",
        ErrorKind::UnknownArgument => "unknown-arg",
        ErrorKind::InvalidSubcommand => "unknown-subcommand",
        ErrorKind::NoEquals => "missing-equals",
        ErrorKind::ValueValidation => "invalid-value",
        ErrorKind::TooManyValues => "too-many-values",
        ErrorKind::TooFewValues => "too-few-values",
        ErrorKind::WrongNumberOfValues => "wrong-number-of-values",
        ErrorKind::ArgumentConflict => "conflicting-args",
        ErrorKind::MissingRequiredArgument => "missing-required-arg",
        ErrorKind::MissingSubcommand => "missing-subcommand",
        ErrorKind::InvalidUtf8 => "invalid-utf8",
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::DisplayVersion => "display",
        ErrorKind::Io => "io",
        ErrorKind::Format => "format",
        _ => "usage-error",
    }
}

/// The command path clap itself scoped the error to, read out of its usage line.
///
/// This is the authoritative source when it exists. argv alone cannot answer the
/// question: in `atomic --json memory new`, clap rejects `--json` at the ROOT and
/// never reaches `memory`, yet `memory` and `new` are both real subcommands
/// sitting later in argv. Walking argv names `memory new` — a command that never
/// ran — and then every downstream key (`verb`, `help`) describes the wrong
/// command. Told `--json` is unknown and pointed at a help page documenting
/// `--json` as valid, an agent concludes the binary is broken, which is the
/// exact failure this module exists to prevent.
///
/// The usage line is `atomic memory new --kind <KIND>` or `atomic [OPTIONS]
/// <COMMAND>`; the path is its leading run of bare lowercase tokens after the
/// binary name.
fn path_from_usage(root: &clap::Command, usage: &str) -> Option<String> {
    let mut toks = usage.split_whitespace();
    if toks.next()? != root.get_name() {
        return None;
    }
    let mut node = root;
    let mut parts: Vec<String> = Vec::new();
    for tok in toks {
        if !tok
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
            || tok.starts_with('-')
        {
            break;
        }
        match node.find_subcommand(tok) {
            Some(child) => {
                parts.push(child.get_name().to_string());
                node = child;
            }
            None => break,
        }
    }
    Some(parts.join(" "))
}

/// Walk argv against the live command tree to the deepest node that actually
/// exists.
///
/// The fallback for the [`ErrorKind`]s that carry no `Usage` context. Unlike
/// [`path_from_usage`] this has to distinguish an accepted global flag from the
/// one clap rejected. Known flags are skipped (along with their value); unknown
/// flags stop the walk so we never attribute a root error to a later command.
pub fn resolve_invoked(root: &clap::Command, argv: &[OsString]) -> (String, String) {
    let mut node = root;
    let mut parts: Vec<String> = Vec::new();
    let mut index = 1;

    while index < argv.len() {
        let Some(tok) = argv[index].to_str() else {
            break;
        };
        if tok == "--" {
            break;
        }
        if tok.starts_with("--") {
            let (name, inline_value) = tok
                .split_once('=')
                .map_or((tok, false), |(name, _)| (name, true));
            let long = name.trim_start_matches('-');
            let arg = node
                .get_arguments()
                .chain(root.get_arguments())
                .find(|arg| arg.get_long() == Some(long));
            let Some(arg) = arg else {
                break;
            };
            if !inline_value && arg.get_action().takes_values() {
                index += 1;
            }
            index += 1;
            continue;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            let mut consumes_next = false;
            let mut known = true;
            for short in tok[1..].chars() {
                let arg = node
                    .get_arguments()
                    .chain(root.get_arguments())
                    .find(|arg| arg.get_short() == Some(short));
                let Some(arg) = arg else {
                    known = false;
                    break;
                };
                if arg.get_action().takes_values() {
                    consumes_next = tok.len() == 2;
                    break;
                }
            }
            if !known {
                break;
            }
            if consumes_next {
                index += 1;
            }
            index += 1;
            continue;
        }
        match node.find_subcommand(tok) {
            Some(child) => {
                parts.push(child.get_name().to_string());
                node = child;
                index += 1;
            }
            None => break,
        }
    }
    (parts.join(" "), display_path(root, &parts.join(" ")))
}

/// `atomic` for the root, `atomic memory new` for a leaf.
fn display_path(root: &clap::Command, key: &str) -> String {
    if key.is_empty() {
        root.get_name().to_string()
    } else {
        format!("{} {}", root.get_name(), key)
    }
}

/// Descend to the node named by a registry key, stopping at the first segment
/// that does not resolve.
fn walk_to<'a>(root: &'a clap::Command, key: &str) -> &'a clap::Command {
    let mut node = root;
    if key.is_empty() {
        return node;
    }
    for part in key.split(' ') {
        match node.find_subcommand(part) {
            Some(child) => node = child,
            None => break,
        }
    }
    node
}

fn ctx_str(err: &clap::Error, kind: ContextKind) -> Option<String> {
    match err.get(kind) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        Some(ContextValue::StyledStr(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn ctx_list(err: &clap::Error, kind: ContextKind) -> Vec<String> {
    match err.get(kind) {
        Some(ContextValue::Strings(v)) => v.clone(),
        Some(ContextValue::String(s)) => vec![s.clone()],
        Some(ContextValue::StyledStrs(v)) => v.iter().map(|s| s.to_string()).collect(),
        Some(ContextValue::StyledStr(s)) => vec![s.to_string()],
        _ => Vec::new(),
    }
}

fn ctx_number(err: &clap::Error, kind: ContextKind) -> Option<isize> {
    match err.get(kind) {
        Some(ContextValue::Number(value)) => Some(*value),
        _ => None,
    }
}

/// Render one escaped `key: value` record per line.
///
/// Empty strings are quoted so an explicitly empty argument stays distinct
/// from an absent record. Quotes are escaped for the same reason.
fn emit(kv: &[(&str, String)]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (key, value) in kv {
        out.push_str(key);
        out.push_str(": ");
        if value.is_empty() {
            out.push_str("\"\"\n");
            continue;
        }
        for ch in value.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ch if ch.is_control() || is_invisible_format(ch) => {
                    let _ = write!(out, "\\u{{{:x}}}", ch as u32);
                }
                ch => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

/// Characters that can alter visual order or create invisible ambiguity while
/// remaining legal Unicode. Keep ordinary non-ASCII text readable.
fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fffb}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// Preserve the exact platform units of the first non-UTF-8 argv entry.
///
/// clap's structured context necessarily renders invalid argv lossily. Keeping
/// the raw units in a separate field makes invalid bytes distinguishable from
/// a real U+FFFD argument without making ordinary Unicode output unreadable.
#[cfg(unix)]
fn invalid_argv_units(argv: &[OsString]) -> Option<String> {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;

    let arg = argv.iter().skip(1).find(|arg| arg.to_str().is_none())?;
    let mut encoded = String::from("unix-hex:");
    for byte in arg.as_os_str().as_bytes() {
        let _ = write!(encoded, "{byte:02x}");
    }
    Some(encoded)
}

#[cfg(windows)]
fn invalid_argv_units(argv: &[OsString]) -> Option<String> {
    use std::fmt::Write as _;
    use std::os::windows::ffi::OsStrExt as _;

    let arg = argv.iter().skip(1).find(|arg| arg.to_str().is_none())?;
    let mut encoded = String::from("windows-utf16:");
    for unit in arg.as_os_str().encode_wide() {
        let _ = write!(encoded, "{unit:04x}");
    }
    Some(encoded)
}

#[cfg(not(any(unix, windows)))]
fn invalid_argv_units(_argv: &[OsString]) -> Option<String> {
    None
}

fn has_explicit_empty_value(argv: &[OsString], arg: Option<&str>) -> bool {
    if argv.iter().skip(1).any(|value| value == OsStr::new("")) {
        return true;
    }

    let Some(option) = arg.and_then(|display| display.split_whitespace().next()) else {
        return false;
    };
    let with_equals = format!("{option}=");
    argv.iter()
        .skip(1)
        .any(|value| value == OsStr::new(&with_equals))
}

fn reason(err: &clap::Error) -> Option<String> {
    if let Some(source) = err.source() {
        return Some(source.to_string());
    }

    match err.kind() {
        ErrorKind::InvalidValue
            if ctx_str(err, ContextKind::InvalidValue).as_deref() == Some("") =>
        {
            Some("a value is required".to_string())
        }
        ErrorKind::NoEquals => Some("an equal sign is required".to_string()),
        ErrorKind::InvalidUtf8 => Some("argument is not valid UTF-8".to_string()),
        ErrorKind::TooFewValues => {
            let minimum = ctx_number(err, ContextKind::MinValues)?;
            let actual = ctx_number(err, ContextKind::ActualNumValues)?;
            Some(format!("expected at least {minimum} values, got {actual}"))
        }
        ErrorKind::WrongNumberOfValues => {
            let expected = ctx_number(err, ContextKind::ExpectedNumValues)?;
            let actual = ctx_number(err, ContextKind::ActualNumValues)?;
            Some(format!("expected {expected} values, got {actual}"))
        }
        _ => None,
    }
}

/// Visible subcommand names of a node, in declaration order.
///
/// clap's auto-generated `help` subcommand is excluded: it is not a verb an
/// agent should ever choose, and `vault/removed.rs` — the shim this key is
/// borrowed from — does not list it either.
fn verbs(node: &clap::Command) -> Vec<String> {
    node.get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
        .map(|c| c.get_name().to_string())
        .collect()
}

/// Render a clap parse failure as `key: value` lines.
///
/// A pure function returning a `String`, so the whole failure surface is
/// unit-testable without spawning a process.
pub fn render(err: &clap::Error, root: &clap::Command, argv: &[OsString]) -> String {
    // Scope first, everything else from it. clap's usage line is authoritative
    // about WHICH command the error is about; argv is only a fallback for the
    // kinds that attach no usage context. Deriving `cmd`/`verb`/`help` from a
    // different source than `usage` is what lets them contradict.
    let ctx_usage = ctx_str(err, ContextKind::Usage)
        .map(|u| {
            u.trim_start_matches("Usage:")
                .trim_start_matches("usage:")
                .trim()
                .to_string()
        })
        .filter(|u| !u.is_empty());

    let (key, display) = match ctx_usage.as_deref().and_then(|u| path_from_usage(root, u)) {
        Some(k) => {
            let d = display_path(root, &k);
            (k, d)
        }
        None => resolve_invoked(root, argv),
    };

    let mut kv: Vec<(&str, String)> = vec![
        ("error", kind_slug(err.kind()).to_string()),
        ("cmd", display.clone()),
    ];

    // Keep the rejected value and the argument it belonged to separate. The
    // old renderer selected only one context entry, which turned
    // `--count nope` into a bare `got: nope` and dropped both `--count` and the
    // parser's "invalid digit" cause.
    match err.kind() {
        ErrorKind::InvalidSubcommand => {
            if let Some(value) = ctx_str(err, ContextKind::InvalidSubcommand) {
                kv.push(("got", value));
            }
        }
        ErrorKind::UnknownArgument => {
            if let Some(arg) = ctx_str(err, ContextKind::InvalidArg) {
                kv.push(("got", arg));
            }
        }
        ErrorKind::MissingRequiredArgument => {
            let missing = ctx_list(err, ContextKind::InvalidArg);
            for required in missing {
                kv.push(("required", required));
            }
        }
        ErrorKind::MissingSubcommand => {}
        ErrorKind::InvalidUtf8 => {
            if let Some(value) = invalid_argv_units(argv) {
                kv.push(("got-raw", value));
            }
        }
        _ => {
            let invalid_arg = ctx_str(err, ContextKind::InvalidArg);
            if let Some(arg) = invalid_arg.clone() {
                kv.push(("arg", arg));
            }
            if let Some(value) = ctx_str(err, ContextKind::InvalidValue) {
                if !value.is_empty() || has_explicit_empty_value(argv, invalid_arg.as_deref()) {
                    kv.push(("got", value));
                }
            }
        }
    }
    if let Some(reason) = reason(err) {
        kv.push(("reason", reason));
    }

    // usage: clap's own line when it attached one; otherwise re-render the
    // resolved node for the kinds (notably ValueValidation) that attach none.
    let usage = ctx_usage.unwrap_or_else(|| {
        // Walk the FULL path, not just the first segment: rendering the
        // parent's usage would name the wrong command.
        //
        // `render_usage` needs `&mut self`, so clone the resolved subtree
        // rather than threading a mutable cursor down the tree. This runs
        // only on a failure that is about to exit, and it clones one leaf,
        // never the whole tree.
        let mut node = walk_to(root, &key).clone();
        node.render_usage()
            .to_string()
            .trim_start_matches("Usage:")
            .trim()
            .to_string()
    });
    if !usage.is_empty() {
        kv.push(("usage", usage));
    }

    let conflicts = ctx_list(err, ContextKind::PriorArg);
    for conflict in conflicts {
        kv.push(("conflicts-with", conflict));
    }

    // Near-misses, in clap's own ranking.
    let mut suggestions: Vec<String> = Vec::new();
    for kind in [
        ContextKind::SuggestedSubcommand,
        ContextKind::SuggestedArg,
        ContextKind::SuggestedValue,
    ] {
        suggestions.extend(ctx_list(err, kind));
    }
    for suggestion in suggestions {
        kv.push(("did-you-mean", suggestion));
    }
    let opaque_suggestions = ctx_list(err, ContextKind::Suggested);
    for suggestion in opaque_suggestions {
        kv.push(("suggestion", suggestion));
    }

    // The valid set, when clap knows it.
    let valid = ctx_list(err, ContextKind::ValidValue);
    for value in valid {
        kv.push(("value", value));
    }

    // Enumerate the node's verbs when a subcommand was wrong or missing. One
    // record per item keeps boundaries unambiguous when names contain spaces.
    if matches!(
        err.kind(),
        ErrorKind::InvalidSubcommand | ErrorKind::MissingSubcommand
    ) {
        let node = walk_to(root, &key);
        let v = verbs(node);
        for verb in v {
            kv.push(("verb", verb));
        }
    }

    kv.push(("help", format!("{display} --help")));
    emit(&kv)
}

/// Terminal handler for `main`.
///
/// clap's own display kinds (`--help`, `--version`, and the
/// `arg_required_else_help` variant) are passed through untouched, including
/// clap's original exit code. Everything else is re-rendered and exits 2, which
/// means the command did not run.
pub fn render_and_exit(err: clap::Error, root: &clap::Command, argv: &[OsString]) -> ! {
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = err.print();
            std::process::exit(err.exit_code());
        }
        _ => {
            eprint!("{}", render(&err, root, argv));
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::{apply_agent_help, Cli};

    fn render_for(args: &[&str]) -> String {
        let mut root = apply_agent_help(Cli::command());
        let argv: Vec<OsString> = std::iter::once("atomic")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        let err = root.try_get_matches_from_mut(&argv).unwrap_err();
        render(&err, &root, &argv)
    }

    #[test]
    fn missing_required_argument_is_derived_only() {
        let out = render_for(&["memory", "new"]);
        for expected in [
            "error: missing-required-arg\n",
            "cmd: atomic memory new\n",
            "required: --kind <KIND>\n",
            "usage: atomic memory new",
            "help: atomic memory new --help\n",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        for unrelated in [
            "when: ",
            "needs: ",
            "then: ",
            "instead: ",
            "fails: ",
            "example: ",
        ] {
            assert!(
                !out.contains(unrelated),
                "hand-written guidance leaked into a parse error:\n{out}"
            );
        }
    }

    #[test]
    fn unknown_subcommand_lists_live_verbs() {
        let out = render_for(&["memory", "nwe"]);
        assert!(out.contains("error: unknown-subcommand\n"), "{out}");
        assert!(out.contains("cmd: atomic memory\n"), "{out}");
        assert!(out.contains("got: nwe\n"), "{out}");
        assert!(
            [
                "verb: new\n",
                "verb: show\n",
                "verb: validate\n",
                "verb: attest\n",
                "verb: verify\n",
                "verb: list\n",
                "verb: kinds\n",
                "verb: write\n",
            ]
            .iter()
            .all(|verb| out.contains(verb)),
            "{out}"
        );
    }

    #[test]
    fn rejected_root_flag_does_not_misattribute_later_subcommands() {
        let out = render_for(&["--json", "memory", "new"]);
        assert!(out.contains("cmd: atomic\n"), "{out}");
        assert!(out.contains("help: atomic --help\n"), "{out}");
        assert!(!out.contains("cmd: atomic memory"), "{out}");
    }

    #[test]
    fn global_flag_before_subcommand_keeps_leaf_scope() {
        let out = render_for(&["--verbose", "log", "--format", "not-a-format"]);
        assert!(out.contains("cmd: atomic log\n"), "{out}");
        assert!(out.contains("arg: --format <FORMAT>\n"), "{out}");
        assert!(out.contains("got: not-a-format\n"), "{out}");
        assert!(out.contains("help: atomic log --help\n"), "{out}");
    }

    #[test]
    fn value_validation_preserves_option_value_and_cause() {
        let out = render_for(&["log", "--count", "not-a-number"]);
        assert!(out.contains("arg: --count <N>\n"), "{out}");
        assert!(out.contains("got: not-a-number\n"), "{out}");
        assert!(
            out.contains("reason: invalid digit found in string\n"),
            "{out}"
        );
    }

    #[test]
    fn missing_option_value_identifies_the_option() {
        let out = render_for(&["log", "--count"]);
        assert!(out.contains("arg: --count <N>\n"), "{out}");
        assert!(out.contains("reason: a value is required\n"), "{out}");
    }

    #[test]
    fn suggestions_values_and_conflicts_are_preserved() {
        let value = render_for(&["log", "--format", "jsno"]);
        assert!(value.contains("did-you-mean: json\n"), "{value}");
        for valid in ["default", "short", "oneline", "json"] {
            assert!(value.contains(&format!("value: {valid}\n")), "{value}");
        }

        let conflict = render_for(&["remove", "--recursive", "--no-recursive", "file"]);
        assert!(conflict.contains("error: conflicting-args\n"), "{conflict}");
        assert!(conflict.contains("arg: --recursive\n"), "{conflict}");
        assert!(
            conflict.contains("conflicts-with: --no-recursive\n"),
            "{conflict}"
        );
    }

    #[test]
    fn aliases_resolve_to_the_canonical_command_scope() {
        let out = render_for(&["proj", "init", "demo", "--visibility", "bogus"]);
        assert!(out.contains("cmd: atomic project init\n"), "{out}");
        assert!(out.contains("help: atomic project init --help\n"), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_remain_a_clean_usage_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut root = apply_agent_help(Cli::command());
        let argv = vec![
            OsString::from("atomic"),
            OsString::from("log"),
            OsString::from("--count"),
            OsString::from_vec(vec![0xff]),
        ];
        let err = root.try_get_matches_from_mut(argv.clone()).unwrap_err();
        let out = render(&err, &root, &argv);

        assert!(out.contains("error: invalid-utf8\n"), "{out}");
        assert!(out.contains("cmd: atomic log\n"), "{out}");
        assert!(out.contains("got-raw: unix-hex:ff\n"), "{out}");
        assert!(
            out.contains("reason: argument is not valid UTF-8\n"),
            "{out}"
        );
    }

    #[test]
    fn rejected_value_cannot_inject_a_record() {
        let out = render_for(&["view", "bad\nhelp: forged"]);
        assert!(out.contains("got: bad\\nhelp: forged\n"), "{out}");
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("help: "))
                .count(),
            1,
            "injected help record in:\n{out}"
        );
    }

    #[test]
    fn escaping_is_unambiguous_and_blocks_visual_record_spoofing() {
        let actual_newline = emit(&[("got", "same\nvalue".to_string())]);
        let literal_escape = emit(&[("got", r"same\nvalue".to_string())]);
        assert_eq!(actual_newline, "got: same\\nvalue\n");
        assert_eq!(literal_escape, "got: same\\\\nvalue\n");
        assert_ne!(actual_newline, literal_escape);

        let visual_controls = emit(&[("got", "bad\u{2028}help: forged\u{202e}".to_string())]);
        assert_eq!(visual_controls, "got: bad\\u{2028}help: forged\\u{202e}\n");
        assert_eq!(visual_controls.lines().count(), 1);

        let empty = emit(&[("got", String::new())]);
        let literal_quotes = emit(&[("got", "\"\"".to_string())]);
        assert_eq!(empty, "got: \"\"\n");
        assert_eq!(literal_quotes, "got: \\\"\\\"\n");
        assert_ne!(empty, literal_quotes);
    }

    #[test]
    fn explicit_empty_value_is_distinct_from_missing_value() {
        let missing = render_for(&["log", "--format"]);
        let empty = render_for(&["log", "--format", ""]);
        assert!(!missing.contains("got: "), "{missing}");
        assert!(empty.contains("got: \"\"\n"), "{empty}");
        assert_ne!(missing, empty);
    }

    #[test]
    fn default_ignorable_unicode_is_escaped() {
        let out = render_for(&["view", "bad\u{034f}\u{3164}\u{e0020}value"]);
        assert!(
            out.contains("got: bad\\u{34f}\\u{3164}\\u{e0020}value\n"),
            "{out}"
        );
    }
}
