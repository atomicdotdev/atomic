//! Temporary debug harness: dumps the alive BLOCK vertices of a path sorted
//! by position, with spans, to expose phantom/empty vertices.
#![cfg(unix)]

use atomic_core::pristine::InodeGraphOps;
use atomic_core::types::EdgeFlags;
use atomic_core::TreeTxnT;

#[test]
fn dump_vertex_count() {
    let root = std::path::Path::new(
        "/private/var/folders/1s/0dxgtx3x7g918slqc05x8c5r0000gn/T/opencode/atomic-victory",
    );
    let path =
        std::env::var("ATOMIC_DUMP_PATH").unwrap_or_else(|_| "atomic-cli/Cargo.toml".to_string());
    let repo = atomic_repository::Repository::open(root).unwrap();
    let rendered = repo.get_file_content(&path).unwrap();
    println!("render len: {:?}", rendered.as_ref().map(|b| b.len()));
    println!(
        "render head: {:?}",
        rendered
            .as_ref()
            .map(|b| String::from_utf8_lossy(&b[..b.len().min(40)]).to_string())
    );

    let txn = repo.pristine().read_txn().unwrap();
    let inode = txn.get_inode(&path).unwrap().expect("tracked");
    let pos = txn.inode_position(inode).unwrap().expect("position");
    println!("inode pos: {pos:?}");

    let edges: Vec<_> = txn
        .iter_inode_edges(inode, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    println!("graph alive BLOCK edges: {}", edges.len());

    let mut rows: Vec<(u64, u64)> = Vec::new();
    for edge in &edges {
        rows.push((edge.dest().pos.get(), edge.dest().change.0.into()));
    }
    rows.sort();
    for (index, (pos, change)) in rows.iter().enumerate() {
        let next = rows.get(index + 1).map(|(p, _)| *p).unwrap_or(*pos);
        println!(
            "  v{index:03} pos={pos} change={change} span_to_next={}",
            next.saturating_sub(*pos)
        );
    }
}
