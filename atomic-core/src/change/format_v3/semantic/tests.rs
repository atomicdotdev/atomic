use super::*;
use crate::{
    change::{
        span_patch::{ExternalFile, SpanOp, SpanPatch, SpanProjectionId},
        Change, ChangeHeader, Encoding, LineOps,
    },
    crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp},
    diff::token::{TokenKind, Tokenizer},
    types::{ChangePosition, Hash, NodeId},
};

fn token(after: Option<LeafId>, text: &[u8]) -> LeafOp {
    LeafOp::Insert {
        after,
        kind: TokenKind::Word,
        content: text.to_vec(),
    }
}

pub(super) fn fixture() -> Vec<FileOps> {
    let trunk = TrunkId::new(NodeId::new(u64::MAX), u32::MAX);
    let b = |o, i| BranchId::new(NodeId::new(o), i);
    let l = |o, i| LeafId::new(NodeId::new(o), i);
    let leaves = vec![
        token(None, b""),
        token(Some(LeafId::ROOT), b"\xff\0"),
        token(Some(l(u64::MAX, u32::MAX)), "é\r\n".as_bytes()),
        LeafOp::Delete { leaf: l(19, 8) },
        LeafOp::Replace {
            leaf: l(6, 2),
            new_content: b"absent old text".to_vec(),
        },
        LeafOp::Restore { leaf: l(0, 92) },
    ];
    let lines = [
        BranchOp::Insert {
            after: None,
            content: leaves.clone(),
        },
        BranchOp::Insert {
            after: Some(BranchId::ROOT),
            content: vec![],
        },
        BranchOp::Delete {
            branch: b(9, 4),
            content: leaves.clone(),
        },
        BranchOp::Modify {
            branch: b(3, 8),
            old_content: leaves,
            new_content: vec![token(None, b"new\n")],
        },
        BranchOp::Restore { branch: b(8, 1) },
        BranchOp::Reparent {
            branch: b(9, 8),
            new_after: Some(b(3, 9)),
        },
        BranchOp::Reparent {
            branch: b(9, 8),
            new_after: None,
        },
    ];
    vec![
        None,
        Some(TrunkOp::Create {
            path: "α".into(),
            encoding: Some(Encoding::Utf8),
        }),
        Some(TrunkOp::Create {
            path: "binary".into(),
            encoding: None,
        }),
        Some(TrunkOp::Delete { trunk }),
        Some(TrunkOp::Move {
            trunk,
            new_path: "renamed".into(),
        }),
        Some(TrunkOp::Undelete { trunk }),
        Some(TrunkOp::SetMode { trunk, mode: 0o755 }),
        Some(TrunkOp::SetKind {
            trunk,
            kind: crate::change::InodeKind::Regular,
        }),
        Some(TrunkOp::ProjectedSpanEdit {
            file: ExternalFile {
                owner: Hash::of(b"owner"),
                ordinal: 5,
            },
            baseline: SpanProjectionId(Hash::of(b"baseline")),
        }),
    ]
    .into_iter()
    .map(|op| {
        let mut file = FileOps::new(trunk, "f".into(), op);
        for (i, op) in lines.iter().enumerate() {
            let mut line = LineOps::new_with_line_nums(
                b(i as u64, i as u32),
                op.clone(),
                (i % 2 == 0).then_some(usize::MAX - i),
                (i % 3 == 0).then_some(i),
            );
            if i % 2 == 0 {
                line.set_content_range(ChangePosition::new(u64::MAX), ChangePosition::new(0));
            }
            file.add_line_op(line);
        }
        file
    })
    .collect()
}

pub(crate) fn large_change() -> Change {
    let mut file = FileOps::create(TrunkId::ROOT, "generated.rs".into(), Some(Encoding::Utf8));
    let mut content = Vec::new();
    let mut leaf = 0;
    for i in 0..4096u32 {
        let text = format!("let variable_{i} = compute({i}, input);\n");
        let start = content.len();
        content.extend(text.as_bytes());
        let mut after = None;
        let tokens = Tokenizer::new(text.as_bytes())
            .map(|t| {
                let op = LeafOp::Insert {
                    after,
                    kind: t.kind(),
                    content: t.content().to_vec(),
                };
                after = Some(LeafId::new(NodeId::ROOT, leaf));
                leaf += 1;
                op
            })
            .collect();
        file.add_line_op(
            LineOps::insert_at(
                BranchId::new(NodeId::ROOT, i),
                i.checked_sub(1).map(|j| BranchId::new(NodeId::ROOT, j)),
                tokens,
                i as usize + 1,
            )
            .with_content_range(
                ChangePosition::new(start as u64),
                ChangePosition::new(content.len() as u64),
            ),
        );
    }
    Change::with_file_ops(
        ChangeHeader::new("compact fixture"),
        vec![],
        vec![file],
        content,
        vec![],
    )
}

#[test]
fn exact_all_fields_variants_and_layouts_without_database() {
    for content in [b"new\n".as_slice(), b"", b"\xff\0new\n"] {
        let ops = fixture();
        for split in [false, true] {
            let encoded = encode::encode_layout(&ops, content, split).unwrap();
            let result = decode(&encoded, content).unwrap();
            assert_eq!(result, ops);
            assert_eq!(
                postcard::to_allocvec(&result).unwrap(),
                postcard::to_allocvec(&ops).unwrap()
            );
            assert_eq!(
                encoded,
                encode::encode_layout(&ops, content, split).unwrap()
            );
        }
    }
    assert!(decode(&encode(&[], b"").unwrap(), b"").unwrap().is_empty());
}

#[test]
fn rejects_malformed_version_content_counts_and_trailing_data() {
    let encoded = encode(&fixture(), b"new\n").unwrap();
    for end in 0..encoded.len() {
        assert!(decode(&encoded[..end], b"new\n").is_err(), "end={end}");
    }
    assert!(decode(&encoded, b"old\n").is_err());
    let mut bad = encoded.clone();
    bad[4] = 99;
    assert!(decode(&bad, b"new\n").is_err());
    let mut bad = encoded;
    bad.push(0);
    assert!(decode(&bad, b"new\n").is_err());
    assert!(wire::get(&mut &[0x80, 0][..]).is_err());
    assert!(wire::get(&mut &[0xff; 10][..]).is_err());
    let mut delta = wire::Delta::default();
    let mut bytes = vec![];
    for n in [0, u64::MAX, 1 << 63, 0, 5] {
        delta.write(&mut bytes, n);
    }
    let mut delta = wire::Delta::default();
    let mut input = bytes.as_slice();
    for n in [0, u64::MAX, 1 << 63, 0, 5] {
        assert_eq!(delta.read(&mut input).unwrap(), n);
    }
}

#[test]
fn adaptive_selection_and_authenticated_file_only_roundtrip() {
    let mut change = large_change();
    let mut bytes = vec![];
    let hash = change.serialize(&mut bytes).unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        super::super::COMPACT_SEMANTIC_FORMAT_VERSION
    );
    let (restored, actual) = Change::deserialize(&mut bytes.as_slice()).unwrap();
    assert_eq!(actual, hash);
    assert_eq!(restored.hashed, change.hashed);
    assert_eq!(restored.contents, change.contents);
    let mut again = vec![];
    assert_eq!(restored.serialize(&mut again).unwrap(), hash);
    assert_eq!(again, bytes);
    change.semantic_encoding = SemanticEncoding::Raw;
    let mut raw = vec![];
    change.serialize(&mut raw).unwrap();
    assert!(bytes.len() < raw.len());
    let (historical, raw_hash) = Change::deserialize(&mut raw.as_slice()).unwrap();
    let mut again = vec![];
    assert_eq!(historical.serialize(&mut again).unwrap(), raw_hash);
    assert_eq!(again, raw);
    let tiny = vec![FileOps::edit(TrunkId::ROOT, "f".into())];
    assert!(!select(&tiny, b"", SemanticEncoding::Auto).unwrap().compact);
}

#[test]
fn compact_spans_retain_owner_context_and_flags() {
    let mut change = large_change();
    let owner = Hash::of(b"foreign semantic owner");
    change.hashed.dependencies.push(owner);
    change.add_file_ops(FileOps::edit(
        TrunkId::new(NodeId::new(37), 4),
        "foreign".into(),
    ));
    change.hashed.span = Some(SpanPatch::new(
        vec![],
        vec![SpanOp::Root {
            path: "generated.rs".into(),
            ordinal: 0,
            start: 0,
            end: change.contents.len() as u64,
        }],
    ));
    change.hashed.span.as_mut().unwrap().semantic_owners.push(
        crate::change::semantic_context::SemanticOwner {
            authored_id: 37,
            owner,
        },
    );
    let mut bytes = vec![];
    let hash = change.serialize(&mut bytes).unwrap();
    let (restored, actual) = Change::deserialize(&mut bytes.as_slice()).unwrap();
    assert_eq!(actual, hash);
    assert_eq!(restored.hashed, change.hashed);
    let header = super::super::FileHeader::from_bytes(bytes[..64].try_into().unwrap()).unwrap();
    assert!(header.has_span());
    let mut raw = change;
    raw.semantic_encoding = SemanticEncoding::Raw;
    let mut bytes = vec![];
    raw.serialize(&mut bytes).unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        super::super::SPAN_FORMAT_VERSION
    );
}

#[test]
fn signatures_survive_file_only_decode_and_reserialization() {
    let seed = [17; 32];
    let public = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();
    for policy in [
        SemanticEncoding::Auto,
        SemanticEncoding::Raw,
        SemanticEncoding::CompactMixed,
    ] {
        let mut change = large_change();
        change.semantic_encoding = policy;
        let hash = change
            .sign_with("did:atomic:codec-test", &seed, 123)
            .unwrap();
        let mut bytes = vec![];
        assert_eq!(change.serialize(&mut bytes).unwrap(), hash);
        let (decoded, actual) = Change::deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!(actual, hash);
        crate::change::signing::verify_change_signature(
            decoded.signature.as_ref().unwrap(),
            &public,
            &actual,
        )
        .unwrap();
        let mut again = vec![];
        assert_eq!(decoded.serialize(&mut again).unwrap(), hash);
        assert_eq!(again, bytes);
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Change::deserialize(&mut bytes.as_slice()).is_err());
    }
}

#[test]
fn rejects_bad_tags_flags_ranges_counts_and_allocation_amplification() {
    use super::wire::{Columns, MAGIC};
    let make = |control: Vec<u8>| Columns {
        split_ids: true,
        content_hash: *blake3::hash(b"").as_bytes(),
        metadata: postcard::to_allocvec(&(TrunkId::ROOT, "f", None::<TrunkOp>)).unwrap(),
        control,
        branches: vec![0, 0],
        kinds: postcard::to_allocvec(&TokenKind::Word).unwrap(),
        ..Default::default()
    };
    for c in [
        make(vec![1, 1, 0, 0, 0, 99]),
        make(vec![1, 1, 2]),
        make(vec![127]),
        make(vec![1, 1, 0, 0, 0, 0, 0, 1, 99]),
        make(vec![1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 2, 1]),
    ] {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&c).unwrap());
        assert!(decode(&bytes, b"").is_err());
    }
    let mut budget = 16;
    assert!(wire::charge(&mut budget, usize::MAX, 2).is_err());
    assert!(wire::charge(&mut budget, 17, 1).is_err());
    let mut c = make(vec![0]);
    c.positions.push(0);
    let mut bytes = MAGIC.to_vec();
    bytes.extend(postcard::to_allocvec(&c).unwrap());
    assert!(decode(&bytes, b"").is_err());
    let mut c = make(vec![0]);
    c.literals.push(1);
    let mut bytes = MAGIC.to_vec();
    bytes.extend(postcard::to_allocvec(&c).unwrap());
    assert!(decode(&bytes, b"").is_err());

    let mut c = make(vec![1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 2, 1]);
    c.references.push(0);
    let mut bytes = MAGIC.to_vec();
    bytes.extend(postcard::to_allocvec(&c).unwrap());
    assert!(decode(&bytes, b"")
        .unwrap_err()
        .to_string()
        .contains("range outside byte pool"));
    c.references[0] = 2;
    c.literals.extend(b"ab"); // decoded offset 1 skips the first pool byte
    let mut bytes = MAGIC.to_vec();
    bytes.extend(postcard::to_allocvec(&c).unwrap());
    assert!(decode(&bytes, b"")
        .unwrap_err()
        .to_string()
        .contains("leave gaps"));
}

#[test]
fn selective_semantic_reader_refuses_incomplete_compact_interpretation() {
    let mut bytes = vec![];
    large_change().serialize(&mut bytes).unwrap();
    let mut input = bytes.as_slice();
    let mut reader = super::super::ChangeReader::open(&mut input).unwrap();
    assert!(reader.semantic_sections().is_err());
}

#[test]
fn schema_four_rejects_section_overflow_and_oversized_framing_before_allocation() {
    let mut bytes = vec![];
    large_change().serialize(&mut bytes).unwrap();
    let mut overflow = bytes.clone();
    overflow[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Change::deserialize(&mut overflow.as_slice()).is_err());
    let hashes = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let mut at = 64 + 32 * hashes;
    while bytes[at] != 0x30 {
        assert_ne!(bytes[at], 0x20, "semantics precede content");
        let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
        at += 5 + len;
    }
    bytes[at + 1..at + 5].copy_from_slice(&u32::MAX.to_le_bytes());
    let error = Change::deserialize(&mut bytes.as_slice())
        .unwrap_err()
        .to_string();
    assert!(error.contains("exceeds limit"), "{error}");
}

#[test]
fn legacy_unknown_flag_bits_keep_their_historical_meaning() {
    let mut header = super::super::FileHeader::default();
    header.flags.set(super::super::FileHeaderFlags::HAS_SPAN);
    header.validate().unwrap();
    assert!(!header.has_span());
    assert_eq!(header.total_section_count(), 2);
}

#[test]
fn schema_four_writer_rejects_oversized_sections_before_writing_them() {
    use super::super::{ChangeWriter, FileHeader, HashDedupTable, SectionType, WriterOptions};
    let mut bytes = vec![];
    let mut writer = ChangeWriter::new(&mut bytes, WriterOptions::default());
    let mut header = FileHeader::builder()
        .hash_table_entries(1)
        .semantic_section_count(1)
        .build();
    header.version = super::super::COMPACT_SEMANTIC_FORMAT_VERSION;
    writer.write_file_header(&header).unwrap();
    writer
        .write_hash_table(&HashDedupTable::new([0; 32]))
        .unwrap();
    writer
        .write_change_header(&ChangeHeader::new("limits"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    let before = writer.stats().total_bytes_written;
    assert!(matches!(
        writer.write_compressed_section(SectionType::Semantic, &[1], wire::MAX_PAYLOAD as u64 + 1),
        Err(FormatError::SemanticLimitExceeded)
    ));
    assert_eq!(writer.stats().total_bytes_written, before);
}

#[test]
fn measure_separate_id_stream_optimization() {
    let change = large_change();
    let mut sizes = vec![];
    for split in [false, true] {
        let bytes = encode::encode_layout(change.file_ops(), &change.contents, split).unwrap();
        assert_eq!(decode(&bytes, &change.contents).unwrap(), change.file_ops());
        sizes.push(zstd::encode_all(bytes.as_slice(), 3).unwrap().len());
    }
    println!(
        "semantic ID layout: mixed={} split={} compressed bytes",
        sizes[0], sizes[1]
    );
    // Sizing is evidence, not a correctness assumption for arbitrary workloads.
}
