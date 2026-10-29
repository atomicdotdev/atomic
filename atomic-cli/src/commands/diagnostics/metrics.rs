//! Read-only repository and redb observations for diagnostics.

use std::path::{Path, PathBuf};

use atomic_core::pristine::tables::{
    node_type, CHANGE_META, GRAPH, NODE_TYPES, VAULT_ENTRIES, VAULT_MANIFEST, VIEWS,
};
use atomic_core::pristine::vault::VaultManifest;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde::Serialize;

use super::collect::SourceError;

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Observation<T> {
    Available { value: T },
    Unavailable { reason: &'static str },
}

impl<T> Observation<T> {
    fn available(value: T) -> Self {
        Self::Available { value }
    }

    fn unavailable(reason: &'static str) -> Self {
        Self::Unavailable { reason }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum RepositoryMetrics {
    NotApplicable {
        reason: &'static str,
    },
    Unavailable {
        reason: &'static str,
        database_bytes: Option<u64>,
    },
    Available {
        database_bytes: Observation<u64>,
        registered_changes: Observation<u64>,
        views: Observation<u64>,
        intents: Observation<IntentCounts>,
        current_view: Observation<CurrentViewCounts>,
        tables: Vec<TableMetric>,
        last_compacted: Observation<String>,
    },
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct IntentCounts {
    pub total: u64,
    pub backlog: u64,
    pub planned: u64,
    pub in_progress: u64,
    pub needs_review: u64,
    pub done: u64,
    pub icebox: u64,
    pub other: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct CurrentViewCounts {
    pub own: u64,
    pub inherited: u64,
    pub membership: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct TableMetric {
    pub name: &'static str,
    #[serde(flatten)]
    pub statistics: Observation<TableStatistics>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TableStatistics {
    pub entries: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
    pub tree_height: u32,
    pub leaf_pages: u64,
    pub branch_pages: u64,
}

fn statistics(table: &impl ReadableTableMetadata) -> Result<TableStatistics, ()> {
    let stats = table.stats().map_err(|_| ())?;
    Ok(TableStatistics {
        entries: table.len().map_err(|_| ())?,
        stored_bytes: stats.stored_bytes(),
        metadata_bytes: stats.metadata_bytes(),
        fragmented_bytes: stats.fragmented_bytes(),
        tree_height: stats.tree_height(),
        leaf_pages: stats.leaf_pages(),
        branch_pages: stats.branch_pages(),
    })
}

fn table_metric(name: &'static str, value: Result<TableStatistics, ()>) -> TableMetric {
    TableMetric {
        name,
        statistics: match value {
            Ok(value) => Observation::available(value),
            Err(()) => Observation::unavailable("table statistics unavailable"),
        },
    }
}

fn current_view(root: &Path) -> Observation<CurrentViewCounts> {
    let result = (|| {
        let repo = atomic_repository::Repository::open_readonly_without_migration(root).ok()?;
        let id = repo.working_copy_id()?;
        let name = repo.desired_view_name(id).ok()?;
        let info = repo.get_view_info(&name).ok()?;
        Some(CurrentViewCounts {
            own: info.own_change_count,
            inherited: info.inherited_change_count,
            membership: info
                .own_change_count
                .checked_add(info.inherited_change_count)?,
        })
    })();
    match result {
        Some(value) => Observation::available(value),
        None => Observation::unavailable("current working-copy view cannot be verified"),
    }
}

fn intents(txn: &redb::ReadTransaction) -> Observation<IntentCounts> {
    let result = (|| {
        let table = txn.open_table(VAULT_MANIFEST).ok()?;
        let value = table.get("manifest").ok()??;
        let manifest: VaultManifest = serde_json::from_slice(value.value()).ok()?;
        let mut counts = IntentCounts::default();
        for intent in manifest.intents.values() {
            counts.total += 1;
            match intent.status.as_str() {
                "backlog" => counts.backlog += 1,
                "planned" => counts.planned += 1,
                "in_progress" => counts.in_progress += 1,
                "needs-review" => counts.needs_review += 1,
                "done" => counts.done += 1,
                "icebox" => counts.icebox += 1,
                _ => counts.other += 1,
            }
        }
        Some(counts)
    })();
    match result {
        Some(value) => Observation::available(value),
        None => Observation::unavailable("vault manifest unavailable or unreadable"),
    }
}

pub(crate) fn collect(workspace: &Result<Option<PathBuf>, SourceError>) -> RepositoryMetrics {
    let root = match workspace {
        Ok(Some(root)) => root,
        Ok(None) => {
            return RepositoryMetrics::NotApplicable {
                reason: "not inside an Atomic workspace",
            }
        }
        Err(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "workspace cannot be determined",
                database_bytes: None,
            }
        }
    };
    let dot = match atomic_repository::Repository::canonical_dot_dir(root) {
        Ok(dot) => dot,
        Err(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "repository database location unavailable",
                database_bytes: None,
            }
        }
    };
    let path = dot.join(atomic_repository::DATABASE_FILE);
    let size = std::fs::metadata(&path).ok().map(|m| m.len());
    // A legacy layout (including an interrupted merge) cannot be interpreted
    // as the authoritative current database without the migration protocol.
    match std::fs::metadata(dot.join(atomic_repository::LEGACY_PRISTINE_FILE)) {
        Ok(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "legacy repository layout requires migration",
                database_bytes: size,
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "repository layout cannot be verified",
                database_bytes: size,
            };
        }
    }
    let database = match redb::ReadOnlyDatabase::open(&path) {
        Ok(database) => database,
        Err(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "repository database cannot be read or is in use",
                database_bytes: size,
            }
        }
    };
    let txn = match database.begin_read() {
        Ok(txn) => txn,
        Err(_) => {
            return RepositoryMetrics::Unavailable {
                reason: "repository database cannot be read",
                database_bytes: size,
            }
        }
    };
    let changes = txn
        .open_table(CHANGE_META)
        .map_err(|_| ())
        .and_then(|table| statistics(&table));
    let views = txn
        .open_table(VIEWS)
        .map_err(|_| ())
        .and_then(|table| statistics(&table));
    let vault = txn
        .open_table(VAULT_ENTRIES)
        .map_err(|_| ())
        .and_then(|table| statistics(&table));
    let graph = txn
        .open_multimap_table(GRAPH)
        .map_err(|_| ())
        .and_then(|table| statistics(&table));
    // Changes used by the graph are registered here; their content is stored
    // in change files, not necessarily in redb's optional CHANGE_META table.
    let registered_changes = match (|| {
        let table = txn.open_table(NODE_TYPES).ok()?;
        let mut count = 0u64;
        for row in table.iter().ok()? {
            let (_, kind) = row.ok()?;
            if kind.value() == node_type::CHANGE {
                count = count.checked_add(1)?;
            }
        }
        Some(count)
    })() {
        Some(count) => Observation::available(count),
        None => Observation::unavailable("registered change count unavailable"),
    };
    let view_count = match &views {
        Ok(value) => Observation::available(value.entries),
        Err(()) => Observation::unavailable("view count unavailable"),
    };
    RepositoryMetrics::Available {
        database_bytes: match std::fs::metadata(&path) {
            Ok(metadata) => Observation::available(metadata.len()),
            Err(_) => Observation::unavailable("database size unavailable"),
        },
        registered_changes,
        views: view_count,
        intents: intents(&txn),
        current_view: current_view(root),
        tables: vec![
            table_metric("change_meta", changes),
            table_metric("views", views),
            table_metric("vault_entries", vault),
            table_metric("graph", graph),
        ],
        last_compacted: Observation::unavailable("last compaction is not recorded"),
    }
}
