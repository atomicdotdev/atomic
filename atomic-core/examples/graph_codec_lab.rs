//! Read-only lossless graph-field experiment; not a production format.
#[path = "graph_codec_lab/codec.rs"]
mod codec;
use anyhow::{ensure, Context, Result};
use atomic_core::{
    change::{
        format_v3::{semantic::SemanticEncoding, ChangeReader, GraphSectionPayload, SectionType},
        Change,
    },
    Base32,
};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Serialize)]
struct Row {
    path: PathBuf,
    source_hash: String,
    baseline_object_bytes: usize,
    graph_bytes: usize,
    candidate_graph_bytes: usize,
    selected_graph_bytes: usize,
    projected_object_bytes: usize,
    semantic_bytes: usize,
    semantic_field_bytes: usize,
    semantic_sequential_bytes: usize,
    selected_semantic_bytes: usize,
    combined_projection_bytes: usize,
    baseline_graph_decode_ms: f64,
    encode_ms: f64,
    decode_ms: f64,
    semantic_encode_ms: f64,
    semantic_decode_ms: f64,
    semantic_sequential_encode_ms: f64,
    semantic_sequential_decode_ms: f64,
}
fn collect(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let p = entry?.path();
            if !p.is_symlink() {
                collect(&p, out)?;
            }
        }
    } else if path.extension().is_some_and(|s| s == "change") {
        out.push(path.into());
    }
    Ok(())
}
fn measure(path: &Path) -> Result<Row> {
    let original = fs::read(path)?;
    let (mut change, hash) = Change::deserialize(&mut original.as_slice())?;
    change.signature = None;
    change.graph_encoding = atomic_core::change::format_v3::graph_fields::GraphEncoding::Raw;
    change.semantic_encoding = SemanticEncoding::Raw;
    let mut baseline = vec![];
    change.serialize(&mut baseline)?;
    if !change.file_ops().is_empty() {
        change.semantic_encoding = SemanticEncoding::Compact;
        let mut compact = vec![];
        change.serialize(&mut compact)?;
        if compact.len() < baseline.len() {
            baseline = compact;
        }
    }
    let mut input = baseline.as_slice();
    let mut reader = ChangeReader::open(&mut input)?;
    let mut graph_bytes = 0;
    let mut candidate_graph_bytes = 0;
    let mut selected_graph_bytes = 0;
    let mut encode_ms = 0.;
    let mut decode_ms = 0.;
    let mut baseline_graph_decode_ms = 0.;
    let mut semantic_bytes = 0;
    while let Some(section) = reader.next_section()? {
        if section.section_type == SectionType::Semantic {
            semantic_bytes += section.compressed_size as usize;
        }
        if section.section_type == SectionType::Graph {
            graph_bytes += section.compressed_size as usize;
            let graph = GraphSectionPayload::from_postcard_bytes(&section.payload)?;
            ensure!(
                postcard::to_allocvec(&graph)? == section.payload,
                "graph serialization is not the measured reference"
            );
            let baseline_frame = zstd::encode_all(section.payload.as_slice(), 3)?;
            ensure!(
                baseline_frame.len() == section.compressed_size as usize,
                "graph baseline compression differs"
            );
            let start = Instant::now();
            let raw = zstd::decode_all(baseline_frame.as_slice())?;
            let parsed = GraphSectionPayload::from_postcard_bytes(&raw)?;
            baseline_graph_decode_ms += start.elapsed().as_secs_f64() * 1000.;
            ensure!(parsed == graph, "graph baseline differs");
            drop(parsed);
            drop(raw);
            let start = Instant::now();
            let encoded = codec::encode(&graph)?;
            encode_ms += start.elapsed().as_secs_f64() * 1000.;
            let start = Instant::now();
            let restored = codec::decode(&encoded)?;
            let parsed = GraphSectionPayload::from_postcard_bytes(&restored)?;
            decode_ms += start.elapsed().as_secs_f64() * 1000.;
            ensure!(restored == section.payload, "graph payload bytes changed");
            ensure!(parsed == graph, "graph operations changed");
            candidate_graph_bytes += encoded.len();
            selected_graph_bytes += encoded.len().min(section.compressed_size as usize);
        }
    }
    reader.verify()?;
    let mut semantic_sizes = [0; 2];
    let mut encode_times = [0.; 2];
    let mut decode_times = [0.; 2];
    if semantic_bytes > 0 {
        for mode in 0..2 {
            let start = Instant::now();
            let encoded = if mode == 0 {
                codec::encode_with_content(&change.hashed.file_ops, &change.contents)?
            } else {
                codec::encode_with_sequential_content(&change.hashed.file_ops, &change.contents)?
            };
            encode_times[mode] = start.elapsed().as_secs_f64() * 1000.;
            semantic_sizes[mode] = encoded.len();
            let start = Instant::now();
            let raw = codec::decode_with_content(&encoded, &change.contents)?;
            let ops: Vec<atomic_core::change::FileOps> = postcard::from_bytes(&raw)?;
            decode_times[mode] = start.elapsed().as_secs_f64() * 1000.;
            ensure!(
                ops == change.file_ops(),
                "field-context semantics changed operations"
            );
            ensure!(
                raw == postcard::to_allocvec(change.file_ops())?,
                "field-context semantics changed original payload bytes"
            );
        }
    }
    let selected_semantic_bytes = semantic_sizes[0].min(semantic_sizes[1]).min(semantic_bytes);
    Ok(Row {
        path: path.into(),
        source_hash: hash.to_base32(),
        baseline_object_bytes: baseline.len(),
        graph_bytes,
        candidate_graph_bytes,
        selected_graph_bytes,
        projected_object_bytes: baseline.len() - graph_bytes + selected_graph_bytes,
        semantic_bytes,
        semantic_field_bytes: semantic_sizes[0],
        semantic_sequential_bytes: semantic_sizes[1],
        selected_semantic_bytes,
        combined_projection_bytes: baseline.len() - graph_bytes + selected_graph_bytes
            - semantic_bytes
            + selected_semantic_bytes,
        baseline_graph_decode_ms,
        encode_ms,
        decode_ms,
        semantic_encode_ms: encode_times[0],
        semantic_decode_ms: decode_times[0],
        semantic_sequential_encode_ms: encode_times[1],
        semantic_sequential_decode_ms: decode_times[1],
    })
}
fn main() -> Result<()> {
    let source = std::env::args()
        .nth(1)
        .context("usage: graph_codec_lab <change-file-or-directory>")?;
    let mut paths = vec![];
    collect(Path::new(&source), &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "no change files");
    let mut rows = vec![];
    let mut failures = vec![];
    for path in paths {
        match measure(&path).with_context(|| path.display().to_string()) {
            Ok(row) => {
                eprintln!(
                    "{}: graph {} -> {} bytes",
                    path.display(),
                    row.graph_bytes,
                    row.candidate_graph_bytes
                );
                rows.push(row);
            }
            Err(error) => {
                eprintln!("{error:#}");
                failures.push(format!("{error:#}"));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"experiment":"graph field-context columns; NOT production","zstd_level":3,
        "comparison":"unsigned frozen schema-2/3/4 ATSC-v1 baseline; full prototype headers counted; exact graph payload reconstruction; smaller-of-raw fallback; no operations coalesced",
        "rows":rows,"failures":failures})
        )?
    );
    ensure!(failures.is_empty(), "input failures; partial results only");
    Ok(())
}
