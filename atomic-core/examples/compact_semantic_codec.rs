//! Read-only, redb-free sizing experiment. See docs/COMPACT-SEMANTIC-CODEC-EXPERIMENT.md.
#[path = "compact_semantic_codec/codec.rs"]
mod codec;

use anyhow::{ensure, Context, Result};
use atomic_core::change::{
    format_v3::{ChangeReader, SectionType},
    FileOps,
};
use atomic_core::Base32;
use serde::Serialize;
use std::{
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Default, Serialize)]
struct Sizes {
    objects: usize,
    original_object_bytes: usize,
    original_semantic_bytes: usize,
    baseline_semantic_bytes: usize,
    compact_semantic_bytes: usize,
    projected_object_bytes: usize,
    compressed_content_bytes: usize,
    authored_content_bytes: usize,
    files: usize,
    lines: usize,
    tokens: usize,
}

impl Sizes {
    fn add(&mut self, s: &Self) {
        self.objects += s.objects;
        self.original_object_bytes += s.original_object_bytes;
        self.original_semantic_bytes += s.original_semantic_bytes;
        self.baseline_semantic_bytes += s.baseline_semantic_bytes;
        self.compact_semantic_bytes += s.compact_semantic_bytes;
        self.projected_object_bytes += s.projected_object_bytes;
        self.compressed_content_bytes += s.compressed_content_bytes;
        self.authored_content_bytes += s.authored_content_bytes;
        self.files += s.files;
        self.lines += s.lines;
        self.tokens += s.tokens;
    }
}

#[derive(Serialize)]
struct Row {
    path: PathBuf,
    schema: u32,
    verified_hash: String,
    sizes: Sizes,
    encode_ms: f64,
    decode_ms: f64,
    exact_roundtrip: bool,
}

fn collect(path: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            collect(&entry?.path(), paths)?;
        }
    } else if path.extension().is_some_and(|s| s == "change") {
        paths.push(path.into());
    }
    Ok(())
}

fn measure(path: &Path, level: i32) -> Result<Row> {
    let mut input = BufReader::new(fs::File::open(path)?);
    let mut reader = ChangeReader::open(&mut input)?;
    let schema = reader.file_header().version;
    let mut semantic = None;
    let mut content = Vec::new();
    let mut sizes = Sizes {
        objects: 1,
        original_object_bytes: fs::metadata(path)?.len().try_into()?,
        ..Sizes::default()
    };
    let mut next_chunk = 0;
    while let Some(section) = reader.next_section()? {
        match section.section_type {
            SectionType::Semantic => {
                ensure!(
                    semantic.is_none(),
                    "experiment expects one aggregate semantic section"
                );
                sizes.original_semantic_bytes += section.compressed_size as usize;
                semantic = Some(section.payload);
            }
            SectionType::Content => {
                let info = section
                    .content_chunk_info
                    .context("missing chunk metadata")?;
                ensure!(
                    info.chunk_index == next_chunk,
                    "nonsequential content chunks"
                );
                ensure!(
                    section.payload.len() == info.uncompressed_len as usize,
                    "chunk length mismatch"
                );
                ensure!(
                    *blake3::hash(&section.payload).as_bytes() == info.chunk_hash,
                    "chunk hash mismatch"
                );
                next_chunk += 1;
                sizes.compressed_content_bytes += section.compressed_size as usize;
                content.extend(section.payload);
            }
            _ => {}
        }
    }
    let hash = reader.verify()?;
    ensure!(
        input.read(&mut [0])? == 0,
        "trailing bytes after change trailer"
    );
    sizes.authored_content_bytes = content.len();
    let mut encode_ms = 0.0;
    let mut decode_ms = 0.0;
    if let Some(raw) = semantic {
        let (ops, rest): (Vec<FileOps>, _) = postcard::take_from_bytes(&raw)?;
        ensure!(rest.is_empty(), "trailing semantic bytes");
        sizes.files = ops.len();
        sizes.lines = ops.iter().map(FileOps::line_count).sum();
        sizes.tokens = ops.iter().map(FileOps::token_count).sum();
        sizes.baseline_semantic_bytes = zstd::encode_all(raw.as_slice(), level)?.len();
        let start = Instant::now();
        let encoded = codec::encode(&ops, &content, level)?;
        encode_ms = start.elapsed().as_secs_f64() * 1000.0;
        sizes.compact_semantic_bytes = encoded.len();
        let start = Instant::now();
        // Only encoded bytes + authored content are passed: no repository, no redb.
        let decoded = codec::decode(&encoded, &content)?;
        decode_ms = start.elapsed().as_secs_f64() * 1000.0;
        ensure!(
            decoded == ops,
            "semantic operation/identity round-trip mismatch"
        );
        ensure!(
            postcard::to_allocvec(&decoded)? == raw,
            "expanded postcard bytes differ"
        );
    }
    sizes.projected_object_bytes =
        sizes.original_object_bytes - sizes.original_semantic_bytes + sizes.compact_semantic_bytes;
    Ok(Row {
        path: path.into(),
        schema,
        verified_hash: atomic_core::types::Hash::from_bytes(hash).to_base32(),
        sizes,
        encode_ms,
        decode_ms,
        exact_roundtrip: true,
    })
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let source = args
        .next()
        .context("usage: compact_semantic_codec <change-file-or-directory> [zstd-level:1..19]")?;
    let level: i32 = args.next().map_or(Ok(3), |s| s.parse())?;
    ensure!(
        (1..=19).contains(&level) && args.next().is_none(),
        "expected zstd level 1..19"
    );
    ensure!(Path::new(&source).exists(), "source does not exist");
    let mut paths = Vec::new();
    collect(Path::new(&source), &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "no .change files found");
    let mut totals = Sizes::default();
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        match measure(&path, level).with_context(|| path.display().to_string()) {
            Ok(row) => {
                eprintln!(
                    "{}: semantic {} -> {} bytes; object {} -> {}; exact round-trip OK",
                    path.file_name().unwrap().to_string_lossy(),
                    row.sizes.original_semantic_bytes,
                    row.sizes.compact_semantic_bytes,
                    row.sizes.original_object_bytes,
                    row.sizes.projected_object_bytes
                );
                totals.add(&row.sizes);
                rows.push(row);
            }
            Err(error) => {
                eprintln!("FAILED: {error:#}");
                failures.push(format!("{error:#}"));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "experiment": "compact-semantic-v1-not-production", "zstd_level": level,
            "notes": "Projected objects replace only the semantic payload; all graph/content/owner/dependency/signature bytes remain counted. New object hashes/signatures would be required. No Git-pack equivalence claimed.",
            "totals": totals, "rows": rows, "failures": failures,
        }))?
    );
    ensure!(
        failures.is_empty(),
        "one or more inputs failed; results cover successful inputs only"
    );
    Ok(())
}
