//! Ordinary product APIs only. Original input is opened read-only; every write
//! is confined to a new disposable output. No prototype feature is required.
use atomic_core::change::{Change, ChangeHeader, VerifiedChange};
use atomic_core::pristine::span_patch_storage::SpanRenderContext;
use atomic_core::pristine::{CachedGraphTxn, GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::Hash;
use atomic_repository::{InsertOptions, RecordOptions, Repository, TrackingOptions};
use std::{collections::BTreeSet, path::Path};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const VIEW: &str = "span-skill-regression";
const SKILL: &str = "skills/triage-review/SKILL.md";
fn sandbox(path: &Path) -> Result<Repository> {
    let mut r = Repository::init(path)?;
    r.create_draft_view(VIEW, "dev")?;
    r.switch_view(r.require_working_copy_id()?, VIEW)?;
    Ok(r)
}
fn transfer(src: &Repository, dst: &Repository, hashes: &[Hash]) -> Result<()> {
    for hash in hashes {
        let bytes = std::fs::read(src.change_store().change_path(hash))?;
        if VerifiedChange::from_bytes(&bytes)?.hash() != *hash {
            return Err("source hash mismatch".into());
        }
        dst.import_change_bytes(hash, &bytes)?;
        if std::fs::read(dst.change_store().change_path(hash))? != bytes {
            return Err("transfer altered original bytes/attachments".into());
        }
    }
    Ok(())
}
fn semantic_ids(r: &Repository) -> Result<serde_json::Value> {
    let snapshot = span_state(r)?.semantic_snapshot()?;
    Ok(
        serde_json::json!({"baseline":snapshot.id,"lines":snapshot.lines.iter().map(|line|
        serde_json::json!({"id":line.id,"tokens":line.tokens.iter().map(|token|token.id).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
    )
}
fn span_state(
    r: &Repository,
) -> Result<atomic_core::pristine::span_patch_storage::SpanRecordState> {
    let (inode, _) = r
        .get_inode_and_position(SKILL)?
        .ok_or("skill inode missing")?;
    let tx = r.pristine().read_txn()?;
    let view = tx.get_view(VIEW)?.ok_or("view missing")?;
    let visible = atomic_repository::repository::effective_projection_closure(&tx, &view)?;
    let cached = CachedGraphTxn::new(&tx)?;
    let ctx = SpanRenderContext::new(&cached, r.change_store(), &visible)?;
    match ctx.record_state(inode)? {
        Some(state) => Ok(state),
        None => ctx
            .legacy_record_state(inode)?
            .ok_or_else(|| "span baseline missing".into()),
    }
}
fn graph_rows(r: &Repository) -> Result<(u64, u64)> {
    use redb::{ReadableDatabase, ReadableTableMetadata};
    let db = r
        .pristine()
        .shared_database()
        .ok_or("native database required")?;
    let txn = db.begin_read()?;
    Ok((
        txn.open_multimap_table(atomic_core::pristine::tables::GRAPH)?
            .len()?,
        txn.open_multimap_table(atomic_core::pristine::tables::INODE_GRAPH)?
            .len()?,
    ))
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: span_skill_replay <preserved-input> <new-output>".into());
    }
    let input = std::fs::canonicalize(&args[1])?;
    let output = Path::new(&args[2]);
    if output.exists() {
        return Err("refuse existing output".into());
    }
    let edit = std::fs::read(input.join(SKILL))?;
    let db = std::fs::read(input.join(".atomic/atomic.redb"))?;
    let source = Repository::open_readonly(&input)?;
    let originals = source
        .get_view_changes(None)?
        .into_iter()
        .map(|(_, h)| h)
        .collect::<Vec<_>>();
    std::fs::create_dir(output)?;
    let mut report = serde_json::json!({"prototype_features":false,"published":false,"signed":false,"recorded":false,"replayed":false,"commutation":false,"originals":originals.len()});
    let attempt = (|| -> Result<()> {
        let sender = sandbox(&output.join("sender"))?;
        transfer(&source, &sender, &originals)?;
        for hash in &originals {
            sender.insert_change_rec(hash, InsertOptions::default().view(VIEW))?;
        }
        sender.materialize_view_to(VIEW, sender.root())?;
        let paths: BTreeSet<_> = originals
            .iter()
            .map(|hash| sender.load_change(hash))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .flat_map(|c| {
                c.hunks()
                    .iter()
                    .filter_map(|op| op.path().map(str::to_owned))
            })
            .chain(std::iter::once(SKILL.into()))
            .collect();
        let before = paths
            .iter()
            .map(|p| Ok((p.clone(), sender.get_file_content_on_view(p, VIEW)?)))
            .collect::<Result<Vec<_>>>()?;
        let baseline = span_state(&sender)?;
        let authored = baseline
            .spans
            .iter()
            .map(|span| (span.target.owner, span.target.start, span.target.end))
            .collect::<BTreeSet<_>>();
        let before_rows = graph_rows(&sender)?;
        std::fs::write(sender.root().join(SKILL), &edit)?;
        report["stage"] = "record".into();
        let recorded = sender.record(
            sender.require_working_copy_id()?,
            ChangeHeader::new("span: preserved triage-review edit"),
            RecordOptions::new()
                .paths(vec![SKILL])
                .save_to_store(true)
                .apply_after_record(true),
        )?;
        let patch = recorded
            .change()
            .hashed
            .span
            .as_ref()
            .ok_or("record did not use span algebra")?;
        if patch.version != atomic_core::change::span_patch::LEGACY_SOURCE_VERSION {
            return Err("legacy context was not explicitly versioned".into());
        }
        if recorded.change().hunks().iter().any(|op| {
            matches!(
                op,
                atomic_core::change::GraphOp::Edit { .. }
                    | atomic_core::change::GraphOp::Replacement { .. }
            )
        }) {
            return Err("record fell back to linewise content graph".into());
        }
        report["recorded"] = true.into();
        report["candidate"] = serde_json::json!(recorded.hash());
        report["inserted_bytes"] = recorded.change().contents.len().into();
        report["span_ops"] = patch.ops.len().into();
        let after_rows = graph_rows(&sender)?;
        if before_rows != after_rows {
            return Err("skill edit expanded content graph rows".into());
        }
        report["graph_rows_before"] = serde_json::json!(before_rows);
        report["graph_rows_after"] = serde_json::json!(after_rows);
        let state = span_state(&sender)?;
        for span in &state.spans {
            if span.target.owner != *recorded.hash()
                && !authored.contains(&(span.target.owner, span.target.start, span.target.end))
            {
                return Err("unchanged bytes acquired a fabricated original owner".into());
            }
        }
        report["authored_ownership_preserved"] = true.into();
        for (path, bytes) in &before {
            if path != SKILL && sender.get_file_content_on_view(path, VIEW)? != *bytes {
                return Err(format!("unrelated file changed: {path}").into());
            }
        }
        let receiver = sandbox(&output.join("receiver"))?;
        let mut all = originals.clone();
        all.push(*recorded.hash());
        transfer(&sender, &receiver, &all)?;
        report["stage"] = "hash-root-replay".into();
        receiver.insert_change_rec(recorded.hash(), InsertOptions::default().view(VIEW))?;
        if receiver.get_file_content_on_view(SKILL, VIEW)?.as_ref() != Some(&edit) {
            return Err("fresh hash-root replay differs".into());
        }
        for hash in &originals {
            receiver.insert_change_rec(hash, InsertOptions::default().view(VIEW))?;
        }
        if semantic_ids(&sender)? != semantic_ids(&receiver)? {
            return Err("replay changed span semantic identities".into());
        }
        report["replayed"] = true.into();
        let probe_path = "commutation-probe.txt";
        std::fs::write(sender.root().join(probe_path), b"independent\n")?;
        sender.add(
            sender.require_working_copy_id()?,
            probe_path,
            TrackingOptions::default(),
        )?;
        let probe = sender.record(
            sender.require_working_copy_id()?,
            ChangeHeader::new("independent span probe"),
            RecordOptions::new()
                .paths(vec![probe_path])
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )?;
        if !probe.change().dependencies().is_empty() {
            return Err("probe must be independent".into());
        }
        transfer(&sender, &receiver, &[*probe.hash()])?;
        receiver.insert_change_rec(probe.hash(), InsertOptions::default().view(VIEW))?;
        let reordered = sandbox(&output.join("independent-first"))?;
        for i in 0..7 {
            let hash = reordered.save_change(&Change::empty(ChangeHeader::new(format!(
                "unselected allocation {i}"
            ))))?;
            let mut tx = reordered.pristine().write_txn()?;
            tx.register_change(&hash)?;
            tx.commit()?;
        }
        all.push(*probe.hash());
        all.reverse();
        transfer(&sender, &reordered, &all)?;
        report["stage"] = "commutation".into();
        reordered.insert_change_rec(probe.hash(), InsertOptions::default().view(VIEW))?;
        reordered.insert_change_rec(recorded.hash(), InsertOptions::default().view(VIEW))?;
        for hash in originals.iter().rev() {
            reordered.insert_change_rec(hash, InsertOptions::default().view(VIEW))?;
        }
        if receiver.view_set_id(VIEW)? != reordered.view_set_id(VIEW)? {
            return Err("commuted SetIds differ".into());
        }
        for path in paths
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(probe_path))
        {
            if receiver.get_file_content_on_view(path, VIEW)?
                != reordered.get_file_content_on_view(path, VIEW)?
            {
                return Err(format!("commuted content differs: {path}").into());
            }
        }
        if semantic_ids(&receiver)? != semantic_ids(&reordered)? {
            return Err("commuted line/token identities differ".into());
        }
        if receiver
            .pristine()
            .read_txn()?
            .get_internal(recorded.hash())?
            == reordered
                .pristine()
                .read_txn()?
                .get_internal(recorded.hash())?
        {
            return Err("allocation control did not shift IDs".into());
        }
        report["commutation"] = true.into();
        report["unchanged_unrelated_entries"] = true.into();
        let identities = semantic_ids(&receiver)?;
        let rows = graph_rows(&receiver)?;
        if rows != graph_rows(&reordered)? {
            return Err("commuted graph row counts differ".into());
        }
        report["commuted_graph_rows"] = serde_json::json!(rows);
        drop(receiver);
        let database_path = output.join("receiver/.atomic/atomic.redb");
        let mut database = redb::Database::open(&database_path)?;
        database.compact()?;
        drop(database);
        let reopened = Repository::open(output.join("receiver"))?;
        if semantic_ids(&reopened)? != identities
            || graph_rows(&reopened)? != rows
            || reopened.get_file_content_on_view(SKILL, VIEW)?.as_ref() != Some(&edit)
        {
            return Err("reopen/compaction changed authored state".into());
        }
        report["reopen_compaction"] = true.into();
        report["receiver_compacted_bytes"] = std::fs::metadata(database_path)?.len().into();
        Ok(())
    })();
    let preserved = std::fs::read(input.join(SKILL))? == edit
        && std::fs::read(input.join(".atomic/atomic.redb"))? == db;
    report["source_preserved"] = preserved.into();
    report["success"] = attempt.is_ok().into();
    if let Err(e) = &attempt {
        report["refusal"] = e.to_string().into();
    }
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !preserved {
        return Err("source changed; no overwrite authorized".into());
    }
    attempt
}
