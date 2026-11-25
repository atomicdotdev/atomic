//! Frozen publication subjects. Collection order never chooses a winning tree.
use std::collections::{BTreeMap, BTreeSet};

use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{intent_bridge, Repository, ViewEntry, ViewEntryKind};
use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliResult};

pub(super) fn invalid(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ViewPin {
    pub name: String,
    pub state: String,
    pub members: BTreeSet<String>,
    pub roots: Vec<String>,
    pub set_id: String,
    pub chain: Vec<SourcePin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SourcePin {
    pub name: String,
    pub state: String,
    pub scope: String,
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Scope {
    pub project: String,
    pub views: Vec<ViewPin>,
    pub source_session: Option<String>,
    pub source_manifest: Option<String>,
    pub roots: Vec<String>,
    pub closure: Vec<String>,
    pub intent_sources: BTreeMap<String, String>,
}

pub(super) fn view_names(value: &str) -> CliResult<Vec<String>> {
    let mut names = BTreeSet::new();
    for part in value.split(',') {
        let name = part.trim();
        if name.is_empty() || name.chars().any(|c| c.is_control() || "*?[]{}".contains(c)) {
            return Err(invalid(
                "--view requires literal nonempty view names separated by commas",
            ));
        }
        names.insert(name.to_string());
        if names.len() > 256 {
            return Err(invalid(
                "publication collection exceeds 256 views; no names were truncated",
            ));
        }
    }
    Ok(names.into_iter().collect())
}

pub(super) fn hashes(values: &[String]) -> CliResult<Vec<Hash>> {
    values
        .iter()
        .map(|value| {
            Hash::from_base32(value.as_bytes())
                .filter(|h| h.to_base32() == *value)
                .ok_or_else(|| invalid("review scope contains a noncanonical change hash"))
        })
        .collect()
}

fn frontier(repo: &Repository, members: &BTreeSet<String>) -> CliResult<Vec<String>> {
    let mut roots = members.clone();
    for hash in hashes(&members.iter().cloned().collect::<Vec<_>>())? {
        let change = repo.load_change(&hash).map_err(CliError::Repository)?;
        for dependency in change.dependencies() {
            roots.remove(&dependency.to_base32());
        }
    }
    if roots.len() > 64 {
        return Err(invalid("scope exceeds the canonical publication limit of 64 independent roots; nothing was truncated"));
    }
    Ok(roots.into_iter().collect())
}

fn projection_members(repo: &Repository, name: &str) -> CliResult<BTreeSet<String>> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| invalid(e.to_string()))?;
    let view = txn
        .get_view(name)
        .map_err(|e| invalid(e.to_string()))?
        .ok_or_else(|| invalid(format!("unknown publication view '{name}'")))?;
    let closure = atomic_repository::repository::effective_projection_closure(&txn, &view)
        .map_err(CliError::Repository)?;
    closure
        .iter_dependency_first()
        .map(|id| {
            txn.get_external(*id)
                .map_err(|e| invalid(e.to_string()))?
                .map(|h| h.to_base32())
                .ok_or_else(|| {
                    invalid("publication projection member has no external change identity")
                })
        })
        .collect()
}

pub(super) fn resolve(
    repo: &Repository,
    views: Option<&str>,
    session: Option<&str>,
) -> CliResult<Scope> {
    let mut source_manifest = None;
    let mut session_roots = None;
    let names = if let Some(session) = session {
        let (record, turns) = repo
            .get_session_ledger(session)
            .map_err(CliError::Repository)?
            .ok_or_else(|| invalid(format!("unknown session '{session}'")))?;
        if record.status.incomplete().is_some() {
            return Err(invalid("source session has incomplete execution; review its recorded changes explicitly before publication"));
        }
        let roots: BTreeSet<_> = turns
            .iter()
            .flat_map(|t| &t.change_hashes)
            .map(Base32::to_base32)
            .collect();
        if roots.is_empty() {
            return Err(invalid(
                "pending: source session has no recorded changes to publish",
            ));
        }
        session_roots = Some(roots);
        source_manifest = repo
            .get_session_head(session)
            .map_err(CliError::Repository)?
            .map(|h| h.to_base32());
        vec![record
            .view_name
            .ok_or_else(|| invalid("source session has no recorded view binding"))?]
    } else {
        view_names(views.unwrap_or(repo.current_view()))?
    };
    let mut pins = Vec::new();
    let mut selected = BTreeSet::new();
    for name in names {
        let info = repo.get_view_info(&name).map_err(CliError::Repository)?;
        let direct: BTreeSet<_> = repo
            .effective_history(Some(&name))
            .map_err(CliError::Repository)?
            .into_iter()
            .map(|entry| entry.hash.to_base32())
            .collect();
        let all = projection_members(repo, &name)?;
        let members = session_roots.clone().unwrap_or(direct);
        if !members.is_subset(&all) {
            return Err(invalid(
                "source session changes are not contained in its recorded source view",
            ));
        }
        let roots = frontier(repo, &members)?;
        selected.extend(members.iter().cloned());
        let mut chain = Vec::new();
        let mut next = Some(name.clone());
        while let Some(source) = next {
            if chain.iter().any(|p: &SourcePin| p.name == source) || chain.len() >= 256 {
                return Err(invalid("cyclic or excessive publication source chain"));
            }
            let source_info = repo.get_view_info(&source).map_err(CliError::Repository)?;
            let manifest = repo.view_manifest(&source).map_err(CliError::Repository)?;
            next = source_info.parent_name.clone();
            chain.push(SourcePin {
                name: source,
                state: source_info.state_base32(),
                scope: source_info.kind_label().to_string(),
                log: manifest.changes.iter().map(Base32::to_base32).collect(),
            });
        }
        let set_id = repo
            .view_set_id(&name)
            .map_err(CliError::Repository)?
            .to_base32();
        if chain.first().map(|p| p.state.as_str()) != Some(info.state_base32().as_str())
            || repo
                .get_view_info(&name)
                .map_err(CliError::Repository)?
                .state_base32()
                != info.state_base32()
        {
            return Err(invalid("publication source changed while its scope was being frozen; retry without widening the original request"));
        }
        pins.push(ViewPin {
            name,
            state: info.state_base32(),
            members: all,
            roots,
            set_id,
            chain,
        });
    }
    if selected.is_empty() {
        return Err(invalid(
            "selected publication scope has no recorded changes",
        ));
    }
    let roots = frontier(repo, &selected)?;
    let closure =
        atomic_repository::repository::provenance_gate::reachable_closure(repo, &hashes(&roots)?)
            .map_err(CliError::Repository)?;
    if closure.len() > 4096 {
        return Err(invalid(
            "publication dependency closure exceeds 4096 changes; nothing was truncated",
        ));
    }
    let mut intent_sources = BTreeMap::new();
    for hash in &closure {
        let change = repo.load_change(hash).map_err(CliError::Repository)?;
        let graphs = repo
            .find_provenance_for_change_scan(hash)
            .map_err(CliError::Repository)?;
        let context = repo.change_intent_context(hash, &change, &graphs);
        let ids = context
            .plans
            .iter()
            .filter_map(|p| p.intent.as_ref())
            .chain(
                context
                    .associated_intents
                    .iter()
                    .filter_map(|p| p.intent.as_ref()),
            );
        for identity in ids {
            let inputs =
                intent_bridge::read_intent(repo, &identity.uid).map_err(CliError::Repository)?;
            let node = intent_bridge::lift(&inputs).map_err(CliError::Repository)?;
            if node.kind != "review" {
                intent_sources.insert(node.id, intent_bridge::source_content_hash(&inputs));
            }
        }
    }
    Ok(Scope {
        project: repo.project_id().map_err(CliError::Repository)?.to_urn(),
        views: pins,
        source_session: session.map(str::to_string),
        source_manifest,
        roots,
        closure: closure
            .iter()
            .map(Base32::to_base32)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        intent_sources,
    })
}

/// Allow only the owned review's metadata additions, never implementation drift.
pub(super) fn recheck(
    repo: &Repository,
    scope: &Scope,
    review_uid: Option<&str>,
    reviewer_session: Option<&str>,
) -> CliResult<()> {
    if repo.project_id().map_err(CliError::Repository)?.to_urn() != scope.project {
        return Err(invalid("stale publication: project identity changed"));
    }
    for (id, expected) in &scope.intent_sources {
        let inputs = intent_bridge::read_intent(repo, id).map_err(CliError::Repository)?;
        if intent_bridge::source_content_hash(&inputs) != *expected {
            return Err(invalid(format!(
                "stale publication: reviewed intent '{id}' changed"
            )));
        }
    }
    for pin in &scope.views {
        for (index, source) in pin.chain.iter().enumerate() {
            let manifest = repo
                .view_manifest(&source.name)
                .map_err(CliError::Repository)?;
            if repo
                .get_view_info(&source.name)
                .map_err(CliError::Repository)?
                .kind_label()
                != source.scope
            {
                return Err(invalid("stale publication: a source view changed scope"));
            }
            let log: Vec<_> = manifest.changes.iter().map(Base32::to_base32).collect();
            if !log.starts_with(&source.log)
                || manifest.parent.as_deref() != pin.chain.get(index + 1).map(|p| p.name.as_str())
            {
                return Err(invalid(
                    "stale publication: source ancestry or ordered membership was rewritten",
                ));
            }
        }
        let now = projection_members(repo, &pin.name)?;
        if !pin.members.is_subset(&now) {
            return Err(invalid(format!(
                "stale publication: '{}' lost recorded changes",
                pin.name
            )));
        }
        for added in now.difference(&pin.members) {
            let (Some(uid), Some(session)) = (review_uid, reviewer_session) else {
                return Err(invalid(format!(
                    "stale publication: '{}' changed after it was frozen",
                    pin.name
                )));
            };
            let hash = hashes(std::slice::from_ref(added))?[0];
            let change = repo.load_change(&hash).map_err(CliError::Repository)?;
            if change.provenance().is_empty()
                || change
                    .provenance()
                    .iter()
                    .any(|p| p.session_id.as_deref() != Some(session))
            {
                return Err(invalid(
                    "stale publication: another execution changed the source view",
                ));
            }
            let paths = repo
                .change_modified_paths(&hash)
                .map_err(CliError::Repository)?;
            let intent_path = format!(".vault/intents/{uid}/intent.md");
            let attestation_path = format!(
                ".vault/{}",
                intent_bridge::attestation_vault_path(repo, uid).map_err(CliError::Repository)?
            );
            if paths.is_empty()
                || paths
                    .iter()
                    .any(|p| p != &intent_path && p != &attestation_path)
            {
                return Err(invalid("publisher modified implementation or unrelated evidence; review scope is stale"));
            }
        }
    }
    Ok(())
}

pub(super) fn check_prepared_context(
    pin: &ViewPin,
    subject: &atomic_canonical::publication_review::ReviewSubject,
) -> CliResult<()> {
    let snapshot = subject
        .snapshot
        .as_ref()
        .ok_or_else(|| invalid("prepared publication lacks an immutable source snapshot"))?;
    if subject.effective_set_id != pin.set_id
        || snapshot.membership != pin.members.iter().cloned().collect::<Vec<_>>()
        || subject.view_chain.len() != pin.chain.len()
        || snapshot.view_logs.len() != pin.chain.len()
    {
        return Err(invalid(
            "prepared publication context differs from the approved frozen view membership",
        ));
    }
    for (index, (actual, expected)) in subject.view_chain.iter().zip(&pin.chain).enumerate() {
        if actual.name != expected.name
            || actual.state != expected.state
            || actual.scope != expected.scope
            || snapshot.view_logs[index] != expected.log
        {
            return Err(invalid(
                "prepared publication source chain/log is not the approved frozen perspective",
            ));
        }
    }
    Ok(())
}

/// Conservative composition: unequal deltas at the same path require an
/// explicit integration view. Never pick a winner based on --view order.
pub(super) fn compose(
    base: &BTreeMap<String, ViewEntry>,
    inputs: &[BTreeMap<String, ViewEntry>],
) -> CliResult<BTreeMap<String, ViewEntry>> {
    let mut deltas: BTreeMap<String, Option<ViewEntry>> = BTreeMap::new();
    for tree in inputs {
        for path in base.keys().chain(tree.keys()).collect::<BTreeSet<_>>() {
            let before = base.get(path);
            let after = tree.get(path);
            if equivalent(before, after) {
                continue;
            }
            if let Some(existing) = deltas.get(path) {
                if !equivalent(existing.as_ref(), after) {
                    return Err(invalid(format!("collection has incompatible deltas at '{path}'; record an explicit integration view first")));
                }
            } else {
                deltas.insert(path.clone(), after.cloned());
            }
        }
    }
    let mut combined = base.clone();
    for (path, entry) in deltas {
        match entry {
            Some(entry) => {
                combined.insert(path, entry);
            }
            None => {
                combined.remove(&path);
            }
        }
    }
    if combined
        .values()
        .any(|e| e.kind == ViewEntryKind::Symlink || e.conflict_marker_line.is_some())
    {
        return Err(invalid("collection composition refuses symlinks and unresolved conflicts; use an explicit integration view"));
    }
    Ok(combined)
}

fn equivalent(a: Option<&ViewEntry>, b: Option<&ViewEntry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.mode == b.mode && a.content == b.content,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: &str, bytes: &[u8]) -> ViewEntry {
        ViewEntry {
            path: path.into(),
            inode: 0,
            kind: ViewEntryKind::File,
            mode: 0o644,
            content: bytes.into(),
            hash: Hash::of(bytes),
            conflict_marker_line: None,
        }
    }
    #[test]
    fn comma_views_are_literal_sorted_and_deduplicated() {
        assert_eq!(view_names(" b,a,b ").unwrap(), ["a", "b"]);
        for bad in ["", "a,", ",a", "a,,b", "*", "a,?"] {
            assert!(view_names(bad).is_err());
        }
    }
    #[test]
    fn disjoint_collection_changes_compose_and_order_does_not_choose_a_winner() {
        let base = BTreeMap::from([("a".into(), file("a", b"old"))]);
        let a = BTreeMap::from([("a".into(), file("a", b"new"))]);
        let b = BTreeMap::from([
            ("a".into(), file("a", b"old")),
            ("b".into(), file("b", b"new")),
        ]);
        let result = compose(&base, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(result["a"].content, b"new");
        assert!(result.contains_key("b"));
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(compose(&base, &[b, a]).unwrap()).unwrap()
        );
    }
    #[test]
    fn conflicting_edits_or_delete_write_are_refused() {
        let base = BTreeMap::from([("a".into(), file("a", b"old"))]);
        let a = BTreeMap::from([("a".into(), file("a", b"one"))]);
        let b = BTreeMap::from([("a".into(), file("a", b"two"))]);
        assert!(compose(&base, &[a.clone(), b]).is_err());
        assert!(compose(&base, &[a, BTreeMap::new()]).is_err());
    }
    #[test]
    fn prepared_context_must_match_frozen_membership_and_ordered_ancestry() {
        use atomic_canonical::publication_review::{
            IntentPin, ReviewSnapshot, ReviewSubject, ReviewViewPin,
        };
        let pin = ViewPin {
            name: "v".into(),
            state: "state".into(),
            members: BTreeSet::from(["original".into()]),
            roots: vec!["original".into()],
            set_id: "set".into(),
            chain: vec![SourcePin {
                name: "v".into(),
                state: "state".into(),
                scope: "draft".into(),
                log: vec!["original".into()],
            }],
        };
        let mut subject = ReviewSubject {
            project: "p".into(),
            roots: pin.roots.clone(),
            changes: vec![],
            review: IntentPin {
                id: "review".into(),
                source_hash: "s".into(),
                signed_hash: "h".into(),
                public_key: "k".into(),
            },
            work: vec![],
            set_aside: vec![],
            profile_hash: "policy".into(),
            view_chain: vec![ReviewViewPin {
                name: "v".into(),
                state: "state".into(),
                scope: "draft".into(),
            }],
            effective_set_id: "set".into(),
            snapshot: Some(ReviewSnapshot {
                version: 1,
                view_logs: vec![vec!["original".into()]],
                membership: vec!["original".into()],
                intent_revisions: vec![],
            }),
        };
        assert!(check_prepared_context(&pin, &subject).is_ok());
        subject
            .snapshot
            .as_mut()
            .unwrap()
            .membership
            .push("foreign".into());
        assert!(check_prepared_context(&pin, &subject).is_err());
        subject.snapshot.as_mut().unwrap().membership = vec!["original".into()];
        subject.view_chain[0].state = "other".into();
        assert!(check_prepared_context(&pin, &subject).is_err());
    }

    #[test]
    fn frozen_membership_includes_dependency_only_projection_members() {
        use atomic_core::change::{Change, ChangeHeader};
        use atomic_core::pristine::MutTxnT;
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let dependency = repo
            .save_change(&Change::empty(ChangeHeader::new("dependency context")))
            .unwrap();
        let mut change = Change::empty(ChangeHeader::new("selected root"));
        change.hashed.dependencies.push(dependency);
        let root = repo.save_change(&change).unwrap();
        let mut txn = repo.pristine().write_txn().unwrap();
        let dependency_id = txn.register_change(&dependency).unwrap();
        let root_id = txn.register_change(&root).unwrap();
        txn.put_dep(root_id, dependency_id).unwrap();
        txn.put_change_deps(dependency_id, &[]).unwrap();
        txn.put_change_deps(root_id, &[dependency]).unwrap();
        let mut view = txn.get_view("dev").unwrap().unwrap();
        txn.put_change(&mut view, root_id, &root).unwrap();
        txn.update_view(&view).unwrap();
        txn.commit().unwrap();
        let frozen = resolve(&repo, Some("dev"), None).unwrap();
        assert_eq!(frozen.roots, vec![root.to_base32()]);
        assert_eq!(
            frozen.views[0].members,
            BTreeSet::from([root.to_base32(), dependency.to_base32()])
        );
        recheck(&repo, &frozen, None, None).unwrap();
    }
}
