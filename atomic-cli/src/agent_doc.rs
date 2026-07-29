//! Typed agent guidance, declared beside the command it describes.
//!
//! Atomic's `--help` is read by an orchestrating agent far more often than by a
//! person. An agent that mis-invokes a command needs three things the stock
//! output does not give it: *should I be running this at all* (`when:`), *what
//! exactly do I type* (`run:`), and — at the moment of failure — *what I must do
//! first, what to do next, and which sibling I actually wanted*.
//!
//! One consumer: [`Doc::error_lines`], rendered by [`crate::agent_error`] onto
//! the clap parse-failure path.
//!
//! Deliberately NOT `--help`. The template's own doc comment states the
//! position — the terse one-line summary is all an agent needs, and prose
//! belongs on the docs website — and the incident that motivated this module
//! agrees: the agent had already read `--help` on three commands before it
//! resorted to dumping `strings` of the binary. Help was not the channel that
//! failed.
//!
//! The KEY is central ([`DOCS`], one line per command, checked by
//! `agent_guards::every_key_resolves`); the CONTENT is a `const` declared beside
//! the command struct it describes, so prose lives where the maintainer editing
//! that command will see it.

/// Budget for a failure line.
///
/// Failure output is written straight to stderr by [`crate::agent_error`],
/// never through clap's help pipeline, so it is not wrapped at any width and
/// cannot produce an orphan continuation. This cap is hygiene, not correctness.
pub const MAX_ERROR_LINE: usize = 100;

/// A typed cross-reference to another command.
///
/// `cmd` is a bare command path with NO `atomic` prefix — the prefix is added at
/// render time — so a reference slot is structurally incapable of holding prose.
/// That is what makes `agent_guards::every_ref_resolves` possible: a lexical
/// lint over free text gives both false positives (a prefix match on a retired
/// path) and false negatives (the word "atomic" inside a sentence).
#[derive(Copy, Clone, Debug)]
pub struct Ref {
    /// Bare path, optionally followed by placeholders/flags, e.g.
    /// `"memory validate <id>"`. Only the leading run of plain lowercase tokens
    /// is resolved against the command tree.
    pub cmd: &'static str,
    /// Optional parenthetical rendered after the path. Never resolved.
    pub note: &'static str,
}

impl Ref {
    /// The empty slot, for rows that have nothing to say in a category.
    pub const NONE: &'static [Ref] = &[];

    /// A [`Fail`] with no command that fixes it.
    pub const UNFIXABLE: Ref = Ref { cmd: "", note: "" };

    /// The resolvable prefix: leading plain-lowercase tokens, stopping at the
    /// first placeholder, flag, quote, or punctuation.
    pub fn path(&self) -> Vec<&'static str> {
        self.cmd
            .split_whitespace()
            .take_while(|t| {
                // Stop at the first flag (`--dry-run`), placeholder (`<id>`),
                // quoted term, or path-shaped argument. A leading `-` must NOT
                // be treated as a path segment.
                !t.is_empty()
                    && !t.starts_with('-')
                    && t.chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
            })
            .collect()
    }

    /// `atomic <cmd>`, with the note in parentheses when there is one.
    pub fn render(&self) -> String {
        if self.note.is_empty() {
            format!("atomic {}", self.cmd)
        } else {
            format!("atomic {} ({})", self.cmd, self.note)
        }
    }
}

/// A named failure mode with its exit code and the command that fixes it.
///
/// `exit` is checked against [`EXIT_CODES`] by
/// `agent_guards::exit_codes_in_taxonomy`. Exit `0` is legal and load-bearing:
/// several of Atomic's worst agent traps are silent-wrong-answer paths that
/// succeed.
#[derive(Copy, Clone, Debug)]
pub struct Fail {
    /// The condition, in terms an agent can evaluate.
    pub cond: &'static str,
    /// The exit code observed for that condition.
    pub exit: i32,
    /// The command that resolves it, or [`Ref::UNFIXABLE`] when there is none.
    pub fix: Ref,
}

impl Fail {
    pub const NONE: &'static [Fail] = &[];
}

/// Everything hand-written about one command.
#[derive(Copy, Clone, Debug)]
pub struct Doc {
    /// The trigger: something an agent can evaluate as true/false against its
    /// current situation — NOT a restatement of `about`. Reaches `--help`.
    pub when: &'static str,
    /// One canonical, copy-pasteable invocation. Must begin with the row's own
    /// path (`agent_guards::run_invokes_own_command`). Reaches `--help` and is
    /// replayed on failure.
    pub run: &'static str,
    /// Upstream prerequisites. Failure path only.
    pub needs: &'static [Ref],
    /// Downstream follow-ups. Failure path only.
    pub then: &'static [Ref],
    /// Lateral — the sibling to reach for instead, and why. Failure path only.
    pub instead: &'static [Ref],
    /// Per-command deviations from the root taxonomy. Failure path only.
    pub fails: &'static [Fail],
}

impl Doc {
    /// Base for struct-update syntax, so a row only names what it has.
    pub const EMPTY: Doc = Doc {
        when: "",
        run: "",
        needs: Ref::NONE,
        then: Ref::NONE,
        instead: Ref::NONE,
        fails: Fail::NONE,
    };

    /// The failure block: unbudgeted, because at the moment of failure the agent
    /// needs the precondition it violated and the way out, in one round trip.
    pub fn error_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if !self.when.is_empty() {
            out.push(format!("when: {}", self.when));
        }
        for r in self.needs {
            out.push(format!("needs: {}", r.render()));
        }
        if !self.run.is_empty() {
            out.push(format!("run: atomic {}", self.run));
        }
        for r in self.then {
            out.push(format!("then: {}", r.render()));
        }
        for r in self.instead {
            out.push(format!("instead: {}", r.render()));
        }
        for f in self.fails {
            if f.fix.cmd.is_empty() {
                out.push(format!("fails: {} -> exit {}", f.cond, f.exit));
            } else {
                out.push(format!(
                    "fails: {} -> exit {}; {}",
                    f.cond,
                    f.exit,
                    f.fix.render()
                ));
            }
        }
        out
    }
}

/// The CLI-wide exit taxonomy, mirroring [`crate::error::CliError::exit_code`].
///
/// Rendered ONCE, on `atomic --help`. Per-command deviations are [`Doc::fails`]
/// — and there are many, which is exactly why the taxonomy is stated once at the
/// root instead of blanket-generated onto every command.
pub const EXIT_CODES: &[(i32, &str)] = &[
    (0, "success"),
    (1, "general error, user-fixable"),
    (2, "usage error - the command did not run"),
    (3, "repository/data error"),
    (4, "remote/network error"),
    (128, "internal error (bug)"),
];

/// The registry: command path -> the row declared beside that command.
///
/// This list is the ONLY central thing. A renamed or retired command turns its
/// key into a test failure (`agent_guards::every_key_resolves`) instead of
/// silently orphaning a row.
///
/// This is a bug tracker for observed agent confusion, not a documentation
/// project — a command earns a row when an agent has been seen to get it wrong.
pub const DOCS: &[(&str, Doc)] = &[
    // memory — the canonical durable-fact family.
    ("memory new", crate::commands::memory::new::DOC),
    ("memory show", crate::commands::memory::show::DOC),
    ("memory validate", crate::commands::memory::validate::DOC),
    ("memory attest", crate::commands::memory::attest::DOC),
    ("memory verify", crate::commands::memory::verify::DOC),
    ("memory list", crate::commands::memory::list::DOC),
    ("memory kinds", crate::commands::memory::kinds::DOC),
    ("memory write", crate::commands::memory::write::DOC),
    // intent — the canonical "why" family.
    ("intent new", crate::commands::intent::new::DOC),
    ("intent validate", crate::commands::intent::validate::DOC),
    ("intent attest", crate::commands::intent::attest::DOC),
    ("intent verify", crate::commands::intent::verify::DOC),
    ("intent list", crate::commands::intent::list::DOC),
    ("intent show", crate::commands::intent::show::DOC),
    ("intent update", crate::commands::intent::update::DOC),
    ("intent link", crate::commands::intent::link::DOC),
    ("intent delete", crate::commands::intent::delete::DOC),
    // The core VCS loop.
    ("record", crate::commands::record::DOC),
    ("add", crate::commands::add::DOC),
    ("status", crate::commands::status::DOC),
    ("log", crate::commands::log::DOC),
    ("diff", crate::commands::diff::DOC),
    // Retrieval: how an agent finds what earlier turns knew.
    ("vault sync", crate::commands::vault::sync::DOC),
    ("vault context", crate::commands::vault::context::DOC),
    ("query search", crate::commands::query::SEARCH_DOC),
    ("query neighbors", crate::commands::query::NEIGHBORS_DOC),
    ("session show", crate::commands::session::SHOW_DOC),
];

/// The row for a command path (`"memory new"`), or `None` when the command is
/// undocumented — in which case it renders exactly as it did before.
pub fn lookup(path: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|(p, _)| *p == path).map(|(_, d)| d)
}
