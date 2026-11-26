use super::*;
use atomic_core::{
    change::{
        span_patch::{ExternalFile, SpanProjectionId},
        Encoding, InodeKind,
    },
    types::Hash,
};

fn insert(after: Option<LeafId>, bytes: &[u8]) -> LeafOp {
    LeafOp::Insert {
        after,
        kind: TokenKind::Word,
        content: bytes.into(),
    }
}

fn fixtures() -> Vec<FileOps> {
    let trunk = TrunkId::new(NodeId::new(u64::MAX), u32::MAX);
    let b = |owner, index| BranchId::new(NodeId::new(owner), index);
    let l = |owner, index| LeafId::new(NodeId::new(owner), index);
    let tokens = vec![
        insert(None, b""),
        insert(Some(LeafId::ROOT), b"\xff\0"),
        insert(Some(l(u64::MAX, u32::MAX)), "é\r\n".as_bytes()),
        LeafOp::Delete { leaf: l(71, 5) },
        LeafOp::Replace {
            leaf: l(3, 0),
            new_content: b"absent old text".to_vec(),
        },
        LeafOp::Restore { leaf: l(0, 99) },
    ];
    let branches = vec![
        BranchOp::Insert {
            after: None,
            content: tokens.clone(),
        },
        BranchOp::Insert {
            after: Some(BranchId::ROOT),
            content: vec![],
        },
        BranchOp::Insert {
            after: Some(b(u64::MAX, 17)),
            content: vec![insert(None, b"new\n")],
        },
        BranchOp::Delete {
            branch: b(7, 8),
            content: tokens.clone(),
        },
        BranchOp::Modify {
            branch: b(9, 10),
            old_content: tokens,
            new_content: vec![insert(None, b"new\n")],
        },
        BranchOp::Restore { branch: b(4, 0) },
        BranchOp::Reparent {
            branch: b(6, u32::MAX),
            new_after: Some(b(8, 5)),
        },
        BranchOp::Reparent {
            branch: b(6, 3),
            new_after: None,
        },
    ];
    let variants = vec![
        None,
        Some(TrunkOp::Create {
            path: "α\0".into(),
            encoding: Some(Encoding::Utf8),
        }),
        Some(TrunkOp::Create {
            path: "binary".into(),
            encoding: None,
        }),
        Some(TrunkOp::Delete { trunk }),
        Some(TrunkOp::Move {
            trunk,
            new_path: "elsewhere".into(),
        }),
        Some(TrunkOp::Undelete { trunk }),
        Some(TrunkOp::SetMode { trunk, mode: 0o755 }),
        Some(TrunkOp::SetKind {
            trunk,
            kind: InodeKind::Regular,
        }),
        Some(TrunkOp::ProjectedSpanEdit {
            file: ExternalFile {
                owner: Hash::of(b"owner"),
                ordinal: 7,
            },
            baseline: SpanProjectionId(Hash::of(b"projection")),
        }),
    ];
    variants
        .into_iter()
        .map(|op| {
            let mut file = FileOps::new(trunk, "path".into(), op);
            for (i, branch) in branches.iter().enumerate() {
                let mut line = LineOps::new_with_line_nums(
                    b(i as u64, i as u32),
                    branch.clone(),
                    (i % 2 == 0).then_some(usize::MAX - i),
                    (i % 3 == 0).then_some(i),
                );
                if i % 2 == 0 {
                    // Preserve metadata even when it is not usable as a content hint.
                    line.set_content_range(ChangePosition::new(u64::MAX), ChangePosition::new(0));
                }
                file.add_line_op(line);
            }
            file
        })
        .collect()
}

#[test]
fn exact_all_variants_foreign_ids_and_metadata_without_database() -> Result<()> {
    let ops = fixtures();
    for content in [&b"new\n"[..], &b""[..], &b"\xff\0new\n"[..]] {
        let bytes = encode(&ops, content, 3)?;
        let restored = decode(&bytes, content)?;
        assert_eq!(restored, ops);
        assert_eq!(
            postcard::to_allocvec(&restored)?,
            postcard::to_allocvec(&ops)?
        );
        assert_eq!(
            bytes,
            encode(&ops, content, 3)?,
            "encoding must be deterministic"
        );
    }
    assert_eq!(decode(&encode(&[], b"", 3)?, b"")?, vec![]);
    Ok(())
}

#[test]
fn exact_content_references_and_literal_fallbacks() -> Result<()> {
    let mut file = FileOps::edit(TrunkId::ROOT, "f".into());
    for (i, text) in [b"a\n".as_slice(), b"b\n", b"a\n", b"not in content"]
        .iter()
        .enumerate()
    {
        file.add_line_op(
            LineOps::insert(
                BranchId::new(NodeId::ROOT, i as u32),
                None,
                vec![insert(None, text)],
            )
            .with_content_range(ChangePosition::new(0), ChangePosition::new(2)),
        );
    }
    let ops = vec![file];
    let encoded = encode(&ops, b"a\nb\n", 3)?;
    assert_eq!(decode(&encoded, b"a\nb\n")?, ops);
    assert!(decode(&encoded, b"a\nc\n").is_err());
    Ok(())
}

#[test]
fn rejects_truncation_wrong_version_trailing_and_bad_columns() -> Result<()> {
    let encoded = encode(&fixtures(), b"new\n", 3)?;
    for end in 0..encoded.len() {
        assert!(decode(&encoded[..end], b"new\n").is_err(), "end={end}");
    }
    let mut bad = encoded.clone();
    bad[4] = 99;
    assert!(decode(&bad, b"new\n").is_err());
    let raw = zstd::decode_all(&encoded[MAGIC.len()..])?;
    let mut c: Columns = postcard::from_bytes(&raw)?;
    c.control.push(0);
    let mut bad = MAGIC.to_vec();
    bad.extend(zstd::encode_all(postcard::to_allocvec(&c)?.as_slice(), 3)?);
    assert!(decode(&bad, b"new\n").is_err());
    let mut raw_trailing = raw.clone();
    raw_trailing.push(1);
    let mut bad = MAGIC.to_vec();
    bad.extend(zstd::encode_all(raw_trailing.as_slice(), 3)?);
    assert!(decode(&bad, b"new\n").is_err());
    Ok(())
}

#[test]
fn varints_and_deltas_cover_full_domain() -> Result<()> {
    let values = [0, 1, 127, 128, u32::MAX as u64, u64::MAX, 0, 1 << 63, 5];
    let mut bytes = Vec::new();
    let mut previous = 0;
    for value in values {
        delta(&mut bytes, &mut previous, value);
    }
    let mut input = bytes.as_slice();
    previous = 0;
    for value in values {
        assert_eq!(undelta(&mut input, &mut previous)?, value);
    }
    assert!(input.is_empty());
    assert!(get(&mut &[0x80, 0][..]).is_err());
    assert!(get(&mut &[0xff; 10][..]).is_err());
    Ok(())
}

#[test]
fn rejects_unknown_operations_flags_counts_and_out_of_bounds_references() -> Result<()> {
    let make = |control: Vec<u8>| Columns {
        content_hash: *blake3::hash(b"").as_bytes(),
        files: vec![(TrunkId::ROOT, "f".into(), None)],
        control,
        owners: vec![0],
        indices: vec![0],
        kinds: postcard::to_allocvec(&TokenKind::Word).unwrap(),
        ..Columns::default()
    };
    for c in [
        make(vec![1, 0, 0, 0, 99]),                  // unknown branch tag
        make(vec![1, 2]),                            // invalid optional line-number flag
        make(vec![127]),                             // line count with no corresponding data
        make(vec![1, 0, 0, 0, 0, 0, 1, 99]),         // unknown leaf tag
        make(vec![1, 0, 0, 0, 0, 0, 1, 0, 0, 2, 1]), // nonempty reference into empty literal pool
    ] {
        let mut encoded = MAGIC.to_vec();
        encoded.extend(zstd::encode_all(postcard::to_allocvec(&c)?.as_slice(), 3)?);
        assert!(decode(&encoded, b"").is_err());
    }
    Ok(())
}

#[test]
fn generated_irregular_semantics_roundtrip() -> Result<()> {
    // Deterministic generated coverage, including high/descending IDs and arbitrary bytes.
    let mut seed = 0x7e57u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..64 {
        let mut file = FileOps::edit(TrunkId::new(NodeId::new(next()), next() as u32), "f".into());
        for _ in 0..16 {
            let leaf = LeafId::new(NodeId::new(next()), next() as u32);
            let content = (0..next() % 50).map(|_| next() as u8).collect::<Vec<_>>();
            let op = match next() % 4 {
                0 => insert(Some(leaf), &content),
                1 => LeafOp::Delete { leaf },
                2 => LeafOp::Replace {
                    leaf,
                    new_content: content,
                },
                _ => LeafOp::Restore { leaf },
            };
            file.add_line_op(
                LineOps::insert(
                    BranchId::new(NodeId::new(next()), next() as u32),
                    None,
                    vec![op],
                )
                .with_old_line_num(next() as usize)
                .with_new_line_num(next() as usize),
            );
        }
        let ops = vec![file];
        assert_eq!(decode(&encode(&ops, b"", 3)?, b"")?, ops);
    }
    Ok(())
}

#[test]
fn measure_synthetic_add_and_edit_without_database() -> Result<()> {
    use atomic_core::diff::token::Tokenizer;
    let mut content = Vec::new();
    let mut file = FileOps::create(TrunkId::ROOT, "generated.rs".into(), Some(Encoding::Utf8));
    let mut leaf_index = 0;
    for i in 0..4096u32 {
        let text = format!("let variable_{i} = compute({i}, input);\n");
        let start = content.len();
        content.extend_from_slice(text.as_bytes());
        let mut previous = None;
        let tokens = Tokenizer::new(text.as_bytes())
            .map(|token| {
                let op = LeafOp::Insert {
                    after: previous,
                    kind: token.kind(),
                    content: token.content().into(),
                };
                previous = Some(LeafId::new(NodeId::ROOT, leaf_index));
                leaf_index += 1;
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
    let old = vec![insert(
        Some(LeafId::new(NodeId::new(19), 900)),
        b"old value not in new content",
    )];
    let mut edit = FileOps::edit(TrunkId::new(NodeId::new(19), 0), "generated.rs".into());
    edit.add_line_op(
        LineOps::modify_at(
            BranchId::new(NodeId::new(19), 64),
            old,
            vec![insert(None, b"new value\n")],
            65,
            65,
        )
        .with_content_range(ChangePosition::new(0), ChangePosition::new(10)),
    );
    for (label, ops, content) in [
        ("4096-line-add", vec![file], content),
        (
            "interior-edit-with-old-snapshot",
            vec![edit],
            b"new value\n".to_vec(),
        ),
    ] {
        let raw = postcard::to_allocvec(&ops)?;
        let baseline = zstd::encode_all(raw.as_slice(), 3)?.len();
        let encoded = encode(&ops, &content, 3)?;
        let no_references = encode(&ops, b"", 3)?.len();
        assert_eq!(decode(&encoded, &content)?, ops);
        assert_eq!(postcard::to_allocvec(&decode(&encoded, &content)?)?, raw);
        println!("{label}: content_zstd3={} baseline_semantic_zstd3={baseline} compact_semantic_zstd3={} compact_without_content_refs={no_references}",
            zstd::encode_all(content.as_slice(), 3)?.len(), encoded.len());
    }
    Ok(())
}
