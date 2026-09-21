//! CB-13A R2: the deterministic diagnostic census for the read-only bridge
//! verify paths.
//!
//! The five layers prove the CURRENT working copy's alignment; the census
//! enumerates the durable classes a recovery would need to face — missing
//! changes, invalid bindings, stale working-copy records, orphaned WIP refs,
//! divergent ref mappings — across ALL working copies, with actionable
//! per-class results instead of implicit recovery or a generic dirty-state
//! short circuit. Everything here is read-only observation: no recovery, no
//! migration, no mutation.

use std::collections::BTreeSet;
use std::path::Path;

use atomic_core::types::Base32;
use atomic_repository::git_binding::verify_binding_cryptography;
use atomic_repository::git_binding::BindingId;
use atomic_repository::{Repository, RepositoryError};

/// One census class outcome: whether the class is clean and the actionable
/// detail (counts and named items) either way.
pub(crate) struct CensusClass {
    pub name: &'static str,
    pub clean: bool,
    pub detail: String,
}

fn repository_error(error: RepositoryError) -> String {
    error.to_string()
}

fn binding_id_hex(id: &BindingId) -> String {
    id.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Enumerate the census classes. `view_name` is the desired view of the
/// inspecting working copy; its full history anchors the change coverage
/// class.
pub(crate) fn census_classes(
    repo: &Repository,
    root: &Path,
    view_name: &str,
) -> Vec<CensusClass> {
    vec![
        change_coverage_class(repo, view_name),
        binding_census_class(repo),
        working_copy_census_class(repo, view_name),
        wip_census_class(repo, root),
        ref_mapping_census_class(repo, root, view_name),
    ]
}

/// Every change in the desired view's effective history loads from the
/// change store.
fn change_coverage_class(repo: &Repository, view_name: &str) -> CensusClass {
    let name = "census/changes";
    match repo.effective_history(Some(view_name)) {
        Ok(entries) => {
            let total = entries.len();
            let mut missing: Vec<String> = Vec::new();
            for entry in &entries {
                if repo.load_change(&entry.hash).is_err() {
                    missing.push(entry.hash.to_base32());
                }
            }
            if missing.is_empty() {
                CensusClass {
                    name,
                    clean: true,
                    detail: format!("{total} recorded change(s) all load"),
                }
            } else {
                CensusClass {
                    name,
                    clean: false,
                    detail: format!(
                        "{}/{} recorded change(s) FAIL to load: {}",
                        missing.len(),
                        total,
                        missing.join(", ")
                    ),
                }
            }
        }
        Err(error) => CensusClass {
            name,
            clean: false,
            detail: format!("cannot enumerate the view history: {error}"),
        },
    }
}

/// Every stored binding loads and passes its cryptographic verification
/// (payload validity + signature).
fn binding_census_class(repo: &Repository) -> CensusClass {
    let name = "census/bindings";
    match repo.binding_ids() {
        Ok(ids) => {
            let total = ids.len();
            let mut invalid: Vec<String> = Vec::new();
            let mut unloaded: Vec<String> = Vec::new();
            for id in &ids {
                match repo.load_binding(id) {
                    Ok(Some(binding)) => {
                        if verify_binding_cryptography(&binding).is_err() {
                            invalid.push(binding_id_hex(id));
                        }
                    }
                    Ok(None) => unloaded.push(binding_id_hex(id)),
                    Err(error) => unloaded.push(format!(
                        "{} ({})",
                        binding_id_hex(id),
                        repository_error(error)
                    )),
                }
            }
            if invalid.is_empty() && unloaded.is_empty() {
                CensusClass {
                    name,
                    clean: true,
                    detail: format!("{total} stored binding(s) verify"),
                }
            } else {
                CensusClass {
                    name,
                    clean: false,
                    detail: format!(
                        "{} invalid binding(s) [{}], {} unloadable [{}]",
                        invalid.len(),
                        invalid.join(", "),
                        unloaded.len(),
                        unloaded.join(", ")
                    ),
                }
            }
        }
        Err(error) => CensusClass {
            name,
            clean: false,
            detail: format!("cannot enumerate bindings: {error}"),
        },
    }
}

/// Every working-copy record names an existing view; the CURRENT working
/// copy's staleness is the durable layer's verdict, and other working
/// copies' stale records are enumerated informationally (an owner
/// reconciles them; they are not corruption).
fn working_copy_census_class(repo: &Repository, current_view: &str) -> CensusClass {
    let name = "census/working-copies";
    let class = (|| -> Result<CensusClass, String> {
        let txn = repo.pristine().read_txn().map_err(|e| e.to_string())?;
        let records = atomic_core::pristine::WorkingCopyTxnT::list_working_copies(&txn)
            .map_err(|e| e.to_string())?;
        let total = records.len();
        let mut broken: Vec<String> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for record in &records {
            // Resolve the desired view id to its name for the report.
            let view_state = atomic_core::pristine::ViewTxnT::get_view_by_id(
                &txn,
                record.desired_view,
            )
            .map_err(|e| e.to_string())?;
            let Some(view_state) = view_state else {
                broken.push(format!(
                    "{} desires missing view id {}",
                    record.id.as_ulid(),
                    record.desired_view
                ));
                continue;
            };
            if view_state.state != record.desired_state {
                if view_state.name == current_view {
                    // The current working copy's staleness is the durable
                    // layer's failure; the census does not double-report it.
                    continue;
                }
                stale.push(format!(
                    "{} desires '{}' at {} (view is at {})",
                    record.id.as_ulid(),
                    view_state.name,
                    record.desired_state.to_base32()[..12].to_string(),
                    view_state.state.to_base32()[..12].to_string()
                ));
            }
        }
        let _ = repo;
        if broken.is_empty() {
            if stale.is_empty() {
                Ok(CensusClass {
                    name,
                    clean: true,
                    detail: format!("{total} working-copy record(s) consistent"),
                })
            } else {
                Ok(CensusClass {
                    name,
                    clean: true,
                    detail: format!(
                        "{total} working-copy record(s) consistent; {} stale OTHER record(s) \
                         (informational, owner-reconciled): {}",
                        stale.len(),
                        stale.join("; ")
                    ),
                })
            }
        } else {
            Ok(CensusClass {
                name,
                clean: false,
                detail: format!(
                    "{} broken working-copy record(s): {}",
                    broken.len(),
                    broken.join("; ")
                ),
            })
        }
    })();
    match class {
        Ok(class) => class,
        Err(error) => CensusClass {
            name,
            clean: false,
            detail: format!("cannot enumerate working copies: {error}"),
        },
    }
}

/// WIP recovery refs (`refs/atomic/wip/…`) are referenced by at least one
/// session record or are orphaned. A ref whose target change cannot load is
/// broken regardless of references.
fn wip_census_class(repo: &Repository, root: &Path) -> CensusClass {
    let name = "census/wip";
    let class = (|| -> Result<CensusClass, String> {
        let git = git2::Repository::open(root).map_err(|e| e.to_string())?;
        let mut refs: Vec<String> = Vec::new();
        for reference in git.references_glob("refs/atomic/wip/*").map_err(|e| e.to_string())? {
            let reference = reference.map_err(|e| e.to_string())?;
            if let Some(name) = reference.name() {
                refs.push(name.to_string());
            }
        }
        if refs.is_empty() {
            return Ok(CensusClass {
                name,
                clean: true,
                detail: "no WIP recovery refs".to_string(),
            });
        }
        // Sessions reference their recovery refs by name (the retention
        // scan reads the same text surface).
        let sessions_dir = Repository::canonical_dot_dir(root)
            .map(|dot| dot.join("sessions"))
            .unwrap_or_else(|_| root.join(".atomic").join("sessions"));
        let mut session_text = String::new();
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
                    if let Ok(bytes) = std::fs::read(&path) {
                        session_text.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
        }
        let mut orphaned: Vec<String> = Vec::new();
        let mut broken: Vec<String> = Vec::new();
        for reference in &refs {
            let target_ok = git
                .find_reference(reference)
                .and_then(|reference| reference.peel_to_commit())
                .is_ok();
            if !target_ok {
                broken.push(reference.to_string());
                continue;
            }
            if !session_text.contains(reference) {
                orphaned.push(reference.to_string());
            }
        }
        if broken.is_empty() && orphaned.is_empty() {
            Ok(CensusClass {
                name,
                clean: true,
                detail: format!("{} WIP recovery ref(s) referenced", refs.len()),
            })
        } else {
            Ok(CensusClass {
                name,
                clean: false,
                detail: format!(
                    "{} broken WIP ref(s) [{}], {} orphaned [{}]",
                    broken.len(),
                    broken.join(", "),
                    orphaned.len(),
                    orphaned.join(", ")
                ),
            })
        }
    })();
    match class {
        Ok(class) => class,
        Err(error) => CensusClass {
            name,
            clean: false,
            detail: format!("cannot enumerate WIP refs: {error}"),
        },
    }
}

/// The desired view's mapped ref tip must agree with the Git branch tip
/// (CB-10A): a branch that moved without the mapping is divergent evidence.
fn ref_mapping_census_class(repo: &Repository, root: &Path, view_name: &str) -> CensusClass {
    let name = "census/refs";
    let class = (|| -> Result<CensusClass, String> {
        let git = git2::Repository::open(root).map_err(|e| e.to_string())?;
        let branch_ref = format!("refs/heads/{view_name}");
        let branch_tip = git
            .find_reference(&branch_ref)
            .ok()
            .and_then(|reference| reference.peel_to_commit().ok())
            .map(|commit| commit.id().to_string());
        let Some(branch_tip) = branch_tip else {
            return Ok(CensusClass {
                name,
                clean: true,
                detail: format!("no Git branch for view '{view_name}' (nothing to compare)"),
            });
        };
        let mapped_tip = super::ref_mapping::mapped_ref_tip(&git, repo, view_name);
        match mapped_tip {
            Some(tip) if tip == branch_tip => Ok(CensusClass {
                name,
                clean: true,
                detail: format!("mapped ref tip agrees with '{branch_ref}'"),
            }),
            Some(tip) => Ok(CensusClass {
                name,
                clean: false,
                detail: format!(
                    "the mapped ref tip {tip} diverges from the branch tip {branch_tip}; \
                     run 'atomic git bridge status' for the reconciliation state"
                ),
            }),
            None => Ok(CensusClass {
                name,
                clean: true,
                detail: "no mapped-ref observation recorded (mapping not yet established)"
                    .to_string(),
            }),
        }
    })();
    match class {
        Ok(class) => class,
        Err(error) => CensusClass {
            name,
            clean: false,
            detail: format!("cannot inspect ref mappings: {error}"),
        },
    }
}

/// The distinct set of class names (for the failure summary).
pub(crate) fn failed_class_names(classes: &[CensusClass]) -> Vec<&'static str> {
    classes
        .iter()
        .filter(|class| !class.clean)
        .map(|class| class.name)
        .collect()
}

/// Names referenced for reporting completeness in callers.
pub(crate) fn census_class_names() -> BTreeSet<&'static str> {
    [
        "census/changes",
        "census/bindings",
        "census/working-copies",
        "census/wip",
        "census/refs",
    ]
    .into_iter()
    .collect()
}
