//! Removal and status of the Atomic-owned advisory hook dispatchers.
//!
//! `atomic git shim enable` installs one dispatcher per hook in
//! `ATOMIC_DISPATCHER_HOOKS`; `atomic git hooks uninstall` (and `atomic git
//! shim disable`, which calls it) is the exact inverse. Only a byte-for-byte
//! Atomic-owned dispatcher or an Atomic-only legacy import hook is ever
//! removed; a custom hook, symlink, or foreign script is reported and left
//! untouched, and a custom `core.hooksPath` is never modified.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::hooks::{
    canonical_root, git_error, git_hook_context, is_legacy_atomic_only, is_owned_dispatcher,
    print_unmanaged_instructions, GitHookContext,
};
use super::observation::{observe_git, GitObservation};
use crate::error::CliResult;
use crate::output::{print_info, print_warning};

/// Every hook `enable_shim` installs as an Atomic-owned dispatcher.
pub(crate) const ATOMIC_DISPATCHER_HOOKS: [&str; 5] = [
    "post-checkout",
    "pre-commit",
    "pre-push",
    "post-rewrite",
    "reference-transaction",
];

/// Hooks earlier releases wrote as synchronous import hooks. An Atomic-only
/// legacy section is removed on uninstall; foreign content there was never
/// Atomic's to report.
const LEGACY_IMPORT_HOOKS: [&str; 2] = ["post-commit", "post-merge"];

/// The outcome of inspecting one hook path for removal.
#[derive(Debug, Eq, PartialEq)]
enum HookRemoval {
    Removed,
    Absent,
    Kept(&'static str),
}

/// Remove every Atomic-owned dispatcher `enable_shim` installed, reporting
/// each hook removed or kept.
pub(crate) fn uninstall_shim(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    let context = git_hook_context(&root)?;
    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_unmanaged_instructions(
            &format!(
                "core.hooksPath is configured as '{}'; Atomic did not modify that hook system",
                custom_hooks_path.display()
            ),
            &binary,
        )?;
        return Ok(());
    }

    let hooks_dir = context.common_dir.join("hooks");
    let dispatchers = ATOMIC_DISPATCHER_HOOKS
        .iter()
        .map(|hook_name| report_dispatcher_removal(&hooks_dir, hook_name))
        .collect::<CliResult<Vec<bool>>>()?;
    let legacy = LEGACY_IMPORT_HOOKS
        .iter()
        .map(|hook_name| report_legacy_removal(&hooks_dir, hook_name))
        .collect::<CliResult<Vec<bool>>>()?;
    if !dispatchers
        .iter()
        .chain(legacy.iter())
        .any(|removed| *removed)
    {
        print_info("No Atomic advisory dispatchers are installed.");
    }
    Ok(())
}

/// Remove one advisory dispatcher when Atomic owns it; returns whether a
/// file was removed.
fn report_dispatcher_removal(hooks_dir: &Path, hook_name: &str) -> CliResult<bool> {
    let path = hooks_dir.join(hook_name);
    match remove_owned_hook(&path)? {
        HookRemoval::Removed => {
            print_info(&format!(
                "Removed the Atomic advisory {hook_name} dispatcher ({})",
                path.display()
            ));
            Ok(true)
        }
        HookRemoval::Absent => Ok(false),
        HookRemoval::Kept(reason) => {
            print_warning(&format!(
                "{hook_name} hook '{}' is {reason} and was left untouched",
                path.display()
            ));
            Ok(false)
        }
    }
}

/// Remove one legacy synchronous import hook when it is Atomic-only.
fn report_legacy_removal(hooks_dir: &Path, hook_name: &str) -> CliResult<bool> {
    let path = hooks_dir.join(hook_name);
    match remove_owned_hook(&path)? {
        HookRemoval::Removed => {
            print_info(&format!(
                "Removed Atomic-owned legacy synchronous hook {}",
                path.display()
            ));
            Ok(true)
        }
        HookRemoval::Absent | HookRemoval::Kept(_) => Ok(false),
    }
}

/// Remove `path` only when it is a regular file whose content Atomic owns.
fn remove_owned_hook(path: &Path) -> CliResult<HookRemoval> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HookRemoval::Absent),
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect hook '{}': {error}",
                path.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(HookRemoval::Kept("an unmanaged symlink"));
    }
    if !metadata.file_type().is_file() {
        return Ok(HookRemoval::Kept("not a regular file"));
    }
    let content = fs::read(path)
        .map_err(|error| git_error(format!("cannot read hook '{}': {error}", path.display())))?;
    if !(is_owned_dispatcher(&content) || is_legacy_atomic_only(&content)) {
        return Ok(HookRemoval::Kept("not Atomic-owned"));
    }
    fs::remove_file(path)
        .map_err(|error| git_error(format!("cannot remove hook '{}': {error}", path.display())))?;
    Ok(HookRemoval::Removed)
}

/// Report the state of every advisory dispatcher.
pub(crate) fn show_status(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let context = git_hook_context(&root)?;
    match dispatcher_states(&context)? {
        DispatcherReport::External { hooks_path } => {
            for hook_name in ATOMIC_DISPATCHER_HOOKS {
                print_info(&format!(
                    "{hook_name}: externally managed through core.hooksPath '{}'",
                    hooks_path.display()
                ));
            }
        }
        DispatcherReport::Inspected(hooks) => {
            for hook in hooks {
                print_info(&format!(
                    "{}: {} ({})",
                    hook.name,
                    hook.state.label(),
                    hook.path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Where the shim's dispatchers live, and the state of each one.
#[derive(Debug)]
pub(crate) enum DispatcherReport {
    /// A custom `core.hooksPath` is set; Atomic never manages hooks there.
    External { hooks_path: PathBuf },
    /// One entry per hook in [`ATOMIC_DISPATCHER_HOOKS`], in that order.
    Inspected(Vec<DispatcherHook>),
}

#[derive(Debug)]
pub(crate) struct DispatcherHook {
    pub(crate) name: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) state: HookState,
}

/// What occupies one hook path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookState {
    NotInstalled,
    AtomicOwned,
    UnmanagedSymlink,
    UnmanagedNonFile,
    Unmanaged,
    UnmanagedUnreadable,
}

impl HookState {
    /// The wording `atomic git hooks status` prints.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::AtomicOwned => "Atomic-owned dispatcher installed",
            Self::UnmanagedSymlink => "unmanaged symlink",
            Self::UnmanagedNonFile => "unmanaged non-file",
            Self::Unmanaged => "unmanaged hook installed",
            Self::UnmanagedUnreadable => "unmanaged unreadable hook",
        }
    }

    /// Stable machine-readable key.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::AtomicOwned => "atomic_owned",
            Self::UnmanagedSymlink => "unmanaged_symlink",
            Self::UnmanagedNonFile => "unmanaged_non_file",
            Self::Unmanaged => "unmanaged",
            Self::UnmanagedUnreadable => "unmanaged_unreadable",
        }
    }
}

/// Inspect the dispatchers of the Git repository colocated with `root`, or
/// `None` when there is no Git repository there. Read-only.
pub(crate) fn inspect_dispatchers(root: &Path) -> CliResult<Option<DispatcherReport>> {
    let root = canonical_root(root)?;
    match observe_git(&root) {
        Ok(GitObservation::NoGit { .. }) => return Ok(None),
        Ok(GitObservation::Repository(_)) => {}
        Err(error) => {
            return Err(git_error(format!(
                "cannot resolve Git administrative paths: {error}"
            )))
        }
    }
    dispatcher_states(&git_hook_context(&root)?).map(Some)
}

/// Inspect every dispatcher path without changing anything.
fn dispatcher_states(context: &GitHookContext) -> CliResult<DispatcherReport> {
    if let Some(hooks_path) = &context.custom_hooks_path {
        return Ok(DispatcherReport::External {
            hooks_path: hooks_path.clone(),
        });
    }
    let hooks_dir = context.common_dir.join("hooks");
    ATOMIC_DISPATCHER_HOOKS
        .into_iter()
        .map(|name| {
            let path = hooks_dir.join(name);
            Ok(DispatcherHook {
                name,
                state: hook_status(&path)?,
                path,
            })
        })
        .collect::<CliResult<_>>()
        .map(DispatcherReport::Inspected)
}

fn hook_status(path: &Path) -> CliResult<HookState> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(HookState::NotInstalled),
        Err(error) => Err(git_error(format!(
            "cannot inspect hook '{}': {error}",
            path.display()
        ))),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(HookState::UnmanagedSymlink),
        Ok(metadata) if !metadata.file_type().is_file() => Ok(HookState::UnmanagedNonFile),
        Ok(_) => Ok(match fs::read(path) {
            Ok(content) if is_owned_dispatcher(&content) => HookState::AtomicOwned,
            Ok(_) => HookState::Unmanaged,
            Err(_) => HookState::UnmanagedUnreadable,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::hooks::enable_shim;
    use super::*;
    use git2::Repository as GitRepository;
    use tempfile::TempDir;

    const OWNED: &[u8] = b"#!/bin/sh\n# atomic:git-bridge-dispatcher:v1\nexit 0\n";
    const CUSTOM: &[u8] = b"#!/bin/sh\necho custom\nexit 0\n";

    /// A Git repository with an Atomic repository and the shim enabled.
    fn enabled_repository() -> TempDir {
        let root = TempDir::new().unwrap();
        drop(GitRepository::init(root.path()).unwrap());
        drop(atomic_repository::Repository::init(root.path()).unwrap());
        enable_shim(root.path(), false).unwrap();
        root
    }

    #[test]
    fn enable_installs_the_dispatcher_table_and_uninstall_removes_it() {
        let root = enabled_repository();
        let hooks_dir = root.path().join(".git").join("hooks");
        for hook in ATOMIC_DISPATCHER_HOOKS {
            let content = fs::read(hooks_dir.join(hook)).unwrap();
            assert!(is_owned_dispatcher(&content), "{hook} is Atomic-owned");
        }
        uninstall_shim(root.path()).unwrap();
        for hook in ATOMIC_DISPATCHER_HOOKS {
            assert!(!hooks_dir.join(hook).exists(), "{hook} is removed");
        }
        // Uninstall is idempotent.
        uninstall_shim(root.path()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_keeps_foreign_hooks_and_symlinks_byte_for_byte() {
        use std::os::unix::fs::symlink;

        let root = enabled_repository();
        let hooks_dir = root.path().join(".git").join("hooks");
        fs::write(hooks_dir.join("pre-push"), CUSTOM).unwrap();
        let target = root.path().join("target-hook");
        fs::write(&target, CUSTOM).unwrap();
        fs::remove_file(hooks_dir.join("post-rewrite")).unwrap();
        symlink(&target, hooks_dir.join("post-rewrite")).unwrap();

        uninstall_shim(root.path()).unwrap();

        assert_eq!(fs::read(hooks_dir.join("pre-push")).unwrap(), CUSTOM);
        assert!(fs::symlink_metadata(hooks_dir.join("post-rewrite"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), CUSTOM);
        for hook in ["post-checkout", "pre-commit", "reference-transaction"] {
            assert!(!hooks_dir.join(hook).exists(), "{hook} is removed");
        }
    }

    #[test]
    fn uninstall_removes_an_atomic_only_legacy_hook_but_not_a_mixed_one() {
        let root = enabled_repository();
        let hooks_dir = root.path().join(".git").join("hooks");
        let legacy: &[u8] =
            b"#!/bin/sh\n# atomic:git:begin\natomic git import --incremental || true\n# atomic:git:end\n";
        let mixed: &[u8] =
            b"#!/bin/sh\necho custom\n# atomic:git:begin\natomic old\n# atomic:git:end\n";
        fs::write(hooks_dir.join("post-commit"), legacy).unwrap();
        fs::write(hooks_dir.join("post-merge"), mixed).unwrap();

        uninstall_shim(root.path()).unwrap();

        assert!(!hooks_dir.join("post-commit").exists());
        assert_eq!(fs::read(hooks_dir.join("post-merge")).unwrap(), mixed);
    }

    #[test]
    fn hook_status_classifies_missing_owned_and_foreign_hooks() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            hook_status(&dir.path().join("missing")).unwrap().label(),
            "not installed"
        );
        let owned = dir.path().join("owned");
        fs::write(&owned, OWNED).unwrap();
        assert_eq!(
            hook_status(&owned).unwrap().label(),
            "Atomic-owned dispatcher installed"
        );
        let foreign = dir.path().join("foreign");
        fs::write(&foreign, CUSTOM).unwrap();
        assert_eq!(
            hook_status(&foreign).unwrap().label(),
            "unmanaged hook installed"
        );
    }
}
