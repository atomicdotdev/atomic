use super::*;
use crate::{
    change::{Atom, Change, ChangeHeader, GraphOp, Insertion, Local},
    ChangePosition, EdgeFlags, Position,
};
use graph_fields::GraphEncoding;
use semantic::SemanticEncoding;

fn change() -> Change {
    let mut c = super::semantic::tests::large_change();
    c.semantic_encoding = SemanticEncoding::PredictiveV2;
    let mut start = 0;
    for (i, line) in c.contents.split_inclusive(|b| *b == b'\n').enumerate() {
        let end = start + line.len();
        c.hashed.hunks.push(GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: vec![Position::new(None, ChangePosition::new(start as u64))],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(start as u64),
                end: ChangePosition::new(end as u64),
                inode: Position::new(None, ChangePosition::new(0)),
            }),
            local: Local::new("generated.rs", i as u64 + 1),
            encoding: Some(crate::change::Encoding::Utf8),
        });
        start = end;
    }
    c
}
#[test]
fn signed_objects_roundtrip_all_codec_combinations() {
    let key = [23; 32];
    let public = ed25519_dalek::SigningKey::from_bytes(&key)
        .verifying_key()
        .to_bytes();
    for graph in [GraphEncoding::Raw, GraphEncoding::FieldsV1] {
        for semantics in [
            SemanticEncoding::Raw,
            SemanticEncoding::Compact,
            SemanticEncoding::CompactMixed,
            SemanticEncoding::PredictiveV2,
        ] {
            let mut c = change();
            c.graph_encoding = graph;
            c.semantic_encoding = semantics;
            let hash = c.sign_with("did:atomic:codec-v5", &key, 123).unwrap();
            let mut bytes = vec![];
            assert_eq!(c.serialize(&mut bytes).unwrap(), hash);
            let (decoded, actual) = Change::deserialize(&mut bytes.as_slice()).unwrap();
            assert_eq!(hash, actual);
            assert_eq!(decoded.hashed, c.hashed);
            assert_eq!(decoded.contents, c.contents);
            let mut again = vec![];
            assert_eq!(decoded.serialize(&mut again).unwrap(), hash);
            assert_eq!(again, bytes);
            crate::change::signing::verify_change_signature(
                decoded.signature.as_ref().unwrap(),
                &public,
                &actual,
            )
            .unwrap();
        }
    }
}
#[test]
fn interpretation_flags_are_authenticated_not_header_hints() {
    let c = change();
    let mut bytes = vec![];
    c.serialize(&mut bytes).unwrap();
    let header = FileHeader::from_bytes(bytes[..64].try_into().unwrap()).unwrap();
    assert!(header.has_graph_fields());
    assert!(header.has_predictive_semantics());
    for bit in [
        FileHeaderFlags::GRAPH_FIELDS,
        FileHeaderFlags::PREDICTIVE_SEMANTICS,
    ] {
        let mut tampered = bytes.clone();
        tampered[8..12].copy_from_slice(&(header.flags.to_raw() ^ bit).to_le_bytes());
        let error = Change::deserialize(&mut tampered.as_slice())
            .unwrap_err()
            .to_string();
        assert!(error.contains("CODECS"), "{error}");
    }
    let mut at = 64 + header.hash_table_entries as usize * 32;
    while bytes[at] != SectionType::Codecs.to_byte() {
        let n = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
        at += 5 + n;
    }
    let n = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
    let mut descriptor = zstd::decode_all(&bytes[at + 5..at + 5 + n]).unwrap();
    descriptor[2] = 1;
    let replacement = zstd::encode_all(descriptor.as_slice(), 3).unwrap();
    assert_eq!(replacement.len(), n);
    bytes[at + 5..at + 5 + n].copy_from_slice(&replacement);
    let mut input = bytes.as_slice();
    let mut reader = ChangeReader::open(&mut input).unwrap();
    reader.read_all_sections().unwrap();
    assert!(
        reader.verify().is_err(),
        "CODECS must contribute to change identity"
    );
}
#[test]
fn historical_encodings_stay_stable_and_complete_cost_fallback_holds() {
    for n in [0, 1, 2, 8, 32, 256] {
        let mut c = change();
        c.hashed.hunks.truncate(n);
        c.hashed.file_ops[0].line_ops_mut().truncate(n);
        c.signature = None;
        c.semantic_encoding = SemanticEncoding::Raw;
        c.graph_encoding = GraphEncoding::Raw;
        let mut raw = vec![];
        c.serialize(&mut raw).unwrap();
        c.semantic_encoding = SemanticEncoding::Compact;
        let mut v1 = vec![];
        c.serialize(&mut v1).unwrap();
        let (old, hash) = Change::deserialize(&mut v1.as_slice()).unwrap();
        let mut repeated = vec![];
        assert_eq!(old.serialize(&mut repeated).unwrap(), hash);
        assert_eq!(v1, repeated);
        c.semantic_encoding = SemanticEncoding::Auto;
        c.graph_encoding = GraphEncoding::Auto;
        let mut selected = vec![];
        c.serialize(&mut selected).unwrap();
        assert!(
            selected.len() <= raw.len().min(v1.len()),
            "manifest overhead must be counted"
        );
    }
    let c = Change::empty(ChangeHeader::new("empty"));
    let mut bytes = vec![];
    c.serialize(&mut bytes).unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        FORMAT_VERSION
    );
}
#[test]
fn manifest_writer_rejects_missing_duplicate_and_invalid_codecs() {
    let mut header = FileHeader::builder()
        .hash_table_entries(1)
        .graph_section_count(1)
        .build();
    let descriptor = codecs::CodecDescriptor {
        graph_fields: true,
        semantic: 0,
        span: false,
    };
    descriptor.apply(&mut header).unwrap();
    let mut bytes = vec![];
    let mut writer = ChangeWriter::new(&mut bytes, WriterOptions::default());
    writer.write_file_header(&header).unwrap();
    writer
        .write_hash_table(&HashDedupTable::new([0; 32]))
        .unwrap();
    writer
        .write_change_header(&ChangeHeader::new("manifest"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    assert!(writer.write_graph_section(&[]).is_err());
    writer.write_codec_descriptor(descriptor).unwrap();
    assert!(writer.write_codec_descriptor(descriptor).is_err());
    for data in [
        &[5, 0, 0, 0][..],
        &[5, 2, 1, 0],
        &[5, 1, 3, 0],
        &[5, 1, 1, 2],
        &[6, 1, 1, 0],
        &[5, 1],
    ] {
        assert!(codecs::CodecDescriptor::from_bytes(data).is_err());
    }
}
