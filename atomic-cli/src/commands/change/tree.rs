//! Presentation-only hierarchy; ledger entries remain in recorded order rather
//! than inventing a causal tree from a DAG. Paths come from the change, not disk.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

struct Tree {
    label: String,
    children: Vec<Tree>,
}

impl Tree {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            children: Vec::new(),
        }
    }

    fn push(&mut self, label: impl Into<String>) {
        self.children.push(Self::new(label));
    }

    fn section(&mut self, section: Self) {
        if !section.children.is_empty() {
            self.children.push(section);
        }
    }

    fn render(&self) -> String {
        let mut out = format!("{}\n", clean(&self.label));
        let mut pending: Vec<_> = self
            .children
            .iter()
            .enumerate()
            .rev()
            .map(|(i, child)| (child, String::new(), i + 1 == self.children.len()))
            .collect();
        while let Some((node, prefix, last)) = pending.pop() {
            let branch = if last { "└──" } else { "├──" };
            out.push_str(&format!("{prefix}{branch} {}\n", clean(&node.label)));
            let prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            for (i, child) in node.children.iter().enumerate().rev() {
                pending.push((child, prefix.clone(), i + 1 == node.children.len()));
            }
        }
        out
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn text_section(title: &str, text: &str) -> Tree {
    let mut section = Tree::new(title);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        section.push(line);
    }
    section
}

pub(super) fn render(
    cmd: &ChangeCmd,
    change: &Change,
    hash: &Hash,
    sequence: Option<u64>,
    data: &ChangeRenderData,
) -> String {
    let hash_len = cmd.get_hash_length();
    let seq = sequence.map(|seq| format!(" (#{seq})")).unwrap_or_default();
    let mut root = Tree::new(format!(
        "change {}{seq}",
        format_hash_with_length(hash, hash_len)
    ));
    let mut metadata = Tree::new("Metadata");
    for author in &change.hashed.header.authors {
        metadata.push(format!("Author: {}", format_author(author)));
    }
    metadata.push(format!(
        "Date: {}",
        format_timestamp(&change.hashed.header.timestamp)
    ));
    root.section(metadata);
    root.section(text_section("Message", &change.hashed.header.message));
    if let Some(description) = &change.hashed.header.description {
        root.section(text_section("Description", description));
    }

    let mut dependencies = Tree::new(format!(
        "Dependencies ({})",
        change.hashed.dependencies.len()
    ));
    for hash in &change.hashed.dependencies {
        let id = format_hash_with_length(hash, hash_len);
        let message = if cmd.show_deps {
            format!(
                " — {}",
                data.dep_messages
                    .get(hash)
                    .map(String::as_str)
                    .unwrap_or("[unable to load]")
            )
        } else {
            String::new()
        };
        dependencies.push(format!("{id}{message}"));
    }
    root.section(dependencies);

    if let Some(git) = change
        .unhashed
        .as_ref()
        .and_then(|metadata| metadata.get("git"))
    {
        let mut section = Tree::new("Git Import");
        if let Some(repository) = git.get("repository").and_then(|v| v.as_str()) {
            section.push(format!("Repository: {repository}"));
        }
        if let Some(sha) = git.get("sha").and_then(|v| v.as_str()) {
            section.push(format!(
                "Git source revision: {}",
                if cmd.full_hash {
                    sha.to_owned()
                } else {
                    sha.chars().take(12).collect::<String>()
                }
            ));
        }
        for (key, label) in [
            ("empty_commit", "Empty Git source revision"),
            ("empty_merge", "Git merge source revision"),
        ] {
            if git.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
                section.push(label);
            }
        }
        root.section(section);
    }

    if let Ok(Some(evidence)) = extract_move_evidence(change) {
        let mut section = Tree::new("Move evidence");
        for moved in evidence.authoritative_moves {
            let authority = match moved.authority {
                MoveAuthority::ExplicitAtomicMove => "explicit atomic move",
                MoveAuthority::StableInodeProjection => "stable inode projection",
            };
            section.push(format!(
                "authoritative: {} → {} ({authority})",
                moved.old_path, moved.new_path
            ));
        }
        for moved in evidence.probable_moves {
            let basis = match moved.basis {
                MoveBasis::ByteIdentity => "byte identity",
                MoveBasis::ContentSimilarity => "content similarity",
            };
            section.push(format!(
                "probable: {} → {} ({}.{:02}%, {basis})",
                moved.old_path,
                moved.new_path,
                moved.score / 100,
                moved.score % 100
            ));
        }
        for loss in evidence.loss_notes {
            match loss {
                LossNote::RenameUnresolved { candidates } => {
                    let mut loss = Tree::new("loss: rename unresolved; retained delete + add");
                    for candidate in candidates {
                        loss.push(format!(
                            "candidate: {} → {} ({}.{:02}%)",
                            candidate.source_path,
                            candidate.destination_path,
                            candidate.score / 100,
                            candidate.score % 100
                        ));
                    }
                    section.children.push(loss);
                }
                LossNote::EmptyDirectory { path } => section.push(format!(
                    "loss: empty directory '{path}' omitted from Git tree projection"
                )),
            }
        }
        root.section(section);
    }

    let (vertices, edges) = count_atoms(&change.hashed.hunks);
    root.push(format!(
        "Graph: +{vertices} vertices, ~{edges} edges, {} bytes",
        change.contents.len()
    ));
    root.section(files(change, cmd.show_hunks));
    // Keep every recorded provenance entry, not just the first provider/model.
    for prov in &change.hashed.provenance {
        root.section(attestation(prov));
    }
    for (_, graph) in &data.ledger_graphs {
        root.section(ledger(graph));
    }
    root.render()
}

#[derive(Default)]
struct PathTree {
    children: BTreeMap<String, PathTree>,
    summary: Option<HunkDisplaySummary>,
    directory: bool,
}

fn files(change: &Change, show_hunks: bool) -> Tree {
    let directories: BTreeSet<_> = change
        .hashed
        .hunks
        .iter()
        .filter_map(|op| match op {
            GraphOp::DirAdd { path, .. }
            | GraphOp::DirDel { path, .. }
            | GraphOp::DirUndel { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let mut paths = PathTree::default();
    for summary in hunk_display_summaries(&change.hashed.hunks) {
        let mut node = &mut paths;
        // Atomic paths use '/', including when the CLI runs on Windows. Do not
        // interpret platform-specific separators or consult the working copy.
        for part in summary.path.split('/') {
            node = node.children.entry(part.to_owned()).or_default();
        }
        node.directory = directories.contains(summary.path.as_str());
        node.summary = Some(summary);
    }
    let mut section = Tree::new(format!(
        "Files changed: {}",
        count_unique_paths(&change.hashed.hunks)
    ));
    section.children = path_nodes(paths, show_hunks);
    section
}

fn path_nodes(paths: PathTree, show_hunks: bool) -> Vec<Tree> {
    paths
        .children
        .into_iter()
        .map(|(name, path)| {
            let slash = if path.directory || !path.children.is_empty() {
                "/"
            } else {
                ""
            };
            let badge = path
                .summary
                .as_ref()
                .map(|summary| format!(" [{}]", status(summary.symbol)))
                .unwrap_or_default();
            let mut node = Tree::new(format!("{name}{slash}{badge}"));
            if show_hunks {
                if let Some(summary) = &path.summary {
                    node.push(summary.info.trim_start_matches('(').trim_end_matches(')'));
                }
            }
            node.children.extend(path_nodes(path, show_hunks));
            node
        })
        .collect()
}

fn status(symbol: &str) -> &str {
    match symbol {
        "+" | "📁+" => "added",
        "-" | "📁-" => "deleted",
        "~" | "±" => "modified",
        "→" => "moved",
        "↑" | "📁↑" => "restored",
        "@" => "attributes",
        "✓" => "resolved",
        "!" => "conflicted",
        "◉" => "root added",
        "⊘" => "root deleted",
        _ => "changed",
    }
}

fn attestation(prov: &Provenance) -> Tree {
    let mut section = Tree::new("Attestation");
    section.push(format!("Vendor: {:?}", prov.vendor));
    section.push(format!("Model: {}", prov.model));
    if let Some(version) = &prov.model_version {
        section.push(format!("Version: {version}"));
    }
    section.push(format!("Tool: {:?}", prov.tool));
    section.push(format!("Type: {:?}", prov.suggestion_type));
    if !prov.tokens.is_empty() {
        let mut tokens = Tree::new("Tokens");
        tokens.push(format!("Input: {}", prov.tokens.input_tokens));
        tokens.push(format!("Output: {}", prov.tokens.output_tokens));
        tokens.push(format!("Total: {}", prov.tokens.total_tokens));
        section.section(tokens);
    }
    if !prov.cost.is_zero() {
        section.push(format!("Cost: ${:.6} USD", prov.cost.usd));
    }
    if let Some(temp) = prov.temperature {
        section.push(format!("Temp: {:.2}", temp as f64 / 1000.0));
    }
    if let Some(id) = &prov.request_id {
        section.push(format!("Request: {id}"));
    }
    if let Some(id) = &prov.session_id {
        section.push(format!("Session: {id}"));
    }
    let mut metadata = Tree::new("Metadata");
    for (key, value) in &prov.metadata {
        let value = if key == "turn_number" {
            value
                .parse::<u32>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .map(|n| n.to_string())
                .unwrap_or_else(|| value.clone())
        } else {
            value.clone()
        };
        metadata.push(format!("{key}: {value}"));
    }
    section.section(metadata);
    section
}

fn ledger(graph: &ProvenanceGraph) -> Tree {
    let mut section = Tree::new("Change Ledger");
    section.push(format!("Session: {}", graph.session_id));
    section.push(format!(
        "Agent: {} ({})",
        graph.agent_display_name, graph.agent_vendor
    ));
    section.push(format!(
        "Nodes: {}  Edges: {}  Changes: {}",
        graph.node_count(),
        graph.edge_count(),
        graph.change_count()
    ));
    for node in &graph.nodes {
        let tool = node
            .tool_name
            .as_ref()
            .map(|tool| format!(" [{tool}]"))
            .unwrap_or_default();
        let duration = node
            .duration_ms
            .map(|ms| format!(" ({ms}ms)"))
            .unwrap_or_default();
        let mut entry = Tree::new(format!("{}: {}{tool}{duration}", node.kind, node.summary));
        if let Some(detail) = node
            .detail
            .as_ref()
            .and_then(|detail| command::format_node_detail(detail))
        {
            entry.push(detail);
        }
        section.children.push(entry);
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Atom, ChangeHeader, EdgeUpdate, Insertion, Local};
    use atomic_core::{ChangePosition, EdgeFlags, Position};

    fn insertion() -> Insertion<Option<Hash>> {
        Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(1),
            inode: Position::new(None, ChangePosition::new(0)),
        }
    }

    fn edges() -> EdgeUpdate<Option<Hash>> {
        EdgeUpdate {
            edges: vec![],
            inode: Position::new(None, ChangePosition::new(0)),
        }
    }

    fn fixture() -> Change {
        Change::new(
            ChangeHeader::new("Update CLI\nKeep it readable"),
            vec![
                GraphOp::DirAdd {
                    path: "src".into(),
                    add_name: insertion(),
                    add_inode: insertion(),
                },
                GraphOp::FileAdd {
                    path: "src/new.ts".into(),
                    add_name: insertion(),
                    add_inode: insertion(),
                    contents: None,
                    encoding: None,
                },
                GraphOp::FileDel {
                    path: "src/old.ts".into(),
                    del: edges(),
                    contents: None,
                    encoding: None,
                },
                GraphOp::Edit {
                    local: Local::new("README.md", 1),
                    change: Atom::Insertion(insertion()),
                    encoding: None,
                },
                GraphOp::Edit {
                    local: Local::new("README.md", 2),
                    change: Atom::Insertion(insertion()),
                    encoding: None,
                },
                GraphOp::FileMove {
                    path: "src/renamed.ts".into(),
                    del: edges(),
                    add: insertion(),
                },
                GraphOp::DirDel {
                    path: "obsolete".into(),
                    del: edges(),
                },
                GraphOp::DirUndel {
                    path: "restored".into(),
                    undel: edges(),
                },
            ],
            vec![b'x'],
            vec![],
        )
    }

    fn data() -> ChangeRenderData {
        ChangeRenderData {
            dep_messages: Default::default(),
            ledger_graphs: vec![],
        }
    }

    #[test]
    fn paths_have_text_status_and_directories_have_slashes_not_emoji() {
        let change = fixture();
        assert_eq!(
            files(&change, false).render(),
            concat!(
                "Files changed: 7\n",
                "├── README.md [modified]\n",
                "├── obsolete/ [deleted]\n",
                "├── restored/ [restored]\n",
                "└── src/ [added]\n",
                "    ├── new.ts [added]\n",
                "    ├── old.ts [deleted]\n",
                "    └── renamed.ts [moved]\n",
            )
        );
    }

    #[test]
    fn path_grouping_is_stable_and_hunk_details_expand_the_same_nodes() {
        let mut change = fixture();
        let compact = files(&change, false).render();
        change.hashed.hunks.reverse();
        assert_eq!(files(&change, false).render(), compact);
        let detailed = files(&change, true).render();
        assert_eq!(detailed.matches("README.md [modified]").count(), 1);
        assert!(detailed.contains("│   └── 2 hunks:"));
        assert!(!compact.contains("span"));
        assert!(detailed.contains("span"));
    }

    #[test]
    fn full_hash_and_dependency_messages_are_preserved() {
        let mut change = fixture();
        let dependency = Hash::of(b"dependency");
        change.hashed.dependencies = vec![dependency];
        let hash = Hash::of(b"change");
        let mut data = data();
        data.dep_messages.insert(dependency, "Initial setup".into());
        let cmd = ChangeCmd::new().with_full_hash(true).with_show_deps(true);
        let text = render(&cmd, &change, &hash, Some(42), &data);
        assert!(text.starts_with(&format!("change {} (#42)\n", hash.to_base32())));
        assert!(text.contains(&format!(
            "│   └── {} — Initial setup\n",
            dependency.to_base32()
        )));
        assert!(text.contains("├── Message\n│   ├── Update CLI\n│   └── Keep it readable\n"));
    }

    #[test]
    fn empty_sections_are_omitted_and_controls_cannot_break_the_tree() {
        let mut change = Change::empty(ChangeHeader::new("\n"));
        change.hashed.header.description = Some(" \n ".into());
        let text = render(
            &ChangeCmd::new(),
            &change,
            &Hash::of(b"empty"),
            None,
            &data(),
        );
        for empty in [
            "Dependencies",
            "Files changed",
            "Attestation",
            "Change Ledger",
            "No provenance",
            "Message",
            "Description",
        ] {
            assert!(!text.contains(empty));
        }
        assert!(text.contains("└── Graph:"));
        let mut node = Tree::new("Files");
        node.push("name\n\u{1b}[31m.ts");
        assert_eq!(node.render(), "Files\n└── name  [31m.ts\n");
    }

    #[test]
    fn git_import_labels_identify_source_revisions_not_atomic_commits() {
        let mut change = fixture();
        let sha = "abcdef0123456789".repeat(4);
        change.unhashed = Some(serde_json::json!({"git": {
            "repository": "source-repository", "sha": sha,
            "empty_commit": true, "empty_merge": true
        }}));
        let hash = Hash::of(b"change");
        let text = render(&ChangeCmd::new(), &change, &hash, None, &data());
        assert!(text.contains("Git source revision: abcdef012345\n"));
        assert!(text.contains("Empty Git source revision"));
        assert!(text.contains("Git merge source revision"));
        assert!(!text.to_lowercase().contains("commit"));
        let text = render(
            &ChangeCmd::new().with_full_hash(true),
            &change,
            &hash,
            None,
            &data(),
        );
        assert!(text.contains(&format!("Git source revision: {sha}\n")));
    }

    #[test]
    fn all_attestations_and_recorded_ledger_nodes_are_nested() {
        use atomic_core::change::provenance_graph::{ProvenanceNode, ProvenanceNodeKind};
        let mut change = fixture();
        for model in ["model-one", "model-two"] {
            let mut prov = Provenance::builder().model(model).cost_usd(0.125).build();
            prov.tokens = atomic_core::change::TokenUsage::full(100, 40, 10, 60, 20);
            prov.metadata.push(("turn_number".into(), "2".into()));
            change.add_provenance(prov);
        }
        let mut data = data();
        data.ledger_graphs.push((
            Hash::of(b"ledger"),
            ProvenanceGraph::builder("session", "opencode")
                .add_node(ProvenanceNode {
                    id: "step-1".into(),
                    kind: ProvenanceNodeKind::Execution,
                    timestamp: 1000,
                    summary: "Run tests".into(),
                    detail: Some(r#"{"command":"cargo test"}"#.into()),
                    change_hash: None,
                    tool_name: Some("shell".into()),
                    tool_call_id: None,
                    duration_ms: Some(150),
                    classified: false,
                    confidence: None,
                    consolidated_from: vec![],
                })
                .build(),
        ));
        let text = render(
            &ChangeCmd::new(),
            &change,
            &Hash::of(b"change"),
            None,
            &data,
        );
        assert_eq!(text.matches("├── Attestation").count(), 2);
        assert!(text.contains("Model: model-one") && text.contains("Model: model-two"));
        assert!(text.contains("│   ├── Tokens\n│   │   ├── Input: 100"));
        assert!(text.contains("Cost: $0.125000 USD"));
        assert!(text.contains("turn_number: 1"));
        assert!(text.contains("└── Change Ledger\n    ├── Session: session"));
        assert!(text
            .contains("    └── execution: Run tests [shell] (150ms)\n        └── $ cargo test\n"));
    }
}
