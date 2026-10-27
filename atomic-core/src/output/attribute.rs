//! Materialization and status projections for graph-backed inode attributes.

use std::collections::HashSet;

use crate::change::{InodeAttr, InodeAttrName, InodeKind};
use crate::pristine::{InodeAttrEvent, InodeAttrTxnT, PristineResult};
use crate::types::{NodeId, Position};

/// Defaults used when a legacy inode has no graph-backed attribute events.
pub const DEFAULT_REGULAR_MODE: u16 = 0o644;

/// The POSIX mode a freshly created symlink carries on this platform.
///
/// Linux `symlink(2)` ignores umask and always yields full permissions
/// (`0o777`); macOS applies umask and typically yields `0o755`. Symlink
/// permission bits are not settable through `std` on Linux (there is no
/// `lchmod`), so leased effect plans must expect the platform's creation
/// mode for `FileKind::Symlink` — never the inode's stored mode, which
/// records Git's kind (120000) semantics rather than physical bits.
pub fn platform_symlink_mode() -> u16 {
    #[cfg(target_os = "linux")]
    {
        0o777
    }
    #[cfg(not(target_os = "linux"))]
    {
        0o755
    }
}

/// An unambiguous inode materialization plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeMaterialization {
    pub mode: u16,
    pub kind: InodeKind,
}

impl Default for InodeMaterialization {
    fn default() -> Self {
        Self {
            mode: DEFAULT_REGULAR_MODE,
            kind: InodeKind::Regular,
        }
    }
}

/// A causally concurrent attribute register that cannot be materialized silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeAttributeConflict {
    pub name: InodeAttrName,
    pub events: Vec<InodeAttrEvent>,
}

/// Result of projecting graph attribute state for one inode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeAttributeProjection {
    pub materialization: InodeMaterialization,
    pub conflicts: Vec<InodeAttributeConflict>,
}

impl InodeAttributeProjection {
    pub fn is_conflicted(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// Permission/type facts consumed by eventual porcelain `P` and `T` status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InodeStatusFacts {
    pub permissions_changed: bool,
    pub type_changed: bool,
}

/// Whether a mode difference between an expected and an observed inode is
/// a reportable fact.
///
/// A symlink's permission bits are not: POSIX ignores them, Git stores every
/// link as `120000`, and `lstat` reports `0o777` on Linux but `0o755` on
/// macOS, so comparing them reports a phantom change on one platform or the
/// other. A kind change to or from a symlink is still a type change.
pub fn permission_bits_matter(expected: InodeKind, observed: InodeKind) -> bool {
    expected != InodeKind::Symlink && observed != InodeKind::Symlink
}

/// Compare only permissions the current platform can observe. Windows' file
/// metadata exposes a readonly bit, not POSIX executable/group/other bits.
pub fn inode_permissions_changed(
    expected_kind: InodeKind,
    expected_mode: u16,
    observed_kind: InodeKind,
    observed_mode: u16,
) -> bool {
    if !permission_bits_matter(expected_kind, observed_kind) {
        return false;
    }
    if cfg!(windows) {
        (expected_mode & 0o222 == 0) != (observed_mode & 0o222 == 0)
    } else {
        expected_mode != observed_mode
    }
}

impl InodeStatusFacts {
    pub fn between(
        expected: InodeMaterialization,
        actual_mode: u16,
        actual_kind: InodeKind,
    ) -> Self {
        Self {
            permissions_changed: inode_permissions_changed(
                expected.kind,
                expected.mode,
                actual_kind,
                actual_mode,
            ),
            type_changed: expected.kind != actual_kind,
        }
    }
}

/// Resolve visible graph attributes, retaining concurrent maxima as conflicts.
pub fn project_inode_attributes<T: InodeAttrTxnT>(
    txn: &T,
    inode: Position<NodeId>,
    visible_changes: &HashSet<NodeId>,
) -> PristineResult<InodeAttributeProjection> {
    let mode = txn.resolve_inode_attr(inode, InodeAttrName::Mode, visible_changes)?;
    let kind = txn.resolve_inode_attr(inode, InodeAttrName::Kind, visible_changes)?;
    let mut projection = InodeAttributeProjection {
        materialization: InodeMaterialization::default(),
        conflicts: Vec::new(),
    };

    project_register(
        InodeAttrName::Mode,
        mode.events(),
        &mut projection,
        |materialization, value| {
            if let InodeAttr::Mode(mode) = value {
                materialization.mode = mode;
            }
        },
    );
    project_register(
        InodeAttrName::Kind,
        kind.events(),
        &mut projection,
        |materialization, value| {
            if let InodeAttr::Kind(kind) = value {
                materialization.kind = kind;
            }
        },
    );
    Ok(projection)
}

fn project_register(
    name: InodeAttrName,
    events: &[InodeAttrEvent],
    projection: &mut InodeAttributeProjection,
    apply: impl FnOnce(&mut InodeMaterialization, InodeAttr),
) {
    match events {
        [] => {}
        [event] => apply(&mut projection.materialization, event.value),
        events => projection.conflicts.push(InodeAttributeConflict {
            name,
            events: events.to_vec(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_facts_distinguish_permissions_from_type() {
        let expected = InodeMaterialization {
            mode: 0o755,
            kind: InodeKind::Regular,
        };
        assert_eq!(
            InodeStatusFacts::between(expected, 0o644, InodeKind::Regular),
            InodeStatusFacts {
                permissions_changed: !cfg!(windows),
                type_changed: false,
            }
        );
        assert!(InodeStatusFacts::between(expected, 0o755, InodeKind::Symlink).type_changed);
    }

    #[test]
    fn regular_permissions_only_compare_facts_the_platform_can_observe() {
        let changed = |expected, observed| {
            inode_permissions_changed(InodeKind::Regular, expected, InodeKind::Regular, observed)
        };
        if cfg!(windows) {
            assert!(!changed(0o644, 0o666));
            assert!(!changed(0o755, 0o666));
            assert!(!changed(0o400, 0o444));
            assert!(changed(0o644, 0o444));
            assert!(changed(0o444, 0o666));
        } else {
            assert!(changed(0o644, 0o666));
            assert!(changed(0o755, 0o644));
            assert!(!changed(0o755, 0o755));
        }
    }

    #[test]
    fn symlink_permission_bits_are_not_a_fact() {
        // Git imports record links as 0o777 while macOS `lstat` reports
        // 0o755: neither side carries meaning, so no change is reported.
        let expected = InodeMaterialization {
            mode: 0o777,
            kind: InodeKind::Symlink,
        };
        assert_eq!(
            InodeStatusFacts::between(expected, 0o755, InodeKind::Symlink),
            InodeStatusFacts::default()
        );
        // A kind change to or from a symlink is still reported as a type
        // change, never as a permission change.
        let facts = InodeStatusFacts::between(expected, 0o644, InodeKind::Regular);
        assert!(facts.type_changed);
        assert!(!facts.permissions_changed);
        assert!(permission_bits_matter(
            InodeKind::Regular,
            InodeKind::Regular
        ));
        assert!(!permission_bits_matter(
            InodeKind::Regular,
            InodeKind::Symlink
        ));
        assert!(!permission_bits_matter(
            InodeKind::Symlink,
            InodeKind::Symlink
        ));
    }
}
