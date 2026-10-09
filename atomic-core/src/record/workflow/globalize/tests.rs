use super::*;

// TESTS

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::*;

    // GlobalizeOptions Tests

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = GlobalizeOptions::new();
        assert!(!opts.include_empty_files());
        assert!(opts.validate_positions());
        assert_eq!(opts.max_hunk_size(), 0);
        assert_eq!(opts.default_encoding(), Encoding::Utf8);
    }

    #[test]
    fn test_options_default() {
        let opts = GlobalizeOptions::default();
        assert!(!opts.include_empty_files());
        assert!(opts.validate_positions());
    }

    #[test]
    fn test_options_include_empty_files() {
        let opts = GlobalizeOptions::new().with_include_empty_files(true);
        assert!(opts.include_empty_files());
    }

    #[test]
    fn test_options_validate_positions() {
        let opts = GlobalizeOptions::new().with_validate_positions(false);
        assert!(!opts.validate_positions());
    }

    #[test]
    fn test_options_max_hunk_size() {
        let opts = GlobalizeOptions::new().with_max_hunk_size(1024);
        assert_eq!(opts.max_hunk_size(), 1024);
    }

    #[test]
    fn test_options_default_encoding() {
        let opts = GlobalizeOptions::new().with_default_encoding(Encoding::Binary);
        assert_eq!(opts.default_encoding(), Encoding::Binary);
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = GlobalizeOptions::new()
            .with_include_empty_files(true)
            .with_validate_positions(false)
            .with_max_hunk_size(2048)
            .with_default_encoding(Encoding::Latin1);

        assert!(opts.include_empty_files());
        assert!(!opts.validate_positions());
        assert_eq!(opts.max_hunk_size(), 2048);
        assert_eq!(opts.default_encoding(), Encoding::Latin1);
    }

    #[test]
    fn test_options_clone() {
        let opts1 = GlobalizeOptions::new().with_include_empty_files(true);
        let opts2 = opts1.clone();
        assert!(opts2.include_empty_files());
    }

    #[test]
    fn test_options_debug() {
        let opts = GlobalizeOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("GlobalizeOptions"));
    }

    // GlobalizeError Tests

    #[test]
    fn test_error_path_not_found() {
        let err = GlobalizeError::PathNotFound {
            path: "test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_error_inode_not_found() {
        let err = GlobalizeError::InodeNotFound {
            inode: Inode::new(42),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_parent_not_found() {
        let err = GlobalizeError::ParentNotFound {
            path: "src/test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("src/test.rs"));
    }

    #[test]
    fn test_error_missing_context() {
        let err = GlobalizeError::MissingContext {
            path: "test.rs".to_string(),
            line: 42,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_missing_field() {
        let err = GlobalizeError::MissingField {
            path: "test.rs".to_string(),
            field: "inode",
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("inode"));
    }

    #[test]
    fn test_error_invalid_line() {
        let err = GlobalizeError::InvalidLine {
            path: "test.rs".to_string(),
            line: 100,
            max_line: 50,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    // CacheStats Tests

    #[test]
    fn test_cache_stats_display() {
        let stats = CacheStats {
            inode_cache_size: 10,
            position_cache_size: 20,
        };
        let display = format!("{}", stats);
        assert!(display.contains("10"));
        assert!(display.contains("20"));
    }

    #[test]
    fn test_cache_stats_equality() {
        let stats1 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats2 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats3 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 15,
        };
        assert_eq!(stats1, stats2);
        assert_ne!(stats1, stats3);
    }

    // Helper Function Tests

    #[test]
    fn test_extract_filename_with_path() {
        assert_eq!(extract_filename("src/lib/mod.rs"), "mod.rs");
    }

    #[test]
    fn test_extract_filename_root_level() {
        assert_eq!(extract_filename("Cargo.toml"), "Cargo.toml");
    }

    #[test]
    fn test_extract_filename_deep_path() {
        assert_eq!(extract_filename("a/b/c/d/e.txt"), "e.txt");
    }

    #[test]
    fn test_extract_filename_empty() {
        assert_eq!(extract_filename(""), "");
    }

    #[test]
    fn test_extract_parent_with_path() {
        assert_eq!(extract_parent("src/lib/mod.rs"), "src/lib");
    }

    #[test]
    fn test_extract_parent_root_level() {
        assert_eq!(extract_parent("Cargo.toml"), "");
    }

    #[test]
    fn test_extract_parent_deep_path() {
        assert_eq!(extract_parent("a/b/c/d/e.txt"), "a/b/c/d");
    }

    #[test]
    fn test_extract_parent_empty() {
        assert_eq!(extract_parent(""), "");
    }

    #[test]
    fn test_ancestor_directories_are_parent_first() {
        assert_eq!(
            ancestor_directories("src/domain/model.rs"),
            vec!["src".to_string(), "src/domain".to_string()]
        );
        assert!(ancestor_directories("Cargo.toml").is_empty());
    }

    // Position Conversion Tests

    #[test]
    fn test_position_to_option_hash_root() {
        let pos = Position::new(NodeId::ROOT, ChangePosition::new(0));
        let converted = position_to_option_hash(pos);
        // ROOT positions use Some(Hash::NONE) to indicate the virtual root span
        assert!(converted.change.is_some());
        assert_eq!(converted.change.unwrap(), Hash::NONE);
        assert_eq!(converted.pos, ChangePosition::new(0));
    }

    #[test]
    fn test_position_to_option_hash_non_root() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let converted = position_to_option_hash(pos);
        // Currently returns None for self-reference
        assert!(converted.change.is_none());
        assert_eq!(converted.pos, ChangePosition::new(100));
    }

    #[test]
    fn test_vertex_to_option_hash() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let converted = vertex_to_option_hash(node);
        assert!(converted.change.is_none());
        assert_eq!(converted.start, ChangePosition::new(0));
        assert_eq!(converted.end, ChangePosition::new(10));
    }

    #[test]
    fn test_node_id_to_option_hash_root() {
        let result = node_id_to_option_hash(NodeId::ROOT);
        // ROOT node uses Some(Hash::NONE) to indicate the virtual root
        assert!(result.is_some());
        assert_eq!(result.unwrap(), Hash::NONE);
    }

    #[test]
    fn test_node_id_to_option_hash_non_root() {
        let result = node_id_to_option_hash(NodeId::new(42));
        // Currently returns None for self-reference
        assert!(result.is_none());
    }

    // GlobalizedFile Tests

    #[test]
    fn test_globalized_file_new() {
        let gf = GlobalizedFile::new("test.rs");
        assert_eq!(gf.path(), "test.rs");
        assert!(gf.is_empty());
        assert_eq!(gf.hunk_count(), 0);
    }

    #[test]
    fn test_globalized_file_set_bytes() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_bytes_added(100);
        assert_eq!(gf.bytes_added(), 100);
    }

    #[test]
    fn test_globalized_file_set_deps() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_dependency_count(5);
        assert_eq!(gf.dependency_count(), 5);
    }

    #[test]
    fn test_globalized_file_into_hunks() {
        let gf = GlobalizedFile::new("test.rs");
        let hunks = gf.into_hunks();
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_globalize_name_conflict_verifies_graph_and_collects_complete_dependencies() {
        use crate::pristine::{MutTxnT, Pristine};
        use crate::SerializedGraphEdge;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let winner_hash = Hash::of(b"winner inode");
        let loser_hash = Hash::of(b"loser inode");
        let parent_hash = Hash::of(b"parent directory");
        let name_hash = Hash::of(b"losing moved name");
        let solve_hash = Hash::of(b"resolution change");

        let (winner, loser, source, name, name_change, solve_change) = {
            let mut txn = pristine.write_txn().unwrap();
            let winner_change = txn.register_change(&winner_hash).unwrap();
            let loser_change = txn.register_change(&loser_hash).unwrap();
            let parent_change = txn.register_change(&parent_hash).unwrap();
            let name_change = txn.register_change(&name_hash).unwrap();
            let solve_change = txn.register_change(&solve_hash).unwrap();

            let winner = Position::new(winner_change, ChangePosition::new(5));
            let loser = Position::new(loser_change, ChangePosition::new(7));
            let source = GraphNode::new(
                parent_change,
                ChangePosition::new(3),
                ChangePosition::new(3),
            );
            let name = GraphNode::new(
                name_change,
                ChangePosition::new(11),
                ChangePosition::new(23),
            );
            let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
            txn.put_graph(
                source,
                SerializedGraphEdge::new(alive, name.start_pos(), name_change),
            )
            .unwrap();
            txn.put_graph(
                source,
                SerializedGraphEdge::new(
                    alive | EdgeFlags::DELETED,
                    name.start_pos(),
                    solve_change,
                ),
            )
            .unwrap();
            txn.put_graph(name, SerializedGraphEdge::new(alive, loser, name_change))
                .unwrap();
            for (inode, introduced_by) in [(winner, winner_change), (loser, loser_change)] {
                txn.put_graph(
                    GraphNode::new(inode.change, inode.pos, inode.pos),
                    SerializedGraphEdge::new(EdgeFlags::PARENT, Position::ROOT, introduced_by),
                )
                .unwrap();
            }
            txn.commit().unwrap();
            (winner, loser, source, name, name_change, solve_change)
        };

        let txn = pristine.read_txn().unwrap();
        let mut invalid_ctx = GlobalizeContext::new(&txn);
        let invalid = globalize_solve_name_conflict(
            &mut invalid_ctx,
            "src/conflict.txt",
            winner,
            [NameConflictClaim::new(loser, source, name, solve_change)],
        );
        assert!(matches!(
            invalid,
            Err(GlobalizeError::InvalidNameConflict { .. })
        ));
        assert!(invalid_ctx.dependencies().is_empty());

        let mut solve_ctx = GlobalizeContext::new(&txn);
        let solve = globalize_solve_name_conflict(
            &mut solve_ctx,
            "src/conflict.txt",
            winner,
            [NameConflictClaim::new(loser, source, name, name_change)],
        )
        .unwrap();
        let GraphOp::SolveNameConflict {
            name: solve_update,
            path,
        } = solve
        else {
            unreachable!();
        };
        assert_eq!(path, "src/conflict.txt");
        assert_eq!(solve_update.inode.change, Some(winner_hash));
        assert_eq!(solve_update.edges[0].from.change, Some(parent_hash));
        assert_eq!(solve_update.edges[0].to.change, Some(name_hash));
        assert_eq!(solve_update.edges[0].introduced_by, Some(name_hash));
        assert_eq!(
            solve_ctx.dependencies(),
            &HashSet::from([winner_hash, loser_hash, parent_hash, name_hash])
        );

        let mut unsolve_ctx = GlobalizeContext::new(&txn);
        let unsolve = globalize_unsolve_name_conflict(
            &mut unsolve_ctx,
            "src/conflict.txt",
            winner,
            [NameConflictClaim::new(loser, source, name, solve_change)],
        )
        .unwrap();
        let GraphOp::UnsolveNameConflict { name, .. } = unsolve else {
            unreachable!();
        };
        assert_eq!(
            name.edges[0].previous,
            EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED
        );
        assert_eq!(name.edges[0].flag, EdgeFlags::FOLDER | EdgeFlags::BLOCK);
        assert_eq!(name.edges[0].introduced_by, Some(solve_hash));
        assert_eq!(
            unsolve_ctx.dependencies(),
            &HashSet::from([winner_hash, loser_hash, parent_hash, name_hash, solve_hash,])
        );
    }

    #[test]
    fn top_level_name_conflict_globalizes_compacts_expands_and_applies() {
        use crate::apply::{write_edge_map, CachedWriteGraphTxn, Workspace};
        use crate::change::format_v3::compact::{CompactGraphOp, Compactor};
        use crate::change::format_v3::HashDedupTable;
        use crate::change::Change;
        use crate::pristine::{MutTxnT, Pristine};
        use crate::SerializedGraphEdge;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let winner_hash = Hash::of(b"top-level winner inode");
        let loser_hash = Hash::of(b"top-level loser inode");
        let name_hash = Hash::of(b"top-level losing name");
        let resolution_hash = Hash::of(b"top-level resolution");
        let winner_inode = Inode::new(41);

        let (winner, loser, name, name_change, resolution_change) = {
            let mut txn = pristine.write_txn().unwrap();
            let winner_change = txn.register_change(&winner_hash).unwrap();
            let loser_change = txn.register_change(&loser_hash).unwrap();
            let name_change = txn.register_change(&name_hash).unwrap();
            let resolution_change = txn.register_change(&resolution_hash).unwrap();
            let winner = Position::new(winner_change, ChangePosition::new(5));
            let loser = Position::new(loser_change, ChangePosition::new(7));
            let name = GraphNode::new(
                name_change,
                ChangePosition::new(11),
                ChangePosition::new(23),
            );
            let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;

            txn.put_graph(
                GraphNode::root(),
                SerializedGraphEdge::new(alive, name.start_pos(), name_change),
            )
            .unwrap();
            txn.put_graph(name, SerializedGraphEdge::new(alive, loser, name_change))
                .unwrap();
            for (inode, introduced_by) in [(winner, winner_change), (loser, loser_change)] {
                txn.put_graph(
                    GraphNode::new(inode.change, inode.pos, inode.pos),
                    SerializedGraphEdge::new(EdgeFlags::PARENT, Position::ROOT, introduced_by),
                )
                .unwrap();
            }
            txn.put_inode(winner_inode, winner).unwrap();
            txn.commit().unwrap();
            (winner, loser, name, name_change, resolution_change)
        };

        let expanded = {
            let txn = pristine.read_txn().unwrap();
            let mut ctx = GlobalizeContext::new(&txn);
            let solve = globalize_solve_name_conflict(
                &mut ctx,
                "conflict.txt",
                winner,
                [NameConflictClaim::new(
                    loser,
                    GraphNode::root(),
                    name,
                    name_change,
                )],
            )
            .unwrap();
            let GraphOp::SolveNameConflict { name, .. } = &solve else {
                unreachable!();
            };
            assert_eq!(name.edges[0].from.change, Some(Hash::NONE));

            // Match Change::serialize: index 0 is the zero placeholder, which is
            // also Hash::NONE. Option::None remains the 0xFFFF wire sentinel.
            let mut table = HashDedupTable::new(*Hash::NONE.as_bytes());
            for dependency in ctx.dependencies_sorted() {
                table.insert(*dependency.as_bytes()).unwrap();
            }
            let compactor = Compactor::new(&table);
            let compact = compactor.compact_graph_op(&solve).unwrap();
            let CompactGraphOp::SolveNameConflict { name, .. } = &compact else {
                unreachable!();
            };
            assert_eq!(name.edges[0].from.change, 0);

            let expanded = compactor.expand_graph_op(&compact).unwrap();
            assert_eq!(expanded, solve);
            let GraphOp::SolveNameConflict { name, .. } = &expanded else {
                unreachable!();
            };
            assert_eq!(name.edges[0].from.change, Some(Hash::NONE));
            expanded
        };

        let GraphOp::SolveNameConflict { name: update, .. } = expanded else {
            unreachable!();
        };
        {
            let txn = pristine.write_txn().unwrap();
            let mut cached = CachedWriteGraphTxn::new(&txn).unwrap();
            let mut workspace = Workspace::new();
            write_edge_map(
                &mut cached,
                &mut workspace,
                resolution_change,
                &update,
                &Change::default(),
                false,
            )
            .unwrap();
            drop(cached);
            txn.commit().unwrap();
        }

        let txn = pristine.read_txn().unwrap();
        let expected = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let mut saw_original = false;
        let mut saw_resolution = false;
        for edge in txn
            .iter_adjacent(GraphNode::root(), EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
        {
            let edge = edge.unwrap();
            if edge.dest() != name.start_pos() {
                continue;
            }
            saw_original |= edge.flag() == expected && edge.introduced_by() == name_change;
            saw_resolution |= edge.flag() == expected | EdgeFlags::DELETED
                && edge.introduced_by() == resolution_change;
        }
        assert!(
            saw_original,
            "additive apply must retain the original claim"
        );
        assert!(saw_resolution, "solve must add the deleted ROOT claim edge");
    }
}
