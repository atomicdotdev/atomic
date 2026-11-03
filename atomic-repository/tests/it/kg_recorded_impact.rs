use std::collections::BTreeSet;

use atomic_core::{
    change::{ops::FileOps, Change, ChangeHeader},
    crdt::TrunkId,
    kg::{iri, ontology::Predicate as P},
    types::{Hash, NodeId},
};
use atomic_repository::Repository;

fn import_recorded_files(repo: &Repository, paths: &[&str]) -> Hash {
    // Stored semantic operations are sufficient for impact projection; no
    // working-copy files or view membership are required.
    let file_ops = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            FileOps::create(
                TrunkId::new(NodeId::new(1), index as u32),
                (*path).into(),
                None,
            )
        })
        .collect();
    let change = Change::with_file_ops(
        ChangeHeader::new("historically recorded files"),
        vec![],
        file_ops,
        vec![],
        vec![],
    );
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    hash
}

fn impact_facts(repo: &Repository, hash: &Hash) -> BTreeSet<(String, String)> {
    let change = iri::mint_change(hash);
    repo.workflow_snapshot()
        .unwrap()
        .edges
        .into_iter()
        .filter(|edge| edge.from_id == change && edge.kind == P::ModifiesFile)
        .map(|edge| (edge.from_id, edge.to_id))
        .collect()
}

#[test]
fn recorded_ignored_file_still_projects_modifies_file() {
    for ignore_file in [".atomicignore", ".gitignore", ".ignore"] {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.init_kg().unwrap();
        let path = "historical/generated.rs";
        std::fs::write(directory.path().join(ignore_file), "historical/\n").unwrap();

        let hash = import_recorded_files(&repo, &[path]);
        let file = iri::mint_file(&repo.project_id().unwrap(), path);
        assert_eq!(
            impact_facts(&repo, &hash),
            BTreeSet::from([(iri::mint_change(&hash), file.clone())]),
            "{ignore_file} must not hide recorded impact"
        );
        let node = repo.vault_kg_node(&file).unwrap().unwrap();
        assert_eq!(node.metadata.unwrap()["path"], path);
    }
}

#[test]
fn rebuilding_after_ignore_changes_preserves_recorded_impact_facts() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    let paths = ["historical/first.rs", "historical/second.rs"];
    let hash = import_recorded_files(&repo, &paths);
    let expected: BTreeSet<_> = paths
        .iter()
        .map(|path| {
            (
                iri::mint_change(&hash),
                iri::mint_file(&repo.project_id().unwrap(), path),
            )
        })
        .collect();
    assert_eq!(impact_facts(&repo, &hash), expected);

    for ignore_file in [".atomicignore", ".gitignore", ".ignore"] {
        for rules in ["historical/\n", "# no longer ignored\n"] {
            std::fs::write(directory.path().join(ignore_file), rules).unwrap();
            assert!(repo.rebuild_kg(true).unwrap());
            assert_eq!(
                impact_facts(&repo, &hash),
                expected,
                "rebuilding with {ignore_file} containing {rules:?} changed historical impact"
            );
        }
    }
}
