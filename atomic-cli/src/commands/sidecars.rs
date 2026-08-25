//! Sidecar object import for sync commands (pull, clone).
//!
//! A `SyncPack` carries more than changes and view snapshots: push queues
//! **provenance graphs** and **attestations** for the changes it publishes,
//! and the receiving repository must ingest them or `atomic change <hash>`
//! loses its agent-provenance section until a later "repair" pull happens.
//! This module owns that ingestion so pull and clone share one path.
//!
//! The model mirrors `.change` files: objects are content-addressed
//! (`key = blake3(bytes)`), so import is
//!
//! 1. parse the advertised key,
//! 2. skip what the repository already has,
//! 3. deserialize, verifying the computed hash matches the advertised key,
//! 4. save — which registers the node, its DEPS edges, and (for provenance)
//!    the session ledger/index.
//!
//! Corrupt or hash-mismatched sidecars are warned about and skipped: a bad
//! sidecar must never fail ingestion of the valid changes traveling beside it.

use atomic_core::types::{Base32, Hash};
use atomic_objects::{ObjectFamily, SyncPack};
use atomic_repository::Repository;

use crate::output::{print_warning, success};

/// How many sidecars were imported, by family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SidecarStats {
    /// Provenance graphs saved (skipped-and-existing not counted).
    pub provenance: usize,
    /// Attestations saved (skipped-and-existing not counted).
    pub attestations: usize,
}

impl SidecarStats {
    /// Whether anything was imported.
    pub fn is_empty(&self) -> bool {
        self.provenance == 0 && self.attestations == 0
    }
}

/// Import every provenance graph and attestation carried by `pack`.
///
/// Shared by `atomic pull` and `atomic clone`: both receive the same
/// `SyncPack` shape from `/code`, and both must land the sidecars or the
/// cloned/pulled repository cannot answer provenance queries until some
/// later pull happens to carry them again. Failures warn and continue —
/// valid change ingestion is never blocked by a corrupt sidecar.
pub fn import_sidecars(repo: &Repository, pack: &SyncPack) -> SidecarStats {
    let mut stats = SidecarStats::default();

    for (key, data) in objects_by_key(pack, ObjectFamily::Provenance) {
        let Some(hash) = Hash::from_base32(key.as_bytes()) else {
            continue;
        };
        if repo.has_provenance_graph(&hash) {
            continue;
        }
        match atomic_core::change::ProvenanceGraph::deserialize(&data) {
            Ok((graph, computed)) => {
                if computed != hash {
                    print_warning(&format!(
                        "Provenance {} failed hash verification — skipped",
                        short(&key)
                    ));
                    continue;
                }
                match repo.save_provenance_graph(&graph) {
                    Ok(_) => stats.provenance += 1,
                    Err(e) => print_warning(&format!(
                        "Failed to register provenance {}: {}",
                        short(&key),
                        e
                    )),
                }
            }
            Err(e) => print_warning(&format!("Corrupt provenance {}: {}", short(&key), e)),
        }
    }

    for (key, data) in objects_by_key(pack, ObjectFamily::Attest) {
        let Some(hash) = Hash::from_base32(key.as_bytes()) else {
            continue;
        };
        if repo.has_attestation(&hash) {
            continue;
        }
        match atomic_core::change::Attestation::deserialize(&data) {
            Ok((attestation, computed)) => {
                if computed != hash {
                    print_warning(&format!(
                        "Attestation {} failed hash verification — skipped",
                        short(&key)
                    ));
                    continue;
                }
                match repo.save_attestation(&attestation) {
                    Ok(_) => stats.attestations += 1,
                    Err(e) => print_warning(&format!(
                        "Failed to register attestation {}: {}",
                        short(&key),
                        e
                    )),
                }
            }
            Err(e) => print_warning(&format!("Corrupt attestation {}: {}", short(&key), e)),
        }
    }

    stats
}

/// Print the one-line ingestion summary pull and clone share.
pub fn report_sidecars(stats: SidecarStats) {
    if stats.provenance > 0 {
        println!(
            "  {} {}",
            success("\u{2713}"),
            count(stats.provenance, "provenance graph")
        );
    }
    if stats.attestations > 0 {
        println!(
            "  {} {}",
            success("\u{2713}"),
            count(stats.attestations, "attestation")
        );
    }
}

/// Index a pack's objects of one family into `content key → bytes`.
fn objects_by_key<'a>(
    pack: &'a SyncPack,
    family: ObjectFamily,
) -> impl Iterator<Item = (String, Vec<u8>)> + 'a {
    pack.objects
        .iter()
        .filter(move |o| o.family == family)
        .map(|o| (o.key.clone(), o.bytes.clone()))
}

/// First 12 chars of a base32 key, for display.
fn short(key: &str) -> &str {
    &key[..12.min(key.len())]
}

fn count(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("{} {}", n, singular)
    } else {
        format!("{} {}s", n, singular)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use atomic_core::change::attestation::{AttestAgent, Attestation};
    use atomic_core::change::ProvenanceGraph;
    use atomic_objects::{ObjectRecord, RefRecord};

    /// A minimal provenance graph explaining `change`.
    fn provenance_for(change: Hash) -> ProvenanceGraph {
        ProvenanceGraph::builder("session-sidecar", "opencode")
            .add_change_explained(change)
            .build()
    }

    /// A minimal attestation covering `change`.
    fn attestation_for(change: Hash) -> Attestation {
        Attestation::builder(
            "session-sidecar",
            AttestAgent::new("opencode", "OpenCode", "atomic"),
        )
        .add_change(change)
        .build()
    }

    fn pack_with(objects: Vec<ObjectRecord>) -> SyncPack {
        SyncPack {
            objects,
            refs: vec![RefRecord {
                name: "dev".to_string(),
                expect_old: None,
                new_target: "SNAP".to_string(),
            }],
        }
    }

    #[test]
    fn imports_provenance_and_attestations_from_pack() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();

        // A real change, saved and applied so it is registered in the graph —
        // the same order clone/pull produce (changes applied, then sidecars
        // imported; `save_provenance_graph` only wires DEPS for
        // already-registered changes).
        let change = atomic_core::change::Change::new(
            atomic_core::change::ChangeHeader::builder()
                .message("sidecar test change")
                .build(),
            vec![],
            vec![],
            vec![],
        );
        let change_hash = repo.save_change(&change).expect("save change");
        if !repo.view_exists("dev").expect("view exists check") {
            repo.create_shared_view("dev").expect("create view");
        }
        atomic_repository::Repository::insert_change(
            &repo,
            &change_hash,
            atomic_repository::InsertOptions::default().view("dev"),
        )
        .expect("insert change");

        let graph = provenance_for(change_hash);
        let prov_bytes = graph.serialize().unwrap();
        let prov_key = Hash::of(&prov_bytes).to_base32();

        let attest = attestation_for(change_hash);
        let attest_bytes = attest.serialize().unwrap();
        let attest_key = Hash::of(&attest_bytes).to_base32();

        let pack = pack_with(vec![
            ObjectRecord::new(ObjectFamily::Change, change_hash.to_base32(), b"".to_vec()),
            ObjectRecord::new(
                ObjectFamily::Provenance,
                prov_key.clone(),
                prov_bytes.clone(),
            ),
            ObjectRecord::new(
                ObjectFamily::Attest,
                attest_key.clone(),
                attest_bytes.clone(),
            ),
        ]);

        let stats = import_sidecars(&repo, &pack);
        assert_eq!(
            stats,
            SidecarStats {
                provenance: 1,
                attestations: 1
            }
        );

        // The exact query `atomic change <hash>` performs.
        let found = repo
            .find_provenance_for_change(&change_hash)
            .expect("find provenance");
        assert_eq!(found.len(), 1, "one graph explains the change");
        assert_eq!(found[0].1.changes_explained, vec![change_hash]);

        let (_, attest_hash) = Attestation::deserialize(&attest_bytes).unwrap();
        assert!(repo.has_attestation(&attest_hash));

        // Idempotent: a second import skips everything already present.
        let again = import_sidecars(&repo, &pack);
        assert_eq!(again, SidecarStats::default());
    }

    #[test]
    fn skips_corrupt_and_mismatched_sidecars_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let change = Hash::of(b"change-bytes");

        // Valid graph bytes, but advertised under the WRONG key.
        let graph = provenance_for(change);
        let prov_bytes = graph.serialize().unwrap();
        let wrong_key = Hash::of(b"not-the-graph").to_base32();

        // Valid attestation bytes under a wrong key too.
        let attest = attestation_for(change);
        let attest_bytes = attest.serialize().unwrap();
        let wrong_attest_key = Hash::of(b"not-the-attest").to_base32();

        let pack = pack_with(vec![
            ObjectRecord::new(ObjectFamily::Provenance, wrong_key, prov_bytes),
            ObjectRecord::new(ObjectFamily::Attest, wrong_attest_key, attest_bytes),
            // Corrupt bodies: neither deserializes.
            ObjectRecord::new(
                ObjectFamily::Provenance,
                Hash::of(b"corrupt-prov").to_base32(),
                b"garbage".to_vec(),
            ),
            ObjectRecord::new(
                ObjectFamily::Attest,
                Hash::of(b"corrupt-attest").to_base32(),
                b"garbage".to_vec(),
            ),
        ]);

        let stats = import_sidecars(&repo, &pack);
        assert_eq!(stats, SidecarStats::default(), "nothing may be saved");
    }

    #[test]
    fn empty_pack_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let stats = import_sidecars(&repo, &SyncPack::default());
        assert_eq!(stats, SidecarStats::default());
    }

    #[test]
    fn import_before_registration_leaves_files_but_no_rev_deps() {
        // Documents the ordering contract: a graph imported while its change
        // is NOT yet registered still lands on disk (nothing is lost), but
        // `find_provenance_for_change` — the REV_DEPS lookup `atomic change`
        // uses — cannot see it. That is why clone/pull import sidecars
        // AFTER applying view manifests.
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let change = Hash::of(b"unregistered-change");
        let graph = provenance_for(change);
        let prov_bytes = graph.serialize().unwrap();
        let prov_key = Hash::of(&prov_bytes).to_base32();

        let pack = pack_with(vec![ObjectRecord::new(
            ObjectFamily::Provenance,
            prov_key,
            prov_bytes,
        )]);

        let stats = import_sidecars(&repo, &pack);
        assert_eq!(stats.provenance, 1, "the graph file is saved");

        let via_rev_deps = repo.find_provenance_for_change(&change).unwrap();
        assert!(via_rev_deps.is_empty(), "REV_DEPS cannot see it yet");

        let via_scan = repo.find_provenance_for_change_scan(&change).unwrap();
        assert_eq!(via_scan.len(), 1, "the disk-scan fallback does");
    }
}
