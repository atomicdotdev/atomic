//! Lift canonical vault data into workflow facts without rewriting signed objects.

use super::kinds::{KindDefinition, KindPath, KindStatus};
use super::*;
use atomic_canonical::kind_spec::{
    has_conflict_markers, is_valid_kind_name, is_valid_ulid, validate_instance, BaseType, KindSpec,
    RefResolver,
};
use atomic_core::pristine::{VaultEntry, VaultEntryType, VaultTxnT};
use atomic_core::{
    kg::{
        iri::{self, ChildKind},
        ontology::{Class as C, Predicate as P},
        project_id::ProjectId,
    },
    KgEdge, KgNode, KgSubgraph,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct VaultProjection {
    project: ProjectId,
    // Canonical IRIs are exact identities. Only historical aliases may fold
    // case, and a collision must stay unresolved rather than pick a winner.
    subjects: BTreeMap<String, (String, C)>,
    aliases: BTreeMap<String, Option<(String, C)>>,
    paths: BTreeMap<String, String>,
    hashes: BTreeMap<String, Hash>,
    // Stored kind definitions, by kind name. Read ahead of the main walk because
    // `<ULID>.md` sorts before `_kind.md`.
    kinds: BTreeMap<String, KindDefinition>,
    // Every label a built-in row carries. Labels mint into the concept namespace
    // kind nodes also use, so a label can name one: see `inert_kind_row`.
    labels: BTreeSet<String>,
}

fn stem(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix)
        .unwrap_or(path)
        .strip_suffix(".md")
        .unwrap_or(path.strip_prefix(prefix).unwrap_or(path))
        .to_owned()
}

/// The concept-namespace name of a kind's definition hub: `kind.<k>`.
pub(super) fn kind_hub_name(kind: &str) -> String {
    format!("kind.{kind}")
}

/// The concept-namespace name of a kind instance: `<k>.<ULID>`.
pub(super) fn kind_instance_name(kind: &str, uid: &str) -> String {
    format!("{kind}.{uid}")
}

/// Whether a stored instance row has anything trustworthy to project.
///
/// Frontmatter under `kinds/` that fails strict parsing is stored as `{}`, so a
/// missing `uid` means there is none. A row that fails this stays inert, for
/// `atomic kind validate` to report.
fn instance_is_projectable(entry: &VaultEntry, fm: &serde_json::Value) -> bool {
    fm.as_object().is_some_and(|map| map.contains_key("uid"))
        && std::str::from_utf8(&entry.content_bytes).is_ok_and(|body| !has_conflict_markers(body))
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
            aliases: BTreeMap::new(),
            paths: BTreeMap::new(),
            hashes,
            kinds: BTreeMap::new(),
            labels: BTreeSet::new(),
        };
        let manifest = txn.get_vault_manifest()?;
        for meta in txn.list_vault_entries("kinds/", Some(VaultEntryType::Scratch))? {
            if let Some(KindPath::Definition { kind }) = KindPath::classify(&meta.path) {
                if let Some(entry) = txn.get_vault_entry(&meta.path)? {
                    let definition = KindDefinition::from_stored(&kind, &entry);
                    context.kinds.insert(kind, definition);
                }
            }
        }
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
                VaultEntryType::Scratch => match KindPath::classify(&meta.path) {
                    Some(KindPath::Definition { kind }) => {
                        let name = kind_hub_name(&kind);
                        (
                            iri::mint_concept(&project, &name),
                            C::Concept,
                            "concept",
                            name,
                        )
                    }
                    Some(KindPath::Instance { kind, uid })
                        if context.kinds.get(&kind).is_some_and(|d| d.spec().is_some())
                            && instance_is_projectable(&entry, &fm) =>
                    {
                        let name = kind_instance_name(&kind, &uid);
                        (
                            iri::mint_concept(&project, &name),
                            C::Concept,
                            "concept",
                            name,
                        )
                    }
                    _ => continue,
                },
                _ => continue,
            };
            if entry.entry_type != VaultEntryType::Scratch {
                context
                    .labels
                    .extend(links(&fm, "labels").into_iter().map(str::to_owned));
            }
            context.paths.insert(meta.path.clone(), subject.clone());
            context
                .subjects
                .insert(subject.clone(), (subject.clone(), class));
            context.add_alias(
                format!("urn:atomic:{local_kind}:{name}"),
                (subject.clone(), class),
            );
            if entry.entry_type == VaultEntryType::Intent {
                for (key, summary) in &manifest.intents {
                    if summary.vault_path == meta.path {
                        context.add_alias(
                            format!("urn:atomic:intent:{key}"),
                            (subject.clone(), class),
                        );
                    }
                }
                if let Some(id) = fm.get("id").and_then(|v| v.as_str()) {
                    context.add_alias(format!("urn:atomic:intent:{id}"), (subject.clone(), class));
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
                            context.add_alias(raw.to_owned(), (id.clone(), class));
                            context.subjects.insert(id.clone(), (id, class));
                        }
                    }
                }
            }
        }
        Ok(context)
    }

    fn add_alias(&mut self, alias: String, target: (String, C)) {
        self.aliases
            .entry(alias.to_lowercase())
            .and_modify(|existing| {
                if existing.as_ref() != Some(&target) {
                    *existing = None;
                }
            })
            .or_insert(Some(target));
    }

    fn resolve(&self, raw: &str) -> Option<(String, C)> {
        if raw.starts_with("memory:") || raw.starts_with("intent:") {
            return self.resolve(&format!("urn:atomic:{raw}"));
        }
        if let Some(target) = self.subjects.get(raw) {
            return Some(target.clone());
        }
        if let Some(target) = self.aliases.get(&raw.to_lowercase()) {
            return target.clone();
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
            // The referring entry owns the edge, never the target's metadata.
            // In particular, a child type does not imply local ownership.
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
        if entry.entry_type == VaultEntryType::Scratch {
            return Ok(self.extract_kind_entry(path, entry));
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
        let class = self.subjects.get(subject).expect("indexed subject").1;
        let label = fm
            .get("title")
            .or_else(|| fm.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(path);
        let mut node = KgNode::new(subject, class, label, "vault")
            .with_metadata(serde_json::json!({"vault_path": path, "derived_from_vault_path": path, "created_at": entry.created_at, "updated_at": entry.updated_at,
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
                                .with_metadata(serde_json::json!({"status": ac.ac_status, "verifiedBy": ac.verified_by, "evidence": ac.evidence, "derived_from_vault_path": path})));
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
                                    .with_metadata(serde_json::json!({"status": task.task_status, "derived_from_vault_path": path})),
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
                                        .with_summary(&item.text)
                                        .with_metadata(
                                            serde_json::json!({"derived_from_vault_path": path}),
                                        ),
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
                                    .with_summary(&constraint.text)
                                    .with_metadata(
                                        serde_json::json!({"derived_from_vault_path": path}),
                                    ),
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
                            "declaresImplementation" => P::DeclaresImplementation,
                            _ => P::References,
                        };
                        let raw = if reference.to.starts_with("urn:") {
                            reference.to.clone()
                        } else {
                            format!("urn:atomic:intent:{}", reference.to)
                        };
                        self.link(&mut graph, subject, &raw, predicate, path);
                        if predicate == P::DeclaresImplementation {
                            if let Some(edge) =
                                graph.edges.last_mut().filter(|edge| edge.kind == predicate)
                            {
                                edge.metadata.get_or_insert_with(|| serde_json::json!({}))
                                    ["declarationEvidence"] =
                                    serde_json::json!(reference.declaration_evidence);
                            }
                        }
                    }
                }
            }
        }
        self.scan_memory_links(&mut graph, subject, &content, path);
        self.scan_file_mentions(&mut graph, subject, &content, class);
        graph.nodes.push(node);
        for edge in &mut graph.edges {
            let metadata = edge.metadata.get_or_insert_with(|| serde_json::json!({}));
            metadata["derived_from_vault_path"] = serde_json::json!(path);
        }
        Ok(graph)
    }

    /// `[[name]]` memory links in an entry's markdown body.
    fn scan_memory_links(&self, graph: &mut KgSubgraph, subject: &str, content: &str, path: &str) {
        for part in content.split("[[").skip(1) {
            if let Some((name, _)) = part.split_once("]]") {
                self.link(
                    graph,
                    subject,
                    &format!("urn:atomic:memory:{name}"),
                    P::References,
                    path,
                );
            }
        }
    }

    /// Repository file paths mentioned in an entry's markdown body.
    fn scan_file_mentions(&self, graph: &mut KgSubgraph, subject: &str, content: &str, class: C) {
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
                    graph,
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
    }

    /// The projection of a `Scratch` row. Only `kinds/` rows have one. It returns
    /// no `Result` because nothing a row holds may fail it: `rebuild_kg` runs in
    /// writable open, so a bad file would lock its user out of the repository.
    fn extract_kind_entry(&self, path: &str, entry: &VaultEntry) -> KgSubgraph {
        let mut graph = KgSubgraph::default();
        let Some(subject) = self.paths.get(path) else {
            self.inert_kind_row(&mut graph, path);
            return graph;
        };
        match KindPath::classify(path) {
            Some(KindPath::Definition { kind }) => {
                self.project_kind_definition(&mut graph, subject, path, &kind, entry);
            }
            Some(KindPath::Instance { kind, uid }) => {
                if let Some(spec) = self.kinds.get(&kind).and_then(KindDefinition::spec) {
                    self.project_kind_instance(&mut graph, subject, path, spec, &uid, entry);
                }
            }
            Some(KindPath::Stray) | None => {}
        }
        graph.edges = merge_kind_edges(std::mem::take(&mut graph.edges), path);
        graph.nodes = merge_kind_nodes(std::mem::take(&mut graph.nodes));
        graph
    }

    /// An instance row with no kind node normally projects nothing. The exception
    /// is a row whose `<k>.<ULID>` a label also spells.
    ///
    /// A label's stub has no owner, so it lands only while the IRI is free. When
    /// a row goes inert, an earlier-sorting labeller still finds the row's old
    /// node at the IRI and its stub does not land; the row's own store then deletes
    /// that node, and `del_kg_node` takes the label's edge with it. A rebuild has
    /// both. Owning a bare node for as long as the row exists keeps the IRI taken
    /// throughout the pass, so every write order ends where a rebuild does.
    fn inert_kind_row(&self, graph: &mut KgSubgraph, path: &str) {
        if let Some(KindPath::Instance { kind, uid }) = KindPath::classify(path) {
            let name = kind_instance_name(&kind, &uid);
            if self.labels.contains(&name) {
                graph.nodes.push(
                    KgNode::new(
                        iri::mint_concept(&self.project, &name),
                        C::Concept,
                        name,
                        "vault",
                    )
                    .with_metadata(serde_json::json!({"derived_from_vault_path": path})),
                );
            }
        }
    }

    fn project_kind_definition(
        &self,
        graph: &mut KgSubgraph,
        subject: &str,
        path: &str,
        kind: &str,
        entry: &VaultEntry,
    ) {
        let definition = KindDefinition::from_stored(kind, entry);
        let spec = definition.spec();
        let problems = match &definition.status {
            KindStatus::Registered(_) => &[][..],
            KindStatus::Invalid(problems) => problems.as_slice(),
        };
        let mut node = KgNode::new(
            subject,
            C::Concept,
            spec.and_then(|spec| spec.title.as_deref()).unwrap_or(kind),
            "vault",
        )
        .with_metadata(serde_json::json!({
            "kind_definition": kind,
            "definition_status": if spec.is_some() { "registered" } else { "invalid" },
            "declared_fields": spec.map(|spec| spec.fields.keys().collect::<Vec<_>>()),
            "declared_edges": spec.map(KindSpec::edges),
            "embed": spec.map(|spec| spec.embed),
            "context": spec.map(|spec| spec.context),
            "problems": problems,
            "vault_path": path,
            "derived_from_vault_path": path,
            "created_at": entry.created_at,
            "updated_at": entry.updated_at,
        }));
        if let Some(title) = spec.and_then(|spec| spec.title.as_deref()) {
            node = node.with_summary(title);
        }
        graph.nodes.push(node);
        if spec.is_none() {
            self.finding(
                graph,
                subject,
                "KindDefinitionInvalid",
                &format!("Kind {kind} is not registered: {}", problems.join("; ")),
                path,
            );
        }
    }

    fn project_kind_instance(
        &self,
        graph: &mut KgSubgraph,
        subject: &str,
        path: &str,
        spec: &KindSpec,
        uid: &str,
        entry: &VaultEntry,
    ) {
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();
        let body = String::from_utf8_lossy(&entry.content_bytes);
        let report = validate_instance(spec, uid, &fm, &body, &ProjectionRefs(self));

        // Field values stay application data: the `field.` prefix keeps every
        // declared name off the keys the SHACL adapter and vault GC read.
        let mut metadata = serde_json::json!({
            "kind_name": spec.name,
            "uid": uid,
            "vault_path": path,
            "derived_from_vault_path": path,
            "created_at": entry.created_at,
            "updated_at": entry.updated_at,
        });
        for (name, field) in &spec.fields {
            if field.field_type.base == BaseType::Ref {
                continue;
            }
            let Some(value) = fm.get(name) else { continue };
            let projectable = match value {
                serde_json::Value::Array(items) => items.iter().all(is_scalar),
                other => is_scalar(other),
            };
            if projectable {
                metadata[format!("field.{name}")] = value.clone();
            }
        }
        let title = fm
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|title| !title.trim().is_empty());
        let mut node =
            KgNode::new(subject, C::Concept, title.unwrap_or(uid), "vault").with_metadata(metadata);
        if let Some(title) = title {
            node = node.with_summary(title);
        }

        let hub = iri::mint_concept(&self.project, &kind_hub_name(&spec.name));
        graph.nodes.push(KgNode::new(
            &hub,
            C::Concept,
            spec.name.as_str(),
            "vault_reference",
        ));
        graph.edges.push(
            KgEdge::new(subject, hub, P::References)
                .with_metadata(serde_json::json!({"edges": ["@kind"]})),
        );
        for resolved in &report.resolved_refs {
            let (Some(edge), Some((target, class))) =
                (&resolved.edge, self.subjects.get(&resolved.target_id))
            else {
                continue;
            };
            graph.nodes.push(KgNode::new(
                target,
                *class,
                target.as_str(),
                "vault_reference",
            ));
            graph.edges.push(
                KgEdge::new(subject, target, P::References).with_metadata(
                    serde_json::json!({"fields": [resolved.field], "edges": [edge]}),
                ),
            );
        }

        // File mentions stay out: a File stub has no owner, so it would outlive the
        // last row that mentions it until a rebuild, and a change of kind state
        // must leave the same graph a rebuild does.
        let scanned_from = graph.edges.len();
        self.scan_memory_links(graph, subject, &body, path);
        for edge in &mut graph.edges[scanned_from..] {
            if edge.from_id == subject {
                edge.metadata.get_or_insert_with(|| serde_json::json!({}))["edges"] =
                    serde_json::json!(["@body"]);
            }
        }

        if !report.conforms {
            let codes: BTreeSet<&str> = report.violations.iter().map(|v| v.code.as_str()).collect();
            let summary = codes.iter().copied().collect::<Vec<_>>().join(", ");
            self.finding(
                graph,
                subject,
                "KindViolation",
                &format!("{} {uid} does not conform: {summary}", spec.name),
                path,
            );
            if let Some(metadata) = graph
                .nodes
                .last_mut()
                .and_then(|finding| finding.metadata.as_mut())
                .and_then(serde_json::Value::as_object_mut)
            {
                metadata.insert("violation_codes".into(), serde_json::json!(codes));
            }
        }
        graph.nodes.push(node);
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
                // Implementation declarations are projected from the current
                // intent source only. A stale signed snapshot must not restore a
                // declaration removed from that source; its original bytes and
                // proof remain inspectable in the attestation object.
                if reference.edge == "declaresImplementation" {
                    continue;
                }
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

/// Resolves kind refs for projection: to the IRI of a subject this projection
/// will emit an owned node for, so a resolved ref never dangles.
///
/// `intent` and `memory` go through the same aliases as every other vault link.
/// A custom target is exact: only an instance of a registered kind is in
/// `subjects`, so a ref to a malformed instance, or to one whose spec is gone,
/// does not resolve.
struct ProjectionRefs<'a>(&'a VaultProjection);

impl RefResolver for ProjectionRefs<'_> {
    fn resolve(&self, target_kind: &str, raw: &str) -> Option<String> {
        let builtin = |class: C| {
            self.0
                .resolve(&format!("urn:atomic:{target_kind}:{raw}"))
                .filter(|(_, resolved)| *resolved == class)
                .map(|(id, _)| id)
        };
        match target_kind {
            "intent" => builtin(C::WorkIntent),
            "memory" => builtin(C::Memory),
            kind if is_valid_kind_name(kind) && is_valid_ulid(raw) => {
                let id = iri::mint_concept(&self.0.project, &kind_instance_name(kind, raw));
                self.0.subjects.contains_key(&id).then_some(id)
            }
            _ => None,
        }
    }
}

fn is_scalar(value: &serde_json::Value) -> bool {
    matches!(
        value,
        serde_json::Value::String(_) | serde_json::Value::Number(_) | serde_json::Value::Bool(_)
    )
}

/// One edge per `(from, to, predicate)`, the identity the KG stores them under,
/// with the `fields` and `edges` of every relation it stands for. Without this,
/// two fields on one target would overwrite each other in write order. Stamps
/// each edge's owner, as the built-in projections do.
fn merge_kind_edges(edges: Vec<KgEdge>, path: &str) -> Vec<KgEdge> {
    let mut merged =
        BTreeMap::<(String, String, P), serde_json::Map<String, serde_json::Value>>::new();
    for edge in edges {
        let key = (edge.from_id, edge.to_id, edge.kind);
        let metadata = merged.entry(key).or_default();
        let incoming = match edge.metadata {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        for (name, value) in incoming {
            if name == "fields" || name == "edges" {
                let mut labels = BTreeSet::new();
                for list in [metadata.get(&name), Some(&value)].into_iter().flatten() {
                    labels.extend(
                        list.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned),
                    );
                }
                metadata.insert(name, serde_json::json!(labels));
            } else {
                metadata.entry(name).or_insert(value);
            }
        }
        metadata.insert("derived_from_vault_path".into(), serde_json::json!(path));
    }
    merged
        .into_iter()
        .map(|((from, to, predicate), metadata)| {
            KgEdge::new(from, to, predicate).with_metadata(serde_json::Value::Object(metadata))
        })
        .collect()
}

/// One node per id, as storing all of them in order would leave it: the last the
/// row owns, or else the first stub, because a stub lands only while the id is
/// free. A body that repeats a link pushes a node each time, and every vault write
/// stores every row's nodes again, so copies would make one file tax every write.
fn merge_kind_nodes(nodes: Vec<KgNode>) -> Vec<KgNode> {
    let owned = |node: &KgNode| {
        node.metadata
            .as_ref()
            .is_some_and(|metadata| metadata.get("derived_from_vault_path").is_some())
    };
    let mut merged = BTreeMap::<String, KgNode>::new();
    for node in nodes {
        match merged.get(&node.id) {
            Some(_) if !owned(&node) => {}
            _ => {
                merged.insert(node.id.clone(), node);
            }
        }
    }
    merged.into_values().collect()
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
