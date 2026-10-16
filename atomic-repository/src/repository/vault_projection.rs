//! Lift canonical vault data into workflow facts without rewriting signed objects.

use super::*;
use atomic_core::pristine::{VaultEntry, VaultEntryType, VaultTxnT};
use atomic_core::{
    kg::{
        iri::{self, ChildKind},
        ontology::{Class as C, Predicate as P},
        project_id::ProjectId,
    },
    KgEdge, KgNode, KgSubgraph,
};
use std::collections::BTreeMap;

pub(super) struct VaultProjection {
    project: ProjectId,
    subjects: BTreeMap<String, (String, C)>,
    paths: BTreeMap<String, String>,
    hashes: BTreeMap<String, Hash>,
}

fn stem(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix)
        .unwrap_or(path)
        .strip_suffix(".md")
        .unwrap_or(path.strip_prefix(prefix).unwrap_or(path))
        .to_owned()
}

impl VaultProjection {
    pub(super) fn new(
        txn: &impl VaultTxnT,
        project: ProjectId,
        hashes: Vec<Hash>,
    ) -> Result<Self, RepositoryError> {
        let hashes = hashes
            .into_iter()
            .map(|hash| (hash.to_base32(), hash))
            .collect();
        let mut context = Self {
            project,
            subjects: BTreeMap::new(),
            paths: BTreeMap::new(),
            hashes,
        };
        let manifest = txn.get_vault_manifest()?;
        for meta in txn.list_vault_entries("", None)? {
            let Some(entry) = txn.get_vault_entry(&meta.path)? else {
                continue;
            };
            let fm: serde_json::Value =
                serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();
            let (subject, class, local_kind, name) = match entry.entry_type {
                VaultEntryType::Intent => {
                    let uid = fm
                        .get("uid")
                        .and_then(|v| v.as_str())
                        .map(str::to_uppercase)
                        .or_else(|| {
                            manifest
                                .intents
                                .values()
                                .find(|summary| {
                                    summary.vault_path == meta.path && !summary.uid.is_empty()
                                })
                                .map(|s| s.uid.to_uppercase())
                        });
                    let Some(uid) = uid else {
                        continue;
                    };
                    let Ok(id) = iri::mint_intent(&project, &uid) else {
                        continue;
                    };
                    (id, C::WorkIntent, "intent", uid)
                }
                VaultEntryType::Memory => {
                    let name = stem(&meta.path, "memory/");
                    (iri::mint_memory(&project, &name), C::Memory, "memory", name)
                }
                VaultEntryType::Skill => {
                    let name = stem(&meta.path, "skills/");
                    (iri::mint_skill(&project, &name), C::Skill, "skill", name)
                }
                VaultEntryType::Session => {
                    let name = meta
                        .path
                        .strip_prefix("goals/")
                        .and_then(|s| s.strip_suffix("/_goal.md"))
                        .unwrap_or(&meta.path)
                        .to_owned();
                    (iri::mint_goal(&project, &name), C::Goal, "goal", name)
                }
                VaultEntryType::ToolResult => (
                    iri::mint_tool_result(&project, &meta.path),
                    C::ToolResult,
                    "tool-result",
                    meta.path.clone(),
                ),
                _ => continue,
            };
            context.paths.insert(meta.path.clone(), subject.clone());
            context
                .subjects
                .insert(subject.to_lowercase(), (subject.clone(), class));
            context.subjects.insert(
                format!("urn:atomic:{local_kind}:{name}").to_lowercase(),
                (subject.clone(), class),
            );
            if entry.entry_type == VaultEntryType::Intent {
                for (key, summary) in &manifest.intents {
                    if summary.vault_path == meta.path {
                        context.subjects.insert(
                            format!("urn:atomic:intent:{key}").to_lowercase(),
                            (subject.clone(), class),
                        );
                    }
                }
                if let Some(id) = fm.get("id").and_then(|v| v.as_str()) {
                    context.subjects.insert(
                        format!("urn:atomic:intent:{id}").to_lowercase(),
                        (subject.clone(), class),
                    );
                }
                if let Some(map) = fm.as_object() {
                    if let Ok(node) = atomic_canonical::lift::lift_intent(
                        map,
                        &String::from_utf8_lossy(&entry.content_bytes),
                    ) {
                        let mut children = Vec::new();
                        children.extend(
                            node.has_task
                                .iter()
                                .map(|c| (&c.id, ChildKind::Task, C::Task)),
                        );
                        children.extend(node.has_acceptance_criterion.iter().map(|c| {
                            (
                                &c.id,
                                ChildKind::AcceptanceCriterion,
                                C::AcceptanceCriterion,
                            )
                        }));
                        children.extend(
                            node.has_scope_in
                                .iter()
                                .chain(&node.has_scope_out)
                                .map(|c| (&c.id, ChildKind::Scope, C::ScopeItem)),
                        );
                        children.extend(
                            node.has_constraint
                                .iter()
                                .map(|c| (&c.id, ChildKind::Constraint, C::Constraint)),
                        );
                        for (raw, kind, class) in children {
                            let local = raw.rsplit(':').next().unwrap_or(raw);
                            let id =
                                iri::mint_child(&project, &name, kind, local).map_err(|e| {
                                    RepositoryError::InvalidOperation {
                                        message: e.to_string(),
                                    }
                                })?;
                            context
                                .subjects
                                .insert(raw.to_lowercase(), (id.clone(), class));
                            context.subjects.insert(id.to_lowercase(), (id, class));
                        }
                    }
                }
            }
        }
        Ok(context)
    }

    fn resolve(&self, raw: &str) -> Option<(String, C)> {
        if raw.starts_with("memory:") || raw.starts_with("intent:") {
            return self.resolve(&format!("urn:atomic:{raw}"));
        }
        if let Some(target) = self.subjects.get(&raw.to_lowercase()) {
            return Some(target.clone());
        }
        if let Ok(hash) = iri::parse_change(raw) {
            return self
                .hashes
                .contains_key(&hash.to_base32())
                .then(|| (iri::mint_change(&hash), C::Change));
        }
        if let Some(prefix) = raw.strip_prefix("urn:atomic:change:") {
            let prefix = prefix.to_uppercase();
            if prefix.is_empty() {
                return None;
            }
            let mut matches = self
                .hashes
                .range(prefix.clone()..)
                .take_while(|(key, _)| key.starts_with(&prefix));
            let (_, hash) = matches.next()?;
            return matches
                .next()
                .is_none()
                .then(|| (iri::mint_change(hash), C::Change));
        }
        None
    }

    fn finding(
        &self,
        graph: &mut KgSubgraph,
        subject: &str,
        code: &str,
        message: &str,
        path: &str,
    ) {
        let id = iri::mint_finding(&self.project, code, subject);
        graph.nodes.push(
            KgNode::new(&id, C::Finding, code, "vault")
                .with_summary(message)
                .with_metadata(
                    serde_json::json!({"finding_code": code, "derived_from_vault_path": path}),
                ),
        );
        graph.edges.push(
            KgEdge::new(id, subject, P::AboutResource)
                .with_metadata(serde_json::json!({"derived_from_vault_path": path})),
        );
    }

    fn link(&self, graph: &mut KgSubgraph, subject: &str, raw: &str, predicate: P, path: &str) {
        if let Some((target, class)) = self.resolve(raw) {
            graph
                .nodes
                .push(KgNode::new(&target, class, raw, "vault_reference"));
            graph.edges.push(
                KgEdge::new(subject, target, predicate)
                    .with_metadata(serde_json::json!({"rdf_target": raw})),
            );
        } else {
            self.finding(
                graph,
                subject,
                if raw.starts_with("urn:atomic:todo:") {
                    "UnresolvedLegacyTodo"
                } else {
                    "UnresolvedReference"
                },
                &format!("Unresolved reference: {raw}"),
                path,
            );
        }
    }

    pub(super) fn extract(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<KgSubgraph, RepositoryError> {
        if entry.entry_type == VaultEntryType::Attestation {
            return self.extract_signed_document(path, entry);
        }
        let mut graph = KgSubgraph::default();
        let Some(subject) = self.paths.get(path) else {
            if entry.entry_type == VaultEntryType::Intent {
                let project = iri::mint_project(&self.project);
                graph.nodes.push(KgNode::new(
                    &project,
                    C::Project,
                    self.project.to_base32(),
                    "vcs",
                ));
                self.finding(
                    &mut graph,
                    &project,
                    "MissingIntentId",
                    &format!("Intent has no canonical ULID: {path}"),
                    path,
                );
            }
            return Ok(graph);
        };
        let fm: serde_json::Value =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();
        let class = self
            .subjects
            .get(&subject.to_lowercase())
            .expect("indexed subject")
            .1;
        let label = fm
            .get("title")
            .or_else(|| fm.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(path);
        let mut node = KgNode::new(subject, class, label, "vault")
            .with_metadata(serde_json::json!({"vault_path": path, "created_at": entry.created_at, "updated_at": entry.updated_at,
                "status": fm.get("status"), "priority": fm.get("priority"), "human_key": fm.get("id"), "uid": fm.get("uid")}));
        if let Some(description) = fm
            .get("description")
            .or_else(|| fm.get("title"))
            .and_then(|v| v.as_str())
        {
            node = node.with_summary(description);
        }
        if class == C::WorkIntent {
            match fm.get("kind").and_then(|v| v.as_str()).unwrap_or("feature") {
                "feature" => node = node.with_type(C::FeatureIntent),
                "bug" => node = node.with_type(C::BugIntent),
                "review" => node = node.with_type(C::ReviewIntent),
                _ => {}
            }
            if matches!(
                fm.get("status").and_then(|v| v.as_str()),
                Some("in_progress" | "needs-review" | "done")
            ) {
                node = node.with_type(C::ManagedIntent);
            }
        }
        for field in [
            "attributedTo",
            "author",
            "created_by",
            "developer",
            "creator",
        ] {
            if let Some(author) = fm
                .get(field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let id = iri::mint_identity(&self.project, author);
                graph
                    .nodes
                    .push(KgNode::new(&id, C::Identity, author, "vault"));
                graph.edges.push(KgEdge::new(subject, id, P::AuthoredBy));
                break;
            }
        }
        if let Some(assignee) = fm.get("assignee").and_then(|v| v.as_str()) {
            let id = iri::mint_identity(&self.project, assignee);
            graph
                .nodes
                .push(KgNode::new(&id, C::Identity, assignee, "vault"));
            graph.edges.push(KgEdge::new(subject, id, P::AssignedTo));
        }
        for label in links(&fm, "labels") {
            let id = iri::mint_concept(&self.project, label);
            graph
                .nodes
                .push(KgNode::new(&id, C::Concept, label, "vault"));
            graph.edges.push(KgEdge::new(subject, id, P::HasLabel));
        }
        for (field, kind, predicate) in [
            ("informedBy", "", P::InformedBy),
            ("derivedFrom", "", P::ProvWasDerivedFrom),
            ("blocked_by", "intent", P::BlockedBy),
            ("goals", "goal", P::LinkedGoal),
            ("intent", "intent", P::LinkedIntent),
        ] {
            for raw in links(&fm, field) {
                let target = if kind.is_empty() || raw.starts_with("urn:") {
                    raw.to_owned()
                } else {
                    format!("urn:atomic:{kind}:{raw}")
                };
                self.link(&mut graph, subject, &target, predicate, path);
                if predicate == P::BlockedBy {
                    if let Some((target, _)) = self.resolve(&target) {
                        graph.edges.push(KgEdge::new(target, subject, P::Blocks));
                    }
                }
            }
        }
        if entry.entry_type == VaultEntryType::ToolResult {
            if let Some(goal) = path
                .strip_prefix("goals/")
                .and_then(|p| p.split_once('/'))
                .map(|p| p.0)
            {
                self.link(
                    &mut graph,
                    subject,
                    &format!("urn:atomic:goal:{goal}"),
                    P::LinkedGoal,
                    path,
                );
            }
        }
        let content = String::from_utf8_lossy(&entry.content_bytes);
        if class == C::WorkIntent {
            if let Some(map) = fm.as_object() {
                if let Ok(intent) = atomic_canonical::lift::lift_intent(map, &content) {
                    for ac in &intent.has_acceptance_criterion {
                        if let Some((id, class)) = self.resolve(&ac.id) {
                            graph.nodes.push(KgNode::new(&id, class, &ac.text, "vault").with_summary(&ac.text)
                                .with_metadata(serde_json::json!({"status": ac.ac_status, "verifiedBy": ac.verified_by, "evidence": ac.evidence})));
                            graph
                                .edges
                                .push(KgEdge::new(subject, id, P::HasAcceptanceCriterion));
                        }
                    }
                    for task in &intent.has_task {
                        if let Some((id, class)) = self.resolve(&task.id) {
                            graph.nodes.push(
                                KgNode::new(&id, class, &task.text, "vault")
                                    .with_summary(&task.text)
                                    .with_metadata(serde_json::json!({"status": task.task_status})),
                            );
                            graph.edges.push(KgEdge::new(subject, &id, P::HasTask));
                            for ac in task.satisfies.as_slice() {
                                self.link(&mut graph, &id, ac, P::Satisfies, path);
                            }
                            for file in &task.touches_file {
                                self.file_link(&mut graph, &id, file, P::Touches);
                            }
                        }
                    }
                    for (items, predicate) in [
                        (&intent.has_scope_in, P::HasScopeIn),
                        (&intent.has_scope_out, P::HasScopeOut),
                    ] {
                        for item in items {
                            if let Some((id, class)) = self.resolve(&item.id) {
                                graph.nodes.push(
                                    KgNode::new(&id, class, &item.text, "vault")
                                        .with_summary(&item.text),
                                );
                                graph.edges.push(KgEdge::new(subject, id, predicate));
                            }
                            if predicate == P::HasScopeOut {
                                for file in &item.files {
                                    self.file_link(&mut graph, subject, file, P::ScopeOutFile);
                                }
                            }
                        }
                    }
                    for constraint in &intent.has_constraint {
                        if let Some((id, class)) = self.resolve(&constraint.id) {
                            graph.nodes.push(
                                KgNode::new(&id, class, &constraint.text, "vault")
                                    .with_summary(&constraint.text),
                            );
                            graph.edges.push(KgEdge::new(subject, id, P::HasConstraint));
                        }
                    }
                    for reference in &intent.depends_on {
                        let predicate = match reference.edge.as_str() {
                            "dependsOn" => P::DependsOnIntent,
                            "blockedBy" => P::BlockedBy,
                            "remediates" => P::Remediates,
                            "reviews" => P::Reviews,
                            _ => P::References,
                        };
                        let raw = if reference.to.starts_with("urn:") {
                            reference.to.clone()
                        } else {
                            format!("urn:atomic:intent:{}", reference.to)
                        };
                        self.link(&mut graph, subject, &raw, predicate, path);
                    }
                }
            }
        }
        for part in content.split("[[").skip(1) {
            if let Some((name, _)) = part.split_once("]]") {
                self.link(
                    &mut graph,
                    subject,
                    &format!("urn:atomic:memory:{name}"),
                    P::References,
                    path,
                );
            }
        }
        for word in content.split_whitespace() {
            let file = word
                .trim_matches([
                    '`', '"', '\'', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';',
                ])
                .trim_end_matches('.');
            if file.len() < 201
                && file.contains('/')
                && [".rs", ".py", ".ts", ".js", ".go", ".md", ".toml", ".json"]
                    .iter()
                    .any(|extension| file.ends_with(extension))
            {
                self.file_link(
                    &mut graph,
                    subject,
                    file,
                    if class == C::ToolResult {
                        P::ProvUsed
                    } else {
                        P::References
                    },
                );
            }
        }
        graph.nodes.push(node);
        for node in &mut graph.nodes {
            if node.id == *subject
                || node.has_type(C::Task)
                || node.has_type(C::AcceptanceCriterion)
                || node.has_type(C::ScopeItem)
                || node.has_type(C::Constraint)
                || node.has_type(C::Finding)
            {
                let metadata = node.metadata.get_or_insert_with(|| serde_json::json!({}));
                metadata["derived_from_vault_path"] = serde_json::json!(path);
            }
        }
        for edge in &mut graph.edges {
            let metadata = edge.metadata.get_or_insert_with(|| serde_json::json!({}));
            metadata["derived_from_vault_path"] = serde_json::json!(path);
        }
        Ok(graph)
    }

    fn extract_signed_document(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<KgSubgraph, RepositoryError> {
        // Signed JSON-LD is an immutable representation of an intent/memory,
        // not a mutable source entry and not a session `.attest` object. Read
        // typed fields without expanding or rewriting its historical context.
        let mut graph = KgSubgraph::default();
        let document = if let Ok(node) =
            serde_json::from_slice::<atomic_canonical::CanonicalNode>(&entry.content_bytes)
        {
            let mut references: Vec<_> = node
                .informed_by
                .into_iter()
                .map(|target| (target, P::InformedBy))
                .collect();
            for reference in node.depends_on {
                let predicate = match reference.edge.as_str() {
                    "dependsOn" => P::DependsOnIntent,
                    "blockedBy" => P::BlockedBy,
                    "remediates" => P::Remediates,
                    "reviews" => P::Reviews,
                    _ => P::References,
                };
                references.push((reference.to, predicate));
            }
            Some((node.id, node.attributed_to, references))
        } else if let Ok(node) =
            serde_json::from_slice::<atomic_canonical::memory::MemoryNode>(&entry.content_bytes)
        {
            Some((
                node.id,
                node.attributed_to,
                node.derived_from
                    .into_iter()
                    .map(|target| (target, P::ProvWasDerivedFrom))
                    .collect(),
            ))
        } else {
            None
        };
        if let Some((id, author, references)) = document {
            if let Some((subject, class)) = self.resolve(&id) {
                graph
                    .nodes
                    .push(KgNode::new(&subject, class, &id, "vault_reference"));
                if let Some(author) = author {
                    let identity = iri::mint_identity(&self.project, &author);
                    graph.nodes.push(KgNode::new(
                        &identity,
                        C::Identity,
                        author,
                        "vault_reference",
                    ));
                    graph
                        .edges
                        .push(KgEdge::new(&subject, identity, P::AuthoredBy));
                }
                for (target, predicate) in references {
                    self.link(&mut graph, &subject, &target, predicate, path);
                }
            } else {
                let project = iri::mint_project(&self.project);
                graph.nodes.push(KgNode::new(
                    &project,
                    C::Project,
                    self.project.to_base32(),
                    "vcs",
                ));
                self.finding(
                    &mut graph,
                    &project,
                    "UnresolvedReference",
                    &format!("Signed document has no locally resolved subject: {id}"),
                    path,
                );
            }
        }
        for edge in &mut graph.edges {
            edge.metadata.get_or_insert_with(|| serde_json::json!({}))["derived_from_vault_path"] =
                serde_json::json!(path);
        }
        Ok(graph)
    }

    fn file_link(&self, graph: &mut KgSubgraph, subject: &str, path: &str, predicate: P) {
        let file = iri::mint_file(&self.project, path);
        graph.nodes.push(
            KgNode::new(&file, C::File, path, "vault")
                .with_metadata(serde_json::json!({"path": path})),
        );
        graph.edges.push(KgEdge::new(subject, file, predicate));
    }
}

fn links<'a>(fm: &'a serde_json::Value, field: &str) -> Vec<&'a str> {
    match fm.get(field) {
        Some(serde_json::Value::Array(values)) => {
            values.iter().filter_map(|v| v.as_str()).collect()
        }
        Some(serde_json::Value::String(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}
