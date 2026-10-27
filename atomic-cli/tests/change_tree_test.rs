use atomic_core::{
    change::{Change, ChangeHeader, EdgeUpdate, GraphOp, Insertion},
    types::{Base32, ChangePosition, Hash, Position},
    EdgeFlags,
};
use atomic_repository::Repository;
use std::{path::Path, process::Command};

fn insertion() -> Insertion<Option<Hash>> {
    Insertion {
        predecessors: vec![],
        successors: vec![],
        flag: EdgeFlags::FOLDER,
        start: ChangePosition::new(0),
        end: ChangePosition::new(0),
        inode: Position::new(None, ChangePosition::new(0)),
    }
}

fn run(root: &Path, hash: &str, flags: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(root)
        .args(["--no-color", "change", hash])
        .args(flags)
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn change_tree_expands_paths_without_emoji_and_preserves_other_formats() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let repo = Repository::init(root).unwrap();
    let mut bytes = Vec::new();
    let dependency = Change::empty(ChangeHeader::new("Initial setup"))
        .serialize(&mut bytes)
        .unwrap();
    repo.import_change_bytes(&dependency, &bytes).unwrap();
    let change = Change::new(
        ChangeHeader::new("Build a CLI"),
        vec![
            GraphOp::DirAdd {
                path: "src".into(),
                add_name: insertion(),
                add_inode: insertion(),
            },
            GraphOp::FileAdd {
                path: "src/hello.ts".into(),
                add_name: insertion(),
                add_inode: insertion(),
                contents: None,
                encoding: None,
            },
            GraphOp::FileDel {
                path: "legacy.ts".into(),
                del: EdgeUpdate {
                    edges: vec![],
                    inode: Position::new(None, ChangePosition::new(0)),
                },
                contents: None,
                encoding: None,
            },
        ],
        vec![],
        vec![dependency],
    );
    bytes.clear();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    drop(repo);
    let database = root.join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let id = hash.to_base32();
    let text = run(root, &id, &[]);
    assert!(text.starts_with(&format!("change {}\n", &id[..12])));
    assert!(text.contains("├── Metadata\n"));
    assert!(text.contains("│   └── Build a CLI\n"));
    assert!(text.contains("└── Files changed: 3\n    ├── legacy.ts [deleted]\n    └── src/ [added]\n        └── hello.ts [added]\n"));
    assert!(!text.contains('📁'));
    assert!(!text.contains("vertices: name"));
    assert!(!text.contains("No provenance"));

    let expanded = run(root, &id, &["--show-hunks", "--show-deps", "--full-hash"]);
    assert!(expanded.starts_with(&format!("change {id}\n")));
    assert!(expanded.contains(&format!("{} — Initial setup", dependency.to_base32())));
    assert!(expanded.contains("        ├── +2 vertices: name, inode\n"));
    assert!(expanded.contains("            └── +2 vertices: name, inode\n"));
    assert_eq!(expanded.matches("hello.ts [added]").count(), 1);

    let short = run(root, &id, &["-f", "short"]);
    assert_eq!(short.lines().count(), 1);
    assert!(short.contains("Build a CLI"));
    assert!(!short.contains("──"));
    let json: serde_json::Value = serde_json::from_str(&run(root, &id, &["-f", "json"])).unwrap();
    assert_eq!(json["hash"], id);
    assert_eq!(json["message"], "Build a CLI");
    assert_eq!(json["hunks"].as_array().unwrap().len(), 3);
    assert_eq!(
        std::fs::read(database).unwrap(),
        before,
        "change display must be read-only"
    );
}
