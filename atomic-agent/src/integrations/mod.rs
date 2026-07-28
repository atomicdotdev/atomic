//! Externally-packaged agent integrations: registry, manifest, installer.
//!
//! Some agent integrations (prompt files, skills, plugins, hook manifests)
//! live in their own repositories — e.g. `atomic-opencode`, `atomic-claude` —
//! published as public projects on Atomic storage. This module is how
//! `atomic agent enable --agent <name>` actually *installs* those packages:
//!
//! ```text
//! registry.toml (embedded, curated)      — agent → storage URL + view
//!        │
//!        ▼  (CLI syncs the package into ~/.atomic/integrations/<agent>/repo
//!            with Atomic's own remote protocol, or --from <path> skips sync)
//! atomic-integration.toml (in the package) — files→destinations, settings
//!        │                                   manifests, requires.atomic gate
//!        ▼
//! install.rs — copies files (never symlinks), merges JSON settings via the
//!              existing hooks::manifest engine, writes a receipt
//!        │
//!        ▼
//! receipt.json (~/.atomic/integrations/<agent>/) — drives uninstall and
//!              user-file protection on reinstall
//! ```
//!
//! The CLI never executes anything from the package: no shell scripts, no
//! postinstall — files are copied and JSON is merged, nothing more.

mod install;
mod manifest;
mod receipt;
mod registry;

pub use install::{
    install_from_dir, uninstall, InstallOptions, InstallOutcome, SkipReason, SkippedFile,
    UninstallOutcome,
};
pub use manifest::{IntegrationManifest, MANIFEST_FILE};
pub use receipt::{agent_dir, cache_repo_dir, integrations_root, receipt_path, Receipt};
pub use registry::{agents as registered_integrations, resolve, IntegrationSpec};
