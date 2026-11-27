//! Production record/import and redb-free server reads of compact semantics.
use atomic_core::{
    change::{
        format_v3::{
            graph_fields::GraphEncoding, semantic::SemanticEncoding, FileHeader,
            FIELD_CODEC_FORMAT_VERSION,
        },
        Change, ChangeHeader,
    },
    pristine::{
        CapabilityMutTxnT, CapabilityTxnT, MutTxnT, CHANGE_FORMAT_VNEXT_CAPABILITY,
        SPAN_RANGE_CAPABILITY,
    },
};
use atomic_repository::{RecordOptions, Repository};

#[test]
fn record_push_bytes_import_and_file_only_queries_preserve_semantics() {
    for span in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        if span {
            let mut txn = repo.pristine().write_txn().unwrap();
            txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
            txn.commit().unwrap();
        }
        let text = (0..2048)
            .map(|i| format!("let variable_{i} = calculate({i}, input);\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("code.rs"), &text).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.add(wc, "code.rs", Default::default()).unwrap();
        let hash = *repo
            .record(
                wc,
                ChangeHeader::new("record compact semantics"),
                RecordOptions::new()
                    .paths(vec!["code.rs"])
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap()
            .hash();
        let original = repo.load_change(&hash).unwrap();
        let bytes = std::fs::read(repo.change_store().change_path(&hash)).unwrap();
        let header = FileHeader::from_bytes(bytes[..64].try_into().unwrap()).unwrap();
        assert!(header.has_compact_semantics());
        if !span {
            assert_eq!(header.version, FIELD_CODEC_FORMAT_VERSION);
            assert!(header.has_graph_fields());
        }
        assert_eq!(
            repo.pristine()
                .read_txn()
                .unwrap()
                .required_capability_version(CHANGE_FORMAT_VNEXT_CAPABILITY.id())
                .unwrap(),
            Some(CHANGE_FORMAT_VNEXT_CAPABILITY.minimum_version())
        );
        let (decoded, actual) = Change::deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!(actual, hash);
        assert_eq!(decoded.hashed, original.hashed);
        assert_eq!(decoded.contents, original.contents);
        let mut raw = decoded.clone();
        raw.signature = None;
        raw.semantic_encoding = SemanticEncoding::Raw;
        raw.graph_encoding = GraphEncoding::Raw;
        let mut raw_bytes = vec![];
        raw.serialize(&mut raw_bytes).unwrap();
        assert!(bytes.len() < raw_bytes.len());
        let mut input = bytes.as_slice();
        let mut reader = atomic_core::change::format_v3::ChangeReader::open(&mut input).unwrap();
        let mut semantic = 0;
        let mut graph = 0;
        let mut content = 0;
        while let Some(section) = reader.next_section().unwrap() {
            use atomic_core::change::format_v3::SectionType;
            match section.section_type {
                SectionType::Semantic => semantic += section.compressed_size,
                SectionType::Graph => graph += section.compressed_size,
                SectionType::Content => content += section.compressed_size,
                _ => {}
            }
        }
        reader.verify().unwrap();
        println!("native record span={span}: object={} raw_object={} semantic={semantic} graph={graph} content={content}",bytes.len(),raw_bytes.len());
        drop(repo);
        drop(dir);
        let server = tempfile::tempdir().unwrap();
        let pushed = server.path().join("pushed.change");
        std::fs::write(&pushed, &bytes).unwrap();
        let mut file = std::fs::File::open(&pushed).unwrap();
        let (queried, actual) = Change::deserialize(&mut file).unwrap();
        assert_eq!(actual, hash);
        assert_eq!(queried.file_ops(), original.file_ops());
        assert!(queried.file_ops().iter().any(|f| f.path() == "code.rs"));
        assert!(!server.path().join(".atomic").exists());
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver = Repository::init(receiver_dir.path()).unwrap();
        assert_eq!(receiver.import_change_bytes(&hash, &bytes).unwrap(), hash);
        assert_eq!(
            std::fs::read(receiver.change_store().change_path(&hash)).unwrap(),
            bytes
        );
        assert_eq!(receiver.load_change(&hash).unwrap().hashed, original.hashed);
        // Explicit new semantic version also works when whole-object selection
        // keeps the older codec for this highly regular fixture.
        let mut predictive = queried.clone();
        predictive.semantic_encoding = SemanticEncoding::PredictiveV2;
        let mut packed = vec![];
        let packed_hash = predictive.serialize(&mut packed).unwrap();
        let packed_header = FileHeader::from_bytes(packed[..64].try_into().unwrap()).unwrap();
        assert!(packed_header.has_predictive_semantics());
        assert_eq!(
            receiver.import_change_bytes(&packed_hash, &packed).unwrap(),
            packed_hash
        );
        drop(receiver);
        drop(receiver_dir);
        let (read, actual) = Change::deserialize(&mut packed.as_slice()).unwrap();
        assert_eq!(actual, packed_hash);
        assert_eq!(read.hashed, original.hashed);
    }
}
