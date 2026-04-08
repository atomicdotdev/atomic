//! Tests for the streaming module.

use super::*;

// ── Layer ───────────────────────────────────────────────────────

#[test]
fn test_layer_from_str() {
    assert_eq!(Layer::from_str_loose("graph"), Some(Layer::Graph));
    assert_eq!(Layer::from_str_loose("GRAPH"), Some(Layer::Graph));
    assert_eq!(Layer::from_str_loose("semantic"), Some(Layer::Semantic));
    assert_eq!(Layer::from_str_loose("content"), Some(Layer::Content));
    assert_eq!(Layer::from_str_loose("unknown"), None);
    assert_eq!(Layer::from_str_loose(""), None);
}

#[test]
fn test_layer_as_str() {
    assert_eq!(Layer::Graph.as_str(), "graph");
    assert_eq!(Layer::Semantic.as_str(), "semantic");
    assert_eq!(Layer::Content.as_str(), "content");
}

#[test]
fn test_layer_display() {
    assert_eq!(format!("{}", Layer::Graph), "graph");
    assert_eq!(format!("{}", Layer::Semantic), "semantic");
    assert_eq!(format!("{}", Layer::Content), "content");
}

#[test]
fn test_layer_json_roundtrip() {
    for layer in [Layer::Graph, Layer::Semantic, Layer::Content] {
        let json = serde_json::to_string(&layer).unwrap();
        let decoded: Layer = serde_json::from_str(&json).unwrap();
        assert_eq!(layer, decoded);
    }
}

// ── LayerSelection ─────────────────────────────────────────────

#[test]
fn test_layer_selection_all() {
    let sel = LayerSelection::all();
    assert!(sel.includes(Layer::Graph));
    assert!(sel.includes(Layer::Semantic));
    assert!(sel.includes(Layer::Content));
    assert!(sel.is_all());
    assert!(!sel.is_empty());
    assert_eq!(sel.len(), 3);
    assert_eq!(sel.to_query_value(), "all");
}

#[test]
fn test_layer_selection_thin_pull() {
    let sel = LayerSelection::thin_pull();
    assert!(sel.includes(Layer::Graph));
    assert!(!sel.includes(Layer::Semantic));
    assert!(sel.includes(Layer::Content));
    assert!(!sel.is_all());
    assert_eq!(sel.len(), 2);
    assert_eq!(sel.to_query_value(), "graph,content");
}

#[test]
fn test_layer_selection_thin_review() {
    let sel = LayerSelection::thin_review();
    assert!(!sel.includes(Layer::Graph));
    assert!(sel.includes(Layer::Semantic));
    assert!(sel.includes(Layer::Content));
    assert!(!sel.is_all());
    assert_eq!(sel.to_query_value(), "semantic,content");
}

#[test]
fn test_layer_selection_graph_only() {
    let sel = LayerSelection::graph_only();
    assert!(sel.includes(Layer::Graph));
    assert!(!sel.includes(Layer::Semantic));
    assert!(!sel.includes(Layer::Content));
    assert_eq!(sel.to_query_value(), "graph");
}

#[test]
fn test_layer_selection_custom() {
    let sel = LayerSelection::custom([Layer::Semantic]);
    assert!(!sel.includes(Layer::Graph));
    assert!(sel.includes(Layer::Semantic));
    assert!(!sel.includes(Layer::Content));
    assert_eq!(sel.to_query_value(), "semantic");
}

#[test]
fn test_layer_selection_from_query_all() {
    let sel = LayerSelection::from_query_value("all");
    assert!(sel.is_all());

    let sel = LayerSelection::from_query_value("ALL");
    assert!(sel.is_all());
}

#[test]
fn test_layer_selection_from_query_thin_pull() {
    let sel = LayerSelection::from_query_value("graph,content");
    assert!(sel.includes(Layer::Graph));
    assert!(!sel.includes(Layer::Semantic));
    assert!(sel.includes(Layer::Content));
}

#[test]
fn test_layer_selection_from_query_with_spaces() {
    let sel = LayerSelection::from_query_value(" graph , content ");
    assert!(sel.includes(Layer::Graph));
    assert!(sel.includes(Layer::Content));
}

#[test]
fn test_layer_selection_from_query_unknown_ignored() {
    let sel = LayerSelection::from_query_value("graph,unknown,content");
    assert!(sel.includes(Layer::Graph));
    assert!(sel.includes(Layer::Content));
    assert!(!sel.includes(Layer::Semantic));
    assert_eq!(sel.len(), 2);
}

#[test]
fn test_layer_selection_from_query_empty() {
    let sel = LayerSelection::from_query_value("");
    assert!(sel.is_empty());
}

#[test]
fn test_layer_selection_default_is_all() {
    let sel = LayerSelection::default();
    assert!(sel.is_all());
}

#[test]
fn test_layer_selection_display() {
    assert_eq!(format!("{}", LayerSelection::all()), "layers=all");
    assert_eq!(
        format!("{}", LayerSelection::thin_pull()),
        "layers=graph,content"
    );
}

#[test]
fn test_layer_selection_query_roundtrip() {
    for sel in [
        LayerSelection::all(),
        LayerSelection::thin_pull(),
        LayerSelection::thin_review(),
        LayerSelection::graph_only(),
    ] {
        let query = sel.to_query_value();
        let decoded = LayerSelection::from_query_value(&query);
        assert_eq!(sel, decoded, "roundtrip failed for '{}'", query);
    }
}

#[test]
fn test_layer_selection_json_roundtrip() {
    let sel = LayerSelection::thin_pull();
    let json = serde_json::to_string(&sel).unwrap();
    let decoded: LayerSelection = serde_json::from_str(&json).unwrap();
    assert_eq!(sel, decoded);
}

// ── ChunkManifestEntry ─────────────────────────────────────────

#[test]
fn test_manifest_entry_new() {
    let entry = ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536);
    assert_eq!(entry.index, 0);
    assert_eq!(entry.hash, [0xAA; 32]);
    assert_eq!(entry.compressed_size, 32000);
    assert_eq!(entry.uncompressed_size, 65536);
}

#[test]
fn test_manifest_entry_compression_ratio() {
    let entry = ChunkManifestEntry::new(0, [0; 32], 500, 1000);
    assert!((entry.compression_ratio() - 0.5).abs() < f64::EPSILON);

    let zero = ChunkManifestEntry::new(0, [0; 32], 0, 0);
    assert!(zero.compression_ratio().is_nan());
}

#[test]
fn test_manifest_entry_display() {
    let entry = ChunkManifestEntry::new(
        3,
        [
            0xAB, 0xCD, 0xEF, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ],
        30000,
        65536,
    );
    let display = format!("{}", entry);
    assert!(display.contains("chunk#3"));
    assert!(display.contains("abcdef01"));
}

#[test]
fn test_manifest_entry_json_roundtrip() {
    let entry = ChunkManifestEntry::new(5, [0x42; 32], 12345, 67890);
    let json = serde_json::to_string(&entry).unwrap();
    let decoded: ChunkManifestEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, decoded);
}

#[test]
fn test_manifest_entry_json_format() {
    let entry = ChunkManifestEntry::new(0, [0xAB; 32], 100, 200);
    let json = serde_json::to_string_pretty(&entry).unwrap();
    // Hash should be hex-encoded
    assert!(json.contains("abababab"));
    assert!(json.contains("\"index\": 0"));
    assert!(json.contains("\"compressed_size\": 100"));
}

// ── ChunkManifest ──────────────────────────────────────────────

#[test]
fn test_manifest_empty() {
    let manifest = ChunkManifest::empty();
    assert!(manifest.is_empty());
    assert_eq!(manifest.chunk_count(), 0);
    assert_eq!(manifest.total_compressed(), 0);
    assert_eq!(manifest.total_uncompressed(), 0);
}

#[test]
fn test_manifest_with_entries() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
    ]);

    assert_eq!(manifest.chunk_count(), 3);
    assert!(!manifest.is_empty());
    assert_eq!(manifest.total_compressed(), 75000);
    assert_eq!(manifest.total_uncompressed(), 171072);
}

#[test]
fn test_manifest_hash_set() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
    ]);

    let set = manifest.hash_set();
    assert_eq!(set.len(), 2);
    assert!(set.contains(&[0xAA; 32]));
    assert!(set.contains(&[0xBB; 32]));
    assert!(!set.contains(&[0xCC; 32]));
}

#[test]
fn test_manifest_find_by_hash() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
    ]);

    let found = manifest.find_by_hash(&[0xBB; 32]);
    assert!(found.is_some());
    assert_eq!(found.unwrap().index, 1);

    assert!(manifest.find_by_hash(&[0xCC; 32]).is_none());
}

#[test]
fn test_manifest_find_by_index() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
    ]);

    assert!(manifest.find_by_index(0).is_some());
    assert!(manifest.find_by_index(1).is_some());
    assert!(manifest.find_by_index(2).is_none());
}

#[test]
fn test_manifest_hashes_iterator() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
    ]);

    let hashes: Vec<&[u8; 32]> = manifest.hashes().collect();
    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes[0], &[0xAA; 32]);
    assert_eq!(hashes[1], &[0xBB; 32]);
}

#[test]
fn test_manifest_display() {
    let manifest = ChunkManifest::new(vec![ChunkManifestEntry::new(0, [0; 32], 32000, 65536)]);
    let display = format!("{}", manifest);
    assert!(display.contains("1 chunks"));
    assert!(display.contains("compressed"));
}

#[test]
fn test_manifest_json_roundtrip() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
    ]);

    let json = serde_json::to_string(&manifest).unwrap();
    let decoded: ChunkManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(manifest, decoded);
}

// ── ChunkNegotiation ───────────────────────────────────────────

#[test]
fn test_negotiation_no_overlap() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
    ]);

    let have: Vec<[u8; 32]> = vec![]; // nothing
    let result = ChunkNegotiation::compute(&manifest, &have);

    assert_eq!(result.needed.len(), 2);
    assert_eq!(result.already_have, 0);
    assert_eq!(result.bytes_saved, 0);
    assert_eq!(result.bytes_needed, 60000);
    assert!(result.is_full_transfer());
    assert!(!result.is_complete());
    assert_eq!(result.total_chunks, 2);
}

#[test]
fn test_negotiation_full_overlap() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
    ]);

    let have = vec![[0xAA; 32], [0xBB; 32]];
    let result = ChunkNegotiation::compute(&manifest, &have);

    assert!(result.needed.is_empty());
    assert_eq!(result.already_have, 2);
    assert_eq!(result.bytes_saved, 60000);
    assert_eq!(result.bytes_needed, 0);
    assert!(result.is_complete());
    assert!(!result.is_full_transfer());
}

#[test]
fn test_negotiation_partial_overlap() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
    ]);

    let have = vec![[0xAA; 32]]; // only chunk 0
    let result = ChunkNegotiation::compute(&manifest, &have);

    assert_eq!(result.needed.len(), 2);
    assert_eq!(result.needed[0].index, 1);
    assert_eq!(result.needed[1].index, 2);
    assert_eq!(result.already_have, 1);
    assert_eq!(result.bytes_saved, 32000);
    assert_eq!(result.bytes_needed, 43000);
    assert_eq!(result.total_chunks, 3);
    assert!(!result.is_complete());
    assert!(!result.is_full_transfer());
}

#[test]
fn test_negotiation_savings_pct() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 50, 100),
        ChunkManifestEntry::new(1, [0xBB; 32], 50, 100),
    ]);

    // Have one of two equal-size chunks → 50% savings
    let have = vec![[0xAA; 32]];
    let result = ChunkNegotiation::compute(&manifest, &have);
    assert!((result.savings_pct() - 50.0).abs() < f64::EPSILON);
}

#[test]
fn test_negotiation_savings_pct_zero() {
    let manifest = ChunkManifest::empty();
    let result = ChunkNegotiation::compute(&manifest, &[]);
    assert!((result.savings_pct() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_negotiation_needed_hashes() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
        ChunkManifestEntry::new(2, [0xCC; 32], 100, 200),
    ]);

    let have = vec![[0xBB; 32]];
    let result = ChunkNegotiation::compute(&manifest, &have);

    let needed = result.needed_hashes();
    assert_eq!(needed.len(), 2);
    assert!(needed.contains(&[0xAA; 32]));
    assert!(needed.contains(&[0xCC; 32]));
    assert!(!needed.contains(&[0xBB; 32]));
}

#[test]
fn test_negotiation_needed_indices() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
        ChunkManifestEntry::new(2, [0xCC; 32], 100, 200),
    ]);

    let have = vec![[0xBB; 32]];
    let result = ChunkNegotiation::compute(&manifest, &have);

    let indices = result.needed_indices();
    assert_eq!(indices, vec![0, 2]);
}

#[test]
fn test_negotiation_display() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
        ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
    ]);

    let have = vec![[0xAA; 32]];
    let result = ChunkNegotiation::compute(&manifest, &have);
    let display = format!("{}", result);
    assert!(display.contains("need 1 of 2"));
    assert!(display.contains("saved"));
}

#[test]
fn test_negotiation_empty_manifest() {
    let manifest = ChunkManifest::empty();
    let result = ChunkNegotiation::compute(&manifest, &[[0xAA; 32]]);
    assert!(result.is_complete());
    assert_eq!(result.total_chunks, 0);
}

#[test]
fn test_negotiation_extra_haves_ignored() {
    // Client claims to have chunks not in the manifest — they're just ignored
    let manifest = ChunkManifest::new(vec![ChunkManifestEntry::new(0, [0xAA; 32], 100, 200)]);

    let have = vec![[0xAA; 32], [0xFF; 32]]; // 0xFF not in manifest
    let result = ChunkNegotiation::compute(&manifest, &have);
    assert!(result.is_complete());
    assert_eq!(result.already_have, 1); // only the one that matched
}

#[test]
fn test_negotiation_json_roundtrip() {
    let manifest = ChunkManifest::new(vec![
        ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
        ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
    ]);

    let result = ChunkNegotiation::compute(&manifest, &[[0xAA; 32]]);
    let json = serde_json::to_string(&result).unwrap();
    let decoded: ChunkNegotiation = serde_json::from_str(&json).unwrap();
    assert_eq!(result, decoded);
}

// ── StreamingPushOptions ───────────────────────────────────────

#[test]
fn test_push_options_default() {
    let opts = StreamingPushOptions::default();
    assert!(opts.use_delta_transfer);
    assert!(opts.report_progress);
    assert_eq!(opts.max_parallel_chunks, 4);
}

#[test]
fn test_push_options_simple() {
    let opts = StreamingPushOptions::simple();
    assert!(!opts.use_delta_transfer);
    assert!(opts.report_progress);
}

// ── StreamingPullOptions ───────────────────────────────────────

#[test]
fn test_pull_options_default() {
    let opts = StreamingPullOptions::default();
    assert!(opts.layers.is_all());
    assert!(!opts.use_delta_transfer);
    assert!(opts.verify_hash);
}

#[test]
fn test_pull_options_thin_pull() {
    let opts = StreamingPullOptions::thin_pull();
    assert!(opts.layers.includes(Layer::Graph));
    assert!(opts.layers.includes(Layer::Content));
    assert!(!opts.layers.includes(Layer::Semantic));
}

#[test]
fn test_pull_options_thin_review() {
    let opts = StreamingPullOptions::thin_review();
    assert!(opts.layers.includes(Layer::Semantic));
    assert!(opts.layers.includes(Layer::Content));
    assert!(!opts.layers.includes(Layer::Graph));
}

#[test]
fn test_pull_options_with_delta() {
    let opts = StreamingPullOptions::default().with_delta_transfer(true);
    assert!(opts.use_delta_transfer);
}

#[test]
fn test_pull_options_without_verify() {
    let opts = StreamingPullOptions::default().with_verify(false);
    assert!(!opts.verify_hash);
}

// ── TransferProgress ───────────────────────────────────────────

#[test]
fn test_progress_started_display() {
    let p = TransferProgress::Started {
        total_sections: 11,
        total_bytes_estimate: 7_500_000,
    };
    let display = format!("{}", p);
    assert!(display.contains("11 sections"));
    assert!(display.contains("7.2 MB"));
}

#[test]
fn test_progress_section_display() {
    let p = TransferProgress::SectionComplete {
        section: "HEADER".to_string(),
        bytes_transferred: 200,
    };
    let display = format!("{}", p);
    assert!(display.contains("HEADER"));
    assert!(display.contains("200 B"));
}

#[test]
fn test_progress_chunk_display() {
    let p = TransferProgress::ChunkComplete {
        index: 3,
        bytes_transferred: 32000,
        skipped: false,
    };
    let display = format!("{}", p);
    assert!(display.contains("chunk #3"));
    assert!(display.contains("31.2 KB"));
}

#[test]
fn test_progress_chunk_skipped_display() {
    let p = TransferProgress::ChunkComplete {
        index: 0,
        bytes_transferred: 0,
        skipped: true,
    };
    let display = format!("{}", p);
    assert!(display.contains("skipped"));
}

#[test]
fn test_progress_finished_display() {
    let p = TransferProgress::Finished {
        total_bytes: 7_500_000,
        elapsed_ms: 3200,
    };
    let display = format!("{}", p);
    assert!(display.contains("complete"));
    assert!(display.contains("3.2s"));
}

// ── TransferStats ──────────────────────────────────────────────

#[test]
fn test_transfer_stats_default() {
    let stats = TransferStats::default();
    assert_eq!(stats.total_chunks(), 0);
    assert_eq!(stats.total_bytes(), 0);
    assert!((stats.savings_pct() - 0.0).abs() < f64::EPSILON);
    assert_eq!(stats.bytes_per_second(), 0);
}

#[test]
fn test_transfer_stats_with_values() {
    let stats = TransferStats {
        sections_transferred: 11,
        chunks_transferred: 5,
        chunks_skipped: 3,
        bytes_transferred: 75000,
        bytes_skipped: 60000,
        elapsed_ms: 1200,
    };

    assert_eq!(stats.total_chunks(), 8);
    assert_eq!(stats.total_bytes(), 135000);
    let savings = stats.savings_pct();
    assert!(savings > 44.0 && savings < 45.0);
    assert!(stats.bytes_per_second() > 0);
}

#[test]
fn test_transfer_stats_display_no_skipped() {
    let stats = TransferStats {
        sections_transferred: 5,
        chunks_transferred: 3,
        chunks_skipped: 0,
        bytes_transferred: 50000,
        bytes_skipped: 0,
        elapsed_ms: 500,
    };
    let display = format!("{}", stats);
    assert!(display.contains("5 sections"));
    assert!(display.contains("3 chunks"));
    assert!(!display.contains("skipped"));
}

#[test]
fn test_transfer_stats_display_with_skipped() {
    let stats = TransferStats {
        sections_transferred: 8,
        chunks_transferred: 2,
        chunks_skipped: 5,
        bytes_transferred: 20000,
        bytes_skipped: 80000,
        elapsed_ms: 300,
    };
    let display = format!("{}", stats);
    assert!(display.contains("5 skipped"));
    assert!(display.contains("savings"));
}

#[test]
fn test_transfer_stats_bytes_per_second() {
    let stats = TransferStats {
        bytes_transferred: 1_000_000,
        elapsed_ms: 1000, // 1 second
        ..Default::default()
    };
    assert_eq!(stats.bytes_per_second(), 1_000_000);
}

#[test]
fn test_transfer_stats_bytes_per_second_zero_elapsed() {
    let stats = TransferStats {
        bytes_transferred: 1_000_000,
        elapsed_ms: 0,
        ..Default::default()
    };
    assert_eq!(stats.bytes_per_second(), 0);
}

// ── hex_hash serde ─────────────────────────────────────────────

#[test]
fn test_hex_hash_roundtrip_via_entry() {
    let hash = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD,
        0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
        0xCD, 0xEF,
    ];

    let entry = ChunkManifestEntry::new(0, hash, 100, 200);
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));

    let decoded: ChunkManifestEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.hash, hash);
}

#[test]
fn test_hex_hash_invalid_length() {
    let json = r#"{"index":0,"hash":"aabb","compressed_size":0,"uncompressed_size":0}"#;
    let result: Result<ChunkManifestEntry, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_hex_hash_invalid_chars() {
    let json = r#"{"index":0,"hash":"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz","compressed_size":0,"uncompressed_size":0}"#;
    let result: Result<ChunkManifestEntry, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

// ── format_size ────────────────────────────────────────────────

#[test]
fn test_format_size_bytes() {
    assert_eq!(types::format_size(0), "0 B");
    assert_eq!(types::format_size(512), "512 B");
    assert_eq!(types::format_size(1023), "1023 B");
}

#[test]
fn test_format_size_kilobytes() {
    assert_eq!(types::format_size(1024), "1.0 KB");
    assert_eq!(types::format_size(32000), "31.2 KB");
}

#[test]
fn test_format_size_megabytes() {
    assert_eq!(types::format_size(1048576), "1.0 MB");
    assert_eq!(types::format_size(7_500_000), "7.2 MB");
}

// ── Integration: realistic delta push scenario ─────────────────

#[test]
fn test_realistic_delta_push_scenario() {
    // Scenario: Client edits 1 line in a 10 MB file.
    // The file has ~150 chunks (10 MB / 64 KB avg).
    // The edit changes 1 chunk. All other chunks are identical.

    let mut entries = Vec::new();
    for i in 0..150 {
        let mut hash = [0u8; 32];
        hash[0] = (i / 256) as u8;
        hash[1] = (i % 256) as u8;
        entries.push(ChunkManifestEntry::new(
            i as u32, hash, 32000, // ~32 KB compressed
            65536, // 64 KB uncompressed
        ));
    }

    // The edit changed chunk 75
    let mut edited_entries = entries.clone();
    edited_entries[75].hash = [0xFF; 32]; // different hash

    let manifest = ChunkManifest::new(edited_entries);

    // Server has all the original chunks
    let server_hashes: Vec<[u8; 32]> = entries.iter().map(|e| e.hash).collect();

    let negotiation = ChunkNegotiation::compute(&manifest, &server_hashes);

    // Only 1 chunk should need transferring
    assert_eq!(negotiation.needed.len(), 1);
    assert_eq!(negotiation.needed[0].index, 75);
    assert_eq!(negotiation.already_have, 149);
    assert_eq!(negotiation.bytes_needed, 32000);
    assert_eq!(negotiation.bytes_saved, 149 * 32000);

    // Savings should be ~99.3%
    assert!(negotiation.savings_pct() > 99.0);

    println!("Delta push scenario: {}", negotiation);
    println!(
        "  Transfer {} instead of {}",
        types::format_size(negotiation.bytes_needed),
        types::format_size(manifest.total_compressed()),
    );
}

#[test]
fn test_realistic_clone_scenario() {
    // Scenario: Fresh clone — client has nothing.
    let entries: Vec<ChunkManifestEntry> = (0..50)
        .map(|i| {
            let mut hash = [0u8; 32];
            hash[0] = i;
            ChunkManifestEntry::new(i as u32, hash, 30000, 65536)
        })
        .collect();

    let manifest = ChunkManifest::new(entries);

    // Client has nothing
    let negotiation = ChunkNegotiation::compute(&manifest, &[]);

    assert!(negotiation.is_full_transfer());
    assert_eq!(negotiation.needed.len(), 50);
    assert_eq!(negotiation.bytes_needed, 50 * 30000);
    assert!((negotiation.savings_pct() - 0.0).abs() < f64::EPSILON);
}
