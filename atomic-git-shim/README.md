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

Without the feature, `atomic git` is absent, Git clone/adoption and
`status --git` / `diff --git` refuse with a build-feature explanation, and
native initialization, recording, and view operations remain available.
An enrolled workspace (including one with a verified checkpoint) refuses
operations before writable repository access or RPC dispatch. Use an enabled
build to continue or explicitly disable enrollment with that build first.

This feature does not remove every transitive Git dependency: repository
guards and agent integrations still use Git metadata. Building the entire
workspace also builds this crate as a workspace member; select `-p atomic-cli`
when checking that the optional adapter is excluded from a CLI build.

For supported workflows, enrollment, recovery, vault policy, and platform
limitations, see the [operating guide](../docs/shim-operating-guide.md) and
[testing walkthrough](../docs/shim-review-testing.md). Git transport does not
replace native synchronization of Atomic's knowledge graph and vault database.
