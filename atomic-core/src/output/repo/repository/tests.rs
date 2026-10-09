//! Tests for the repository output module.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::change::MemoryChangeStore;
    use crate::output::repo::conflict::{FileConflict, FileConflictType};
    use crate::output::repo::file::{
        output_file_with_filter, FileOutputError, FileOutputOptions, FileOutputResult,
    };
    use crate::output::repo::repository::types::{
        MaterializeError, MaterializeOptions, OutputItem,
    };
    use crate::output::repo::repository::{materialize_view, MaterializeResult};
    use crate::output::Memory;
    use crate::pristine::{GraphTxnT, PristineError, TreeTxnT};
    use crate::types::{
        ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge,
    };
    use std::collections::HashMap;
    use std::time::SystemTime;

    // ========================================================================
    // MaterializeOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new() {
        let opts = MaterializeOptions::new();

        assert!(opts.prefix.is_empty());
        assert!(opts.if_modified_since.is_none());
        assert!(opts.output_name_conflicts);
        assert!(!opts.include_deleted);
        assert!(opts.max_vertices_per_file.is_none());
        assert_eq!(opts.salt, 0);
        assert!(!opts.parallel);
        assert_eq!(opts.num_workers, 1);
    }

    #[test]
    fn test_options_default() {
        let opts = MaterializeOptions::default();

        assert!(opts.prefix.is_empty());
        assert!(!opts.parallel);
    }

    #[test]
    fn test_options_prefix() {
        let opts = MaterializeOptions::new().prefix("src/");

        assert_eq!(opts.prefix, "src/");
    }

    #[test]
    fn test_options_prefix_empty() {
        let opts = MaterializeOptions::new().prefix("");

        assert!(opts.prefix.is_empty());
    }

    #[test]
    fn test_options_if_modified_since() {
        let time = SystemTime::now();
        let opts = MaterializeOptions::new().if_modified_since(time);

        assert!(opts.if_modified_since.is_some());
    }

    #[test]
    fn test_options_output_name_conflicts() {
        let opts = MaterializeOptions::new().output_name_conflicts(false);

        assert!(!opts.output_name_conflicts);
    }

    #[test]
    fn test_options_include_deleted() {
        let opts = MaterializeOptions::new().include_deleted(true);

        assert!(opts.include_deleted);
    }

    #[test]
    fn test_options_max_vertices_per_file() {
        let opts = MaterializeOptions::new().max_vertices_per_file(5000);

        assert_eq!(opts.max_vertices_per_file, Some(5000));
    }

    #[test]
    fn test_options_salt() {
        let opts = MaterializeOptions::new().salt(42);

        assert_eq!(opts.salt, 42);
    }

    #[test]
    fn test_options_parallel() {
        let opts = MaterializeOptions::new().parallel(true);

        assert!(opts.parallel);
    }

    #[test]
    fn test_options_num_workers() {
        let opts = MaterializeOptions::new().num_workers(8);

        assert_eq!(opts.num_workers, 8);
    }

    #[test]
    fn test_options_chaining() {
        let opts = MaterializeOptions::new()
            .prefix("src/")
            .include_deleted(true)
            .output_name_conflicts(false)
            .salt(100)
            .parallel(true)
            .num_workers(4);

        assert_eq!(opts.prefix, "src/");
        assert!(opts.include_deleted);
        assert!(!opts.output_name_conflicts);
        assert_eq!(opts.salt, 100);
        assert!(opts.parallel);
        assert_eq!(opts.num_workers, 4);
    }

    #[test]
    fn test_options_matches_prefix_empty() {
        let opts = MaterializeOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_options_matches_prefix_with_prefix() {
        let opts = MaterializeOptions::new().prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
    }

    #[test]
    fn test_options_to_file_options_default() {
        let opts = MaterializeOptions::new();
        let file_opts = opts.to_file_options();

        assert!(!file_opts.include_deleted);
        assert!(file_opts.max_vertices.is_none());
    }

    #[test]
    fn test_options_to_file_options_with_deleted() {
        let opts = MaterializeOptions::new().include_deleted(true);
        let file_opts = opts.to_file_options();

        assert!(file_opts.include_deleted);
    }

    #[test]
    fn test_options_to_file_options_with_max() {
        let opts = MaterializeOptions::new().max_vertices_per_file(1000);
        let file_opts = opts.to_file_options();

        assert_eq!(file_opts.max_vertices, Some(1000));
    }

    #[test]
    fn test_options_clone() {
        let opts = MaterializeOptions::new().prefix("test/");
        let cloned = opts.clone();

        assert_eq!(opts.prefix, cloned.prefix);
    }

    #[test]
    fn test_options_debug() {
        let opts = MaterializeOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("MaterializeOptions"));
    }

    // ========================================================================
    // MaterializeResult Tests
    // ========================================================================

    #[test]
    fn test_result_new() {
        let result = MaterializeResult::new();

        assert_eq!(result.files_written, 0);
        assert_eq!(result.files_skipped, 0);
        assert_eq!(result.directories_created, 0);
        assert_eq!(result.bytes_written, 0);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_result_default() {
        let result = MaterializeResult::default();

        assert_eq!(result.files_written, 0);
    }

    #[test]
    fn test_result_has_conflicts_empty() {
        let result = MaterializeResult::new();

        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_result_has_conflicts_with_conflict() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        assert!(result.has_conflicts());
    }

    #[test]
    fn test_result_conflict_count() {
        let mut result = MaterializeResult::new();

        assert_eq!(result.conflict_count(), 0);

        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));

        assert_eq!(result.conflict_count(), 2);
    }

    #[test]
    fn test_result_add_conflict() {
        let mut result = MaterializeResult::new();
        let conflict = FileConflict::new("test.rs".to_string(), FileConflictType::Cyclic);

        result.add_conflict(conflict);

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].conflict_type, FileConflictType::Cyclic);
    }

    #[test]
    fn test_result_conflicts_of_type() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Order,
        ));

        let order_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Order).collect();
        assert_eq!(order_conflicts.len(), 2);

        let name_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Name).collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_name_conflicts() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Order,
        ));

        let name_conflicts: Vec<_> = result.name_conflicts().collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_content_conflicts() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Cyclic,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Zombie,
        ));
        result.add_conflict(FileConflict::new(
            "d.rs".to_string(),
            FileConflictType::Name,
        ));

        let content_conflicts: Vec<_> = result.content_conflicts().collect();
        assert_eq!(content_conflicts.len(), 3);
    }

    #[test]
    fn test_result_merge_file_result() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT)
            .with_bytes_written(1024)
            .with_vertices_processed(10)
            .with_edges_traversed(20);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_written, 1);
        assert_eq!(result.bytes_written, 1024);
        assert_eq!(result.vertices_processed, 10);
        assert_eq!(result.edges_traversed, 20);
    }

    #[test]
    fn test_result_merge_file_result_with_conflicts() {
        let mut result = MaterializeResult::new();

        let mut file_result = FileOutputResult::empty("test.rs", Inode::ROOT);
        file_result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        result.merge_file_result(file_result, false);

        assert_eq!(result.conflict_count(), 1);
    }

    #[test]
    fn test_result_merge_file_result_truncated() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT).with_truncated(true);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_truncated, 1);
    }

    #[test]
    fn test_result_merge_file_result_store() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT);

        result.merge_file_result(file_result, true);

        assert!(result.file_results.contains_key("test.rs"));
    }

    #[test]
    fn test_result_record_skipped() {
        let mut result = MaterializeResult::new();

        result.record_skipped();
        result.record_skipped();

        assert_eq!(result.files_skipped, 2);
    }

    #[test]
    fn test_result_record_directory() {
        let mut result = MaterializeResult::new();

        result.record_directory();

        assert_eq!(result.directories_created, 1);
    }

    #[test]
    fn test_result_to_outcome() {
        let mut result = MaterializeResult::new();
        result.files_written = 5;
        result.directories_created = 2;
        result.files_skipped = 1;
        result.bytes_written = 10000;

        let outcome = result.to_outcome();

        assert_eq!(outcome.files_written(), 5);
        assert_eq!(outcome.directories_created(), 2);
        assert_eq!(outcome.files_skipped(), 1);
        assert_eq!(outcome.bytes_written, 10000);
    }

    #[test]
    fn test_result_clone() {
        let mut result = MaterializeResult::new();
        result.files_written = 3;

        let cloned = result.clone();

        assert_eq!(result.files_written, cloned.files_written);
    }

    #[test]
    fn test_result_debug() {
        let result = MaterializeResult::new();
        let debug = format!("{:?}", result);

        assert!(debug.contains("MaterializeResult"));
    }

    // ========================================================================
    // MaterializeError Tests
    // ========================================================================

    #[test]
    fn test_error_display_pristine() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::Pristine(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });
        let display = format!("{}", err);

        assert!(display.contains("Pristine error"));
    }

    #[test]
    fn test_error_display_change_store() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::ChangeStore("not found".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Change store error"));
    }

    #[test]
    fn test_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: MaterializeError<std::io::Error> = MaterializeError::Io(io_err);
        let display = format!("{}", err);

        assert!(display.contains("I/O error"));
    }

    #[test]
    fn test_error_display_working_copy() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: MaterializeError<std::io::Error> = MaterializeError::WorkingCopy(io_err);
        let display = format!("{}", err);

        assert!(display.contains("Working copy error"));
    }

    #[test]
    fn test_error_display_tree() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::TreeError("invalid tree".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Tree traversal error"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::other("test");
        let err: MaterializeError<std::io::Error> = io_err.into();

        match err {
            MaterializeError::Io(_) => (),
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_from_pristine() {
        let pristine_err = crate::pristine::PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err: MaterializeError<std::io::Error> = pristine_err.into();

        match err {
            MaterializeError::Pristine(_) => (),
            _ => panic!("Expected Pristine variant"),
        }
    }

    #[test]
    fn test_error_debug() {
        let err: MaterializeError<std::io::Error> = MaterializeError::TreeError("test".to_string());
        let debug = format!("{:?}", err);

        assert!(debug.contains("TreeError"));
    }

    #[test]
    fn test_error_source_pristine() {
        use std::error::Error;

        let err: MaterializeError<std::io::Error> =
            MaterializeError::Pristine(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_io() {
        use std::error::Error;

        let io_err = std::io::Error::other("test");
        let err: MaterializeError<std::io::Error> = MaterializeError::Io(io_err);

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_change_store() {
        use std::error::Error;

        let err: MaterializeError<std::io::Error> =
            MaterializeError::ChangeStore("test".to_string());

        assert!(err.source().is_none());
    }

    // ========================================================================
    // OutputItem Tests
    // ========================================================================

    #[test]
    fn test_output_item_file() {
        let item = OutputItem::file("src/main.rs", Inode::ROOT, Position::ROOT);

        assert_eq!(item.path, "src/main.rs");
        assert_eq!(item.inode, Inode::ROOT);
        assert!(!item.is_directory);
    }

    #[test]
    fn test_output_item_directory() {
        let item = OutputItem::directory("src/lib", Inode::ROOT);

        assert_eq!(item.path, "src/lib");
        assert!(item.is_directory);
    }

    #[test]
    fn test_output_item_with_metadata() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT)
            .with_metadata(crate::output::traits::FileMetadata::executable());

        assert!(item.metadata.is_executable());
    }

    #[test]
    fn test_output_item_clone() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let cloned = item.clone();

        assert_eq!(item.path, cloned.path);
    }

    #[test]
    fn test_output_item_debug() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let debug = format!("{:?}", item);

        assert!(debug.contains("OutputItem"));
        assert!(debug.contains("test.rs"));
    }

    #[derive(Default)]
    struct ScriptedTxn {
        tree: Vec<(String, Inode)>,
        positions: HashMap<Inode, Position<NodeId>>,
        edges: HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>>,
        blocks: HashMap<Position<NodeId>, GraphNode<NodeId>>,
        block_ends: HashMap<Position<NodeId>, GraphNode<NodeId>>,
        external: HashMap<NodeId, Hash>,
        forward_fault: Option<GraphNode<NodeId>>,
    }

    impl ScriptedTxn {
        fn add_tree_file(&mut self, path: &str, inode: Inode, position: Position<NodeId>) {
            self.tree.push((path.to_string(), inode));
            self.positions.insert(inode, position);
        }

        fn add_edge(
            &mut self,
            source: GraphNode<NodeId>,
            flags: EdgeFlags,
            dest: Position<NodeId>,
            introduced_by: NodeId,
        ) {
            self.edges
                .entry(source)
                .or_default()
                .push(SerializedGraphEdge::new(flags, dest, introduced_by));
        }

        fn fail_forward_adjacency(&mut self, node: GraphNode<NodeId>) {
            self.forward_fault = Some(node);
        }
    }

    impl GraphTxnT for ScriptedTxn {
        type Adj = std::vec::IntoIter<Result<SerializedGraphEdge, PristineError>>;

        fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
            Ok(self.external.get(&id).copied())
        }

        fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError> {
            Ok(self
                .external
                .iter()
                .find_map(|(id, candidate)| (candidate == hash).then_some(*id)))
        }

        fn iter_adjacent(
            &self,
            node: GraphNode<NodeId>,
            min_flag: EdgeFlags,
            max_flag: EdgeFlags,
        ) -> Result<Self::Adj, PristineError> {
            if self.forward_fault == Some(node) && !min_flag.contains(EdgeFlags::PARENT) {
                return Ok(vec![Err(PristineError::Inconsistent {
                    message: "scripted forward adjacency failure".to_string(),
                })]
                .into_iter());
            }

            let edges = self
                .edges
                .get(&node)
                .into_iter()
                .flatten()
                .filter(|edge| edge.flag() >= min_flag && edge.flag() <= max_flag)
                .copied()
                .map(Ok)
                .collect::<Vec<_>>();
            Ok(edges.into_iter())
        }

        fn find_block(
            &self,
            position: Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            self.blocks
                .get(&position)
                .copied()
                .ok_or(PristineError::BlockNotFound {
                    change: position.change.get(),
                    pos: position.pos.get(),
                })
        }

        fn find_block_end(
            &self,
            position: Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            self.block_ends
                .get(&position)
                .copied()
                .ok_or(PristineError::BlockNotFound {
                    change: position.change.get(),
                    pos: position.pos.get(),
                })
        }

        fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError> {
            Ok(self.edges.contains_key(&node))
        }

        fn get_node_type(&self, _node_id: NodeId) -> Result<Option<u8>, PristineError> {
            Ok(None)
        }

        fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
            Ok(Vec::new())
        }

        fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
            Ok(self.edges.keys().any(|node| node.change == change_id))
        }
    }

    impl TreeTxnT for ScriptedTxn {
        fn get_inode(&self, path: &str) -> Result<Option<Inode>, PristineError> {
            Ok(self
                .tree
                .iter()
                .find_map(|(candidate, inode)| (candidate == path).then_some(*inode)))
        }

        fn get_directory_flags(&self, _inode: Inode) -> Result<Option<u8>, PristineError> {
            Ok(None)
        }

        fn get_path(&self, inode: Inode) -> Result<Option<String>, PristineError> {
            Ok(self
                .tree
                .iter()
                .find_map(|(path, candidate)| (*candidate == inode).then(|| path.clone())))
        }

        fn inode_position(&self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError> {
            Ok(self.positions.get(&inode).copied())
        }

        fn position_inode(
            &self,
            position: Position<NodeId>,
        ) -> Result<Option<Inode>, PristineError> {
            Ok(self
                .positions
                .iter()
                .find_map(|(inode, candidate)| (*candidate == position).then_some(*inode)))
        }

        fn iter_tree(
            &self,
        ) -> Result<
            Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>,
            PristineError,
        > {
            Ok(Box::new(self.tree.iter().cloned().map(Ok)))
        }

        fn iter_inode_vertices(
            &self,
            _inode: Inode,
        ) -> Result<
            Box<
                dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                    + '_,
            >,
            PristineError,
        > {
            Ok(Box::new(std::iter::empty()))
        }

        fn get_file_index(
            &self,
            _path: &str,
        ) -> Result<Option<(i64, u32, u64, Hash)>, PristineError> {
            Ok(None)
        }

        fn iter_file_index(&self) -> Result<Vec<(String, i64, u32, u64, Hash)>, PristineError> {
            Ok(Vec::new())
        }
    }

    fn test_position(change: u64, pos: u64) -> Position<NodeId> {
        Position::new(NodeId::new(change), ChangePosition::new(pos))
    }

    fn test_node(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(
            NodeId::new(change),
            ChangePosition::new(start),
            ChangePosition::new(end),
        )
    }

    fn sorted_paths(working_copy: &Memory) -> Vec<String> {
        let mut paths = working_copy.list_all_paths();
        paths.sort();
        paths
    }

    #[test]
    fn materialize_view_renders_entire_batch_before_mutating_working_copy() {
        let first_position = test_position(1, 0);
        let second_position = test_position(2, 0);
        let first_inode = Inode::new(10);
        let second_inode = Inode::new(11);

        let mut txn = ScriptedTxn::default();
        txn.add_tree_file("first.txt", first_inode, first_position);
        txn.add_tree_file("newdir/second.txt", second_inode, second_position);
        txn.fail_forward_adjacency(second_position.inode_node());

        let changes = MemoryChangeStore::new();
        let working_copy = Memory::new();
        working_copy.add_file("first.txt", b"preexisting first content");
        let original_inode = working_copy.get_inode("first.txt");
        let original_paths = sorted_paths(&working_copy);

        let error = materialize_view(&txn, &changes, &working_copy, MaterializeOptions::new())
            .expect_err("the second file should fail during graph rendering");

        match error {
            MaterializeError::FileOutput {
                path,
                source: FileOutputError::Graph(PristineError::Inconsistent { message }),
            } => {
                assert_eq!(path, "newdir/second.txt");
                assert_eq!(message, "scripted forward adjacency failure");
            }
            other => panic!("unexpected materialize error: {other:?}"),
        }

        assert_eq!(
            working_copy.get_file_contents("first.txt").as_deref(),
            Some(b"preexisting first content".as_slice())
        );
        assert_eq!(working_copy.get_inode("first.txt"), original_inode);
        assert_eq!(sorted_paths(&working_copy), original_paths);
        assert_eq!(working_copy.get_file_contents("newdir/second.txt"), None);
    }

    #[test]
    fn single_file_output_does_not_open_writer_before_content_render_succeeds() {
        let position = test_position(1, 0);
        let content_position = test_position(2, 0);
        let content_node = test_node(2, 0, 4);
        let inode = Inode::new(12);

        let mut txn = ScriptedTxn::default();
        txn.add_edge(
            position.inode_node(),
            EdgeFlags::BLOCK,
            content_position,
            NodeId::new(2),
        );
        txn.add_edge(
            content_node,
            EdgeFlags::PARENT | EdgeFlags::BLOCK,
            position,
            NodeId::new(2),
        );
        txn.blocks.insert(content_position, content_node);
        txn.external.insert(NodeId::new(2), Hash::of(b"missing"));

        let changes = MemoryChangeStore::new();
        let working_copy = Memory::new();
        working_copy.add_file("existing.txt", b"preexisting content");
        let original_inode = working_copy.get_inode("existing.txt");
        let original_paths = sorted_paths(&working_copy);

        let error = output_file_with_filter(
            &txn,
            &changes,
            &working_copy,
            inode,
            position,
            "existing.txt",
            FileOutputOptions::new(),
            None,
        )
        .expect_err("missing change content should fail the render phase");

        assert!(matches!(error, FileOutputError::ChangeStore(_)));
        assert_eq!(
            working_copy.get_file_contents("existing.txt").as_deref(),
            Some(b"preexisting content".as_slice())
        );
        assert_eq!(working_copy.get_inode("existing.txt"), original_inode);
        assert_eq!(sorted_paths(&working_copy), original_paths);
    }
}
