use std::path::PathBuf;

fn collect_protos(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("proto dir readable")
        .map(|e| e.expect("proto entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_protos(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("proto") {
            out.push(path);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest.join("proto");
    let mut files = Vec::new();
    collect_protos(&proto_root, &mut files);
    assert!(!files.is_empty(), "no protos found under {}", proto_root.display());

    // protox is a pure-Rust protoc replacement: no system protoc dependency.
    // The include root carries `atomic/...`, matching the imports
    // (`atomic/common/common.proto`).
    let fds = protox::compile(
        &files,
        [proto_root.clone()],
    )?;

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(fds)?;
    Ok(())
}
