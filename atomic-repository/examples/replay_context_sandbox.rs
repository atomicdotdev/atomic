//! Disposable replay only. Never opens the input repository for writing.
#[cfg(feature = "prototype-context-completion")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use atomic_core::change::{ChangeHeader, VerifiedChange};
    use atomic_core::types::{Base32, Hash};
    use atomic_repository::{record::RecordOptions, Repository};
    use std::path::Path;
    const VIEW: &str = "experimental-context";
    const SKILL: &str = "skills/triage-review/SKILL.md";
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: replay_context_sandbox <preserved-input> <new-output>".into());
    }
    let input = std::fs::canonicalize(&args[1])?;
    let output = Path::new(&args[2]);
    if output.exists() {
        return Err("refuse existing output".into());
    }
    let original_edit = std::fs::read(input.join(SKILL))?;
    let original_db = std::fs::read(input.join(".atomic/atomic.redb"))?;
    let source = Repository::open_readonly(&input)?;
    let selected: Vec<_> = source
        .get_view_changes(None)?
        .into_iter()
        .map(|(_, h)| h)
        .collect();
    std::fs::create_dir(output)?;
    let mut report = serde_json::json!({"experimental":true,"signed":false,"published":false,
        "original_count":selected.len(),"source_database":Hash::of(&original_db),
        "completion_inserted":false,"record_attempted":false,"second_replay":false});
    let attempt = (|| -> Result<(), Box<dyn std::error::Error>> {
        fn sandbox(path: &Path) -> Result<Repository, Box<dyn std::error::Error>> {
            let mut repo = Repository::init(path)?;
            repo.create_draft_view(VIEW, "dev")?;
            repo.switch_view(repo.require_working_copy_id()?, VIEW)?;
            Ok(repo)
        }
        fn transfer(
            from: &Repository,
            to: &Repository,
            hashes: &[Hash],
        ) -> Result<(), Box<dyn std::error::Error>> {
            for hash in hashes {
                let bytes = std::fs::read(from.change_store().change_path(hash))?;
                if VerifiedChange::from_bytes(&bytes)?.hash() != *hash {
                    return Err("original hash mismatch".into());
                }
                to.import_change_bytes(hash, &bytes)?;
                if std::fs::read(to.change_store().change_path(hash))? != bytes {
                    return Err("changed stored bytes".into());
                }
            }
            Ok(())
        }
        let first = sandbox(&output.join("first"))?;
        transfer(&source, &first, &selected)?;
        let consumer = Hash::from_base32(b"FWMGY5TBGUV5RJOMN6F4CXR5UT3DX3YNSRAWS2WBZHECZDFU6Y3A")
            .ok_or("bad consumer")?;
        let completion = first.prototype_create_completion(&selected, consumer, 15)?;
        report["completion"] = serde_json::json!(completion);
        let mut roots = selected.clone();
        roots.push(completion);
        first.prototype_insert_composed(&[completion])?;
        report["completion_inserted"] = true.into();
        report["completed_scope_count"] = first.get_view_changes(None)?.len().into();
        report["context"] = first.prototype_context_report()?;
        let completed_hashes: Vec<_> = first
            .get_view_changes(None)?
            .into_iter()
            .map(|(_, h)| h)
            .collect();
        let completion_replay = sandbox(&output.join("completion-replay"))?;
        transfer(&first, &completion_replay, &completed_hashes)?;
        completion_replay.prototype_insert_composed(&completed_hashes)?;
        if completion_replay.view_set_id(VIEW)? != first.view_set_id(VIEW)? {
            return Err("completed scope replay changed its selected patch set".into());
        }
        first.materialize_view_to(VIEW, first.root())?;
        completion_replay.materialize_view_to(VIEW, completion_replay.root())?;
        let mut paths = std::collections::BTreeSet::new();
        for hash in &completed_hashes {
            paths.extend(
                first
                    .load_change(hash)?
                    .hunks()
                    .iter()
                    .filter_map(|h| h.path().map(str::to_owned)),
            );
        }
        for path in paths {
            let left = first.root().join(&path);
            let right = completion_replay.root().join(&path);
            if (left.is_file() || right.is_file()) && std::fs::read(left)? != std::fs::read(right)?
            {
                return Err("completed scope replay changed materialized bytes".into());
            }
        }
        report["completion_replayed_without_sidecar"] = true.into();
        let next_consumer =
            Hash::from_base32(b"HFAP35CGF5DYCB24NZIQ74IWTC5BG2BH6WJ4UA3HT75GL4IMBIMA")
                .ok_or("bad second consumer")?;
        let mut completed_symbols = std::collections::BTreeSet::new();
        let mut completions = Vec::new();
        loop {
            match first.prototype_insert_composed(&roots) {
                Ok(_) => break,
                Err(error) => {
                    // Bounded diagnostic driver, not a production error parser
                    // or implicit insertion policy. Only this exact consumer's
                    // reported missing owner may trigger a new stored patch;
                    // every patch independently requires a complete witness.
                    let text = error.to_string();
                    let marker = format!("while composing {next_consumer}: Invalid change: inode prototype: no typed owner in explicit graph context for symbol ");
                    let Some((_, symbol)) = text.rsplit_once(&marker) else {
                        return Err(error.into());
                    };
                    let symbol: u64 = symbol.parse()?;
                    if completed_symbols.len() >= 32 || !completed_symbols.insert(symbol) {
                        return Err(error.into());
                    }
                    let before = first.view_set_id(VIEW)?;
                    let completion =
                        first.prototype_create_completion(&selected, next_consumer, symbol)?;
                    if first.view_set_id(VIEW)? != before {
                        return Err("completion factory changed membership".into());
                    }
                    roots.push(completion);
                    completions.push(serde_json::json!({"consumer":next_consumer,"symbol":symbol,"completion":completion}));
                    report["additional_completions"] = serde_json::json!(completions);
                }
            }
        }
        report["completion_inserted"] = true.into();
        report["context"] = first.prototype_context_report()?;
        first.materialize_view_to(VIEW, first.root())?;
        let original_paths: std::collections::BTreeSet<_> = selected
            .iter()
            .map(|hash| first.load_change(hash))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .flat_map(|change| {
                change
                    .hunks()
                    .iter()
                    .filter_map(|h| h.path().map(str::to_owned))
            })
            .collect();
        let mut baseline = std::collections::BTreeMap::new();
        for path in original_paths {
            baseline.insert(path.clone(), first.get_file_content_on_view(&path, VIEW)?);
        }
        std::fs::write(first.root().join(SKILL), &original_edit)?;
        report["record_attempted"] = true.into();
        let candidate = first.record(
            first.require_working_copy_id()?,
            ChangeHeader::new("EXPERIMENTAL SANDBOX: preserved triage-review 1.1.1 edit"),
            RecordOptions::new()
                .paths(vec![SKILL])
                .save_to_store(true)
                .apply_after_record(true),
        )?;
        report["candidate"] = serde_json::json!(candidate.hash());
        first.materialize_view_to(VIEW, first.root())?;
        for (path, bytes) in &baseline {
            if path != SKILL && first.get_file_content_on_view(path, VIEW)? != *bytes {
                return Err(format!("record altered unrelated file {path}").into());
            }
        }
        report["unrelated_files_unchanged"] = true.into();
        let replay_hashes: Vec<_> = first
            .get_view_changes(None)?
            .into_iter()
            .map(|(_, h)| h)
            .collect();
        let second = sandbox(&output.join("second"))?;
        transfer(&first, &second, &replay_hashes)?;
        second.prototype_insert_composed(&[*candidate.hash()])?;
        second.materialize_view_to(VIEW, second.root())?;
        if std::fs::read(second.root().join(SKILL))? != original_edit {
            return Err("fresh replay did not preserve the skill edit".into());
        }
        transfer(&source, &second, &selected)?;
        report["second_replay"] = true.into();
        report["candidate_root_replay"] = true.into();
        second.prototype_insert_composed(&replay_hashes)?;

        // An actual independent graph/content patch, not just an empty marker.
        report["stage"] = "independent-producer".into();
        let producer = sandbox(&output.join("independent-producer"))?;
        let probe_path = "commutation-probe.txt";
        std::fs::write(producer.root().join(probe_path), b"independent patch\n")?;
        producer.add(
            producer.require_working_copy_id()?,
            probe_path,
            atomic_repository::TrackingOptions::default(),
        )?;
        let probe = producer.record(
            producer.require_working_copy_id()?,
            ChangeHeader::new("EXPERIMENTAL: independent file patch"),
            RecordOptions::new()
                .paths(vec![probe_path])
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )?;
        if !probe.change().dependencies().is_empty() {
            return Err("commutation probe must be independent".into());
        }
        transfer(&producer, &second, &[*probe.hash()])?;
        report["stage"] = "independent-after".into();
        second.prototype_insert_composed(&[*probe.hash()])?;
        let reordered = sandbox(&output.join("independent-first"))?;
        use atomic_core::pristine::{GraphTxnT, MutTxnT};
        for i in 0..7 {
            let padding = reordered.save_change(&atomic_core::change::Change::empty(
                ChangeHeader::new(format!("unselected allocation padding {i}")),
            ))?;
            let mut txn = reordered.pristine().write_txn()?;
            txn.register_change(&padding)?;
            txn.commit()?;
        }
        transfer(&producer, &reordered, &[*probe.hash()])?;
        report["stage"] = "independent-before".into();
        reordered.prototype_insert_composed(&[*probe.hash()])?;
        let mut reverse_transfer = replay_hashes.clone();
        reverse_transfer.reverse();
        transfer(&first, &reordered, &reverse_transfer)?;
        reordered.prototype_insert_composed(&reverse_transfer)?;
        report["stage"] = "commutation-content-check".into();
        if second.view_set_id(VIEW)? != reordered.view_set_id(VIEW)? {
            return Err("independent compositions have different SetIds".into());
        }
        second.materialize_view_to(VIEW, second.root())?;
        reordered.materialize_view_to(VIEW, reordered.root())?;
        for path in baseline
            .keys()
            .map(String::as_str)
            .chain(std::iter::once(probe_path))
        {
            // materialize_view_to is additive and may leave stale bytes for a
            // historically deleted path. Compare canonical presence/content,
            // then verify materialized bytes only for present entries.
            let left = second.get_file_content_on_view(path, VIEW)?;
            let right = reordered.get_file_content_on_view(path, VIEW)?;
            if left != right {
                return Err(
                    format!("independent change failed content commutation for {path}").into(),
                );
            }
            if let Some(bytes) = left {
                if std::fs::read(second.root().join(path))? != bytes
                    || std::fs::read(reordered.root().join(path))? != bytes
                {
                    return Err(format!(
                        "materialization disagrees with commuted projection for {path}"
                    )
                    .into());
                }
            }
        }
        let left_id = second
            .pristine()
            .read_txn()?
            .get_internal(candidate.hash())?;
        let right_id = reordered
            .pristine()
            .read_txn()?
            .get_internal(candidate.hash())?;
        if left_id == right_id {
            return Err("allocation control did not shift candidate identity".into());
        }
        report["commutation_different_allocations"] = true.into();
        report["commutation_set_id"] = serde_json::json!(second.view_set_id(VIEW)?);
        fn semantic_snapshot(
            repo: &Repository,
            path: &str,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
            use atomic_core::crdt::tables::{decode_branch_id, decode_leaf_id, encode_trunk_id};
            use atomic_core::pristine::CrdtTxnT;
            let txn = repo.pristine().read_txn()?;
            let trunk = txn
                .get_trunk_by_path(path)?
                .ok_or("semantic file is missing")?;
            let external = |id| -> Result<Hash, Box<dyn std::error::Error>> {
                Ok(txn.get_external(id)?.ok_or("semantic owner hash missing")?)
            };
            let mut branches = std::collections::BTreeMap::new();
            for branch_key in txn.iter_trunk_branches(&encode_trunk_id(&trunk))? {
                let branch_key = branch_key?;
                let id = decode_branch_id(&branch_key);
                let row = txn
                    .get_crdt_branch(&branch_key)?
                    .ok_or("semantic line missing")?;
                if row.trunk_id != trunk {
                    return Err("line ownership disagrees with semantic file".into());
                }
                let after = txn.get_crdt_branch_after(&branch_key)?;
                let after = match after {
                    Some(key) if key != [0; 12] => {
                        let id = decode_branch_id(&key);
                        serde_json::json!([external(id.change_id())?, id.branch_idx()])
                    }
                    Some(_) => serde_json::json!("start"),
                    None => serde_json::Value::Null,
                };
                let mut leaves = std::collections::BTreeMap::new();
                for key in txn.iter_branch_leaves(&branch_key)? {
                    let key = key?;
                    let leaf = decode_leaf_id(&key);
                    let row = txn.get_crdt_leaf(&key)?.ok_or("semantic token missing")?;
                    if row.branch_id != id {
                        return Err("token ownership disagrees with line".into());
                    }
                    leaves.insert(
                        (external(leaf.change_id())?, leaf.leaf_idx()),
                        serde_json::json!({
                        "kind":format!("{:?}",row.kind),"state":format!("{:?}",row.state),
                        "content_range":[row.content_start,row.content_end]}),
                    );
                }
                branches.insert(
                    (external(id.change_id())?, id.branch_idx()),
                    serde_json::json!({
                    "after":after,"state":format!("{:?}",row.state),"line_hash":row.line_hash,
                    "leaves":leaves.into_iter().collect::<Vec<_>>()}),
                );
            }
            if branches.is_empty() {
                return Err("semantic snapshot unexpectedly empty".into());
            }
            Ok(
                serde_json::json!({"trunk":[external(trunk.change_id())?,trunk.file_idx()],
                "branches":branches.into_iter().collect::<Vec<_>>()}),
            )
        }
        for path in [SKILL, probe_path] {
            if semantic_snapshot(&second, path)? != semantic_snapshot(&reordered, path)? {
                return Err(
                    format!("independent change failed semantic commutation for {path}").into(),
                );
            }
        }
        report["commutation_semantic_names_order_tokens"] = true.into();
        Ok(())
    })();
    let preserved = std::fs::read(input.join(SKILL))? == original_edit
        && std::fs::read(input.join(".atomic/atomic.redb"))? == original_db;
    report["source_preserved"] = preserved.into();
    report["success"] = attempt.is_ok().into();
    if let Err(error) = &attempt {
        report["refusal"] = error.to_string().into();
    }
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !preserved {
        return Err("input changed during experiment; no overwrite authorized".into());
    }
    attempt
}

#[cfg(not(feature = "prototype-context-completion"))]
fn main() {
    eprintln!(
        "requires --features prototype-context-completion; disposable sandbox experiment only"
    );
    std::process::exit(2);
}
