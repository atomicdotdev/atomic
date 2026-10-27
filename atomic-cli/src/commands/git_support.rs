//! Build-time availability of the optional Git adapter.

#[cfg(not(feature = "git-shim"))]
pub(crate) fn unavailable_error() -> crate::error::CliError {
    crate::error::CliError::InvalidArgument {
        message: "this binary was built without Git interoperability; use an Atomic build with the `git-shim` feature".into(),
    }
}

/// A feature-disabled executable must not update an enrolled working copy
/// without its Git/checkpoint publication adapter. Inspect before RPC startup
/// or a writable repository open; do not silently downgrade it to native mode.
#[cfg(not(feature = "git-shim"))]
pub(crate) fn ensure_current_workspace_supported() -> crate::error::CliResult<()> {
    // Remote sandbox caches have no local repository database to inspect.
    if crate::remote_sandbox::current().is_some() {
        return Ok(());
    }
    let Ok(root) = super::find_repository_root() else {
        return Ok(());
    };
    // Mirror Repository::shim_workspace_active using only enrollment metadata.
    // Opening even a read-only database here would prevent native commands
    // from performing required schema migrations through their normal entry.
    // Any checkpoint (including a malformed one) requires an enabled build.
    let active = if root.join(".atomic/bridge/workspace.json").try_exists()? {
        true
    } else if root.join(atomic_repository::SANDBOX_POINTER).is_file() {
        // A sandbox does not inherit its host's enrollment.
        false
    } else {
        let dot_dir = atomic_repository::canonical_dot_dir_for(&root).ok_or_else(|| {
            crate::error::CliError::InvalidRepository {
                reason: "cannot resolve repository configuration to check Git shim enrollment"
                    .into(),
            }
        })?;
        atomic_config::RepoConfig::load(&dot_dir.join("config.toml"))?
            .git
            .shim
            .enabled
    };
    if active {
        return Err(crate::error::CliError::StaleBaseline {
            report: "this working copy is enrolled in the Git shim, but this binary was built without `git-shim`; use a shim-enabled binary to continue or explicitly disable the shim with that binary".into(),
        });
    }
    Ok(())
}
