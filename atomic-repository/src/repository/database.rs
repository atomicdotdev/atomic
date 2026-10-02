//! Where a repository keeps its redb database.
//!
//! All repository state lives in `.atomic/atomic.redb`. Repositories created
//! before that split it between `pristine.redb` (graph, views, sessions, vault)
//! and `changes.redb` (provenance journal). The first open of such a repository
//! merges both into `atomic.redb` and moves the old files to
//! `.atomic/legacy/<id>/`, so an older atomic can no longer find them and write
//! history the new file never sees.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_core::pristine::merge::{merged_legacy_dir, LegacyDatabases};
use atomic_core::pristine::PristineError;

use crate::RepositoryError;

/// Repository database filename inside `.atomic/`.
pub const DATABASE_FILE: &str = "atomic.redb";

/// Graph database of repositories created before [`DATABASE_FILE`].
pub const LEGACY_PRISTINE_FILE: &str = "pristine.redb";

/// Provenance journal of repositories created before [`DATABASE_FILE`].
pub const LEGACY_CHANGE_STORE_FILE: &str = "changes.redb";

/// Directory inside `.atomic/` that keeps the files a merge replaced.
pub const LEGACY_DIR: &str = "legacy";

/// Scratch file a merge writes before publishing it as [`DATABASE_FILE`].
const MERGING_FILE: &str = "atomic.redb.merging";

/// Whether `dot_dir` holds a repository database in either layout.
pub fn has_database(dot_dir: &Path) -> bool {
    dot_dir.join(DATABASE_FILE).is_file() || dot_dir.join(LEGACY_PRISTINE_FILE).is_file()
}

/// Return `.atomic/atomic.redb`, merging a legacy layout into it first.
///
/// # Errors
///
/// - `DatabaseBusy` while another process holds a database file; retry.
/// - `LegacyChangeStoreBusy` when an owner from an older atomic holds
///   `changes.redb`.
/// - `InvalidRepository` when both layouts exist and no interrupted merge
///   explains it.
pub fn ensure_database(dot_dir: &Path) -> Result<PathBuf, RepositoryError> {
    let database = dot_dir.join(DATABASE_FILE);
    if !dot_dir.join(LEGACY_PRISTINE_FILE).is_file() {
        return existing(database, dot_dir);
    }

    // Lock the legacy graph before deciding anything, so concurrent opens of
    // one repository queue behind a single merge instead of racing it.
    let mut legacy = LegacyDatabases::new();
    if let Err(error) = legacy.add(&dot_dir.join(LEGACY_PRISTINE_FILE)) {
        // Another open finished the merge and moved the file away meanwhile.
        if is_not_found(&error) {
            return existing(database, dot_dir);
        }
        return Err(error.into());
    }
    add_change_store(&mut legacy, dot_dir)?;
    if database.is_file() {
        finish_retiring_legacy(dot_dir, &legacy)?;
    } else {
        merge_legacy(dot_dir, &legacy)?;
    }
    Ok(database)
}

fn existing(database: PathBuf, dot_dir: &Path) -> Result<PathBuf, RepositoryError> {
    if database.is_file() {
        Ok(database)
    } else {
        Err(RepositoryError::NotFound {
            path: dot_dir.display().to_string(),
        })
    }
}

fn merge_legacy(dot_dir: &Path, legacy: &LegacyDatabases) -> Result<(), RepositoryError> {
    let merging = dot_dir.join(MERGING_FILE);
    remove_if_present(&merging)?;
    let legacy_name = unused_legacy_name(dot_dir);
    let report = legacy.merge_into(&merging, &legacy_name)?;
    std::fs::rename(&merging, dot_dir.join(DATABASE_FILE))?;
    sync_dir(dot_dir)?;
    retire_legacy_files(dot_dir, &legacy_name, legacy)?;

    let retained = dot_dir.join(LEGACY_DIR).join(&legacy_name);
    log::info!(
        "merged {} rows from {} tables into {}; previous files kept in {}",
        report.rows(),
        report.tables.len(),
        dot_dir.join(DATABASE_FILE).display(),
        retained.display()
    );
    if !report.left_behind.is_empty() {
        log::info!(
            "retired tables not carried into {}: {}",
            DATABASE_FILE,
            report.left_behind.join(", ")
        );
    }
    Ok(())
}

/// Finish a merge that was interrupted after publishing `atomic.redb` but
/// before the legacy files were moved away.
fn finish_retiring_legacy(dot_dir: &Path, legacy: &LegacyDatabases) -> Result<(), RepositoryError> {
    let legacy_name = merged_legacy_dir(&dot_dir.join(DATABASE_FILE))?;
    let Some(legacy_name) = legacy_name.filter(|name| {
        !dot_dir
            .join(LEGACY_DIR)
            .join(name)
            .join(LEGACY_PRISTINE_FILE)
            .exists()
    }) else {
        return Err(RepositoryError::InvalidRepository {
            reason: format!(
                "both {} and {} exist in {}; {} is the repository database, so move {} \
                 away if it is stale",
                DATABASE_FILE,
                LEGACY_PRISTINE_FILE,
                dot_dir.display(),
                DATABASE_FILE,
                LEGACY_PRISTINE_FILE
            ),
        });
    };
    legacy.verify_unchanged(&dot_dir.join(DATABASE_FILE))?;
    retire_legacy_files(dot_dir, &legacy_name, legacy)?;
    log::warn!(
        "finished an interrupted database merge in {}",
        dot_dir.display()
    );
    Ok(())
}

fn add_change_store(legacy: &mut LegacyDatabases, dot_dir: &Path) -> Result<(), RepositoryError> {
    let changes = dot_dir.join(LEGACY_CHANGE_STORE_FILE);
    if !changes.is_file() {
        return Ok(());
    }
    legacy.add(&changes).map_err(|error| {
        if is_already_open(&error) {
            RepositoryError::LegacyChangeStoreBusy {
                path: changes.display().to_string(),
            }
        } else {
            error.into()
        }
    })
}

/// Move the legacy files this process holds into `.atomic/legacy/<name>/`.
///
/// A file it does not hold was never merged and stays put. The graph moves
/// last: while `pristine.redb` remains, the next open finishes the move.
fn retire_legacy_files(
    dot_dir: &Path,
    legacy_name: &str,
    legacy: &LegacyDatabases,
) -> Result<(), RepositoryError> {
    let target = dot_dir.join(LEGACY_DIR).join(legacy_name);
    std::fs::create_dir_all(&target)?;
    let held: Vec<&Path> = legacy.paths().collect();
    for file in [LEGACY_CHANGE_STORE_FILE, LEGACY_PRISTINE_FILE] {
        let source = dot_dir.join(file);
        if held.contains(&source.as_path()) {
            std::fs::rename(&source, target.join(file))?;
        }
    }
    sync_dir(&target)?;
    sync_dir(&dot_dir.join(LEGACY_DIR))?;
    sync_dir(dot_dir)?;
    Ok(())
}

fn unused_legacy_name(dot_dir: &Path) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let base = seconds.to_string();
    let legacy = dot_dir.join(LEGACY_DIR);
    (0..)
        .map(|attempt| {
            if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-{attempt}")
            }
        })
        .find(|name| !legacy.join(name).exists())
        .expect("an unused legacy directory name exists")
}

fn remove_if_present(path: &Path) -> Result<(), RepositoryError> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Make renames in `dir` durable. Directory fsync is only portable on Unix;
/// elsewhere the renames are still atomic.
fn sync_dir(dir: &Path) -> Result<(), RepositoryError> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn is_already_open(error: &PristineError) -> bool {
    matches!(error, PristineError::Database(inner)
        if matches!(inner.as_ref(), redb::DatabaseError::DatabaseAlreadyOpen))
}

fn is_not_found(error: &PristineError) -> bool {
    matches!(error, PristineError::Database(inner)
        if matches!(inner.as_ref(), redb::DatabaseError::Storage(redb::StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::NotFound))
}
