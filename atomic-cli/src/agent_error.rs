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
//! Everything emitted here is derived: the slug from [`ErrorKind`], the path
//! from argv walked against the live tree, the usage from clap. The only
//! hand-written content is the [`crate::agent_doc`] row, appended verbatim.

use clap::error::{ContextKind, ContextValue, ErrorKind};

use crate::agent_doc;
use crate::emit::emit;

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
/// ran — and then every downstream key (`verbs`, the [`agent_doc`] row, `help`)
/// describes the wrong command. Told `--json` is unknown and pointed at a help
/// page documenting `--json` as valid, an agent concludes the binary is broken,
/// which is the exact failure this module exists to prevent.
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
/// [`path_from_usage`] this cannot tell an accepted global flag from the one
/// clap rejected, so the walk STOPS at the first flag rather than stepping over
/// it: naming the parent is recoverable, naming a command that never ran is not.
pub fn resolve_invoked(root: &clap::Command, argv: &[String]) -> (String, String) {
    let mut node = root;
    let mut parts: Vec<String> = Vec::new();
    for tok in argv.iter().skip(1) {
        if tok.starts_with('-') {
            break;
        }
        match node.find_subcommand(tok.as_str()) {
            Some(child) => {
                parts.push(child.get_name().to_string());
                node = child;
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

/// The `error_lines` keys are a closed set; interning them keeps [`emit`]'s
/// signature `&[(&str, String)]` without allocating key strings.
fn intern(key: &str) -> &'static str {
    match key {
        "when" => "when",
        "needs" => "needs",
        "run" => "run",
        "then" => "then",
        "instead" => "instead",
        "fails" => "fails",
        _ => "note",
    }
}

/// Render a clap parse failure as `key: value` lines.
///
/// A pure function returning a `String`, so the whole failure surface is
/// unit-testable without spawning a process.
pub fn render(err: &clap::Error, root: &clap::Command, argv: &[String]) -> String {
    // Scope first, everything else from it. clap's usage line is authoritative
    // about WHICH command the error is about; argv is only a fallback for the
    // kinds that attach no usage context. Deriving `cmd`/`verbs`/the row/`help`
    // from a different source than `usage` is what let them contradict.
    let ctx_usage = ctx_str(err, ContextKind::Usage)
        .map(|u| {
            u.trim_start_matches("Usage:")
                .trim_start_matches("usage:")
                .trim()
                .to_string()
        })
        .filter(|u| !u.is_empty());

    let (key, display) = match ctx_usage
        .as_deref()
        .and_then(|u| path_from_usage(root, u))
    {
        Some(k) => {
            let d = display_path(root, &k);
            (k, d)
        }
        None => resolve_invoked(root, argv),
    };

    let mut kv: Vec<(&str, String)> = Vec::new();
    kv.push(("error", kind_slug(err.kind()).to_string()));
    kv.push(("cmd", display.clone()));

    // usage: clap's own line when it attached one; otherwise re-render the
    // resolved node for the kinds (notably InvalidValue) that attach none.
    let usage = ctx_usage
        .unwrap_or_else(|| {
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

    // What was actually typed that got rejected.
    for kind in [
        ContextKind::InvalidSubcommand,
        ContextKind::InvalidValue,
        ContextKind::InvalidArg,
    ] {
        if let Some(v) = ctx_str(err, kind) {
            kv.push(("got", v));
            break;
        }
    }
    if err.kind() == ErrorKind::MissingRequiredArgument {
        let missing = ctx_list(err, ContextKind::InvalidArg);
        if !missing.is_empty() {
            kv.push(("required", missing.join(" ")));
        }
    }
    if let Some(prior) = ctx_str(err, ContextKind::PriorArg) {
        kv.push(("conflicts-with", prior));
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
    if !suggestions.is_empty() {
        kv.push(("did-you-mean", suggestions.join(" ")));
    }

    // The valid set, when clap knows it.
    let valid = ctx_list(err, ContextKind::ValidValue);
    if !valid.is_empty() {
        kv.push(("values", valid.join(" ")));
    }

    // Enumerate the node's verbs when a subcommand was wrong or missing — the
    // same `verbs:` key `vault/removed.rs` already emits.
    if matches!(
        err.kind(),
        ErrorKind::InvalidSubcommand | ErrorKind::MissingSubcommand
    ) {
        let node = walk_to(root, &key);
        let v = verbs(node);
        if !v.is_empty() {
            kv.push(("verbs", v.join(" ")));
        }
    }

    // The registry payload: the trigger, the prerequisites, the working
    // invocation and the known failure modes, AT the point of failure, so the
    // agent needs no second round trip and has no reason to suspect the binary.
    if let Some(doc) = agent_doc::lookup(&key) {
        for line in doc.error_lines() {
            match line.split_once(": ") {
                Some((k, v)) => kv.push((intern(k), v.to_string())),
                None => kv.push(("note", line)),
            }
        }
    }

    kv.push(("help", format!("{display} --help")));
    emit(&kv)
}

/// Terminal handler for `main`.
///
/// clap's own display kinds (`--help`, `--version`, and the
/// `arg_required_else_help` variant) are passed through untouched: they already
/// flow through `AGENT_HELP_TEMPLATE` and are successes, not failures.
/// Everything else is re-rendered and exits 2 — a usage error, which in the root
/// taxonomy means "the command did NOT run".
pub fn render_and_exit(err: clap::Error, root: &clap::Command) -> ! {
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = err.print();
            std::process::exit(err.exit_code());
        }
        _ => {
            // `std::env::args()` panics on non-UTF-8 argv. main.rs hands clap
            // `args_os()`, so the failure path must be just as tolerant —
            // otherwise a stray byte turns a clean exit-2 usage error into a
            // panic (exit 101), which is not in EXIT_CODES and would make
            // `ErrorKind::InvalidUtf8` unreachable.
            let argv: Vec<String> = std::env::args_os()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            eprint!("{}", render(&err, root, &argv));
            std::process::exit(2);
        }
    }
}
