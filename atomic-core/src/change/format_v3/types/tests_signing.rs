//! Roundtrip checks for the ChangeSignature section.
#[cfg(test)]
mod tests {
    use super::super::section::ChangeSignature;

    #[test]
    fn change_signature_postcard_roundtrip() {
        let sig = ChangeSignature::new("did:atomic:test", [7u8; 64], [9u8; 32], 42);
        let bytes = postcard::to_allocvec(&sig).expect("serialize");
        let decoded: ChangeSignature = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(sig, decoded);
    }
}

#[cfg(test)]
mod full_change_roundtrip {
    use crate::change::format_v3::{ChangeReader, SectionType};
    use crate::change::{signing, Author, Change, ChangeHeader};
    use crate::types::Hash;

    #[test]
    fn signed_change_serializes_and_deserializes() {
        let header = ChangeHeader::builder()
            .message("test")
            .author(Author::new("T", Some("t@t.dev")))
            .build();
        let mut change = Change::new(header, Vec::new(), b"hello world\n".to_vec(), Vec::new());
        let hash = change
            .sign_with("did:atomic:test", &[3u8; 32], 12345)
            .expect("sign");
        assert!(change.signature.is_some());

        let mut bytes = Vec::new();
        let re_hash = change.serialize(&mut bytes).expect("serialize");
        assert_eq!(re_hash, hash, "signature must not change hash");

        // The SIGNATURE section survives a section-level round-trip.
        let mut saw_signature = false;
        {
            let mut cursor = std::io::Cursor::new(&bytes);
            let mut reader = ChangeReader::open(&mut cursor).expect("open");
            while let Some(section) = reader.next_section().expect("section") {
                if section.section_type == SectionType::Signature {
                    let decoded_sig: crate::change::format_v3::ChangeSignature =
                        postcard::from_bytes(&section.payload).expect("standalone decode");
                    assert_eq!(decoded_sig.timestamp, 12345);
                    saw_signature = true;
                }
            }
        }
        assert!(saw_signature, "SIGNATURE section present in file");
        let (decoded, decoded_hash) =
            Change::deserialize(&mut std::io::Cursor::new(&bytes)).expect("deserialize");
        assert_eq!(decoded_hash, hash);
        assert!(decoded.signature.is_some());
        assert_eq!(decoded.signature, change.signature);
        let _ = Hash::of(b"");
        let _ = signing::CHANGE_SIGNATURE_DOMAIN;
    }
}
