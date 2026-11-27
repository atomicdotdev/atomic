use super::*;
use crate::{
    change::{FileOps, LineOps},
    crdt::{BranchId, LeafOp, TrunkId},
    diff::token::TokenKind,
};

#[test]
fn direct_reader_preserves_all_fields_and_variants() {
    let ops = super::super::tests::fixture();
    for content in [b"new\n".as_slice(), b"", b"\xff\0new\n"] {
        let encoded = encode(&ops, content).unwrap();
        let decoded = super::super::decode(&encoded, content).unwrap();
        assert_eq!(decoded, ops);
        assert_eq!(
            postcard::to_allocvec(&decoded).unwrap(),
            postcard::to_allocvec(&ops).unwrap()
        );
        assert_eq!(encode(&decoded, content).unwrap(), encoded);
    }
}

fn unusual() -> (Vec<FileOps>, Vec<u8>) {
    let content = b"foobar  \xc3\xa9".to_vec();
    let mut file = FileOps::edit(TrunkId::ROOT, "f".into());
    let mut tokens = [b"fo".as_slice(), b"obar", b"  ", b"", b"\xc3\xa9"]
        .into_iter()
        .map(|s| LeafOp::Insert {
            after: None,
            kind: TokenKind::Comment,
            content: s.to_vec(),
        })
        .collect::<Vec<_>>();
    for _ in 0..5 {
        tokens.push(LeafOp::Insert {
            after: None,
            kind: TokenKind::Comment,
            content: b"x".to_vec(),
        });
    }
    file.add_line_op(LineOps::insert(BranchId::ROOT, None, tokens));
    (vec![file], content)
}
#[test]
fn authored_boundaries_kinds_unicode_empty_and_long_tokens_survive() {
    let (ops, content) = unusual();
    let encoded = encode(&ops, &content).unwrap();
    assert_eq!(super::super::decode(&encoded, &content).unwrap(), ops);
    let bytes = vec![b'a'; 5000];
    let mut file = FileOps::edit(TrunkId::ROOT, "long".into());
    file.add_line_op(LineOps::insert(
        BranchId::ROOT,
        None,
        vec![LeafOp::Insert {
            after: None,
            kind: TokenKind::Other,
            content: bytes.clone(),
        }],
    ));
    let encoded = encode(&[file.clone()], &bytes).unwrap();
    assert_eq!(super::super::decode(&encoded, &bytes).unwrap(), vec![file]);
}
#[test]
fn malformed_versions_masks_and_columns_are_rejected() {
    let (ops, content) = unusual();
    let encoded = encode(&ops, &content).unwrap();
    for n in 0..encoded.len() {
        assert!(
            super::super::decode(&encoded[..n], &content).is_err(),
            "n={n}"
        );
    }
    let mut extra = encoded.clone();
    extra.push(0);
    assert!(super::super::decode(&extra, &content).is_err());
    let mut predictor = encoded.clone();
    predictor[5] = 2;
    assert!(super::super::decode(&predictor, &content).is_err());
    assert!(super::super::decode(&encoded, b"wrong bytes").is_err());
    assert!(Mask::new(1, &[255], &[]).is_err());
    assert!(Mask::new(u32::MAX, &[], &[]).is_err());
    let mut mask = Mask::new(1, &[1], &[]).unwrap();
    assert!(mask.actual().is_err());
}
#[test]
fn predictor_one_has_frozen_ascii_rules() {
    for (input, n) in [
        (b"hello world".as_slice(), 5),
        (b"12.5;", 4),
        (b"// comment\nnext", 10),
        (b"\r\nx", 2),
        (b"<<=x", 3),
        (b"\"a\\\"b\"x", 6),
        (b"'open\n", 5),
        (b"\xc3\xa9", 1),
        (b"", 0),
    ] {
        assert_eq!(predictor_v1::length(input), n, "{input:?}");
    }
    assert_eq!(predictor_v1::length(&vec![b'a'; 5000]), 4096);
    assert_eq!(predictor_v1::kind("é".as_bytes()), TokenKind::Other);
}

#[test]
fn frozen_predictive_wire_vector() {
    let (ops, content) = unusual();
    let bytes = encode(&ops, &content).unwrap();
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        "786b929b60781380609b9297d44701f8b920508bae06997b38e0feb102b75330"
    );
    assert_eq!(super::super::decode(&bytes, &content).unwrap(), ops);
}
