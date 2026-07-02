//! Canonical authoring commands — the CLI surface Recording the Why names:
//!
//! ```text
//! atomic intent new WORD-5 --title "Add name prompt modal"
//! atomic intent show WORD-5.md
//! atomic intent validate WORD-5.md --shacl
//! atomic memory new upload-assumptions --kind constraint --about storage
//! atomic memory validate upload-assumptions.md --shacl
//! ```
//!
//! The flow is the doc's validation loop for agents: author markdown
//! (frontmatter spine + typed directives) → the lift extracts the canonical
//! JSON-LD node → attest with your atomic identity (in-memory; the source
//! file is never rewritten) → the tier-1 gate checks the shapes → `--shacl`
//! additionally runs the tier-2 formal gate (pyshacl). Failures come back
//! structured (`--json`), so an agent can fix the surface and resubmit.
//!
//! Templates constrain the spine and the directive blocks, never the prose —
//! the moment you template the reason you're templating the fake.

mod intent;
mod memory;

pub use intent::Intent;
pub use memory::Memory;

use atomic_identity::keypair::KeyPair;
use atomic_identity::{Identity, IdentityStore};

use crate::error::{CliError, CliResult};

/// Resolve the signing identity: `--identity <name>` when given, else the
/// store's default. The keypair is required because validation attests the
/// lifted node in-memory (the gate demands a proof — authorship is part of
/// what "valid" means).
pub(crate) fn resolve_signing_identity(
    identity_name: Option<&str>,
    password: Option<&str>,
) -> CliResult<(Identity, KeyPair)> {
    let store = IdentityStore::open_default().map_err(|e| {
        CliError::Internal(anyhow::anyhow!(
            "identity store not available: {e}. Create one with `atomic identity new`."
        ))
    })?;

    let identity = match identity_name {
        Some(name) => store
            .load_by_name(name)
            .map_err(|_| CliError::IdentityNotFound(name.to_string()))?,
        None => store
            .get_default()
            .ok()
            .flatten()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "no default identity. Set one with `atomic identity default <name>` \
                          or pass --identity <name>."
                    .to_string(),
            })?,
    };

    let keypair = store.load_keypair(&identity.id, password).map_err(|e| {
        CliError::Internal(anyhow::anyhow!(
            "could not load the secret key for '{}': {e}. If the key is \
             password-protected, pass --password.",
            identity.name
        ))
    })?;

    Ok((identity, keypair))
}

/// Shared validate output: tier-1 (always) + tier-2 (when requested).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ValidateOutput {
    /// Tier-1 (in-process gate) conformance.
    pub conforms: bool,
    pub violations: Vec<atomic_canonical::gate::Violation>,

    /// Tier-2 (pyshacl) result, present when `--shacl` ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shacl: Option<ShaclOutput>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ShaclOutput {
    pub conforms: bool,
    pub report: String,
}

/// Run the shared tail of a validate command: tier-1 report, optional tier-2,
/// optional canonical emit, human or JSON output, non-zero exit on any
/// non-conformance. `value` must already be attested.
pub(crate) fn finish_validate(
    label: &str,
    report: atomic_canonical::gate::ValidationReport,
    value: &serde_json::Value,
    run_shacl: bool,
    emit: Option<&std::path::Path>,
    json: bool,
) -> CliResult<()> {
    if let Some(path) = emit {
        let pretty = serde_json::to_string_pretty(value)
            .expect("canonical value serialization is infallible");
        std::fs::write(path, pretty).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("failed to write {}: {e}", path.display()))
        })?;
        eprintln!("canonical JSON-LD written to {}", path.display());
    }

    let shacl = if run_shacl {
        if !atomic_canonical::shacl::is_available() {
            return Err(CliError::InvalidArgument {
                message: "no SHACL engine found — install pyshacl or set ATOMIC_PYSHACL"
                    .to_string(),
            });
        }
        let result = atomic_canonical::shacl::validate_value(value)
            .map_err(|e| CliError::Internal(anyhow::anyhow!(e.to_string())))?;
        Some(ShaclOutput {
            conforms: result.conforms,
            report: result.report,
        })
    } else {
        None
    };

    let output = ValidateOutput {
        conforms: report.conforms,
        violations: report.results,
        shacl,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        if output.conforms {
            println!("{label}: tier-1 gate: Conforms");
        } else {
            println!("{label}: tier-1 gate: DOES NOT CONFORM");
            for v in &output.violations {
                let path = v.path.as_deref().unwrap_or("-");
                println!(
                    "  ✗ [{}] {} ({}): {}",
                    v.shape, v.focus_node, path, v.message
                );
            }
        }
        if let Some(shacl) = &output.shacl {
            println!(
                "{label}: tier-2 SHACL: {}",
                if shacl.conforms {
                    "Conforms"
                } else {
                    "DOES NOT CONFORM"
                }
            );
            if !shacl.conforms {
                println!("{}", shacl.report.trim());
            }
        }
    }

    let all_conform = output.conforms && output.shacl.as_ref().map(|s| s.conforms).unwrap_or(true);
    if all_conform {
        Ok(())
    } else {
        Err(CliError::InvalidArgument {
            message: format!("{label} does not conform to the gate"),
        })
    }
}

/// Refuse to overwrite an existing file when scaffolding.
pub(crate) fn write_new_file(path: &std::path::Path, content: &str) -> CliResult<()> {
    if path.exists() {
        return Err(CliError::InvalidArgument {
            message: format!("{} already exists — not overwriting", path.display()),
        });
    }
    std::fs::write(path, content).map_err(|e| {
        CliError::Internal(anyhow::anyhow!("failed to write {}: {e}", path.display()))
    })?;
    println!("created {}", path.display());
    Ok(())
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
