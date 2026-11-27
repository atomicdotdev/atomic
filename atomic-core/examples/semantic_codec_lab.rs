//! Standalone profiling/encoding lab. Never changes production wire formats.
#[path = "semantic_codec_lab/context.rs"]
mod context;
#[path = "semantic_codec_lab/control.rs"]
mod control;
#[path = "semantic_codec_lab/lengths.rs"]
mod lengths;
#[path = "semantic_codec_lab/transforms.rs"]
mod transforms;
use anyhow::{ensure, Context, Result};
use atomic_core::{
    change::{
        format_v3::{semantic, ChangeReader, SectionType},
        Change, FileOps,
    },
    Base32,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Instant,
};

const MAGIC: &[u8; 5] = b"ATSL\x01";
const NAMES: [&str; 10] = [
    "metadata",
    "control",
    "branches",
    "leaves",
    "old_numbers",
    "new_numbers",
    "positions",
    "kinds",
    "references",
    "literals",
];

#[derive(Serialize, Deserialize)]
struct View<'a> {
    split_ids: bool,
    content_hash: [u8; 32],
    #[serde(borrow)]
    metadata: &'a [u8],
    #[serde(borrow)]
    control: &'a [u8],
    #[serde(borrow)]
    branches: &'a [u8],
    #[serde(borrow)]
    leaves: &'a [u8],
    #[serde(borrow)]
    old_numbers: &'a [u8],
    #[serde(borrow)]
    new_numbers: &'a [u8],
    #[serde(borrow)]
    positions: &'a [u8],
    #[serde(borrow)]
    kinds: &'a [u8],
    #[serde(borrow)]
    references: &'a [u8],
    #[serde(borrow)]
    literals: &'a [u8],
}
impl<'a> View<'a> {
    fn columns(&self) -> [&'a [u8]; 10] {
        [
            self.metadata,
            self.control,
            self.branches,
            self.leaves,
            self.old_numbers,
            self.new_numbers,
            self.positions,
            self.kinds,
            self.references,
            self.literals,
        ]
    }
    fn from_columns(split_ids: bool, content_hash: [u8; 32], c: [&'a [u8]; 10]) -> Self {
        Self {
            split_ids,
            content_hash,
            metadata: c[0],
            control: c[1],
            branches: c[2],
            leaves: c[3],
            old_numbers: c[4],
            new_numbers: c[5],
            positions: c[6],
            kinds: c[7],
            references: c[8],
            literals: c[9],
        }
    }
}
#[derive(Serialize)]
struct Column {
    method: u8,
    raw_len: u32,
    data: Vec<u8>,
}
#[derive(Serialize)]
struct Frame {
    split_ids: bool,
    content_hash: [u8; 32],
    columns: [Column; 10],
}
#[derive(Deserialize)]
struct BorrowedColumn<'a> {
    method: u8,
    raw_len: u32,
    #[serde(borrow)]
    data: &'a [u8],
}
#[derive(Deserialize)]
struct BorrowedFrame<'a> {
    split_ids: bool,
    content_hash: [u8; 32],
    #[serde(borrow)]
    columns: [BorrowedColumn<'a>; 10],
}

fn numeric(i: usize) -> bool {
    (1..9).contains(&i)
}
fn compressed(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::encode_all(bytes, 3)?)
}

fn make_column(view: &View<'_>, content: &[u8], i: usize, mut method: u8) -> Result<Column> {
    let raw = view.columns()[i];
    if method == 8 {
        method = match i {
            1 => 5,
            7 => 7,
            8 => 6,
            _ => 0,
        };
    }
    if method == 10 {
        method = match i {
            1 => 9,
            7 => 7,
            _ => 0,
        };
    }
    let actual = if !numeric(i) || raw.is_empty() {
        0
    } else {
        match method {
            5 if i != 1 => 0,
            6 if i != 8 => 0,
            7 if i != 7 => 0,
            9 if i != 1 => 0,
            _ => method,
        }
    };
    let data = match actual {
        0..=4 => transforms::encode(actual, raw)?,
        5 => control::split(raw)?,
        6 => context::split_references(view)?,
        7 => context::predict_kinds(view, content)?,
        9 => lengths::split(view, content)?,
        _ => anyhow::bail!("unknown candidate"),
    };
    Ok(Column {
        method: actual,
        raw_len: raw.len().try_into()?,
        data,
    })
}

fn encode_frame(view: &View<'_>, content: &[u8], method: Option<u8>) -> Result<Vec<u8>> {
    let mut columns = vec![];
    let mut predicted_control = false;
    for (i, raw) in view.columns().into_iter().enumerate() {
        let mut best = make_column(view, content, i, method.unwrap_or(0))?;
        if numeric(i) && !raw.is_empty() && method.is_none() {
            let mut score = compressed(&postcard::to_allocvec(&best)?)?.len();
            let extra = match i {
                1 => vec![5, 9],
                7 => vec![7],
                8 if !predicted_control => vec![6],
                _ => vec![],
            };
            for method in (1..transforms::METHODS.len() as u8).chain(extra) {
                let candidate = make_column(view, content, i, method)?;
                let size = compressed(&postcard::to_allocvec(&candidate)?)?.len();
                if size < score {
                    best = candidate;
                    score = size;
                }
            }
        }
        if i == 1 {
            predicted_control = best.method == 9;
        }
        columns.push(best);
    }
    let frame = Frame {
        split_ids: view.split_ids,
        content_hash: view.content_hash,
        columns: columns.try_into().ok().context("column count")?,
    };
    let mut out = MAGIC.to_vec();
    out.extend(compressed(&postcard::to_allocvec(&frame)?)?);
    Ok(out)
}

fn decode_frame(encoded: &[u8], content: &[u8]) -> Result<Vec<u8>> {
    ensure!(content.len() <= transforms::LIMIT, "authored content limit");
    let compressed = encoded.strip_prefix(MAGIC).context("unknown lab version")?;
    let mut bytes = vec![];
    let limit = transforms::LIMIT * 2 + 4096;
    zstd::stream::read::Decoder::new(compressed)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "lab frame limit");
    let (frame, rest): (BorrowedFrame<'_>, _) = postcard::take_from_bytes(&bytes)?;
    ensure!(rest.is_empty(), "trailing lab data");
    ensure!(
        frame.content_hash == *blake3::hash(content).as_bytes(),
        "lab content binding mismatch"
    );
    let mut columns: Vec<Vec<u8>> = vec![];
    let mut deferred_kind = None;
    let mut deferred_control = None;
    let mut total = 0usize;
    for (i, column) in frame.columns.into_iter().enumerate() {
        total = total
            .checked_add(column.raw_len as usize)
            .context("column size overflow")?;
        ensure!(total <= transforms::LIMIT, "expanded lab payload limit");
        columns.push(if column.method == 9 {
            ensure!(i == 1, "length transform on wrong column");
            deferred_control = Some((column.data, column.raw_len as usize));
            vec![]
        } else if column.method == 7 {
            ensure!(i == 7, "kind transform on wrong column");
            deferred_kind = Some((column.data, column.raw_len as usize));
            vec![]
        } else if column.method == 6 {
            ensure!(i == 8, "reference transform on wrong column");
            ensure!(
                deferred_control.is_none(),
                "cyclic reference/control transforms"
            );
            context::join_references(column.data, &columns[1], column.raw_len as usize)?
        } else if column.method == 5 {
            ensure!(i == 1, "control transform on wrong column");
            control::join(column.data, column.raw_len as usize)?
        } else {
            transforms::decode(column.method, column.data, column.raw_len as usize)?
        });
    }
    if let Some((data, len)) = deferred_control {
        let refs: [&[u8]; 10] = columns
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
            .try_into()
            .ok()
            .context("column count")?;
        let view = View::from_columns(frame.split_ids, frame.content_hash, refs);
        let control = lengths::join(data, &view, content, len)?;
        columns[1] = control;
    }
    if let Some((data, len)) = deferred_kind {
        let refs: [&[u8]; 10] = columns
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
            .try_into()
            .ok()
            .context("column count")?;
        let view = View::from_columns(frame.split_ids, frame.content_hash, refs);
        let kinds = context::restore_kinds(&view, content, data, len)?;
        columns[7] = kinds;
    }
    let refs: [&[u8]; 10] = columns
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .context("column count")?;
    let view = View::from_columns(frame.split_ids, frame.content_hash, refs);
    let mut original = b"ATSC\x01".to_vec();
    original.extend(postcard::to_allocvec(&view)?);
    ensure!(
        original.len() <= transforms::LIMIT,
        "restored production payload limit"
    );
    Ok(original)
}

#[derive(Serialize)]
struct Profile {
    name: String,
    raw: usize,
    standalone_zstd3: usize,
}
#[derive(Serialize)]
struct Candidate {
    name: String,
    semantic_bytes: usize,
    projected_object_bytes: usize,
    encode_ms: f64,
    decode_ms: f64,
}
#[derive(Serialize)]
struct Row {
    path: PathBuf,
    source_hash: String,
    baseline_object_bytes: usize,
    baseline_semantic_bytes: usize,
    baseline_decode_ms: f64,
    sections: Vec<(String, usize)>,
    columns: Vec<Profile>,
    candidates: Vec<Candidate>,
}

fn measure(path: &Path) -> Result<Row> {
    let original = fs::read(path)?;
    let (mut change, source_hash) = Change::deserialize(&mut original.as_slice())?;
    change.signature = None;
    change.graph_encoding = atomic_core::change::format_v3::graph_fields::GraphEncoding::Raw;
    change.semantic_encoding = semantic::SemanticEncoding::Raw;
    let mut baseline = vec![];
    change.serialize(&mut baseline)?;
    if !change.file_ops().is_empty() {
        change.semantic_encoding = semantic::SemanticEncoding::Compact;
        let mut compact = vec![];
        change.serialize(&mut compact)?;
        if compact.len() < baseline.len() {
            baseline = compact;
        }
    }
    let mut input = baseline.as_slice();
    let mut reader = ChangeReader::open(&mut input)?;
    let version = reader.file_header().version;
    let mut sections = vec![];
    let mut baseline_semantic_bytes = 0;
    let mut semantic_payload = None;
    while let Some(section) = reader.next_section()? {
        sections.push((
            section.section_type.name().into(),
            section.compressed_size as usize,
        ));
        if section.section_type == SectionType::Semantic {
            baseline_semantic_bytes += section.compressed_size as usize;
            semantic_payload = Some(section.payload);
        }
    }
    reader.verify()?;
    let mut columns = vec![];
    let mut candidates = vec![];
    let mut baseline_decode_ms = 0.;
    if let Some(baseline_payload) = semantic_payload {
        let baseline_frame = compressed(&baseline_payload)?;
        ensure!(
            baseline_frame.len() == baseline_semantic_bytes,
            "baseline compression differs"
        );
        let start = Instant::now();
        let decoded = zstd::decode_all(baseline_frame.as_slice())?;
        let ops: Vec<FileOps> = if version == 4 {
            semantic::decode(&decoded, &change.contents)?
        } else {
            postcard::from_bytes(&decoded)?
        };
        baseline_decode_ms = start.elapsed().as_secs_f64() * 1000.;
        ensure!(ops == change.file_ops(), "baseline semantic mismatch");
        drop(ops);
        let production = semantic::encode(change.file_ops(), &change.contents)?;
        let view: View<'_> = postcard::from_bytes(&production[5..])?;
        ensure!(
            [
                b"ATSC\x01".as_slice(),
                postcard::to_allocvec(&view)?.as_slice()
            ]
            .concat()
                == production,
            "profile layout differs"
        );
        for (i, column) in view.columns().into_iter().enumerate() {
            columns.push(Profile {
                name: NAMES[i].into(),
                raw: column.len(),
                standalone_zstd3: compressed(column)?.len(),
            });
        }
        for method in [
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
            Some(7),
            Some(8),
            Some(9),
            Some(10),
            None,
        ] {
            let name = method
                .map_or("adaptive", |m| match m {
                    5 => "control_fields",
                    6 => "reference_fields",
                    7 => "predicted_kinds",
                    8 => "contextual_combined",
                    9 => "predicted_lengths",
                    10 => "predicted_lengths_and_kinds",
                    _ => transforms::METHODS[m as usize],
                })
                .to_string();
            let start = Instant::now();
            let frame = encode_frame(&view, &change.contents, method)?;
            let encode_ms = start.elapsed().as_secs_f64() * 1000.;
            let start = Instant::now();
            let restored = decode_frame(&frame, &change.contents)?;
            let decoded = semantic::decode(&restored, &change.contents)?;
            let decode_ms = start.elapsed().as_secs_f64() * 1000.;
            ensure!(
                restored == production,
                "{name} lost production payload bytes"
            );
            ensure!(decoded == change.file_ops(), "{name} changed operations");
            candidates.push(Candidate {
                name,
                semantic_bytes: frame.len(),
                projected_object_bytes: baseline.len() - baseline_semantic_bytes + frame.len(),
                encode_ms,
                decode_ms,
            });
        }
    }
    Ok(Row {
        path: path.into(),
        source_hash: source_hash.to_base32(),
        baseline_object_bytes: baseline.len(),
        baseline_semantic_bytes,
        baseline_decode_ms,
        sections,
        columns,
        candidates,
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
        .context("usage: semantic_codec_lab <change-file-or-directory>")?;
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
                    "{}: production semantics={} candidate minimum={}",
                    path.display(),
                    row.baseline_semantic_bytes,
                    row.candidates
                        .iter()
                        .map(|c| c.semantic_bytes)
                        .min()
                        .unwrap_or(0)
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
            &serde_json::json!({"experiment":"not a production wire format","zstd_level":3,
        "comparison":"unsigned frozen schema-2/3/4 ATSC-v1 baseline; all candidate headers counted; projections retain other object bytes; independent column sizes are diagnostic, not additive",
        "rows":rows,"failures":failures})
        )?
    );
    ensure!(
        failures.is_empty(),
        "some inputs failed; results are partial"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_envelope_and_semantics_without_database() -> Result<()> {
        let ops = vec![FileOps::edit(atomic_core::crdt::TrunkId::ROOT, "f".into())];
        let production = semantic::encode(&ops, b"bytes")?;
        let view: View<'_> = postcard::from_bytes(&production[5..])?;
        for method in [
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
            Some(7),
            Some(8),
            Some(9),
            Some(10),
            None,
        ] {
            let encoded = encode_frame(&view, b"bytes", method)?;
            let restored = decode_frame(&encoded, b"bytes")?;
            assert_eq!(restored, production);
            assert_eq!(semantic::decode(&restored, b"bytes")?, ops);
            for end in 0..encoded.len() {
                assert!(decode_frame(&encoded[..end], b"bytes").is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn rejects_cyclic_context_dependencies() -> Result<()> {
        let ops = vec![FileOps::edit(atomic_core::crdt::TrunkId::ROOT, "f".into())];
        let production = semantic::encode(&ops, b"bytes")?;
        let view: View<'_> = postcard::from_bytes(&production[5..])?;
        let mut columns = (0..10)
            .map(|i| make_column(&view, b"bytes", i, 0))
            .collect::<Result<Vec<_>>>()?;
        columns[1] = make_column(&view, b"bytes", 1, 9)?;
        columns[8] = Column {
            method: 6,
            raw_len: 0,
            data: context::split_references(&view)?,
        };
        let frame = Frame {
            split_ids: view.split_ids,
            content_hash: view.content_hash,
            columns: columns.try_into().ok().context("column count")?,
        };
        let mut encoded = MAGIC.to_vec();
        encoded.extend(compressed(&postcard::to_allocvec(&frame)?)?);
        assert!(decode_frame(&encoded, b"bytes")
            .unwrap_err()
            .to_string()
            .contains("cyclic"));
        Ok(())
    }
}
