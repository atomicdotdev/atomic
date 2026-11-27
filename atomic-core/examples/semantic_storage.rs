//! Read-only measurement of production serialization. No Repository/redb access.
use anyhow::{ensure, Context, Result};
use atomic_core::{
    change::{
        format_v3::{
            graph_fields::GraphEncoding, semantic::SemanticEncoding, ChangeReader, SectionType,
        },
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

#[derive(Default, Serialize)]
struct Sizes {
    objects: usize,
    original_bytes: usize,
    raw_bytes: usize,
    selected_bytes: usize,
    raw_semantic: usize,
    selected_semantic: usize,
    content: usize,
    compact_objects: usize,
    field_graph_objects: usize,
    predictive_objects: usize,
    v1_object_bytes: usize,
    v1_semantic_bytes: usize,
    v1_graph_bytes: usize,
    selected_graph_bytes: usize,
}
impl Sizes {
    fn add(&mut self, s: &Self) {
        self.objects += s.objects;
        self.original_bytes += s.original_bytes;
        self.raw_bytes += s.raw_bytes;
        self.selected_bytes += s.selected_bytes;
        self.raw_semantic += s.raw_semantic;
        self.selected_semantic += s.selected_semantic;
        self.content += s.content;
        self.compact_objects += s.compact_objects;
        self.field_graph_objects += s.field_graph_objects;
        self.predictive_objects += s.predictive_objects;
        self.v1_object_bytes += s.v1_object_bytes;
        self.v1_semantic_bytes += s.v1_semantic_bytes;
        self.v1_graph_bytes += s.v1_graph_bytes;
        self.selected_graph_bytes += s.selected_graph_bytes;
    }
}

#[derive(Serialize)]
struct Row {
    path: PathBuf,
    source_hash: String,
    sizes: Sizes,
    encode_ms: f64,
    decode_ms: f64,
    v1_decode_ms: f64,
}

fn sections(bytes: &[u8]) -> Result<(usize, usize, usize, u32)> {
    let mut input = bytes;
    let mut reader = ChangeReader::open(&mut input)?;
    let version = reader.file_header().version;
    let mut semantic = 0;
    let mut content = 0;
    let mut graph = 0;
    while let Some(section) = reader.next_section()? {
        match section.section_type {
            SectionType::Semantic => semantic += section.compressed_size as usize,
            SectionType::Content => content += section.compressed_size as usize,
            SectionType::Graph => graph += section.compressed_size as usize,
            _ => {}
        }
    }
    reader.verify()?;
    Ok((semantic, content, graph, version))
}

fn measure(path: &Path) -> Result<Row> {
    let original = fs::read(path)?;
    let (mut change, source_hash) = Change::deserialize(&mut original.as_slice())?;
    // These are new unsigned objects, not rewritten historical identities. Old
    // signatures cannot authenticate new representations; never reuse them.
    change.signature = None;
    change.semantic_encoding = SemanticEncoding::Raw;
    change.graph_encoding = GraphEncoding::Raw;
    let mut raw = vec![];
    change.serialize(&mut raw)?;
    let mut v1 = vec![];
    if change.file_ops().is_empty() {
        v1 = raw.clone();
    } else {
        change.semantic_encoding = SemanticEncoding::Compact;
        change.serialize(&mut v1)?;
        if v1.len() > raw.len() {
            v1 = raw.clone();
        }
    }
    let start = Instant::now();
    let (previous, _) = Change::deserialize(&mut v1.as_slice())?;
    let v1_decode_ms = start.elapsed().as_secs_f64() * 1000.;
    ensure!(
        previous.hashed == change.hashed && previous.contents == change.contents,
        "v1 baseline differs"
    );
    drop(previous);
    change.semantic_encoding = SemanticEncoding::Auto;
    change.graph_encoding = GraphEncoding::Auto;
    let mut selected = vec![];
    let start = Instant::now();
    let new_hash = change.serialize(&mut selected)?;
    let encode_ms = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let (decoded, hash) = Change::deserialize(&mut selected.as_slice())?;
    let decode_ms = start.elapsed().as_secs_f64() * 1000.;
    ensure!(
        hash == new_hash && decoded.hashed == change.hashed && decoded.contents == change.contents,
        "lossless production round-trip failed"
    );
    let mut repeated = vec![];
    ensure!(
        decoded.serialize(&mut repeated)? == hash && repeated == selected,
        "wire round-trip failed"
    );
    ensure!(
        selected.len() <= raw.len(),
        "adaptive encoding regressed object size"
    );
    let (raw_semantic, _, _, _) = sections(&raw)?;
    let (v1_semantic, _, v1_graph, _) = sections(&v1)?;
    let (selected_semantic, content, selected_graph, _) = sections(&selected)?;
    let header =
        atomic_core::change::format_v3::FileHeader::from_bytes(selected[..64].try_into()?)?;
    ensure!(
        selected.len() <= v1.len(),
        "new encoding exceeds complete v1 baseline"
    );
    Ok(Row {
        path: path.into(),
        source_hash: source_hash.to_base32(),
        encode_ms,
        decode_ms,
        v1_decode_ms,
        sizes: Sizes {
            objects: 1,
            original_bytes: original.len(),
            raw_bytes: raw.len(),
            selected_bytes: selected.len(),
            raw_semantic,
            selected_semantic,
            content,
            compact_objects: usize::from(header.has_compact_semantics()),
            field_graph_objects: usize::from(header.has_graph_fields()),
            predictive_objects: usize::from(header.has_predictive_semantics()),
            v1_object_bytes: v1.len(),
            v1_semantic_bytes: v1_semantic,
            v1_graph_bytes: v1_graph,
            selected_graph_bytes: selected_graph,
        },
    })
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
fn main() -> Result<()> {
    let source = std::env::args()
        .nth(1)
        .context("usage: semantic_storage <change-file-or-directory>")?;
    let mut paths = vec![];
    collect(Path::new(&source), &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "no change files");
    let mut totals = Sizes::default();
    let mut rows = vec![];
    let mut failures = vec![];
    for path in paths {
        match measure(&path).with_context(|| path.display().to_string()) {
            Ok(row) => {
                eprintln!(
                    "{}: {} -> {} bytes",
                    path.display(),
                    row.sizes.raw_bytes,
                    row.sizes.selected_bytes
                );
                totals.add(&row.sizes);
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
        serde_json::to_string_pretty(&serde_json::json!({"zstd_level":3,
        "comparison":"new unsigned canonical raw vs adaptive objects; historical files untouched; not a Git-pack comparison",
        "totals":totals,"rows":rows,"failures":failures}))?
    );
    ensure!(
        failures.is_empty(),
        "input failures; totals cover only successful inputs"
    );
    Ok(())
}
