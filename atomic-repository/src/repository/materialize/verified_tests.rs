use super::*;
use atomic_core::change::ChangeHeader;
use atomic_core::pristine::{CapabilityMutTxnT, SPAN_RANGE_CAPABILITY};

#[derive(Default)]
struct NoEffects(usize);
impl MaterializationEffectExecutor for NoEffects {
    fn create_directory(&mut self, _: &str) -> Result<bool, RepositoryError> {
        self.0 += 1;
        Ok(false)
    }
    fn write_file(&mut self, _: &str, _: &[u8]) -> Result<bool, RepositoryError> {
        self.0 += 1;
        Ok(false)
    }
    fn remove_path(&mut self, _: &str) -> Result<bool, RepositoryError> {
        self.0 += 1;
        Ok(false)
    }
}

#[test]
fn verified_plan_payload_cannot_hide_deleted_or_corrupted_sources_at_execution() {
    for delete in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        {
            let mut txn = repo.pristine().write_txn().unwrap();
            txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
            txn.commit().unwrap();
        }
        let file = directory.path().join("file.txt");
        std::fs::write(&file, b"authored bytes\n").unwrap();
        repo.add(wc, "file.txt", Default::default()).unwrap();
        let recorded = repo
            .record(
                wc,
                ChangeHeader::new("span"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true)
                    .sync_vault(false)
                    .enrich_kg(false),
            )
            .unwrap();
        let hash = *recorded.hash();
        std::fs::remove_file(&file).unwrap();
        let (visibility, view_id) = {
            let txn = repo.pristine().read_txn().unwrap();
            let view = txn.get_view("dev").unwrap().unwrap();
            (graph_visibility_closure(&txn, &view).unwrap(), view.id)
        };
        let cache = MaterializationVerifiedChanges::new(&repo.change_store, 4);
        let before = repo.working_copy_record(wc).unwrap();
        let before_head = repo
            .operation_log(OperationScope::WorkingCopy(wc), Some(1), false)
            .unwrap()
            .head_state;
        let plan = repo
            .plan_materialization_with_visibility(
                wc,
                None,
                visibility.clone(),
                view_id,
                Some(&cache),
            )
            .unwrap();
        assert!(plan.iter().any(|entry| entry.path == "file.txt"));
        let source = repo.change_store.change_path(&hash);
        if delete {
            std::fs::remove_file(source).unwrap();
        } else {
            let mut bytes = std::fs::read(&source).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(source, bytes).unwrap();
        }
        let mut executor = NoEffects::default();
        assert!(repo
            .materialize_parallel_with_visibility_mode(
                wc,
                None,
                visibility,
                view_id,
                true,
                Some(&mut executor),
                None,
                Some(&cache)
            )
            .is_err());
        assert_eq!(
            executor.0, 0,
            "source rejection must happen before any executor effect"
        );
        assert!(!file.exists());
        assert_eq!(repo.working_copy_record(wc).unwrap(), before);
        assert_eq!(
            repo.operation_log(OperationScope::WorkingCopy(wc), Some(1), false)
                .unwrap()
                .head_state,
            before_head
        );
    }
}
