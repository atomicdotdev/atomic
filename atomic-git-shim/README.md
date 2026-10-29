# atomic-git-shim

Git interoperability for Atomic: import, checkpoint/reconciliation, hooks,
branch projection, binding, and transport. This crate owns the adapter that
used to live under `atomic-cli/src/commands/git` and has no dependency on the
CLI executable. Graph history, closure semantics, repository transactions,
and working-copy identity remain in `atomic-core` / `atomic-repository`.

The initial extraction preserves the command-facing clap types and output.
`commands` also exposes the adapter operations used by native CLI commands
to coordinate recording, switching, and publication. Applications can install
an `output::set_reporter` callback for notices; tables, progress, and JSON
remain command output. This is not yet a presentation-free SDK.

Fresh eligible imports use the rolling pipeline by default. `--no-stream`
selects the ordinary importer; `--stream-window-mib` controls the rolling
payload budget (32 MiB by default), not a process RSS limit. `--agent-mode`
selects the import animation when both output streams are terminals. The
rolling dashboard, animation, and their terminal dependencies live in this
crate and are excluded from CLI builds without `git-shim`.

An embedding reporter must record diagnostics before calling
`output::live::capture`, then suppress immediate console output when capture
returns true. The CLI and shim visuals share that capture state, so logging
continues during animation and the two visuals do not compete for the screen.
Without a reporter, the crate captures notices itself before printing them.

## Building

```sh
# Build and test the adapter independently of the CLI.
cargo check -p atomic-git-shim
cargo test -p atomic-git-shim --features adoption-test-injection

# Existing CLI behavior: teams and git-shim are enabled by default.
cargo build -p atomic-cli

# CLI without Git interoperability; retain teams if desired.
cargo build -p atomic-cli --no-default-features
cargo build -p atomic-cli --no-default-features --features teams

# Run the native CLI suite, including feature-unavailable refusal tests.
# Git command integration suites require git-shim and are skipped here.
cargo test -p atomic-cli --no-default-features

# Integration tests share the consolidated `it` target.
cargo test -p atomic-cli --test it git_shim_feature_test::
cargo test -p atomic-cli --features adoption-test-injection --test it git_shim_
```

The `git-shim` Cargo feature controls whether the adapter is compiled into
the CLI. The existing per-repository `[git.shim] enabled` setting controls
enrollment at runtime; these are separate decisions. Default builds preserve
the existing opt-in policy.

Without the feature, `atomic git`, `atomic stage`, and `atomic unstage` are
absent. Git clone/adoption and `status --git` / `diff --git` refuse with a
build-feature explanation, and native initialization, recording, and view
operations remain available.
An enrolled workspace (including one with a verified checkpoint) refuses
operations before writable repository access or RPC dispatch. Use an enabled
build to continue or explicitly disable enrollment with that build first.
Read-only `atomic diagnostics` remains available, including in enrolled or
misconfigured workspaces; it reports Git hook inspection as unavailable when
the adapter is not compiled in.

`atomic stage` and `atomic unstage` are Git interoperability commands. They
write the colocated repository's Git index or restore its entries to Git HEAD,
respectively, while preserving worktree edits. They do not provide a separate
native Atomic staging area. Use `atomic add` to establish tracking and
`atomic record` to record native changes without requiring staging.

This feature does not remove every transitive Git dependency: repository
guards and agent integrations still use Git metadata. Building the entire
workspace also builds this crate as a workspace member; select `-p atomic-cli`
when checking that the optional adapter is excluded from a CLI build.

For supported workflows, enrollment, recovery, vault policy, and platform
limitations, see the [operating guide](../docs/shim-operating-guide.md) and
[testing walkthrough](../docs/shim-review-testing.md). Git transport does not
replace native synchronization of Atomic's knowledge graph and vault database.
