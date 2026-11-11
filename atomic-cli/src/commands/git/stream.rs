//! Direct first-parent Git ODB -> in-memory graph compiler -> private batches.
//! No ordinary recorder, prepared-source repository, replay spool or checkout.

use super::*;
use atomic_core::change::InodeAttr;
use atomic_repository::{
    GitGraphImportSession, GitStreamLimits, GitStreamOutcome, PreparedGitGraphChange,
};

#[path = "rolling_dashboard.rs"]
mod rolling_dashboard;
#[path = "span_compiler.rs"]
mod span_compiler;

#[cfg(test)]
thread_local! { static FORBID_ORDINARY_RECORDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(super) fn assert_ordinary_recorder_allowed() {
    FORBID_ORDINARY_RECORDER.with(|flag| {
        assert!(
            !flag.get(),
            "direct Git stream called the ordinary recorder"
        )
    });
}
#[cfg(test)]
struct RecorderGuard(bool);
#[cfg(test)]
impl RecorderGuard {
    fn new() -> Self {
        Self(FORBID_ORDINARY_RECORDER.with(|flag| flag.replace(true)))
    }
}
#[cfg(test)]
impl Drop for RecorderGuard {
    fn drop(&mut self) {
        FORBID_ORDINARY_RECORDER.with(|flag| flag.set(self.0));
    }
}

fn input_budget(git: &GitRepository, commit: &git2::Commit<'_>, limit: usize) -> CliResult<()> {
    let bytes = input_blob_bytes(git, commit)?;
    if bytes > limit {
        return Err(git_error(format!("direct stream commit {} needs {bytes} input bytes, above {limit}; increase the explicit bound", commit.id())));
    }
    Ok(())
}

fn input_blob_bytes(git: &GitRepository, commit: &git2::Commit<'_>) -> CliResult<usize> {
    let tree = commit.tree().map_err(|e| git_error(e.to_string()))?;
    let parent = if commit.parent_count() == 0 {
        None
    } else {
        Some(
            commit
                .parent(0)
                .and_then(|parent| parent.tree())
                .map_err(|e| git_error(e.to_string()))?,
        )
    };
    let diff = git
        .diff_tree_to_tree(parent.as_ref(), Some(&tree), None)
        .map_err(|e| git_error(e.to_string()))?;
    let odb = git.odb().map_err(|e| git_error(e.to_string()))?;
    let mut bytes = 0usize;
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            // Gitlinks name commits in another repository. Their objects need
            // not exist here; only blob payloads consume this input budget.
            if file.id().is_zero() || file.mode() == git2::FileMode::Commit {
                continue;
            }
            let (size, kind) = odb
                .read_header(file.id())
                .map_err(|e| git_error(e.to_string()))?;
            if kind == ObjectType::Blob {
                bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| git_error("input size overflow"))?;
            }
        }
    }
    Ok(bytes)
}

#[derive(Default)]
struct Compiler {
    index: ImportLineIndex,
    attrs: HashMap<String, (u16, atomic_core::change::InodeKind, ContentHash)>,
    last_hash: Option<ContentHash>,
    spans: Option<span_compiler::SpanCompiler>,
}

impl Compiler {
    fn compile(
        &mut self,
        parsed: &ParsedCommit,
        policy: &ConversionPolicy,
        boundaries: &[&'static str],
        mut provenance: serde_json::Value,
    ) -> CliResult<PreparedGitGraphChange> {
        let mut lowered = parsed.clone();
        lowered.files.clear();
        let mut losses = Vec::new();
        for file in &parsed.files {
            let path =
                RepoPath::from_bytes(file.path.as_bytes()).map_err(|e| git_error(e.to_string()))?;
            if policy.exclusions.exclusion(&path).is_some() {
                continue;
            }
            if matches!(
                file.operation,
                FileOperation::Modified | FileOperation::Deleted | FileOperation::Renamed
            ) {
                let old_path = if file.operation == FileOperation::Renamed {
                    file.old_path
                        .as_deref()
                        .ok_or_else(|| git_error("rename missing old path"))?
                } else {
                    &file.path
                };
                let indexed = self.index.files.get(old_path).ok_or_else(|| {
                    git_error(format!("stream has no parent anchor for {old_path}"))
                })?;
                let old = file
                    .old_content
                    .as_deref()
                    .ok_or_else(|| git_error(format!("missing Git parent blob for {old_path}")))?;
                if !indexed
                    .lines
                    .iter()
                    .flat_map(|line| line.content.iter().copied())
                    .eq(old.iter().copied())
                {
                    return Err(git_error(format!("in-memory stream parent differs from Git at {old_path}; no recorder fallback")));
                }
            }
            if file.operation == FileOperation::Renamed {
                let old = file.old_path.as_ref().unwrap();
                let mut deleted = file.clone();
                deleted.path = old.clone();
                deleted.operation = FileOperation::Deleted;
                deleted.new_content = None;
                deleted.new_mode = None;
                deleted.old_path = None;
                let mut added = file.clone();
                added.operation = FileOperation::Added;
                added.old_content = None;
                added.old_path = None;
                lowered.files.extend([deleted, added]);
                losses.push(serde_json::json!({"from":old,"to":file.path,"basis":"Git similarity is not stable inode authority; delete+add"}));
            } else {
                lowered.files.push(file.clone());
            }
        }
        lowered.files.sort_by_key(|file| {
            if file.operation == FileOperation::Deleted {
                0
            } else {
                1
            }
        });
        let mut header = ChangeHeader::builder()
            .message(&parsed.metadata.message)
            .author(Author::new(
                &parsed.metadata.author_name,
                parsed.metadata.author_email.as_deref(),
            ))
            .timestamp(parsed.metadata.timestamp);
        if let Some(description) = &parsed.metadata.description {
            header = header.description(description);
        }
        let header = header.build();
        let no_byte_delta = lowered.files.iter().all(|file| {
            file.operation == FileOperation::Modified && file.old_content == file.new_content
        });
        let (change, pending, _) = if no_byte_delta {
            (Change::empty(header.clone()), Vec::new(), Vec::new())
        } else {
            build_graph_first_change(header.clone(), &lowered, &self.index, true).map_err(
                |skips| {
                    git_error(format!(
                        "graph-first compile refused {}: {}",
                        parsed.git_sha,
                        graph_first_skip_summary(&skips, &lowered)
                    ))
                },
            )?
        };
        let mut hunks = change.hunks().to_vec();
        let mut deps: HashSet<_> = change.dependencies().iter().copied().collect();
        // This producer is explicitly a first-parent projection. Its cache is
        // based on the complete previous interpretation, including deletions
        // behind still-alive successor anchors, not just visible line writers.
        if let Some(parent) = self.last_hash {
            deps.insert(parent);
        }
        let added: HashMap<_, _> = hunks
            .iter()
            .filter_map(|op| match op {
                GraphOp::FileAdd {
                    path, add_inode, ..
                } => Some((
                    atomic_repository::canonical_path_identity(path),
                    Position {
                        change: None,
                        pos: add_inode.start,
                    },
                )),
                _ => None,
            })
            .collect();
        let mut attr_updates = Vec::new();
        for file in &lowered.files {
            if file.operation == FileOperation::Deleted {
                continue;
            }
            let expected = expected_materialization(
                file.new_mode
                    .ok_or_else(|| git_error("missing stream Git mode"))?,
            )?;
            let previous = if added.contains_key(&file.path) {
                None
            } else {
                self.attrs.get(&file.path)
            };
            if previous
                .is_some_and(|(mode, kind, _)| *mode == expected.mode() && *kind == expected.kind())
            {
                continue;
            }
            let inode = added
                .get(&file.path)
                .copied()
                .or_else(|| self.index.files.get(&file.path).map(|file| file.inode_pos))
                .ok_or_else(|| git_error("missing stream attribute anchor"))?;
            deps.extend(position_hashes(&inode));
            if let Some((_, _, writer)) = previous {
                deps.insert(*writer);
            }
            hunks.push(GraphOp::SetAttr {
                inode,
                path: repository_path(&file.path).into_owned(),
                value: InodeAttr::Mode(expected.mode()),
            });
            hunks.push(GraphOp::SetAttr {
                inode,
                path: repository_path(&file.path).into_owned(),
                value: InodeAttr::Kind(expected.kind()),
            });
            attr_updates.push((file.path.clone(), expected.mode(), expected.kind()));
        }
        let mut surviving: HashSet<String> = self.index.files.keys().cloned().collect();
        for file in &lowered.files {
            if file.operation == FileOperation::Deleted {
                surviving.remove(&file.path);
            } else {
                surviving.insert(file.path.clone());
            }
        }
        let parents: HashSet<String> = surviving
            .iter()
            .flat_map(|path| ancestor_directories(path))
            .collect();
        let mut retired: Vec<_> = self
            .index
            .directories
            .keys()
            .filter(|path| !parents.contains(*path))
            .cloned()
            .collect();
        retired.sort_by(|a, b| {
            b.matches('/')
                .count()
                .cmp(&a.matches('/').count())
                .then_with(|| a.cmp(b))
        });
        for path in &retired {
            let inode = self.index.directories[path];
            let owner = inode
                .change
                .ok_or_else(|| git_error("unfinalized retired directory anchor"))?;
            let parent = extract_parent(path);
            let parent_pos = if parent.is_empty() {
                Position {
                    change: Some(ContentHash::NONE),
                    pos: ChangePosition::ROOT,
                }
            } else {
                self.index.directories[parent]
            };
            deps.extend(position_hashes(&parent_pos));
            deps.insert(owner);
            let start = ChangePosition::new(
                inode
                    .pos
                    .get()
                    .checked_sub(repository_path(extract_filename(path)).len() as u64)
                    .ok_or_else(|| git_error("invalid stream directory name span"))?,
            );
            let flags = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
            hunks.push(GraphOp::DirDel {
                path: repository_path(path).into_owned(),
                del: EdgeUpdate {
                    inode,
                    edges: vec![
                        NewEdge {
                            previous: flags,
                            flag: flags | EdgeFlags::DELETED,
                            from: parent_pos,
                            to: GraphNode {
                                change: Some(owner),
                                start,
                                end: inode.pos,
                            },
                            introduced_by: Some(owner),
                        },
                        NewEdge {
                            previous: flags,
                            flag: flags | EdgeFlags::DELETED,
                            from: Position {
                                change: Some(owner),
                                pos: start,
                            },
                            to: GraphNode {
                                change: Some(owner),
                                start: inode.pos,
                                end: inode.pos,
                            },
                            introduced_by: Some(owner),
                        },
                    ],
                },
            });
        }
        let mut deps: Vec<_> = deps.into_iter().collect();
        deps.sort();
        let span_lowering = self
            .spans
            .as_ref()
            .map(|spans| spans.lower(&pending, &mut hunks))
            .transpose()?;
        let mut compiled = Change::new(header, hunks, change.contents, deps);
        if let Some((ops, _)) = &span_lowering {
            compiled.hashed.span = Some(atomic_core::change::span_patch::SpanPatch::new(
                Vec::new(),
                ops.clone(),
            ));
        }
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let parents = parsed
            .parent_oids
            .iter()
            .map(|oid| git_object_id(algorithm, *oid))
            .collect::<CliResult<Vec<_>>>()?;
        let derivation = if parents.is_empty() {
            GitDerivation::Root
        } else if parents.len() > 1 {
            GitDerivation::MultiParent
        } else if parse_squash_merge_format(&parsed.full_message()).is_some() {
            GitDerivation::Squash
        } else if parsed.is_empty {
            GitDerivation::EmptyCommit
        } else {
            GitDerivation::FirstParent
        };
        let origin = GitSynthesisOrigin::with_derivation(
            git_object_id_for_sha(algorithm, &parsed.git_sha)?,
            git_object_id(algorithm, parsed.tree_oid)?,
            parents,
            derivation,
        )
        .map_err(CliError::from)?;
        provenance["git"]["stream"] = serde_json::json!({"semantics":"graph-only; derived, not native token provenance","history":"first-parent; side legs not separately imported","rename_identity_loss":losses});
        let unhashed = git_synthesis_metadata(&origin, boundaries, Some(provenance));
        let mut facts = foreign_commit_facts(parsed)?;
        if parsed.is_empty {
            let mut record: serde_json::Value =
                serde_json::from_slice(&facts).map_err(|error| git_error(error.to_string()))?;
            record["format"] = serde_json::json!(EMPTY_COMMIT_FACTS_FORMAT);
            record["tree_algorithm"] = record["algorithm"].clone();
            facts = serde_json::to_vec(&record).map_err(|error| git_error(error.to_string()))?;
        }
        let prepared = if span_lowering.is_some() {
            PreparedGitGraphChange::new_span(compiled, origin, facts, unhashed)
        } else {
            PreparedGitGraphChange::new(compiled, origin, facts, unhashed)
        }
        .map_err(CliError::from)?;
        let hash = prepared.hash();
        if let (Some(spans), Some((_, roots))) = (&mut self.spans, &span_lowering) {
            spans.finish(hash, &pending, roots)?;
        }
        self.last_hash = Some(hash);
        apply_line_index_updates(&mut self.index, hash, pending);
        for path in retired {
            self.index.directories.remove(&path);
        }
        for file in &lowered.files {
            if file.operation == FileOperation::Deleted {
                self.attrs.remove(&file.path);
            }
        }
        for (path, mode, kind) in attr_updates {
            self.attrs.insert(path, (mode, kind, hash));
        }
        for file in &lowered.files {
            if file.operation != FileOperation::Deleted {
                let cached = self
                    .index
                    .files
                    .get(&file.path)
                    .ok_or_else(|| git_error("compiler lost current file"))?;
                if !cached
                    .lines
                    .iter()
                    .flat_map(|line| line.content.iter().copied())
                    .eq(file
                        .new_content
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .copied())
                {
                    return Err(git_error(format!(
                        "stream cache update differs from Git at {}",
                        file.path
                    )));
                }
            }
        }
        Ok(prepared)
    }
}

impl ParallelImporter {
    /// Validate the restricted stream without retaining historical payloads.
    pub(crate) fn validate_graph_stream_prospectively(
        &self,
        branch: &str,
        tip: Oid,
        limits: GitStreamLimits,
    ) -> CliResult<ProspectiveImportPlan> {
        let (git, policy, ordered) = self.graph_stream_inputs(tip)?;
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut prospective = ProspectiveProjectTree::new(policy.clone(), None)?;
        let mut changed = std::collections::BTreeSet::new();
        let (mut merges, mut empty, mut files) = (0usize, 0usize, 0usize);
        let mut last = None;
        for (index, oid) in ordered.iter().copied().enumerate() {
            let commit = git
                .find_commit(oid)
                .map_err(|error| git_error(error.to_string()))?;
            input_budget(&git, &commit, limits.bytes())?;
            let parsed = parse_commit(&git, oid, index, &positions)?;
            for file in &parsed.files {
                changed.insert(file.path.clone());
                if let Some(old) = &file.old_path {
                    changed.insert(old.clone());
                }
            }
            merges += usize::from(parsed.is_merge);
            empty += usize::from(parsed.is_empty && !parsed.is_merge);
            files += parsed.files.len();
            let (project, verified) = prospective.apply_commit(&parsed, Some(&git))?;
            prospective
                .manifests
                .retain(|tree, _| *tree == parsed.tree_oid);
            last = Some((project, verified, parsed.tree_oid));
        }
        let (project, verified, raw_tree) =
            last.ok_or_else(|| git_error("direct stream needs a nonempty history"))?;
        self.verify_worktree_path_set(&project, changed.iter().map(String::as_str), branch, &git)?;
        Ok(ProspectiveImportPlan {
            commits: Vec::new(),
            boundaries: Vec::new(),
            stream: Some(StreamPreflight {
                tip,
                ordered,
                expected_tree: verified.expected_git_tree().clone(),
                raw_tree: git_object_id(policy.object_format, raw_tree)?,
                changed_paths: changed.into_iter().map(PathBuf::from).collect(),
                merges,
                empty,
                files,
            }),
        })
    }

    pub(crate) fn import_graph_stream_prevalidated(
        &self,
        repo: &mut Repository,
        plan: ProspectiveImportPlan,
        limits: GitStreamLimits,
    ) -> CliResult<ImportStats> {
        let stream = plan
            .stream
            .ok_or_else(|| git_error("missing compact stream preflight"))?;
        let total = stream.ordered.len();
        let (merges, empty, files) = (stream.merges, stream.empty, stream.files);
        let started = Instant::now();
        let outcome =
            self.import_graph_stream_rolling(repo, stream.tip, limits, &stream.ordered)?;
        let kind = if self.options.span_stream {
            "span"
        } else {
            "graph"
        };
        print_info(&format!("Direct {kind} stream: {} commits in {} private batches; final tip verified (historical states not independently verified)", outcome.commits, outcome.batches));
        Ok(ImportStats {
            commits_found: total,
            commits_parsed: total,
            changes_written: total - merges - empty,
            merge_commits: merges,
            empty_commits: empty,
            files_processed: files,
            phase2_duration: started.elapsed(),
            ..Default::default()
        })
    }

    /// Rebuild the compiler from the exact persisted prefix, then append only
    /// the unimported suffix through the journaled private span stage.
    /// This internal path does not inspect Git HEAD/index or the worktree; the
    /// command boundary must hold the common lock and prove those separately.
    pub(crate) fn import_span_incremental<F>(
        &self,
        repo: &mut Repository,
        tip: Oid,
        limits: GitStreamLimits,
        expected_order: &[Oid],
        mut validate_git: F,
    ) -> CliResult<GitStreamOutcome>
    where
        F: FnMut() -> CliResult<()>,
    {
        use atomic_core::pristine::{GraphTxnT, ViewTxnT};

        if !self.options.graph_only
            || !self.options.mainline_only
            || !self.options.span_stream
            || !self.options.imported_shas.is_empty()
            || self.options.target_view != repo.current_view()
        {
            return Err(git_error("incremental span stream requires an unfiltered first-parent replay into the current view"));
        }
        let (git, policy, ordered) = self.graph_stream_inputs(tip)?;
        if ordered != expected_order {
            return Err(git_error(
                "pinned Git stream history changed after preflight",
            ));
        }
        let (stage_hashes, target_members, target_state) = {
            let txn = repo
                .pristine()
                .read_txn()
                .map_err(|error| git_error(error.to_string()))?;
            let target = txn
                .get_view(repo.current_view())
                .map_err(|error| git_error(error.to_string()))?
                .ok_or_else(|| git_error("incremental span target view missing"))?;
            let stage = txn
                .get_view("git-stream-private")
                .map_err(|error| git_error(error.to_string()))?
                .ok_or_else(|| git_error("incremental span private stage missing"))?;
            let hashes = |view: &atomic_core::pristine::ViewState| -> CliResult<Vec<ContentHash>> {
                txn.iter_changes(view, 0)
                    .map_err(|error| git_error(error.to_string()))?
                    .map(|row| {
                        let (_, id, _) = row.map_err(|error| git_error(error.to_string()))?;
                        txn.get_external(id)
                            .map_err(|error| git_error(error.to_string()))?
                            .ok_or_else(|| git_error("stream member has no change hash"))
                    })
                    .collect()
            };
            (
                hashes(&stage)?,
                hashes(&target)?.into_iter().collect::<HashSet<_>>(),
                target.state,
            )
        };
        if stage_hashes.is_empty() || stage_hashes.len() > ordered.len() {
            return Err(git_error(
                "incremental span history is not an ancestor of the pinned Git tip",
            ));
        }
        let base_len = stage_hashes
            .iter()
            .take_while(|hash| target_members.contains(*hash))
            .count();
        if base_len == 0
            || stage_hashes[base_len..]
                .iter()
                .any(|hash| target_members.contains(hash))
        {
            return Err(git_error(
                "incremental span stage and target do not share a contiguous prefix",
            ));
        }
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut compiler = Compiler {
            spans: Some(Default::default()),
            ..Default::default()
        };
        let mut prefix = Vec::with_capacity(base_len);
        let mut staged = Vec::with_capacity(stage_hashes.len() - base_len);
        for (index, oid) in ordered.iter().copied().take(stage_hashes.len()).enumerate() {
            let commit = git
                .find_commit(oid)
                .map_err(|error| git_error(error.to_string()))?;
            input_budget(&git, &commit, limits.bytes())?;
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false)?;
            let metadata = self.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let prepared = compiler.compile(&parsed, &policy, &[], metadata)?;
            if prepared.hash() != stage_hashes[index] {
                return Err(git_error(format!(
                    "replayed Git commit {oid} differs from the persisted private stage"
                )));
            }
            if index < base_len {
                prefix.push((oid.to_string(), prepared.hash()));
            } else {
                staged.push((oid.to_string(), prepared.hash()));
            }
        }
        let baseline = import_expected_tree_oid(
            &git,
            git.find_commit(ordered[base_len - 1])
                .map_err(|error| git_error(error.to_string()))?
                .tree_id(),
            &policy,
        )?;
        validate_git()?;
        let mut session = if staged.is_empty() {
            GitGraphImportSession::begin_span_incremental(repo, limits, &prefix, &policy, &baseline)
        } else {
            GitGraphImportSession::resume_span_incremental(
                repo, limits, &prefix, &staged, &policy, &baseline,
            )
        }
        .map_err(CliError::from)?;
        if stage_hashes.len() == ordered.len() && staged.is_empty() {
            return Ok(GitStreamOutcome {
                commits: 0,
                batches: 0,
                state: target_state,
                private_publish_seconds: 0.0,
                final_proof_promotion_seconds: 0.0,
            });
        }
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        for (index, oid) in ordered.iter().copied().enumerate().skip(stage_hashes.len()) {
            let commit = git
                .find_commit(oid)
                .map_err(|error| git_error(error.to_string()))?;
            input_budget(&git, &commit, limits.bytes())?;
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false)?;
            let metadata = self.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let prepared = compiler.compile(&parsed, &policy, &[], metadata)?;
            if prepared.payload_bytes() > limits.bytes() {
                return Err(git_error(
                    "single compiled span change exceeds the stream budget",
                ));
            }
            if !batch.is_empty()
                && (batch.len() == limits.commits()
                    || prepared.payload_bytes() > limits.bytes() - bytes)
            {
                validate_git()?;
                session.publish(&batch).map_err(CliError::from)?;
                batch.clear();
                bytes = 0;
            }
            bytes += prepared.payload_bytes();
            batch.push(prepared);
        }
        if !batch.is_empty() {
            validate_git()?;
            session.publish(&batch).map_err(CliError::from)?;
        }
        finish_graph_stream_with_guard(session, &git, &policy, tip, || {
            validate_git().map_err(
                |error| atomic_repository::RepositoryError::InvalidOperation {
                    message: format!("Git observation changed before span promotion: {error}"),
                },
            )
        })
    }

    /// Refuse unless the handle's current view is already this importer's
    /// target. Incremental span import never selects or switches a view.
    pub(crate) fn require_target_is_current_view(&self, repo: &Repository) -> CliResult<()> {
        if self.options.target_view != repo.current_view() {
            return Err(git_error(
                "span incremental import requires the current Atomic view to match the Git branch",
            ));
        }
        Ok(())
    }

    pub(crate) fn import_span_incremental_prevalidated<F>(
        &self,
        repo: &mut Repository,
        plan: ProspectiveImportPlan,
        limits: GitStreamLimits,
        validate_git: F,
    ) -> CliResult<GitStreamOutcome>
    where
        F: FnMut() -> CliResult<()>,
    {
        let stream = plan
            .stream
            .ok_or_else(|| git_error("missing incremental span preflight"))?;
        self.import_span_incremental(repo, stream.tip, limits, &stream.ordered, validate_git)
    }

    fn graph_stream_inputs(
        &self,
        tip: Oid,
    ) -> CliResult<(GitRepository, ConversionPolicy, Vec<Oid>)> {
        let git = self.open_git_repo()?;
        if crate::commands::git::import::has_binding_transport(&git)? {
            return Err(git_error("direct stream does not restore Atomic binding transport; use --no-stream for the ordinary importer"));
        }
        if !detect_git_boundaries(&git)?.is_empty() {
            return Err(git_error(
                "direct stream refuses shallow/promisor/replacement/graft boundaries",
            ));
        }
        let policy = conversion_policy(&git)?;
        let ordered = self.collect_commit_oids_from_tip(&git, tip)?;
        verify_commit_closure_objects_with_shallow(&git, &ordered, &HashSet::new())?;
        for oid in &ordered {
            let commit = git
                .find_commit(*oid)
                .map_err(|e| git_error(e.to_string()))?;
            let message = String::from_utf8_lossy(commit.message_bytes());
            if parse_push_trailer(&message).is_some()
                || parse_atomic_changes_trailer(&message).is_some()
            {
                return Err(git_error("direct stream refuses Atomic provenance/binding claims; use --no-stream for the ordinary importer"));
            }
            let tree = commit.tree().map_err(|e| git_error(e.to_string()))?;
            // Share the ordinary importer's kind/mode validation. The parser
            // already carries raw Git names through the reversible escape;
            // there is no stream-specific character or UTF-8 restriction.
            validate_import_tree_capabilities(&git, &tree, "")?;
        }
        Ok((git, policy, ordered))
    }

    /// Read ahead at most 500 commits and publish up to 100 at a time. Both
    /// queued input blob reservations and prepared batches have explicit caps.
    fn import_graph_stream_rolling(
        &self,
        repo: &mut Repository,
        tip: Oid,
        limits: GitStreamLimits,
        expected_order: &[Oid],
    ) -> CliResult<GitStreamOutcome> {
        use std::collections::VecDeque;
        use std::io::IsTerminal;
        #[cfg(test)]
        let _recorder_guard = RecorderGuard::new();
        if !self.options.graph_only
            || !self.options.mainline_only
            || self.options.incremental
            || !self.options.imported_shas.is_empty()
            || self.options.target_view != repo.current_view()
        {
            return Err(git_error("direct stream requires graph-only fresh first-parent import into the current empty view"));
        }
        let (git, policy, ordered) = self.graph_stream_inputs(tip)?;
        if ordered != expected_order {
            return Err(git_error(
                "pinned Git stream history changed after preflight",
            ));
        }
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut session = if self.options.span_stream {
            GitGraphImportSession::begin_span(repo, limits)
        } else {
            GitGraphImportSession::begin(repo, limits)
        }
        .map_err(CliError::from)?;
        let enabled = std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_some_and(|term| term != "dumb");
        let mut dashboard = rolling_dashboard::Dashboard::start(500, limits.bytes(), enabled);
        dashboard.update(|state| {
            state.total = ordered.len();
            state.checked = ordered.len();
        });
        let mut compiler = Compiler::default();
        if self.options.span_stream {
            compiler.spans = Some(Default::default());
        }
        let mut pending = VecDeque::new();
        let (mut next, mut processed, mut pending_bytes) = (0usize, 0usize, 0usize);
        let started = Instant::now();
        while processed < ordered.len() {
            dashboard.update(|state| state.phase = "loading window");
            while pending.len() < 500 && next < ordered.len() {
                let oid = ordered[next];
                let commit = git
                    .find_commit(oid)
                    .map_err(|error| git_error(error.to_string()))?;
                let reserved = input_blob_bytes(&git, &commit)?;
                if reserved > limits.bytes() {
                    return Err(git_error(format!("commit {oid} exceeds the stream input-blob budget; raise --stream-window-mib")));
                }
                if reserved > limits.bytes() - pending_bytes {
                    break;
                }
                let parsed = parse_commit_with_scope(&git, oid, next, &positions, false)?;
                pending.push_back((parsed, reserved));
                pending_bytes += reserved;
                next += 1;
                dashboard.update(|state| {
                    state.loaded = next;
                    state.queued = pending.len();
                    state.reserved_bytes = pending_bytes;
                });
            }
            let mut batch = Vec::new();
            let (mut bytes, mut flushed) = (0usize, 0usize);
            for _ in 0..limits.commits().min(pending.len()) {
                let (parsed, reserved) = pending.pop_front().expect("nonempty rolling window");
                pending_bytes -= reserved;
                dashboard.update(|state| {
                    state.phase = "compiling";
                    state.queued = pending.len();
                    state.reserved_bytes = pending_bytes;
                    state.in_flight = next - pending.len() - processed - flushed;
                });
                let metadata = self.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
                let prepared = compiler.compile(&parsed, &policy, &[], metadata)?;
                if prepared.payload_bytes() > limits.bytes() {
                    return Err(git_error("single compiled stream change exceeds the stream budget; raise --stream-window-mib"));
                }
                if !batch.is_empty() && prepared.payload_bytes() > limits.bytes() - bytes {
                    dashboard.update(|state| state.phase = "publishing batch");
                    session.publish(&batch).map_err(CliError::from)?;
                    flushed += batch.len();
                    dashboard.update(|state| {
                        state.published = processed + flushed;
                        state.in_flight = next - pending.len() - state.published;
                    });
                    batch.clear();
                    bytes = 0;
                }
                bytes += prepared.payload_bytes();
                batch.push(prepared);
            }
            if !batch.is_empty() {
                dashboard.update(|state| state.phase = "publishing batch");
                session.publish(&batch).map_err(CliError::from)?;
            }
            processed = next - pending.len();
            dashboard.update(|state| {
                state.published = processed;
                state.in_flight = 0;
            });
            if !dashboard.enabled() {
                print_info(&format!(
                    "Direct stream staged {processed}/{} commits privately ({:.2}s)",
                    ordered.len(),
                    started.elapsed().as_secs_f64()
                ));
            }
        }
        dashboard.update(|state| state.phase = "final proof / promote");
        let outcome = finish_graph_stream(session, &git, &policy, tip)?;
        dashboard.finish();
        Ok(outcome)
    }
    /// Experimental direct graph-only import. No fallback to working-copy
    /// recording; caller controls the optional final checkout after success.
    pub(crate) fn import_graph_stream(
        &self,
        repo: &mut Repository,
        tip: Oid,
        limits: GitStreamLimits,
    ) -> CliResult<GitStreamOutcome> {
        #[cfg(test)]
        let _recorder_guard = RecorderGuard::new();
        if !self.options.graph_only
            || !self.options.mainline_only
            || self.options.incremental
            || !self.options.imported_shas.is_empty()
            || self.options.target_view != repo.current_view()
        {
            return Err(git_error("direct stream initially requires graph-only fresh first-parent import into the current empty view; eager CRDT/incremental/all-parent modes are unsupported"));
        }
        let (git, policy, ordered) = self.graph_stream_inputs(tip)?;
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut session = if self.options.span_stream {
            GitGraphImportSession::begin_span(repo, limits)
        } else {
            GitGraphImportSession::begin(repo, limits)
        }
        .map_err(CliError::from)?;
        let mut compiler = Compiler::default();
        if self.options.span_stream {
            compiler.spans = Some(Default::default());
        }
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        let mut staged = 0usize;
        let started = Instant::now();
        for (index, oid) in ordered.iter().copied().enumerate() {
            let commit = git.find_commit(oid).map_err(|e| git_error(e.to_string()))?;
            input_budget(&git, &commit, limits.bytes())?;
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false)?;
            let metadata = self.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let prepared = compiler.compile(&parsed, &policy, &[], metadata)?;
            if prepared.payload_bytes() > limits.bytes() {
                return Err(git_error(
                    "single compiled stream change exceeds byte bound",
                ));
            }
            if !batch.is_empty()
                && (batch.len() == limits.commits()
                    || bytes + prepared.payload_bytes() > limits.bytes())
            {
                session.publish(&batch).map_err(CliError::from)?;
                staged += batch.len();
                print_info(&format!(
                    "Direct stream staged {staged}/{} commits privately ({:.2}s)",
                    ordered.len(),
                    started.elapsed().as_secs_f64()
                ));
                batch.clear();
                bytes = 0;
            }
            bytes += prepared.payload_bytes();
            batch.push(prepared);
        }
        if !batch.is_empty() {
            session.publish(&batch).map_err(CliError::from)?;
            staged += batch.len();
            print_info(&format!(
                "Direct stream staged {staged}/{} commits privately ({:.2}s)",
                ordered.len(),
                started.elapsed().as_secs_f64()
            ));
        }
        finish_graph_stream(session, &git, &policy, tip)
    }
}

fn finish_graph_stream(
    session: GitGraphImportSession<'_>,
    git: &GitRepository,
    policy: &ConversionPolicy,
    tip: Oid,
) -> CliResult<GitStreamOutcome> {
    finish_graph_stream_with_guard(session, git, policy, tip, || Ok(()))
}

fn finish_graph_stream_with_guard<F>(
    session: GitGraphImportSession<'_>,
    git: &GitRepository,
    policy: &ConversionPolicy,
    tip: Oid,
    guard: F,
) -> CliResult<GitStreamOutcome>
where
    F: FnOnce() -> Result<(), atomic_repository::RepositoryError>,
{
    let tree = git
        .find_commit(tip)
        .and_then(|commit| commit.tree())
        .map_err(|e| git_error(e.to_string()))?;
    let entries = repository_entries_from_git_tree(git, tree.clone(), policy)?;
    let manifest = RepositoryManifest::new(
        SetId::ZERO,
        policy.root().content_key,
        entries.into_values().collect(),
    )
    .map_err(|error| git_error(error.to_string()))?;
    let project = ProjectTree::from_manifest(manifest, policy)
        .map_err(|error| git_error(error.to_string()))?;
    let expected = import_expected_tree_oid(git, tree.id(), policy)?;
    let proof = verify_prospective_equivalence(&project, &expected)
        .map_err(|error| git_error(error.to_string()))?;
    session
        .finish_with_guard(&project, &proof, guard)
        .map_err(CliError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Optional process-level samples for the manual rolling benchmark. `ps`
    /// reads current RSS; getrusage records the process-lifetime high-water
    /// mark, including peaks that occur between our explicit samples.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn rolling_memory_sample(
        phase: &str,
        processed: usize,
        loaded: usize,
        queued: usize,
        reserved: usize,
    ) -> CliResult<()> {
        let Some(path) = std::env::var_os("ATOMIC_ROLLING_MEMORY_LOG") else {
            return Ok(());
        };
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .map_err(|e| git_error(format!("cannot sample RSS: {e}")))?;
        if !output.status.success() {
            return Err(git_error("ps failed to sample rolling process RSS"));
        }
        let rss_kib: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|e| git_error(format!("invalid ps RSS: {e}")))?;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the provided rusage on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return Err(git_error(format!(
                "getrusage failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: the successful call above initialized this value.
        let peak_raw = unsafe { usage.assume_init() }.ru_maxrss as u64;
        #[cfg(target_os = "macos")]
        let peak_rss_bytes = peak_raw;
        #[cfg(target_os = "linux")]
        let peak_rss_bytes = peak_raw * 1024;
        let sample = serde_json::json!({
            "phase": phase, "processed": processed, "loaded": loaded,
            "queued": queued, "reserved_blob_bytes": reserved,
            "rss_bytes": rss_kib * 1024, "peak_rss_bytes": peak_rss_bytes,
        });
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| git_error(format!("cannot open memory log: {e}")))?;
        writeln!(file, "{sample}")
            .map_err(|e| git_error(format!("cannot write memory log: {e}")))?;
        if std::env::var_os("ATOMIC_ROLLING_DASHBOARD").is_none() {
            println!("ROLLING MEMORY {sample}");
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn rolling_memory_sample(
        _phase: &str,
        _processed: usize,
        _loaded: usize,
        _queued: usize,
        _reserved: usize,
    ) -> CliResult<()> {
        if std::env::var_os("ATOMIC_ROLLING_MEMORY_LOG").is_some() {
            return Err(git_error("rolling memory samples require macOS or Linux"));
        }
        Ok(())
    }

    /// Test-only prototype: preflight one commit at a time, then keep a
    /// count- and input-blob-budgeted window feeding the existing private publisher.
    /// It deliberately does not replace the CLI's checkout/adoption preflight.
    fn rolling_sample(
        importer: &ParallelImporter,
        repo: &mut Repository,
        tip: Oid,
        window_size: usize,
        batch_size: usize,
        window_blob_limit: usize,
    ) -> CliResult<(GitStreamOutcome, usize, usize, usize, usize, f64)> {
        assert!(window_size > 0 && batch_size > 0 && batch_size <= window_size);
        let _recorder_guard = RecorderGuard::new();
        let mut dashboard = rolling_dashboard::Dashboard::start(
            window_size,
            window_blob_limit,
            std::env::var_os("ATOMIC_ROLLING_DASHBOARD").is_some(),
        );
        rolling_memory_sample("before_preflight", 0, 0, 0, 0)?;
        let preflight_start = Instant::now();
        let (git, policy, ordered) = importer.graph_stream_inputs(tip)?;
        dashboard.update(|state| state.total = ordered.len());
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let limits = GitStreamLimits::new(batch_size, 32 * 1024 * 1024).map_err(CliError::from)?;

        // Retain only the immediately preceding prospective project. For a
        // different parent, apply_commit reloads its tree from the ODB.
        let mut prospective = ProspectiveProjectTree::new(policy.clone(), None)?;
        for (index, oid) in ordered.iter().copied().enumerate() {
            let commit = git.find_commit(oid).map_err(|e| git_error(e.to_string()))?;
            input_budget(&git, &commit, limits.bytes())?;
            if input_blob_bytes(&git, &commit)? > window_blob_limit {
                return Err(git_error(format!(
                    "commit {oid} exceeds rolling window input-blob budget"
                )));
            }
            let parsed = parse_commit(&git, oid, index, &positions)?;
            prospective.apply_commit(&parsed, Some(&git))?;
            prospective
                .manifests
                .retain(|tree, _| *tree == parsed.tree_oid);
            dashboard.update(|state| state.checked = index + 1);
        }
        drop(prospective);
        let preflight_seconds = preflight_start.elapsed().as_secs_f64();
        rolling_memory_sample("after_preflight", 0, 0, 0, 0)?;

        let mut session = GitGraphImportSession::begin(repo, limits).map_err(CliError::from)?;
        let mut compiler = Compiler::default();
        let mut pending = VecDeque::new();
        let mut next = 0usize;
        let mut processed = 0usize;
        let mut advances = 0usize;
        let mut peak = 0usize;
        let mut pending_bytes = 0usize;
        let mut peak_bytes = 0usize;
        let mut budget_stalls = 0usize;
        while processed < ordered.len() {
            dashboard.update(|state| state.phase = "loading window");
            let start = next;
            while pending.len() < window_size && next < ordered.len() {
                let oid = ordered[next];
                let commit = git.find_commit(oid).map_err(|e| git_error(e.to_string()))?;
                let reserved = input_blob_bytes(&git, &commit)?;
                if reserved > window_blob_limit {
                    return Err(git_error(format!(
                        "commit {oid} exceeds rolling window input-blob budget"
                    )));
                }
                if reserved > window_blob_limit - pending_bytes {
                    budget_stalls += 1;
                    break;
                }
                let parsed = parse_commit_with_scope(&git, oid, next, &positions, false)?;
                pending.push_back((parsed, reserved));
                pending_bytes += reserved;
                next += 1;
                dashboard.update(|state| {
                    state.loaded = next;
                    state.queued = pending.len();
                    state.reserved_bytes = pending_bytes;
                });
            }
            if start > 0 && next > start {
                advances += 1;
                if !dashboard.enabled() {
                    println!("ROLLING refill {start}..{next} after {processed} published");
                }
            }
            peak = peak.max(pending.len());
            peak_bytes = peak_bytes.max(pending_bytes);
            if next > start {
                rolling_memory_sample(
                    "after_refill",
                    processed,
                    next,
                    pending.len(),
                    pending_bytes,
                )?;
            }

            let mut batch = Vec::new();
            let mut bytes = 0usize;
            let mut flushed = 0usize;
            for _ in 0..batch_size.min(pending.len()) {
                let (parsed, reserved) = pending.pop_front().expect("window was filled");
                pending_bytes -= reserved;
                dashboard.update(|state| {
                    state.phase = "compiling";
                    state.queued = pending.len();
                    state.reserved_bytes = pending_bytes;
                    state.in_flight = next - pending.len() - processed - flushed;
                });
                let metadata =
                    importer.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
                let prepared = compiler.compile(&parsed, &policy, &[], metadata)?;
                if prepared.payload_bytes() > limits.bytes() {
                    return Err(git_error(
                        "single compiled stream change exceeds byte bound",
                    ));
                }
                if !batch.is_empty() && bytes + prepared.payload_bytes() > limits.bytes() {
                    dashboard.update(|state| state.phase = "publishing batch");
                    session.publish(&batch).map_err(CliError::from)?;
                    flushed += batch.len();
                    dashboard.update(|state| {
                        state.published = processed + flushed;
                        state.in_flight = next - pending.len() - state.published;
                    });
                    batch.clear();
                    bytes = 0;
                }
                bytes += prepared.payload_bytes();
                batch.push(prepared);
            }
            // If the byte cap flushed part of this group, the number processed
            // is the number removed from the window, not the last sub-batch.
            if !batch.is_empty() {
                dashboard.update(|state| state.phase = "publishing batch");
                session.publish(&batch).map_err(CliError::from)?;
            }
            drop(batch);
            processed = next - pending.len();
            dashboard.update(|state| {
                state.published = processed;
                state.in_flight = 0;
            });
            rolling_memory_sample(
                "after_publish",
                processed,
                next,
                pending.len(),
                pending_bytes,
            )?;
        }

        dashboard.update(|state| state.phase = "final proof / promote");
        let tree = git
            .find_commit(tip)
            .and_then(|commit| commit.tree())
            .map_err(|e| git_error(e.to_string()))?;
        let entries = repository_entries_from_git_tree(&git, tree.clone(), &policy)?;
        let manifest = RepositoryManifest::new(
            SetId::ZERO,
            policy.root().content_key,
            entries.into_values().collect(),
        )
        .map_err(|error| git_error(error.to_string()))?;
        let project = ProjectTree::from_manifest(manifest, &policy)
            .map_err(|error| git_error(error.to_string()))?;
        let expected = import_expected_tree_oid(&git, tree.id(), &policy)?;
        let proof = verify_prospective_equivalence(&project, &expected)
            .map_err(|error| git_error(error.to_string()))?;
        rolling_memory_sample("before_final_proof", processed, next, 0, 0)?;
        let outcome = session.finish(&project, &proof).map_err(CliError::from)?;
        rolling_memory_sample("after_final_proof", processed, next, 0, 0)?;
        dashboard.finish();
        Ok((
            outcome,
            advances,
            peak,
            peak_bytes,
            budget_stalls,
            preflight_seconds,
        ))
    }

    // Construct Git objects directly: the host filesystem must not normalize
    // Unicode names or reject raw bytes before the importer can see them.
    type RawFiles = BTreeMap<Vec<u8>, (Vec<u8>, i32)>;

    fn raw_tree(git: &GitRepository, files: &RawFiles, prefix: &[u8]) -> Oid {
        let mut builder = git.treebuilder(None).unwrap();
        let mut dirs = std::collections::BTreeSet::new();
        for (path, (bytes, mode)) in files {
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            if let Some(slash) = rest.iter().position(|byte| *byte == b'/') {
                dirs.insert(&rest[..slash]);
            } else {
                let oid = if *mode == 0o160000 {
                    Oid::from_str(std::str::from_utf8(bytes).unwrap()).unwrap()
                } else {
                    git.blob(bytes).unwrap()
                };
                builder.insert(rest, oid, *mode).unwrap();
            }
        }
        for dir in dirs {
            let mut child = prefix.to_vec();
            child.extend_from_slice(dir);
            child.push(b'/');
            builder
                .insert(dir, raw_tree(git, files, &child), 0o040000)
                .unwrap();
        }
        builder.write().unwrap()
    }

    fn raw_commit(git: &GitRepository, files: &RawFiles, parent: Option<Oid>) -> Oid {
        let tree = git.find_tree(raw_tree(git, files, b"")).unwrap();
        let signature = git2::Signature::new(
            "Stream paths",
            "paths@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let parent = parent.map(|oid| git.find_commit(oid).unwrap());
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "raw tree names and kinds",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn assert_raw_history(git: &GitRepository, tips: &[Oid]) {
        let policy = conversion_policy(git).unwrap();
        for tip in tips {
            let mut state = None;
            for (batch, rolling) in [(1, false), (100, false), (100, true)] {
                let output = tempfile::tempdir().unwrap();
                let mut repo = Repository::init(output.path()).unwrap();
                let importer = ParallelImporter::new(
                    git,
                    ParallelImportOptions {
                        graph_only: true,
                        target_view: repo.current_view().into(),
                        ..Default::default()
                    },
                );
                let limits = GitStreamLimits::new(batch, 32 * 1024 * 1024).unwrap();
                let outcome = if rolling {
                    let (_, _, ordered) = importer.graph_stream_inputs(*tip).unwrap();
                    importer.import_graph_stream_rolling(&mut repo, *tip, limits, &ordered)
                } else {
                    importer.import_graph_stream(&mut repo, *tip, limits)
                }
                .unwrap_or_else(|error| {
                    panic!("tip {tip}, batch {batch}, rolling {rolling}: {error}")
                });
                for entry in repo.effective_history(None).unwrap() {
                    assert!(repo.load_change(&entry.hash).unwrap().file_ops().is_empty());
                }
                if let Some(state) = state {
                    assert_eq!(outcome.state, state, "batch size changed identity at {tip}");
                } else {
                    state = Some(outcome.state);
                }
                let source = git.find_commit(*tip).unwrap().tree_id();
                let projection = repo.project_tree(repo.current_view(), &policy);
                if policy.platform.lossless_unix_paths {
                    let actual = projection.unwrap();
                    assert_eq!(
                        actual.git.root.as_bytes(),
                        source.as_bytes(),
                        "raw Git tree bytes must survive at {tip}"
                    );
                } else {
                    // The import itself verifies the staged graph against the
                    // source tree before promotion. The separate graph-to-Git
                    // projection API deliberately refuses this host policy.
                    assert!(matches!(
                        projection,
                        Err(atomic_repository::repository::ProjectTreeError::UnsupportedPlatformPath)
                    ));
                }
                // Exercise the Windows policy contract on Unix too, without
                // claiming the host can project arbitrary Git paths natively.
                let mut unsupported_policy = policy.clone();
                unsupported_policy.platform.lossless_unix_paths = false;
                assert!(matches!(
                    repo.project_tree(repo.current_view(), &unsupported_policy),
                    Err(atomic_repository::repository::ProjectTreeError::UnsupportedPlatformPath)
                ));
                assert_eq!(fs::read_dir(output.path()).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn direct_stream_raw_tree_names_round_trip_across_edits_and_retired_directories() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let mut files = RawFiles::new();
        for name in [
            b"test-images/lot 5.png".as_slice(),
            "caf\u{e9}.txt".as_bytes(),
            "cafe\u{301}.txt".as_bytes(),
            "d\u{ed}r/child.txt".as_bytes(),
            b"d\xffr/f\xfele.txt",
            b"aA.txt",
            b"a%41.txt",
            b"a%2541.txt",
            b"%2Fdir/%FFliteral",
            b"back\\slash.txt",
            b"tabs\tand\nnewlines.txt",
        ] {
            files.insert(name.to_vec(), ([name, b"\n\0\xff\n"].concat(), 0o100644));
        }
        let all_name_bytes: Vec<_> = (1..=255).filter(|byte| *byte != b'/').collect();
        files.insert(
            all_name_bytes,
            (b"all legal component bytes\n".to_vec(), 0o100644),
        );
        let root = raw_commit(&git, &files, None);
        files
            .get_mut(b"test-images/lot 5.png".as_slice())
            .unwrap()
            .0 = vec![0, 2, 255];
        let renamed = files.remove(b"d\xffr/f\xfele.txt".as_slice()).unwrap();
        files.insert(b"moved \xfe/renamed %FF.txt".to_vec(), renamed);
        files.remove(b"%2Fdir/%FFliteral".as_slice()).unwrap();
        files.remove("d\u{ed}r/child.txt".as_bytes()).unwrap();
        let edit = raw_commit(&git, &files, Some(root));
        files.insert(
            b"d\xffr/recreated file".to_vec(),
            (b"again\n".to_vec(), 0o100755),
        );
        files.insert(
            "d\u{ed}r/recreated.txt".as_bytes().to_vec(),
            (b"unicode directory\n".to_vec(), 0o100755),
        );
        let tip = raw_commit(&git, &files, Some(edit));
        assert_raw_history(&git, &[root, edit, tip]);
    }

    #[test]
    fn streamed_directory_markers_do_not_alias_unrelated_file_names() {
        for directory in ["dir", "d\u{ed}r with space"] {
            let input = tempfile::tempdir().unwrap();
            let git = GitRepository::init_bare(input.path()).unwrap();
            let files = RawFiles::from([
                (b"a.txt".to_vec(), (b"root\n".to_vec(), 0o100644)),
                (
                    format!("{directory}/child.txt").into_bytes(),
                    (b"child\n".to_vec(), 0o100644),
                ),
            ]);
            let tip = raw_commit(&git, &files, None);
            let parsed = parse_commit(&git, tip, 0, &HashMap::from([(tip, 0)])).unwrap();
            let mut compiler = Compiler::default();
            compiler
                .compile(
                    &parsed,
                    &conversion_policy(&git).unwrap(),
                    &[],
                    serde_json::json!({"git": {}}),
                )
                .unwrap();
            let marker = compiler.index.directories
                [&atomic_repository::escape_repo_path(directory.as_bytes())]
                .pos
                .get();
            for (path, indexed) in &compiler.index.files {
                let name_start = indexed.inode_pos.pos.get()
                    - repository_path(extract_filename(path)).len() as u64;
                assert_ne!(
                    name_start, marker,
                    "a later move must not confuse its file name with the destination directory"
                );
            }
        }
    }

    #[test]
    fn direct_stream_symlinks_round_trip_targets_and_typechanges() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let mut files = RawFiles::from([
            (b"CLAUDE.md".to_vec(), (b"AGENTS.md".to_vec(), 0o120000)),
            (
                b"AGENTS.md".to_vec(),
                (b"instructions\n".to_vec(), 0o100644),
            ),
            (
                b"dangling link".to_vec(),
                (b"../missing\xff".to_vec(), 0o120000),
            ),
            (b"flip".to_vec(), (b"AGENTS.md".to_vec(), 0o100755)),
        ]);
        let root = raw_commit(&git, &files, None);
        // Same bytes, different kind: this must still emit attribute updates.
        files.get_mut(b"flip".as_slice()).unwrap().1 = 0o120000;
        files.get_mut(b"CLAUDE.md".as_slice()).unwrap().0 = b"missing".to_vec();
        let edit = raw_commit(&git, &files, Some(root));
        files.get_mut(b"flip".as_slice()).unwrap().1 = 0o100644;
        files.remove(b"dangling link".as_slice()).unwrap();
        let tip = raw_commit(&git, &files, Some(edit));
        assert_raw_history(&git, &[root, edit, tip]);
    }

    #[test]
    fn direct_stream_gitlinks_do_not_require_submodule_objects() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let mut files = RawFiles::from([(
            b"vendor/sub module".to_vec(),
            (
                b"1111111111111111111111111111111111111111".to_vec(),
                0o160000,
            ),
        )]);
        let root = raw_commit(&git, &files, None);
        files.get_mut(b"vendor/sub module".as_slice()).unwrap().0 =
            b"2222222222222222222222222222222222222222".to_vec();
        let edit = raw_commit(&git, &files, Some(root));
        let target = files.remove(b"vendor/sub module".as_slice()).unwrap();
        files.insert(b"moved %FF/sub module".to_vec(), target);
        let rename = raw_commit(&git, &files, Some(edit));
        // Gitlinks may also become ordinary files (and vice versa) without
        // interpreting either the old or the new bytes as a local commit.
        files.get_mut(b"moved %FF/sub module".as_slice()).unwrap().1 = 0o100644;
        let regular = raw_commit(&git, &files, Some(rename));
        files.get_mut(b"moved %FF/sub module".as_slice()).unwrap().1 = 0o160000;
        let gitlink = raw_commit(&git, &files, Some(regular));
        files.clear();
        let tip = raw_commit(&git, &files, Some(gitlink));
        assert_raw_history(&git, &[root, edit, rename, regular, gitlink, tip]);
    }

    #[test]
    fn direct_stream_explicit_budget_accepts_commits_above_32_mib() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let size = 33 * 1024 * 1024;
        let files = RawFiles::from([(b"large.bin".to_vec(), (vec![0; size], 0o100644))]);
        let tip = raw_commit(&git, &files, None);
        drop(files);
        let commit = git.find_commit(tip).unwrap();
        assert!(input_budget(&git, &commit, 32 * 1024 * 1024).is_err());

        let output = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(output.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let outcome = importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(100, 128 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        assert_eq!(outcome.commits, 1);
        let content = repo.get_file_content("large.bin").unwrap().unwrap();
        assert_eq!(content.len(), size);
        assert!(content.iter().all(|byte| *byte == 0));
    }

    type Files = BTreeMap<String, (Vec<u8>, i32)>;
    fn tree(git: &GitRepository, files: &Files, prefix: &str) -> Oid {
        let mut builder = git.treebuilder(None).unwrap();
        let mut dirs = std::collections::BTreeSet::new();
        for (path, (bytes, mode)) in files {
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            if let Some((dir, _)) = rest.split_once('/') {
                dirs.insert(dir);
            } else {
                builder
                    .insert(rest, git.blob(bytes).unwrap(), *mode)
                    .unwrap();
            }
        }
        for dir in dirs {
            builder
                .insert(dir, tree(git, files, &format!("{prefix}{dir}/")), 0o040000)
                .unwrap();
        }
        builder.write().unwrap()
    }
    fn fixture(count: usize) -> (tempfile::TempDir, GitRepository, Oid, Files) {
        let temp = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(temp.path()).unwrap();
        let mut files = Files::new();
        let mut previous = None;
        for index in 0..count {
            let put = |files: &mut Files, path: &str, bytes: &[u8], mode| {
                files.insert(path.into(), (bytes.to_vec(), mode));
            };
            match index % 16 {
                0 => {
                    put(&mut files, "hot.txt", b"alpha\nbeta\nlast\n", 0o100644);
                    put(&mut files, "stable.txt", b"stable\n", 0o100644);
                }
                1 => {
                    put(&mut files, "hot.txt", b"alpha\nchanged\nlast\n", 0o100644);
                }
                2 => {
                    put(&mut files, "nested/deep/empty", b"", 0o100644);
                }
                3 => {
                    put(&mut files, "blob.bin", &[0, 1, 2, 255], 0o100644);
                }
                4 => {
                    files.get_mut("hot.txt").unwrap().1 = 0o100755;
                }
                5 => {
                    let file = files.remove("hot.txt").unwrap();
                    files.insert("moved.txt".into(), file);
                }
                6 => {
                    put(&mut files, "moved.txt", b"alpha\nlast\n", 0o100755);
                }
                7 => {
                    files.remove("nested/deep/empty").unwrap();
                }
                8 => {
                    put(&mut files, "nested/deep/recreated", b"again\n", 0o100644);
                }
                9 => {
                    put(&mut files, "blob.bin", &[0, 9, 8, 7], 0o100644);
                }
                10 => {
                    put(&mut files, "moved.txt", b"", 0o100755);
                }
                11 => {
                    files.get_mut("moved.txt").unwrap().1 = 0o100644;
                }
                12 => {
                    files.remove("blob.bin").unwrap();
                }
                13 => {
                    files.remove("moved.txt").unwrap();
                    put(&mut files, "hot.txt", b"alpha\nrestored\nlast\n", 0o100644);
                }
                14 => {
                    files.remove("nested/deep/recreated").unwrap();
                }
                15 => {}
                _ => unreachable!(),
            }
            let tree = git.find_tree(tree(&git, &files, "")).unwrap();
            let signature = git2::Signature::new(
                "Direct Stream",
                "stream@example.invalid",
                &git2::Time::new(1_700_000_000 + index as i64, 0),
            )
            .unwrap();
            let parent = previous.map(|oid| git.find_commit(oid).unwrap());
            previous = Some(
                git.commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    &format!("mixed {index}"),
                    &tree,
                    &parent.iter().collect::<Vec<_>>(),
                )
                .unwrap(),
            );
        }
        (temp, git, previous.unwrap(), files)
    }

    fn assert_imported_authored_ranges(
        repo: &Repository,
        expected: &[u8],
        minimum_owners: usize,
    ) -> atomic_core::pristine::span_patch_storage::SpanProjectionId {
        use atomic_core::pristine::span_patch_storage::SpanRenderContext;
        use atomic_core::pristine::{GraphTxnT, TreeTxnT, ViewTxnT};
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view(repo.current_view()).unwrap().unwrap();
        let visibility = atomic_repository::graph_visibility_closure(&txn, &view).unwrap();
        let inode = txn.get_inode("hot.txt").unwrap().unwrap();
        let context = SpanRenderContext::new(&txn, repo.change_store(), &visibility).unwrap();
        let state = context.record_state(inode).unwrap().unwrap();
        assert_eq!(state.content, expected);
        let mut reconstructed = Vec::new();
        let mut owners = std::collections::BTreeSet::new();
        for range in state.authored_ranges(0..state.content.len()).unwrap() {
            let change = repo.load_change(&range.target.owner).unwrap();
            let start = (range.target.start + range.target_start) as usize;
            let end = start + range.output.len();
            reconstructed.extend_from_slice(&change.contents[start..end]);
            owners.insert(range.target.owner);
        }
        assert_eq!(reconstructed, state.content);
        assert!(
            owners.len() >= minimum_owners,
            "fixture did not exercise the requested distinct authorship spans"
        );
        let mut semantic_bytes = Vec::new();
        let snapshot = state.semantic_snapshot().unwrap();
        let id = snapshot.id;
        for line in snapshot.lines {
            for token in line.tokens {
                let mut token_bytes = Vec::new();
                for fragment in token.authors {
                    let change = repo.load_change(&fragment.target.owner).unwrap();
                    let start = (fragment.target.start + fragment.target_start) as usize;
                    let end = start + fragment.output.len();
                    token_bytes.extend_from_slice(&change.contents[start..end]);
                }
                assert_eq!(token_bytes, state.content[token.output.clone()]);
                semantic_bytes.extend_from_slice(&token_bytes);
            }
        }
        assert_eq!(semantic_bytes, state.content);
        id
    }

    #[test]
    fn span_stream_preserves_each_git_state_and_reopens() {
        use atomic_core::pristine::{GitShaIndexTxnT, GraphTxnT};

        let (_git_dir, git, tip, files) = fixture(16);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let outcome = importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        assert_eq!(outcome.commits, 16);
        let mut commits = Vec::new();
        let mut current = tip;
        loop {
            commits.push(current);
            let commit = git.find_commit(current).unwrap();
            if commit.parent_count() == 0 {
                break;
            }
            current = commit.parent_id(0).unwrap();
        }
        commits.reverse();
        let mut expected_closure = Vec::new();
        let mut closures = Vec::new();
        for oid in commits {
            let hash = {
                let txn = repo.pristine().read_txn().unwrap();
                let id = txn.get_by_git_sha(&oid.to_string()).unwrap().unwrap();
                txn.get_external(id).unwrap().unwrap()
            };
            expected_closure.push(hash);
            let mut sorted_closure = expected_closure.clone();
            sorted_closure.sort();
            sorted_closure.dedup();
            assert_eq!(
                repo.git_commit_closure(&oid.to_string()).unwrap(),
                Some(sorted_closure.clone()),
                "closure for {oid}"
            );
            closures.push((oid, sorted_closure));
            let change = repo.load_change(&hash).unwrap();
            assert!(change.hashed.span.is_some(), "{oid} is not a span change");
            assert!(
                change.file_ops().is_empty(),
                "Git stream must not synthesize native semantic identity"
            );
            let tree = git.find_commit(oid).unwrap().tree().unwrap();
            for path in [
                "hot.txt",
                "moved.txt",
                "stable.txt",
                "nested/deep/empty",
                "nested/deep/recreated",
                "blob.bin",
            ] {
                let expected = tree
                    .get_path(Path::new(path))
                    .ok()
                    .map(|entry| git.find_blob(entry.id()).unwrap().content().to_vec());
                let actual = repo
                    .get_file_content_after_change_on_view(path, &hash, repo.current_view())
                    .unwrap();
                assert_eq!(actual, expected, "{oid} {path}");
            }
        }
        for (path, (bytes, _)) in &files {
            assert_eq!(
                repo.get_file_content(path).unwrap().as_deref(),
                Some(bytes.as_slice()),
                "{path}"
            );
        }
        assert_imported_authored_ranges(&repo, b"alpha\nrestored\nlast\n", 1);
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        for (path, (bytes, _)) in &files {
            assert_eq!(
                std::fs::read(target.path().join(path)).unwrap(),
                *bytes,
                "{path}"
            );
        }
        assert!(repo
            .status(
                repo.require_working_copy_id().unwrap(),
                atomic_repository::StatusOptions::default()
            )
            .unwrap()
            .is_clean());
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_imported_authored_ranges(&reopened, b"alpha\nrestored\nlast\n", 1);
        for (oid, closure) in closures {
            assert_eq!(
                reopened.git_commit_closure(&oid.to_string()).unwrap(),
                Some(closure)
            );
        }
        for (path, (bytes, _)) in &files {
            assert_eq!(
                reopened.get_file_content(path).unwrap().as_deref(),
                Some(bytes.as_slice()),
                "{path}"
            );
        }
    }

    #[test]
    fn git_span_authored_ranges_preserve_multiple_change_owners() {
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let snapshot = assert_imported_authored_ranges(&repo, b"alpha\nchanged\nlast\n", 2);
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            assert_imported_authored_ranges(&reopened, b"alpha\nchanged\nlast\n", 2),
            snapshot
        );
    }

    #[test]
    fn span_compiler_replays_imported_prefix_and_matches_fresh_extension() {
        use atomic_core::pristine::{GitShaIndexTxnT, GraphTxnT, MutTxnT};

        let (_git_dir, git, tip, _) = fixture(5);
        let mut prefix = tip;
        for _ in 0..3 {
            prefix = git.find_commit(prefix).unwrap().parent_id(0).unwrap();
        }
        let partial_dir = tempfile::tempdir().unwrap();
        let mut partial = Repository::init(partial_dir.path()).unwrap();
        let make_importer = |repo: &Repository| {
            ParallelImporter::new(
                &git,
                ParallelImportOptions {
                    graph_only: true,
                    span_stream: true,
                    target_view: repo.current_view().into(),
                    ..Default::default()
                },
            )
        };
        make_importer(&partial)
            .import_graph_stream(
                &mut partial,
                prefix,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        drop(partial);
        let mut partial = Repository::open(partial_dir.path()).unwrap();

        let full_dir = tempfile::tempdir().unwrap();
        let mut full = Repository::init(full_dir.path()).unwrap();
        let importer = make_importer(&full);
        importer
            .import_graph_stream(
                &mut full,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let (git, policy, ordered) = importer.graph_stream_inputs(tip).unwrap();
        assert_eq!(ordered.len(), 5);
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut compiler = Compiler {
            spans: Some(Default::default()),
            ..Default::default()
        };
        let partial_txn = partial.pristine().read_txn().unwrap();
        let full_txn = full.pristine().read_txn().unwrap();
        let mut replayed_prefix = Vec::new();
        for (index, oid) in ordered.iter().copied().enumerate() {
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false).unwrap();
            let metadata = importer.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let replayed = compiler
                .compile(&parsed, &policy, &[], metadata)
                .unwrap()
                .hash();
            let full_id = full_txn.get_by_git_sha(&oid.to_string()).unwrap().unwrap();
            assert_eq!(
                full_txn.get_external(full_id).unwrap(),
                Some(replayed),
                "fresh extension at {oid}"
            );
            let existing = partial_txn.get_by_git_sha(&oid.to_string()).unwrap();
            if index < 2 {
                assert_eq!(
                    partial_txn.get_external(existing.unwrap()).unwrap(),
                    Some(replayed),
                    "existing prefix at {oid}"
                );
                replayed_prefix.push((oid.to_string(), replayed));
            } else {
                assert!(existing.is_none(), "replay must not publish {oid}");
            }
        }
        drop(partial_txn);
        drop(full_txn);
        partial.create_shared_view("unrelated").unwrap();
        let database = partial_dir.path().join(".atomic/atomic.redb");
        let before = std::fs::read(&database).unwrap();
        partial
            .validate_span_git_prefix(partial.current_view(), &replayed_prefix)
            .unwrap();
        assert!(partial
            .validate_span_git_prefix(partial.current_view(), &[])
            .is_err());
        let mut wrong_hash = replayed_prefix.clone();
        wrong_hash[1].1 = atomic_core::types::Hash::of(b"different change");
        assert!(partial
            .validate_span_git_prefix(partial.current_view(), &wrong_hash)
            .is_err());
        let mut wrong_order = replayed_prefix.clone();
        wrong_order.reverse();
        assert!(partial
            .validate_span_git_prefix(partial.current_view(), &wrong_order)
            .is_err());
        let mut unknown = replayed_prefix.clone();
        unknown.push((
            ordered[2].to_string(),
            atomic_core::types::Hash::of(b"unimported change"),
        ));
        assert!(partial
            .validate_span_git_prefix(partial.current_view(), &unknown)
            .is_err());
        assert!(partial
            .validate_span_git_prefix("unrelated", &replayed_prefix)
            .is_err());
        assert_eq!(
            std::fs::read(database).unwrap(),
            before,
            "prefix validation must be read-only"
        );
        let mut txn = partial.pristine().write_txn().unwrap();
        let id = txn.get_by_git_sha(&replayed_prefix[1].0).unwrap().unwrap();
        txn.put_change_deps(id, &[]).unwrap();
        txn.commit().unwrap();
        assert!(
            partial
                .validate_span_git_prefix(partial.current_view(), &replayed_prefix)
                .is_err(),
            "a damaged dependency index must not validate"
        );
    }

    #[test]
    fn incremental_span_private_stage_promotes_only_after_final_tree_proof() {
        use atomic_core::operation::OperationScope;
        use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT, MutTxnT};

        for scenario in ["success", "failed-proof", "abandoned-stage"] {
            let (_git_dir, git, tip, files) = fixture(5);
            let mut prefix = tip;
            for _ in 0..3 {
                prefix = git.find_commit(prefix).unwrap().parent_id(0).unwrap();
            }
            let target = tempfile::tempdir().unwrap();
            let mut repo = Repository::init(target.path()).unwrap();
            let importer = ParallelImporter::new(
                &git,
                ParallelImportOptions {
                    graph_only: true,
                    span_stream: true,
                    target_view: repo.current_view().into(),
                    ..Default::default()
                },
            );
            let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
            importer
                .import_graph_stream(&mut repo, prefix, limits)
                .unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            let before_state = repo.get_view_info(repo.current_view()).unwrap().state;
            let before_history = repo.effective_history(None).unwrap();
            let before_working = repo.working_copy_record(wc).unwrap();

            let (git, policy, ordered) = importer.graph_stream_inputs(tip).unwrap();
            let positions: HashMap<_, _> = ordered
                .iter()
                .copied()
                .enumerate()
                .map(|(index, oid)| (oid, index))
                .collect();
            let mut compiler = Compiler {
                spans: Some(Default::default()),
                ..Default::default()
            };
            let mut prefix_entries = Vec::new();
            let mut suffix = Vec::new();
            for (index, oid) in ordered.iter().copied().enumerate() {
                let parsed = parse_commit_with_scope(&git, oid, index, &positions, false).unwrap();
                let metadata =
                    importer.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
                let prepared = compiler.compile(&parsed, &policy, &[], metadata).unwrap();
                if index < 2 {
                    prefix_entries.push((oid.to_string(), prepared.hash()));
                } else {
                    suffix.push(prepared);
                }
            }
            let baseline_tree =
                import_expected_tree_oid(&git, git.find_commit(prefix).unwrap().tree_id(), &policy)
                    .unwrap();
            let staged_replay: Vec<_> = ordered[2..]
                .iter()
                .zip(&suffix)
                .map(|(oid, change)| (oid.to_string(), change.hash()))
                .collect();
            let wrong_tree =
                import_expected_tree_oid(&git, git.find_commit(tip).unwrap().tree_id(), &policy)
                    .unwrap();
            match GitGraphImportSession::begin_span_incremental(
                &mut repo,
                limits,
                &prefix_entries,
                &policy,
                &wrong_tree,
            ) {
                Ok(_) => panic!("wrong Git baseline must be refused"),
                Err(error) => assert!(
                    error.to_string().contains("baseline tree mismatch"),
                    "{error}"
                ),
            }
            assert_eq!(
                repo.get_view_info(repo.current_view()).unwrap().state,
                before_state
            );
            assert_eq!(repo.working_copy_record(wc).unwrap(), before_working);
            // An existing global SHA must never be silently rebound by staging.
            let next_sha = ordered[2].to_string();
            {
                let mut txn = repo.pristine().write_txn().unwrap();
                let existing = txn.get_by_git_sha(&prefix_entries[0].0).unwrap().unwrap();
                txn.put_git_sha(&next_sha, existing).unwrap();
                txn.commit().unwrap();
            }
            let before_collision = repo
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            let before_global_collision = repo
                .operation_log(OperationScope::Repository, None, false)
                .unwrap();
            {
                let mut session = GitGraphImportSession::begin_span_incremental(
                    &mut repo,
                    limits,
                    &prefix_entries,
                    &policy,
                    &baseline_tree,
                )
                .unwrap();
                let error = session.publish(&suffix).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("already has a repository identity"),
                    "{error}"
                );
            }
            assert_eq!(
                repo.operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap(),
                before_collision
            );
            assert_eq!(
                repo.operation_log(OperationScope::Repository, None, false)
                    .unwrap(),
                before_global_collision
            );
            {
                let mut txn = repo.pristine().write_txn().unwrap();
                assert!(txn.del_git_sha(&next_sha).unwrap());
                txn.commit().unwrap();
            }
            let mut session = GitGraphImportSession::begin_span_incremental(
                &mut repo,
                limits,
                &prefix_entries,
                &policy,
                &baseline_tree,
            )
            .unwrap();
            session.publish(&suffix).unwrap();
            if scenario == "abandoned-stage" {
                drop(session);
                assert_eq!(
                    repo.get_view_info(repo.current_view()).unwrap().state,
                    before_state
                );
                assert_eq!(repo.effective_history(None).unwrap(), before_history);
                assert_eq!(repo.working_copy_record(wc).unwrap(), before_working);
                drop(repo);
                let mut reopened = Repository::open(target.path()).unwrap();
                let before_retry = reopened
                    .operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap();
                let before_global_retry = reopened
                    .operation_log(OperationScope::Repository, None, false)
                    .unwrap();
                match GitGraphImportSession::begin_span_incremental(
                    &mut reopened,
                    limits,
                    &prefix_entries,
                    &policy,
                    &baseline_tree,
                ) {
                    Ok(_) => {
                        panic!("abandoned private stage must not be mistaken for a clean baseline")
                    }
                    Err(error) => assert!(
                        error
                            .to_string()
                            .contains("unchanged Git-only target and private stage"),
                        "{error}"
                    ),
                }
                assert_eq!(
                    reopened
                        .operation_log(OperationScope::WorkingCopy(wc), None, false)
                        .unwrap(),
                    before_retry
                );
                assert_eq!(
                    reopened
                        .operation_log(OperationScope::Repository, None, false)
                        .unwrap(),
                    before_global_retry
                );
                let mut wrong_staged = staged_replay.clone();
                wrong_staged[0].1 = atomic_core::types::Hash::of(b"wrong staged change");
                let refused = GitGraphImportSession::resume_span_incremental(
                    &mut reopened,
                    limits,
                    &prefix_entries,
                    &wrong_staged,
                    &policy,
                    &baseline_tree,
                )
                .is_err();
                assert!(refused, "a mismatched staged suffix must not resume");
                assert_eq!(
                    reopened
                        .operation_log(OperationScope::WorkingCopy(wc), None, false)
                        .unwrap(),
                    before_retry
                );
                assert_eq!(
                    reopened
                        .operation_log(OperationScope::Repository, None, false)
                        .unwrap(),
                    before_global_retry
                );
                let session = GitGraphImportSession::resume_span_incremental(
                    &mut reopened,
                    limits,
                    &prefix_entries,
                    &staged_replay,
                    &policy,
                    &baseline_tree,
                )
                .unwrap();
                let outcome = finish_graph_stream(session, &git, &policy, tip).unwrap();
                assert_eq!(outcome.commits, 3);
                for (path, (bytes, _)) in &files {
                    assert_eq!(
                        reopened.get_file_content(path).unwrap(),
                        Some(bytes.clone()),
                        "{path}"
                    );
                }
                drop(reopened);
                let reopened_again = Repository::open(target.path()).unwrap();
                for (path, (bytes, _)) in &files {
                    assert_eq!(
                        reopened_again.get_file_content(path).unwrap(),
                        Some(bytes.clone()),
                        "{path}"
                    );
                }
                continue;
            }
            let result = finish_graph_stream(
                session,
                &git,
                &policy,
                if scenario == "failed-proof" {
                    prefix
                } else {
                    tip
                },
            );
            if scenario == "failed-proof" {
                assert!(result.is_err(), "incorrect final tree must not publish");
                assert_eq!(
                    repo.get_view_info(repo.current_view()).unwrap().state,
                    before_state
                );
                assert_eq!(repo.effective_history(None).unwrap(), before_history);
                assert_eq!(repo.working_copy_record(wc).unwrap(), before_working);
                assert_eq!(
                    repo.get_file_content("hot.txt").unwrap(),
                    Some(b"alpha\nchanged\nlast\n".to_vec())
                );
            } else {
                let outcome = result.unwrap();
                assert_eq!(outcome.commits, 3);
                assert_eq!(repo.effective_history(None).unwrap().len(), 5);
                for (path, (bytes, _)) in &files {
                    assert_eq!(
                        repo.get_file_content(path).unwrap(),
                        Some(bytes.clone()),
                        "{path}"
                    );
                }
                let log = repo
                    .operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap();
                let original: std::collections::HashSet<_> = before_collision
                    .entries
                    .iter()
                    .map(|entry| entry.operation.id())
                    .collect();
                let global = repo
                    .operation_log(OperationScope::Repository, None, false)
                    .unwrap();
                let appended: Vec<_> = log
                    .entries
                    .iter()
                    .filter(|entry| {
                        !original.contains(&entry.operation.id())
                            && entry.operation.payload().kind
                                == atomic_core::operation::OperationKind::SynthesizeGit
                    })
                    .collect();
                assert_eq!(
                    appended.len(),
                    2,
                    "one private batch and one final promotion"
                );
                for entry in appended {
                    assert_eq!(
                        entry.verification,
                        atomic_repository::repository::OperationVerificationState::Verified
                    );
                    assert!(global
                        .entries
                        .iter()
                        .any(|other| other.operation.id() == entry.operation.id()));
                    assert!(!repo
                        .operation_details(entry.operation.id())
                        .unwrap()
                        .receipts
                        .is_empty());
                }
            }
            drop(repo);
            let mut reopened = Repository::open(target.path()).unwrap();
            if scenario == "failed-proof" {
                assert_eq!(
                    reopened
                        .get_view_info(reopened.current_view())
                        .unwrap()
                        .state,
                    before_state
                );
                assert_eq!(
                    reopened.get_file_content("hot.txt").unwrap(),
                    Some(b"alpha\nchanged\nlast\n".to_vec())
                );
                let session = GitGraphImportSession::resume_span_incremental(
                    &mut reopened,
                    limits,
                    &prefix_entries,
                    &staged_replay,
                    &policy,
                    &baseline_tree,
                )
                .unwrap();
                assert_eq!(
                    finish_graph_stream(session, &git, &policy, tip)
                        .unwrap()
                        .commits,
                    3
                );
                for (path, (bytes, _)) in &files {
                    assert_eq!(
                        reopened.get_file_content(path).unwrap(),
                        Some(bytes.clone()),
                        "{path}"
                    );
                }
            } else {
                for (path, (bytes, _)) in &files {
                    assert_eq!(
                        reopened.get_file_content(path).unwrap(),
                        Some(bytes.clone()),
                        "{path}"
                    );
                }
            }
        }
    }

    #[test]
    fn incremental_span_private_stage_refuses_native_target_changes() {
        use atomic_core::operation::OperationScope;
        use atomic_core::pristine::{GitShaIndexTxnT, GraphTxnT};
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
        importer
            .import_graph_stream(&mut repo, tip, limits)
            .unwrap();
        let (git, policy, ordered) = importer.graph_stream_inputs(tip).unwrap();
        let prefix: Vec<_> = {
            let txn = repo.pristine().read_txn().unwrap();
            ordered
                .iter()
                .map(|oid| {
                    let id = txn.get_by_git_sha(&oid.to_string()).unwrap().unwrap();
                    (oid.to_string(), txn.get_external(id).unwrap().unwrap())
                })
                .collect()
        };
        let tree = import_expected_tree_oid(&git, git.find_commit(tip).unwrap().tree_id(), &policy)
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        let path = target.path().join("native.txt");
        std::fs::write(&path, b"native user work\n").unwrap();
        repo.add(wc, "native.txt", Default::default()).unwrap();
        repo.record(
            wc,
            ChangeHeader::new("native addition"),
            atomic_repository::RecordOptions::new().add_path("native.txt"),
        )
        .unwrap();
        let before_state = repo.get_view_info(repo.current_view()).unwrap().state;
        let before_working = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        let before_global = repo
            .operation_log(OperationScope::Repository, None, false)
            .unwrap();
        match GitGraphImportSession::begin_span_incremental(
            &mut repo, limits, &prefix, &policy, &tree,
        ) {
            Ok(_) => panic!("native additions must not enter the restricted incremental path"),
            Err(error) => assert!(
                error.to_string().contains("unchanged Git-only target"),
                "{error}"
            ),
        }
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before_state
        );
        assert_eq!(
            repo.operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap(),
            before_working
        );
        assert_eq!(
            repo.operation_log(OperationScope::Repository, None, false)
                .unwrap(),
            before_global
        );
        assert_eq!(std::fs::read(path).unwrap(), b"native user work\n");
    }

    #[test]
    fn incremental_span_private_stage_accepts_two_successive_updates() {
        let (_git_dir, git, tip, files) = fixture(7);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
        let (git, policy, ordered) = importer.graph_stream_inputs(tip).unwrap();
        assert_eq!(ordered.len(), 7);
        importer
            .import_graph_stream(&mut repo, ordered[1], limits)
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        let ignore = target.path().join(".atomicignore");
        std::fs::write(&ignore, b"# native bootstrap\n").unwrap();
        repo.add(wc, ".atomicignore", Default::default()).unwrap();
        repo.record(
            wc,
            ChangeHeader::new("Initialize repository"),
            atomic_repository::RecordOptions::new().add_path(".atomicignore"),
        )
        .unwrap();
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut compiler = Compiler {
            spans: Some(Default::default()),
            ..Default::default()
        };
        let mut prefix = Vec::new();
        let mut first = Vec::new();
        let mut second = Vec::new();
        let mut middle_prefix = Vec::new();
        for (index, oid) in ordered.iter().copied().enumerate() {
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false).unwrap();
            let metadata = importer.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let prepared = compiler.compile(&parsed, &policy, &[], metadata).unwrap();
            if index < 2 {
                prefix.push((oid.to_string(), prepared.hash()));
            } else if index < 5 {
                middle_prefix.push((oid.to_string(), prepared.hash()));
                first.push(prepared);
            } else {
                second.push(prepared);
            }
        }
        let baseline = import_expected_tree_oid(
            &git,
            git.find_commit(ordered[1]).unwrap().tree_id(),
            &policy,
        )
        .unwrap();
        let mut session = GitGraphImportSession::begin_span_incremental(
            &mut repo, limits, &prefix, &policy, &baseline,
        )
        .unwrap();
        session.publish(&first).unwrap();
        assert_eq!(
            finish_graph_stream(session, &git, &policy, ordered[4])
                .unwrap()
                .commits,
            3
        );
        prefix.extend(middle_prefix);
        let middle = import_expected_tree_oid(
            &git,
            git.find_commit(ordered[4]).unwrap().tree_id(),
            &policy,
        )
        .unwrap();
        let mut session = GitGraphImportSession::begin_span_incremental(
            &mut repo, limits, &prefix, &policy, &middle,
        )
        .unwrap();
        session.publish(&second).unwrap();
        assert_eq!(
            finish_graph_stream(session, &git, &policy, tip)
                .unwrap()
                .commits,
            2
        );
        assert_eq!(repo.effective_history(None).unwrap().len(), 8);
        assert_eq!(
            repo.get_file_content(".atomicignore").unwrap(),
            Some(b"# native bootstrap\n".to_vec())
        );
        assert_eq!(std::fs::read(&ignore).unwrap(), b"# native bootstrap\n");
        for (path, (bytes, _)) in &files {
            assert_eq!(
                repo.get_file_content(path).unwrap(),
                Some(bytes.clone()),
                "{path}"
            );
        }
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            reopened.get_file_content(".atomicignore").unwrap(),
            Some(b"# native bootstrap\n".to_vec())
        );
        for (path, (bytes, _)) in &files {
            assert_eq!(
                reopened.get_file_content(path).unwrap(),
                Some(bytes.clone()),
                "{path}"
            );
        }
    }

    #[cfg(feature = "experimental-span-import")]
    #[test]
    #[serial_test::file_serial]
    fn incremental_span_private_stage_accepts_real_cli_bootstrap() {
        use crate::commands::Command;
        struct RestoreDirectory(std::path::PathBuf);
        impl Drop for RestoreDirectory {
            fn drop(&mut self) {
                std::env::set_current_dir(&self.0).unwrap();
            }
        }

        let (source, bare, tip, files) = fixture(5);
        let mut prefix = tip;
        for _ in 0..3 {
            prefix = bare.find_commit(prefix).unwrap().parent_id(0).unwrap();
        }
        bare.set_head("refs/heads/main").unwrap();
        let target = tempfile::tempdir().unwrap();
        let git = GitRepository::clone(source.path().to_str().unwrap(), target.path()).unwrap();
        let commit = git.find_commit(prefix).unwrap();
        git.branch("bootstrap", &commit, false).unwrap();
        drop(commit);
        git.set_head("refs/heads/bootstrap").unwrap();
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let _directory = RestoreDirectory(std::env::current_dir().unwrap());
        std::env::set_current_dir(target.path()).unwrap();
        crate::commands::git::import::Import {
            span: true,
            no_vault: true,
            ..Default::default()
        }
        .run()
        .unwrap();
        let ignore = target.path().join(".atomicignore");
        let ignore_bytes = std::fs::read(&ignore).unwrap();
        let user_ignore_bytes = b"# unrecorded user ignore settings\n";
        std::fs::write(&ignore, user_ignore_bytes).unwrap();
        let git_head = git.head().unwrap().target().unwrap();
        let git_index = std::fs::read(git.path().join("index")).unwrap();
        let working_bytes = std::fs::read(target.path().join("hot.txt")).unwrap();
        let mut repo = Repository::open(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let (git, policy, ordered) = importer.graph_stream_inputs(tip).unwrap();
        let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
        let positions: HashMap<_, _> = ordered
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect();
        let mut compiler = Compiler {
            spans: Some(Default::default()),
            ..Default::default()
        };
        let mut baseline = Vec::new();
        let mut staged = Vec::new();
        for (index, oid) in ordered.iter().copied().take(3).enumerate() {
            let parsed = parse_commit_with_scope(&git, oid, index, &positions, false).unwrap();
            let metadata = importer.build_git_metadata(&parsed, parsed.is_empty, parsed.is_merge);
            let prepared = compiler.compile(&parsed, &policy, &[], metadata).unwrap();
            if index < 2 {
                baseline.push((oid.to_string(), prepared.hash()));
            } else {
                staged.push(prepared);
            }
        }
        let baseline_tree =
            import_expected_tree_oid(&git, git.find_commit(prefix).unwrap().tree_id(), &policy)
                .unwrap();
        let mut session = GitGraphImportSession::begin_span_incremental(
            &mut repo,
            limits,
            &baseline,
            &policy,
            &baseline_tree,
        )
        .unwrap();
        session.publish(&staged).unwrap();
        drop(session);
        drop(repo);
        let mut repo = Repository::open(target.path()).unwrap();
        assert_eq!(
            importer
                .import_span_incremental(&mut repo, tip, limits, &ordered, || Ok(()))
                .unwrap()
                .commits,
            3
        );
        let wc = repo.require_working_copy_id().unwrap();
        let before_noop = repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                None,
                false,
            )
            .unwrap();
        let before_global_noop = repo
            .operation_log(
                atomic_core::operation::OperationScope::Repository,
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            importer
                .import_span_incremental(&mut repo, tip, limits, &ordered, || Ok(()))
                .unwrap()
                .commits,
            0
        );
        let mut wrong_order = ordered.clone();
        wrong_order.reverse();
        assert!(importer
            .import_span_incremental(&mut repo, tip, limits, &wrong_order, || Ok(()))
            .is_err());
        assert_eq!(
            repo.operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                None,
                false
            )
            .unwrap(),
            before_noop
        );
        assert_eq!(
            repo.operation_log(
                atomic_core::operation::OperationScope::Repository,
                None,
                false
            )
            .unwrap(),
            before_global_noop
        );
        assert_eq!(
            repo.get_file_content(".atomicignore").unwrap(),
            Some(ignore_bytes.clone())
        );
        assert_eq!(std::fs::read(&ignore).unwrap(), user_ignore_bytes);
        assert_eq!(
            std::fs::read(target.path().join("hot.txt")).unwrap(),
            working_bytes
        );
        assert_eq!(git.head().unwrap().target().unwrap(), git_head);
        assert_eq!(std::fs::read(git.path().join("index")).unwrap(), git_index);
        for (path, (bytes, _)) in &files {
            assert_eq!(
                repo.get_file_content(path).unwrap(),
                Some(bytes.clone()),
                "{path}"
            );
        }
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            reopened.get_file_content(".atomicignore").unwrap(),
            Some(ignore_bytes)
        );
        for (path, (bytes, _)) in &files {
            assert_eq!(
                reopened.get_file_content(path).unwrap(),
                Some(bytes.clone()),
                "{path}"
            );
        }
    }

    #[test]
    fn incremental_span_git_observation_failure_keeps_target_and_resumes() {
        let (_git_dir, git, tip, files) = fixture(5);
        let mut prefix = tip;
        for _ in 0..3 {
            prefix = git.find_commit(prefix).unwrap().parent_id(0).unwrap();
        }
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
        importer
            .import_graph_stream(&mut repo, prefix, limits)
            .unwrap();
        let (_, _, ordered) = importer.graph_stream_inputs(tip).unwrap();
        let before = repo.get_view_info(repo.current_view()).unwrap().state;
        let wc = repo.require_working_copy_id().unwrap();
        let before_working = repo.working_copy_record(wc).unwrap();
        let checks = std::cell::Cell::new(0);
        let error = importer
            .import_span_incremental(&mut repo, tip, limits, &ordered, || {
                let count = checks.get() + 1;
                checks.set(count);
                if count == 3 {
                    Err(git_error("injected Git observation drift"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(
            checks.get(),
            3,
            "final check must be exercised after staging"
        );
        assert!(
            error.to_string().contains("injected Git observation drift"),
            "{error}"
        );
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before
        );
        assert_eq!(repo.working_copy_record(wc).unwrap(), before_working);
        drop(repo);
        let mut reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            reopened
                .get_view_info(reopened.current_view())
                .unwrap()
                .state,
            before
        );
        assert_eq!(
            importer
                .import_span_incremental(&mut reopened, tip, limits, &ordered, || Ok(()))
                .unwrap()
                .commits,
            3
        );
        for (path, (bytes, _)) in &files {
            assert_eq!(
                reopened.get_file_content(path).unwrap(),
                Some(bytes.clone()),
                "{path}"
            );
        }
    }

    #[test]
    fn span_import_without_semantic_history_records_native_edit() {
        use atomic_core::operation::OperationScope;

        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let before_state = repo.get_view_info(repo.current_view()).unwrap().state;
        let before_ops = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        let path = target.path().join("hot.txt");
        std::fs::write(&path, b"alpha\nworking edit\nlast\n").unwrap();
        let outcome = repo
            .record(
                wc,
                ChangeHeader::new("native edit"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        assert!(outcome.change().file_ops().iter().any(|ops| matches!(
            ops.trunk_op(),
            Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { .. })
        )));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"alpha\nworking edit\nlast\n"
        );
        assert_ne!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before_state
        );
        let after_ops = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        assert!(after_ops.entries.len() > before_ops.entries.len());
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nworking edit\nlast\n".to_vec())
        );
        let (review, _) = atomic_repository::diff_engine::change_file_diffs(
            &repo,
            outcome.change(),
            outcome.hash(),
            &Default::default(),
        )
        .unwrap();
        let review = review
            .iter()
            .find(|file| file.new_path == "hot.txt")
            .unwrap();
        assert!(review
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.content == "changed"));
        assert!(review
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.content == "working edit"));
        assert_imported_authored_ranges(&repo, b"alpha\nworking edit\nlast\n", 2);
        let error = repo
            .materialize_crdt_from_changes(Default::default())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot be materialized as CRDT rows"),
            "{error}"
        );
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nworking edit\nlast\n".to_vec())
        );
        drop(repo);
        let mut repo = Repository::open(target.path()).unwrap();
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nworking edit\nlast\n".to_vec())
        );
        std::fs::remove_file(&path).unwrap();
        repo.materialize(wc).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"alpha\nworking edit\nlast\n");
    }

    #[test]
    fn projected_native_span_edits_transfer_and_reopen() {
        use atomic_core::pristine::MutTxnT;
        let (_git_dir, git, tip, _) = fixture(2);
        let source_dir = tempfile::tempdir().unwrap();
        let mut source = Repository::init(source_dir.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: source.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut source,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = source.require_working_copy_id().unwrap();
        source.materialize(wc).unwrap();
        let path = source_dir.path().join("hot.txt");
        for (message, bytes) in [
            ("native one", b"alpha\nfirst native\nlast\n".as_slice()),
            (
                "native two",
                b"alpha\nfirst native\nsecond native\n".as_slice(),
            ),
        ] {
            std::fs::write(&path, bytes).unwrap();
            source
                .record(
                    wc,
                    ChangeHeader::new(message),
                    atomic_repository::RecordOptions::new()
                        .with_all(true)
                        .save_to_store(true)
                        .apply_after_record(true),
                )
                .unwrap();
        }
        let expected = b"alpha\nfirst native\nsecond native\n";
        let snapshot = assert_imported_authored_ranges(&source, expected, 3);
        let history = source.effective_history(None).unwrap();
        let native = history
            .iter()
            .map(|entry| entry.hash)
            .find(|hash| source.load_change(hash).unwrap().hashed.header.message == "native two")
            .unwrap();
        {
            use atomic_core::pristine::{
                CapabilityTxnT, GitShaIndexTxnT, GraphTxnT, SPAN_PROJECTED_SEMANTICS_CAPABILITY,
            };
            use atomic_core::types::Base32;
            let host_dir = tempfile::tempdir().unwrap();
            let mut host = Repository::init(host_dir.path()).unwrap();
            let source_txn = source.pristine().read_txn().unwrap();
            let git_tip_id = source_txn
                .get_by_git_sha(&tip.to_string())
                .unwrap()
                .unwrap();
            let git_tip_hash = source_txn.get_external(git_tip_id).unwrap().unwrap();
            drop(source_txn);
            for entry in &history {
                let change = source.load_change(&entry.hash).unwrap();
                if matches!(
                    change.origin(),
                    atomic_core::change::ChangeOrigin::GitSynthesized { .. }
                ) {
                    let bytes =
                        std::fs::read(source.change_store().change_path(&entry.hash)).unwrap();
                    host.import_change_bytes(&entry.hash, &bytes).unwrap();
                }
            }
            let parent = host.current_view().to_string();
            host.create_draft_view("submitted", &parent).unwrap();
            host.insert_change_rec(
                &git_tip_hash,
                atomic_repository::InsertOptions::default().view("submitted"),
            )
            .unwrap();
            assert_eq!(
                host.pristine()
                    .read_txn()
                    .unwrap()
                    .required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                    .unwrap(),
                None
            );
            let first_native = history
                .iter()
                .map(|entry| entry.hash)
                .find(|hash| {
                    source.load_change(hash).unwrap().hashed.header.message == "native one"
                })
                .unwrap();
            let bytes = std::fs::read(source.change_store().change_path(&first_native)).unwrap();
            let base = host
                .sandbox_effective_state("submitted")
                .unwrap()
                .to_base32();
            host.insert_submitted_change("submitted", &base, &first_native, &bytes)
                .unwrap()
                .unwrap();
            assert_eq!(
                host.get_file_content_on_view("hot.txt", "submitted")
                    .unwrap(),
                Some(b"alpha\nfirst native\nlast\n".to_vec())
            );
            assert_eq!(
                host.pristine()
                    .read_txn()
                    .unwrap()
                    .required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                    .unwrap(),
                Some(SPAN_PROJECTED_SEMANTICS_CAPABILITY.minimum_version())
            );
        }
        {
            use atomic_core::operation::OperationScope;
            use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};
            let collision_dir = tempfile::tempdir().unwrap();
            let mut collision = Repository::init(collision_dir.path()).unwrap();
            for entry in &history {
                let bytes = std::fs::read(source.change_store().change_path(&entry.hash)).unwrap();
                collision.import_change_bytes(&entry.hash, &bytes).unwrap();
            }
            let (_, _, commits) = importer.graph_stream_inputs(tip).unwrap();
            let sha = commits[0].to_string();
            let fake = {
                let mut txn = collision.pristine().write_txn().unwrap();
                let fake = txn
                    .register_change(&ContentHash::of(b"foreign Git identity"))
                    .unwrap();
                txn.put_git_sha(&sha, fake).unwrap();
                txn.commit().unwrap();
                fake
            };
            let parent = collision.current_view().to_string();
            collision.create_draft_view("received", &parent).unwrap();
            let wc = collision.require_working_copy_id().unwrap();
            let before = collision.get_view_info("received").unwrap().state;
            let working = collision
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            let global = collision
                .operation_log(OperationScope::Repository, None, false)
                .unwrap();
            let error = collision
                .insert_change_rec(
                    &native,
                    atomic_repository::InsertOptions::default().view("received"),
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("different repository identity"),
                "{error}"
            );
            assert_eq!(collision.get_view_info("received").unwrap().state, before);
            assert_eq!(
                collision
                    .operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap(),
                working
            );
            assert_eq!(
                collision
                    .operation_log(OperationScope::Repository, None, false)
                    .unwrap(),
                global
            );
            assert_eq!(
                collision
                    .pristine()
                    .read_txn()
                    .unwrap()
                    .get_by_git_sha(&sha)
                    .unwrap(),
                Some(fake)
            );
        }
        let receiver_dir = tempfile::tempdir().unwrap();
        let mut receiver = Repository::init(receiver_dir.path()).unwrap();
        {
            let mut txn = receiver.pristine().write_txn().unwrap();
            txn.register_change(&ContentHash::of(b"receiver-only identity"))
                .unwrap();
            txn.commit().unwrap();
        }
        for entry in &history {
            let bytes = std::fs::read(source.change_store().change_path(&entry.hash)).unwrap();
            assert_eq!(
                receiver.import_change_bytes(&entry.hash, &bytes).unwrap(),
                entry.hash
            );
        }
        let parent = receiver.current_view().to_string();
        receiver.create_draft_view("received", &parent).unwrap();
        receiver
            .insert_change_rec(
                &native,
                atomic_repository::InsertOptions::default().view("received"),
            )
            .unwrap();
        {
            use atomic_core::pristine::{GitCommitClosureTxnT, GitShaIndexTxnT, GraphTxnT};
            let source_txn = source.pristine().read_txn().unwrap();
            let receiver_txn = receiver.pristine().read_txn().unwrap();
            let (_, _, commits) = importer.graph_stream_inputs(tip).unwrap();
            for oid in commits {
                let sha = oid.to_string();
                let original_id = source_txn.get_by_git_sha(&sha).unwrap().unwrap();
                let received_id = receiver_txn.get_by_git_sha(&sha).unwrap().unwrap();
                assert_eq!(
                    source_txn.get_external(original_id).unwrap(),
                    receiver_txn.get_external(received_id).unwrap(),
                    "{sha}"
                );
                assert_eq!(
                    source_txn.get_git_commit_closure(&sha).unwrap(),
                    receiver_txn.get_git_commit_closure(&sha).unwrap(),
                    "{sha}"
                );
            }
        }
        let wc = receiver.require_working_copy_id().unwrap();
        std::fs::write(receiver_dir.path().join("untracked.txt"), b"user bytes\n").unwrap();
        receiver.switch_view(wc, "received").unwrap();
        assert_eq!(
            std::fs::read(receiver_dir.path().join("hot.txt")).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read(receiver_dir.path().join("untracked.txt")).unwrap(),
            b"user bytes\n"
        );
        assert_eq!(
            assert_imported_authored_ranges(&receiver, expected, 3),
            snapshot
        );
        drop(receiver);
        let receiver = Repository::open(receiver_dir.path()).unwrap();
        assert_eq!(
            receiver.get_file_content("hot.txt").unwrap(),
            Some(expected.to_vec())
        );
        assert_eq!(
            assert_imported_authored_ranges(&receiver, expected, 3),
            snapshot
        );
        {
            use atomic_core::pristine::{GitCommitClosureTxnT, GitShaIndexTxnT};
            let source_txn = source.pristine().read_txn().unwrap();
            let receiver_txn = receiver.pristine().read_txn().unwrap();
            let (_, _, commits) = importer.graph_stream_inputs(tip).unwrap();
            for oid in commits {
                let sha = oid.to_string();
                assert!(
                    receiver_txn.get_by_git_sha(&sha).unwrap().is_some(),
                    "{sha}"
                );
                assert_eq!(
                    source_txn.get_git_commit_closure(&sha).unwrap(),
                    receiver_txn.get_git_commit_closure(&sha).unwrap(),
                    "{sha}"
                );
            }
        }
        {
            use atomic_core::operation::OperationScope;
            use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};
            let (_, _, commits) = importer.graph_stream_inputs(tip).unwrap();
            let sha = commits[0].to_string();
            let mut txn = receiver.pristine().write_txn().unwrap();
            assert!(txn.del_git_sha(&sha).unwrap());
            txn.commit().unwrap();
            let wc = receiver.require_working_copy_id().unwrap();
            let state = receiver
                .get_view_info(receiver.current_view())
                .unwrap()
                .state;
            let working = receiver
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            let global = receiver
                .operation_log(OperationScope::Repository, None, false)
                .unwrap();
            let error = receiver
                .insert_change_rec(&native, atomic_repository::InsertOptions::default())
                .unwrap_err();
            assert!(error.to_string().contains("identity is missing"), "{error}");
            assert_eq!(
                receiver
                    .get_view_info(receiver.current_view())
                    .unwrap()
                    .state,
                state
            );
            assert_eq!(
                receiver
                    .operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap(),
                working
            );
            assert_eq!(
                receiver
                    .operation_log(OperationScope::Repository, None, false)
                    .unwrap(),
                global
            );
            assert_eq!(
                receiver
                    .pristine()
                    .read_txn()
                    .unwrap()
                    .get_by_git_sha(&sha)
                    .unwrap(),
                None
            );
            assert_eq!(
                std::fs::read(receiver_dir.path().join("hot.txt")).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn projected_native_span_edits_compose_across_views_and_refuse_overlap() {
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let parent = repo.current_view().to_string();
        for name in ["left", "right", "overlap"] {
            repo.create_draft_view(name, &parent).unwrap();
        }
        let path = target.path().join("hot.txt");
        let options = || {
            atomic_repository::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
        };
        repo.switch_view(wc, "left").unwrap();
        std::fs::write(&path, b"ALPHA\nchanged\nlast\n").unwrap();
        repo.record(wc, ChangeHeader::new("left edit"), options())
            .unwrap();
        repo.switch_view(wc, "right").unwrap();
        std::fs::write(&path, b"alpha\nchanged\nLAST\n").unwrap();
        let right = *repo
            .record(wc, ChangeHeader::new("right edit"), options())
            .unwrap()
            .hash();
        repo.switch_view(wc, "overlap").unwrap();
        std::fs::write(&path, b"different\nchanged\nlast\n").unwrap();
        let overlap = *repo
            .record(wc, ChangeHeader::new("overlapping edit"), options())
            .unwrap()
            .hash();
        repo.switch_view(wc, "left").unwrap();
        repo.insert_change_rec(
            &right,
            atomic_repository::InsertOptions::default().view("left"),
        )
        .unwrap();
        let expected = b"ALPHA\nchanged\nLAST\n";
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(expected.to_vec())
        );
        let before = repo.get_view_info("left").unwrap().state;
        let log = repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                None,
                false,
            )
            .unwrap();
        let error = repo
            .insert_change_rec(
                &overlap,
                atomic_repository::InsertOptions::default().view("left"),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("overlap") || error.to_string().contains("ambiguous"),
            "{error}"
        );
        assert_eq!(repo.get_view_info("left").unwrap().state, before);
        assert_eq!(
            repo.operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                None,
                false
            )
            .unwrap(),
            log
        );
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(expected.to_vec())
        );
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            reopened.get_file_content("hot.txt").unwrap(),
            Some(expected.to_vec())
        );
    }

    #[test]
    fn projected_native_record_undo_keeps_semantic_capability() {
        use atomic_core::pristine::{
            CapabilityMutTxnT, CapabilityTxnT, MutTxnT, SPAN_PROJECTED_SEMANTICS_CAPABILITY,
        };
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        std::fs::write(&path, b"alpha\nworking edit\nlast\n").unwrap();
        let outcome = repo
            .record(
                wc,
                ChangeHeader::new("native edit"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        let hash = *outcome.hash();
        let log = repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                None,
                false,
            )
            .unwrap();
        let recorded_operation = log
            .entries
            .iter()
            .find(|entry| {
                entry.operation.payload().kind == atomic_core::operation::OperationKind::Record
                    && entry.operation.payload().evidence.contains(&hash)
            })
            .unwrap()
            .operation
            .id();
        repo.undo_operation(wc, None).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"alpha\nworking edit\nlast\n"
        );
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nchanged\nlast\n".to_vec())
        );
        assert_eq!(
            repo.pristine()
                .read_txn()
                .unwrap()
                .required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                .unwrap(),
            Some(SPAN_PROJECTED_SEMANTICS_CAPABILITY.minimum_version())
        );
        assert!(repo.load_change(&hash).is_ok());
        {
            let mut txn = repo.pristine().write_txn().unwrap();
            assert!(txn
                .del_required_capability(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                .is_err());
            txn.abort().unwrap();
        }
        repo.restore_operation(wc, recorded_operation).unwrap();
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nworking edit\nlast\n".to_vec())
        );
        drop(repo);
        let repo = Repository::open(target.path()).unwrap();
        assert_eq!(
            repo.pristine()
                .read_txn()
                .unwrap()
                .required_capability_version(SPAN_PROJECTED_SEMANTICS_CAPABILITY.id())
                .unwrap(),
            Some(SPAN_PROJECTED_SEMANTICS_CAPABILITY.minimum_version())
        );
        assert_eq!(std::fs::read(path).unwrap(), b"alpha\nworking edit\nlast\n");
    }

    #[test]
    fn projected_native_edits_preserve_empty_binary_and_line_endings() {
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        let cases: &[&[u8]] = &[
            b"",
            b"no newline",
            b"first\r\nsecond\r\n",
            &[0, 255, 1, 0],
            b"back to text\n",
            b"",
        ];
        for (index, bytes) in cases.iter().enumerate() {
            std::fs::write(&path, bytes).unwrap();
            repo.record(
                wc,
                ChangeHeader::new(format!("projected {index}")),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap_or_else(|error| panic!("case {index}: {error}"));
            assert_eq!(
                repo.get_file_content("hot.txt").unwrap().as_deref(),
                Some(*bytes),
                "case {index}"
            );
            drop(repo);
            repo = Repository::open(target.path()).unwrap();
            assert_eq!(
                repo.get_file_content("hot.txt").unwrap().as_deref(),
                Some(*bytes),
                "reopen case {index}"
            );
        }
    }

    #[test]
    fn projected_token_edits_roundtrip_common_sequences() {
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        let cases: &[&str] = &[
            "let count = 1;\n",
            "let total = 1;\n",
            "let total = 10;\n",
            "pub let total = 10;\n",
            "pub let total = 10; // text\n",
            "pub let total = 10; // edited\n",
            "pub let total = 10;\n",
            "let total = 10;\nlet next = 2;\n",
            "let total=10;\nlet next = 2;\n",
            "let total=10;\r\nlet next = 2;\r\n",
            "let total=10;\r\n",
            "héllo αβ\n",
            "héllo αβ!\n",
            "héllo αβ!",
            "x",
            "",
            "x y z\n",
            "x z\n",
            "x y z\n",
            "x zz\n",
        ];
        for (index, text) in cases.iter().enumerate() {
            std::fs::write(&path, text.as_bytes()).unwrap();
            repo.record(
                wc,
                ChangeHeader::new(format!("token sequence {index}")),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap_or_else(|error| panic!("case {index} {text:?}: {error}"));
            assert_eq!(
                repo.get_file_content("hot.txt").unwrap().as_deref(),
                Some(text.as_bytes()),
                "case {index}"
            );
            drop(repo);
            repo = Repository::open(target.path()).unwrap();
            assert_eq!(
                repo.get_file_content("hot.txt").unwrap().as_deref(),
                Some(text.as_bytes()),
                "reopen case {index}"
            );
        }
    }

    #[test]
    fn projected_native_edit_retains_unchanged_token_authorship() {
        use atomic_core::pristine::span_patch_storage::SpanRenderContext;
        use atomic_core::pristine::{TreeTxnT, ViewTxnT};
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let snapshot = |repo: &Repository| {
            let txn = repo.pristine().read_txn().unwrap();
            let view = txn.get_view(repo.current_view()).unwrap().unwrap();
            let visibility = atomic_repository::graph_visibility_closure(&txn, &view).unwrap();
            let inode = txn.get_inode("hot.txt").unwrap().unwrap();
            SpanRenderContext::new(&txn, repo.change_store(), &visibility)
                .unwrap()
                .record_state(inode)
                .unwrap()
                .unwrap()
                .semantic_snapshot()
                .unwrap()
        };
        let before = snapshot(&repo);
        assert_eq!(before.lines[1].tokens.len(), 2);
        let original_word = before.lines[1].tokens[0].id;
        let original_newline = before.lines[1].tokens[1].id;
        let original_author = before.lines[1].tokens[0].authors[0].target.owner;
        std::fs::write(
            target.path().join("hot.txt"),
            b"alpha\nchanged native\nlast\n",
        )
        .unwrap();
        let native = *repo
            .record(
                wc,
                ChangeHeader::new("append token"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap()
            .hash();
        let after = snapshot(&repo);
        let tokens = &after.lines[1].tokens;
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].id, original_word);
        assert_eq!(tokens[0].authors[0].target.owner, original_author);
        assert_eq!(tokens[1].authors[0].target.owner, native);
        assert_eq!(tokens[2].authors[0].target.owner, native);
        assert_eq!(tokens[3].id, original_newline);
        assert_eq!(tokens[3].authors[0].target.owner, original_author);
        drop(repo);
        let repo = Repository::open(target.path()).unwrap();
        let reopened = snapshot(&repo);
        assert_eq!(reopened.id, after.id);
        let options = || {
            atomic_repository::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
        };
        std::fs::write(target.path().join("hot.txt"), "α\n".as_bytes()).unwrap();
        repo.record(wc, ChangeHeader::new("first scalar"), options())
            .unwrap();
        std::fs::write(target.path().join("hot.txt"), "β\n".as_bytes()).unwrap();
        let beta = *repo
            .record(wc, ChangeHeader::new("replace scalar"), options())
            .unwrap()
            .hash();
        let unicode = snapshot(&repo);
        assert_eq!(unicode.lines[0].tokens.len(), 3);
        assert_eq!(unicode.lines[0].tokens[0].authors[0].target.owner, beta);
        assert_eq!(unicode.lines[0].tokens[1].authors[0].target.owner, beta);
    }

    #[test]
    fn projected_token_budget_refuses_before_record_publication() {
        use atomic_core::operation::OperationScope;
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        let first = (0..1100)
            .map(|index| format!("old{index} "))
            .collect::<String>();
        let second = (0..1100)
            .map(|index| format!("new{index} "))
            .collect::<String>();
        let options = || {
            atomic_repository::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
        };
        std::fs::write(&path, &first).unwrap();
        repo.record(wc, ChangeHeader::new("large first"), options())
            .unwrap();
        let state = repo.get_view_info(repo.current_view()).unwrap().state;
        let working = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        let global = repo
            .operation_log(OperationScope::Repository, None, false)
            .unwrap();
        std::fs::write(&path, &second).unwrap();
        let error = repo
            .record(wc, ChangeHeader::new("too large"), options())
            .unwrap_err();
        assert!(error.to_string().contains("safe memory budget"), "{error}");
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            state
        );
        assert_eq!(
            repo.operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap(),
            working
        );
        assert_eq!(
            repo.operation_log(OperationScope::Repository, None, false)
                .unwrap(),
            global
        );
        assert_eq!(std::fs::read(&path).unwrap(), second.as_bytes());
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap().as_deref(),
            Some(first.as_bytes())
        );
    }

    #[test]
    fn projected_and_native_crdt_files_can_share_one_record() {
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        std::fs::write(
            target.path().join("hot.txt"),
            b"alpha\nnew imported edit\nlast\n",
        )
        .unwrap();
        std::fs::write(target.path().join("native.txt"), b"native file\n").unwrap();
        repo.add(wc, "native.txt", Default::default()).unwrap();
        let options = || {
            atomic_repository::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
        };
        let result = repo
            .record(wc, ChangeHeader::new("mixed edit"), options())
            .unwrap();
        assert!(result
            .change()
            .file_ops()
            .iter()
            .any(|ops| ops.path() == "hot.txt"
                && matches!(
                    ops.trunk_op(),
                    Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { .. })
                )));
        assert!(result
            .change()
            .file_ops()
            .iter()
            .any(|ops| ops.path() == "native.txt" && ops.is_create()));
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nnew imported edit\nlast\n".to_vec())
        );
        assert_eq!(
            repo.get_file_content("native.txt").unwrap(),
            Some(b"native file\n".to_vec())
        );
        drop(repo);
        let repo = Repository::open(target.path()).unwrap();
        std::fs::write(target.path().join("native.txt"), b"native file edited\n").unwrap();
        repo.record(wc, ChangeHeader::new("edit native file"), options())
            .unwrap();
        assert_eq!(
            repo.get_file_content("native.txt").unwrap(),
            Some(b"native file edited\n".to_vec())
        );
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nnew imported edit\nlast\n".to_vec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn projected_edit_and_mode_change_share_one_record() {
        use std::os::unix::fs::PermissionsExt;
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        std::fs::write(&path, b"alpha\nchanged plus\nlast\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = repo
            .record(
                wc,
                ChangeHeader::new("content and mode"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        assert_eq!(
            result
                .change()
                .file_ops()
                .iter()
                .filter(|ops| ops.path() == "hot.txt")
                .count(),
            1
        );
        assert!(result.change().hunks().iter().any(|op| matches!(op, atomic_core::change::GraphOp::SetAttr { path, value: atomic_core::change::InodeAttr::Mode(0o755), .. } if path == "hot.txt")));
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nchanged plus\nlast\n".to_vec())
        );
        drop(repo);
        let repo = Repository::open(target.path()).unwrap();
        std::fs::remove_file(&path).unwrap();
        repo.materialize(wc).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"alpha\nchanged plus\nlast\n"
        );
        assert_ne!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o111,
            0
        );
    }

    #[test]
    fn projected_edit_with_wrong_baseline_or_path_refuses_before_publication() {
        use atomic_core::change::span_patch::SpanProjectionId;
        use atomic_core::crdt::TrunkOp;
        use atomic_core::operation::OperationScope;
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        std::fs::write(target.path().join("hot.txt"), b"alpha\nnew content\nlast\n").unwrap();
        let outcome = repo
            .record(
                wc,
                ChangeHeader::new("valid native"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        let valid = outcome.change();
        let stored_path = repo.change_store().change_path(outcome.hash());
        let original_bytes = std::fs::read(&stored_path).unwrap();
        assert_eq!(repo.save_change(valid).unwrap(), *outcome.hash());
        assert_eq!(std::fs::read(&stored_path).unwrap(), original_bytes);
        let mut alternate = valid.clone();
        alternate.unhashed = Some(serde_json::json!({"different": "unhashed metadata"}));
        assert_eq!(alternate.hash().unwrap(), *outcome.hash());
        let error = repo.save_change(&alternate).unwrap_err();
        assert!(
            error.to_string().contains("refusing to overwrite"),
            "{error}"
        );
        assert_eq!(std::fs::read(&stored_path).unwrap(), original_bytes);
        let marker = valid
            .file_ops()
            .iter()
            .find(|ops| matches!(ops.trunk_op(), Some(TrunkOp::ProjectedSpanEdit { .. })))
            .unwrap();
        let (file, baseline) = match marker.trunk_op().unwrap() {
            TrunkOp::ProjectedSpanEdit { file, baseline } => (file.clone(), *baseline),
            _ => unreachable!(),
        };
        for (path, baseline, expected) in [
            (
                "hot.txt",
                SpanProjectionId(ContentHash::of(b"wrong baseline")),
                "baseline",
            ),
            ("not-hot.txt", baseline, "path"),
        ] {
            let mut forged = valid.clone();
            forged.hashed.file_ops = vec![atomic_core::change::FileOps::projected_span_edit(
                path.into(),
                file.clone(),
                baseline,
            )];
            let hash = repo.save_change(&forged).unwrap();
            let state = repo.get_view_info(repo.current_view()).unwrap().state;
            let working = repo
                .operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap();
            let global = repo
                .operation_log(OperationScope::Repository, None, false)
                .unwrap();
            let error = repo
                .insert_change(&hash, atomic_repository::InsertOptions::default())
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(
                repo.get_view_info(repo.current_view()).unwrap().state,
                state
            );
            assert_eq!(
                repo.operation_log(OperationScope::WorkingCopy(wc), None, false)
                    .unwrap(),
                working
            );
            assert_eq!(
                repo.operation_log(OperationScope::Repository, None, false)
                    .unwrap(),
                global
            );
            assert_eq!(
                std::fs::read(target.path().join("hot.txt")).unwrap(),
                b"alpha\nnew content\nlast\n"
            );
        }
    }

    #[test]
    fn projected_record_object_collision_aborts_prepared_operation() {
        use atomic_core::operation::{OperationKind, OperationScope};
        use atomic_core::pristine::SPAN_PROJECTED_SEMANTICS_CAPABILITY;
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        std::fs::write(&path, b"alpha\nnew content\nlast\n").unwrap();
        let header = ChangeHeader::new("deterministic collision");
        let dry = repo
            .record(
                wc,
                header.clone(),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(false)
                    .apply_after_record(false),
            )
            .unwrap();
        repo.pristine()
            .require_repository_capability(SPAN_PROJECTED_SEMANTICS_CAPABILITY)
            .unwrap();
        let mut alternate = dry.change().clone();
        alternate.unhashed = Some(serde_json::json!({"retained": "user metadata"}));
        assert_eq!(repo.save_change(&alternate).unwrap(), *dry.hash());
        let stored = repo.change_store().change_path(dry.hash());
        let before_bytes = std::fs::read(&stored).unwrap();
        let before_state = repo.get_view_info(repo.current_view()).unwrap().state;
        let error = repo
            .record(
                wc,
                header,
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("refusing to overwrite"),
            "{error}"
        );
        assert_eq!(std::fs::read(&stored).unwrap(), before_bytes);
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nnew content\nlast\n");
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before_state
        );
        let working = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        let head = working.entries.iter().find(|entry| entry.is_head).unwrap();
        assert_eq!(head.operation.payload().kind, OperationKind::Recover);
        assert_eq!(
            head.verification,
            atomic_repository::repository::OperationVerificationState::Verified
        );
        assert!(!repo
            .operation_details(head.operation.id())
            .unwrap()
            .receipts
            .is_empty());
        let global = repo
            .operation_log(OperationScope::Repository, None, false)
            .unwrap();
        assert!(global
            .entries
            .iter()
            .any(|entry| entry.operation.id() == head.operation.id()));
        drop(repo);
        let repo = Repository::open(target.path()).unwrap();
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before_state
        );
        assert_eq!(std::fs::read(stored).unwrap(), before_bytes);
    }

    #[test]
    fn stale_projected_record_refuses_overlap_before_graph_commit() {
        use atomic_core::operation::OperationScope;
        let (_git_dir, git, tip, _) = fixture(2);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                span_stream: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        let path = target.path().join("hot.txt");
        std::fs::write(&path, b"alpha\nleft\nlast\n").unwrap();
        let pending = target.path().join("pending.txt");
        std::fs::write(&pending, b"new user file\n").unwrap();
        repo.add(wc, "pending.txt", Default::default()).unwrap();
        let stale = repo
            .record(
                wc,
                ChangeHeader::new("stale edit"),
                atomic_repository::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(false)
                    .apply_after_record(false),
            )
            .unwrap();
        assert!(stale.change().hunks().iter().any(|op| matches!(op, atomic_core::change::GraphOp::FileAdd { path, .. } if path == "pending.txt")));
        std::fs::write(&path, b"alpha\nright\nlast\n").unwrap();
        repo.record(
            wc,
            ChangeHeader::new("concurrent edit"),
            atomic_repository::RecordOptions::new()
                .add_path("hot.txt")
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
        assert_eq!(repo.save_change(stale.change()).unwrap(), *stale.hash());
        let before = repo.get_view_info(repo.current_view()).unwrap().state;
        let working = repo
            .operation_log(OperationScope::WorkingCopy(wc), None, false)
            .unwrap();
        let global = repo
            .operation_log(OperationScope::Repository, None, false)
            .unwrap();
        let journal = target.path().join(".atomic/deferred-tree-ops.json");
        let before_journal = std::fs::read(&journal).ok();
        let error = repo
            .write_recorded(&stale, atomic_repository::InsertOptions::default())
            .unwrap_err();
        assert!(
            error.to_string().contains("overlap") || error.to_string().contains("ambiguous"),
            "{error}"
        );
        assert_eq!(
            repo.get_view_info(repo.current_view()).unwrap().state,
            before
        );
        assert_eq!(
            repo.operation_log(OperationScope::WorkingCopy(wc), None, false)
                .unwrap(),
            working
        );
        assert_eq!(
            repo.operation_log(OperationScope::Repository, None, false)
                .unwrap(),
            global
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nright\nlast\n");
        assert_eq!(std::fs::read(&pending).unwrap(), b"new user file\n");
        assert_eq!(std::fs::read(&journal).ok(), before_journal);
        assert_eq!(
            repo.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nright\nlast\n".to_vec())
        );
        drop(repo);
        let reopened = Repository::open(target.path()).unwrap();
        assert_eq!(
            reopened.get_file_content("hot.txt").unwrap(),
            Some(b"alpha\nright\nlast\n".to_vec())
        );
    }

    #[test]
    fn rolling_sample_matches_direct_import_and_advances_window() {
        let (_git_dir, git, tip, _) = fixture(32);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let (rolling, advances, peak, _, _, _) =
            rolling_sample(&importer, &mut repo, tip, 8, 3, 1024 * 1024).unwrap();
        assert_eq!(rolling.commits, 32);
        assert!(advances >= 5);
        assert_eq!(peak, 8);
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);

        let other = tempfile::tempdir().unwrap();
        let mut baseline = Repository::init(other.path()).unwrap();
        let ordinary = importer
            .import_graph_stream(
                &mut baseline,
                tip,
                GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        assert_eq!(rolling.state, ordinary.state);
        let a: Vec<_> = repo
            .effective_history(None)
            .unwrap()
            .iter()
            .map(|e| e.hash)
            .collect();
        let b: Vec<_> = baseline
            .effective_history(None)
            .unwrap()
            .iter()
            .map(|e| e.hash)
            .collect();
        assert_eq!(a, b);
        assert_eq!(fs::read_dir(other.path()).unwrap().count(), 1);
    }

    #[test]
    fn production_rolling_is_graph_only_and_matches_direct_api() {
        let (_git_dir, git, tip, _) = fixture(32);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let limits = GitStreamLimits::new(3, 32 * 1024 * 1024).unwrap();
        let plan = importer
            .validate_graph_stream_prospectively("main", tip, limits)
            .unwrap();
        let stream = plan.stream.as_ref().unwrap();
        assert!(plan.commits.is_empty());
        assert_eq!(stream.ordered.len(), 32);
        let outcome = importer
            .import_graph_stream_rolling(&mut repo, tip, limits, &stream.ordered)
            .unwrap();
        assert_eq!(
            outcome.batches, 11,
            "hot files must not force identity flushes"
        );
        for entry in repo.effective_history(None).unwrap() {
            assert!(repo.load_change(&entry.hash).unwrap().file_ops().is_empty());
        }
        let other = tempfile::tempdir().unwrap();
        let mut baseline = Repository::init(other.path()).unwrap();
        let direct = importer
            .import_graph_stream(
                &mut baseline,
                tip,
                GitStreamLimits::new(1, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        assert_eq!(outcome.state, direct.state);
        let hashes = |repo: &Repository| {
            repo.effective_history(None)
                .unwrap()
                .into_iter()
                .map(|e| e.hash)
                .collect::<Vec<_>>()
        };
        assert_eq!(hashes(&repo), hashes(&baseline));
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn production_rolling_refills_under_byte_pressure() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let signature = git2::Signature::new(
            "Rolling",
            "rolling@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let mut prior = None;
        for index in 0..10 {
            let parent = prior.map(|oid| git.find_commit(oid).unwrap());
            let old_tree = parent.as_ref().map(|commit| commit.tree().unwrap());
            let mut builder = git.treebuilder(old_tree.as_ref()).unwrap();
            let mut bytes = vec![0u8; 256 * 1024];
            bytes[1] = index as u8;
            builder
                .insert(format!("{index}.bin"), git.blob(&bytes).unwrap(), 0o100644)
                .unwrap();
            let tree = git.find_tree(builder.write().unwrap()).unwrap();
            prior = Some(
                git.commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    &format!("commit {index}"),
                    &tree,
                    &parent.iter().collect::<Vec<_>>(),
                )
                .unwrap(),
            );
        }
        let tip = prior.unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let limits = GitStreamLimits::new(100, 1024 * 1024).unwrap();
        let plan = importer
            .validate_graph_stream_prospectively("main", tip, limits)
            .unwrap();
        let stream = plan.stream.as_ref().unwrap();
        let outcome = importer
            .import_graph_stream_rolling(&mut repo, tip, limits, &stream.ordered)
            .unwrap();
        assert_eq!(outcome.commits, 10);
        assert!(
            outcome.batches >= 3,
            "byte pressure must force refills and publications"
        );
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn graph_stream_retains_suffix_bytes_after_repeated_edits() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let signature = git2::Signature::new(
            "Graph stream",
            "stream@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let versions: &[&[u8]] = &[
            b"a\nb\nc\n",
            b"a\nx\nc\n",
            b"a\ny\nc\n",
            b"a\nx\nmiddle\nc\n",
            b"top\na\nx\nmiddle\nc\n",
            b"top\nc\n",
        ];
        let mut prior = None;
        let mut tips = Vec::new();
        for bytes in versions {
            let mut files = Files::new();
            files.insert("hot.txt".into(), (bytes.to_vec(), 0o100644));
            let tree = git.find_tree(tree(&git, &files, "")).unwrap();
            let parent = prior.map(|id| git.find_commit(id).unwrap());
            let tip = git
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "edit hot file",
                    &tree,
                    &parent.iter().collect::<Vec<_>>(),
                )
                .unwrap();
            tips.push(tip);
            prior = Some(tip);
        }
        for (tip, bytes) in tips.into_iter().zip(versions) {
            let target = tempfile::tempdir().unwrap();
            let mut repo = Repository::init(target.path()).unwrap();
            let importer = ParallelImporter::new(
                &git,
                ParallelImportOptions {
                    graph_only: true,
                    target_view: repo.current_view().into(),
                    ..Default::default()
                },
            );
            let limits = GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap();
            let plan = importer
                .validate_graph_stream_prospectively("main", tip, limits)
                .unwrap();
            let ordered = &plan.stream.as_ref().unwrap().ordered;
            let outcome = importer
                .import_graph_stream_rolling(&mut repo, tip, limits, ordered)
                .unwrap();
            assert_eq!(
                outcome.batches, 1,
                "repeated edits share a graph-only batch"
            );
            for entry in repo.effective_history(None).unwrap() {
                assert!(repo.load_change(&entry.hash).unwrap().file_ops().is_empty());
            }
            assert_eq!(
                repo.get_file_content("hot.txt").unwrap().as_deref(),
                Some(*bytes)
            );
            assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn rolling_sample_applies_blob_budget_and_refuses_oversized_commit() {
        let (_git_dir, git, tip, _) = fixture(32);
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let (_, _, _, peak_bytes, stalls, _) =
            rolling_sample(&importer, &mut repo, tip, 8, 3, 80).unwrap();
        assert!(stalls > 0);
        assert!(peak_bytes <= 80);
        let other = tempfile::tempdir().unwrap();
        let mut refused = Repository::init(other.path()).unwrap();
        assert!(rolling_sample(&importer, &mut refused, tip, 8, 3, 1).is_err());
        assert_eq!(refused.effective_history(None).unwrap().len(), 0);
    }

    #[test]
    fn rolling_sample_refuses_late_atomic_claim_before_publishing() {
        let (_git_dir, git, tip, mut files) = fixture(16);
        files.insert("late-file".into(), (b"target".to_vec(), 0o100644));
        let tree = git.find_tree(tree(&git, &files, "")).unwrap();
        let parent = git.find_commit(tip).unwrap();
        let signature = git2::Signature::now("Sample", "sample@example.invalid").unwrap();
        let bad_tip = git
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!(
                    "claimed final commit\n\nAtomic-Changes: {}",
                    ContentHash::of(b"unverified claim").to_base32()
                ),
                &tree,
                &[&parent],
            )
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(target.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        assert!(rolling_sample(&importer, &mut repo, bad_tip, 8, 3, 1024 * 1024).is_err());
        assert!(repo.effective_history(None).unwrap().is_empty());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    #[ignore = "real rolling import sample; set ATOMIC_ROLLING_GIT and ATOMIC_ROLLING_ARTIFACT"]
    fn rolling_sample_hyperfine() {
        let git = GitRepository::open(std::env::var_os("ATOMIC_ROLLING_GIT").unwrap()).unwrap();
        let artifact = PathBuf::from(std::env::var_os("ATOMIC_ROLLING_ARTIFACT").unwrap());
        fs::create_dir_all(&artifact).unwrap();
        assert!(
            !artifact.join("repo").exists(),
            "sample target must be fresh"
        );
        let mut repo = Repository::init(artifact.join("repo")).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let tip = Oid::from_str("c5161524980522320c7d3425e714cc0965a869a5").unwrap();
        let ordered = importer.collect_commit_oids_from_tip(&git, tip).unwrap();
        assert_eq!(ordered.len(), 1097);
        let start = Instant::now();
        let window_mib: usize = std::env::var("ATOMIC_ROLLING_WINDOW_MIB")
            .unwrap_or_else(|_| "512".into())
            .parse()
            .unwrap();
        let window_bytes = window_mib.checked_mul(1024 * 1024).unwrap();
        let (outcome, advances, peak, peak_bytes, stalls, preflight_seconds) =
            rolling_sample(&importer, &mut repo, tip, 500, 100, window_bytes).unwrap();
        let elapsed_seconds = start.elapsed().as_secs_f64();
        assert_eq!(outcome.commits, ordered.len());
        assert!(advances >= 5);
        assert!(peak <= 500);
        assert_eq!(fs::read_dir(repo.root()).unwrap().count(), 1);
        drop(repo);
        let repo = Repository::open(artifact.join("repo")).unwrap();
        assert_eq!(repo.effective_history(None).unwrap().len(), ordered.len());
        for oid in &ordered {
            let hash = repo.checked_git_sha(&oid.to_string()).unwrap().unwrap();
            assert!(repo.load_change(&hash).unwrap().file_ops().is_empty());
        }
        assert!(!artifact.join("baseline").exists());
        let mut baseline = Repository::init(artifact.join("baseline")).unwrap();
        let baseline_start = Instant::now();
        let direct = importer
            .import_graph_stream(
                &mut baseline,
                tip,
                GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let baseline_seconds = baseline_start.elapsed().as_secs_f64();
        assert_eq!(outcome.state, direct.state);
        let rolling_history: Vec<_> = repo
            .effective_history(None)
            .unwrap()
            .iter()
            .map(|entry| entry.hash)
            .collect();
        let baseline_history: Vec<_> = baseline
            .effective_history(None)
            .unwrap()
            .iter()
            .map(|entry| entry.hash)
            .collect();
        assert_eq!(rolling_history, baseline_history);
        assert_eq!(fs::read_dir(baseline.root()).unwrap().count(), 1);
        let report = serde_json::json!({
            "commits": outcome.commits, "batches": outcome.batches,
            "window_size": 500, "batch_size": 100, "peak_window_commits": peak,
            "window_blob_budget": window_bytes, "peak_reserved_blob_bytes": peak_bytes,
            "blob_budget_stalls": stalls,
            "window_advances": advances, "preflight_seconds": preflight_seconds,
            "elapsed_seconds": elapsed_seconds, "state": outcome.state.to_base32(),
            "direct_api_baseline_seconds": baseline_seconds, "ordered_hashes_match_baseline": true,
            "private_publish_seconds": outcome.private_publish_seconds,
            "final_proof_promotion_seconds": outcome.final_proof_promotion_seconds,
            "historical_graph_states_verified": false, "materialized": false,
            "checkout_adoption_preflight": false, "hard_rss_bound": false,
        });
        fs::write(
            artifact.join("rolling-result.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("ROLLING RESULT {report}");
    }

    #[test]
    #[ignore = "separate-process direct import RSS baseline; set ATOMIC_ROLLING_GIT, ATOMIC_ROLLING_ARTIFACT and ATOMIC_ROLLING_MEMORY_LOG"]
    fn rolling_memory_direct_baseline() {
        assert!(std::env::var_os("ATOMIC_ROLLING_MEMORY_LOG").is_some());
        let git = GitRepository::open(std::env::var_os("ATOMIC_ROLLING_GIT").unwrap()).unwrap();
        let artifact = PathBuf::from(std::env::var_os("ATOMIC_ROLLING_ARTIFACT").unwrap());
        fs::create_dir_all(&artifact).unwrap();
        assert!(!artifact.join("direct-baseline").exists());
        let mut repo = Repository::init(artifact.join("direct-baseline")).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let tip = Oid::from_str("c5161524980522320c7d3425e714cc0965a869a5").unwrap();
        rolling_memory_sample("before_direct", 0, 0, 0, 0).unwrap();
        let start = Instant::now();
        let outcome = importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        let elapsed_seconds = start.elapsed().as_secs_f64();
        rolling_memory_sample(
            "after_direct_final_proof",
            outcome.commits,
            outcome.commits,
            0,
            0,
        )
        .unwrap();
        assert_eq!(outcome.commits, 1097);
        assert_eq!(fs::read_dir(repo.root()).unwrap().count(), 1);
        let report = serde_json::json!({"commits": outcome.commits, "elapsed_seconds": elapsed_seconds,
            "state": outcome.state.to_base32(), "materialized": false});
        fs::write(
            artifact.join("direct-baseline-result.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("DIRECT MEMORY BASELINE {report}");
    }

    #[test]
    fn direct_stream_mixed_history_never_uses_ordinary_recorder() {
        let (_git_dir, git, tip, expected) = fixture(32);
        let mut expected_state = None;
        for batch in [1, 4, 16, 100] {
            let target = tempfile::tempdir().unwrap();
            let mut repo = Repository::init(target.path()).unwrap();
            let importer = ParallelImporter::new(
                &git,
                ParallelImportOptions {
                    graph_only: true,
                    target_view: repo.current_view().into(),
                    ..Default::default()
                },
            );
            let outcome = importer
                .import_graph_stream(
                    &mut repo,
                    tip,
                    GitStreamLimits::new(batch, 32 * 1024 * 1024).unwrap(),
                )
                .unwrap();
            assert_eq!(outcome.commits, 32);
            if let Some(state) = expected_state {
                assert_eq!(outcome.state, state, "batch size changed history identity");
            } else {
                expected_state = Some(outcome.state);
            }
            assert_eq!(outcome.batches, 32usize.div_ceil(batch));
            assert_eq!(
                std::fs::read_dir(target.path()).unwrap().count(),
                1,
                "no intermediate checkout"
            );
            for (path, (bytes, mode)) in &expected {
                assert_eq!(
                    repo.get_file_content(path).unwrap().as_deref(),
                    Some(bytes.as_slice())
                );
                let attrs = repo
                    .projected_attributes_on_view_excluding(path, repo.current_view(), &[])
                    .unwrap()
                    .unwrap();
                assert_eq!(attrs.materialization.mode, (*mode & 0o777) as u16);
            }
            let history = repo.effective_history(None).unwrap();
            assert_eq!(history.len(), 32);
            for entry in history {
                let change = repo.load_change(&entry.hash).unwrap();
                assert!(change.file_ops().is_empty());
                let sha = change.unhashed.as_ref().unwrap()["git"]["sha"]
                    .as_str()
                    .unwrap();
                assert_eq!(repo.checked_git_sha(sha).unwrap(), Some(entry.hash));
            }
            let wc = repo.require_working_copy_id().unwrap();
            repo.materialize(wc).unwrap();
            assert!(repo
                .status(wc, atomic_repository::StatusOptions::default())
                .unwrap()
                .is_clean());
        }
    }

    #[test]
    fn direct_stream_refuses_eager_semantics_before_publication() {
        let (_git_dir, git, tip, _) = fixture(2);
        let temp = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(temp.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        assert!(importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap()
            )
            .is_err());
        assert!(repo.effective_history(None).unwrap().is_empty());
    }

    #[test]
    fn direct_stream_refuses_binding_transport_before_publication() {
        let (_input, git, tip, _) = fixture(1);
        git.reference("refs/atomic/bindings/00/0000000000000000000000000000000000000000000000000000000000000000", tip, false, "binding fixture").unwrap();
        let output = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(output.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let error = importer
            .import_graph_stream(
                &mut repo,
                tip,
                GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("binding transport"));
        assert!(repo.effective_history(None).unwrap().is_empty());
        assert!(!repo.view_exists("git-stream-private").unwrap());
    }

    #[test]
    fn direct_stream_typechange_tip_matches_existing_git_materialization() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let mut previous = None;
        for (index, (path, bytes)) in [
            ("flip", b"file\n".as_slice()),
            ("flip/child", b"inside\n".as_slice()),
            ("flip", b"back\n".as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let mut files = Files::new();
            files.insert(path.into(), (bytes.to_vec(), 0o100644));
            let tree = git.find_tree(tree(&git, &files, "")).unwrap();
            let sig = git2::Signature::new(
                "Stream",
                "stream@example.invalid",
                &git2::Time::new(1_700_000_000 + index as i64, 0),
            )
            .unwrap();
            let parent = previous.map(|oid| git.find_commit(oid).unwrap());
            previous = Some(
                git.commit(
                    Some("refs/heads/main"),
                    &sig,
                    &sig,
                    path,
                    &tree,
                    &parent.iter().collect::<Vec<_>>(),
                )
                .unwrap(),
            );
        }
        let output = tempfile::tempdir().unwrap();
        // Match the CLI's supported contract: Git has already materialized
        // the final checkout. Import must leave it unchanged, not replay
        // historical filesystem effects. Generic fresh-directory materializer
        // handling of old file/dir tombstones is a separate concern.
        fs::write(output.path().join("flip"), b"back\n").unwrap();
        let modified = fs::metadata(output.path().join("flip"))
            .unwrap()
            .modified()
            .unwrap();
        let mut repo = Repository::init(output.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        importer
            .import_graph_stream(
                &mut repo,
                previous.unwrap(),
                GitStreamLimits::new(2, 32 * 1024 * 1024).unwrap(),
            )
            .unwrap();
        assert_eq!(
            repo.get_file_content("flip").unwrap(),
            Some(b"back\n".to_vec())
        );
        assert_eq!(repo.get_file_content("flip/child").unwrap(), None);
        assert_eq!(
            fs::metadata(output.path().join("flip"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        let wc = repo.require_working_copy_id().unwrap();
        assert!(repo
            .status(wc, atomic_repository::StatusOptions::default())
            .unwrap()
            .is_clean());
    }

    #[test]
    fn direct_stream_merge_uses_first_parent_delta_not_side_parent_union() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let sig = git2::Signature::new(
            "Stream",
            "stream@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let mut files = Files::new();
        files.insert("common.txt".into(), (b"base\n".to_vec(), 0o100644));
        let root_tree = git.find_tree(tree(&git, &files, "")).unwrap();
        let root = git
            .commit(None, &sig, &sig, "root", &root_tree, &[])
            .unwrap();
        let mut side_files = files.clone();
        side_files.insert("side.txt".into(), (b"side\n".to_vec(), 0o100644));
        let side_tree = git.find_tree(tree(&git, &side_files, "")).unwrap();
        let side = git
            .commit(
                None,
                &sig,
                &sig,
                "side",
                &side_tree,
                &[&git.find_commit(root).unwrap()],
            )
            .unwrap();
        files.insert(
            ".github/dependabot.yml".into(),
            (b"version: 2\n".to_vec(), 0o100644),
        );
        let main_tree = git.find_tree(tree(&git, &files, "")).unwrap();
        let main = git
            .commit(
                None,
                &sig,
                &sig,
                "main",
                &main_tree,
                &[&git.find_commit(root).unwrap()],
            )
            .unwrap();
        files.insert("side.txt".into(), (b"side\n".to_vec(), 0o100644));
        let merge_tree = git.find_tree(tree(&git, &files, "")).unwrap();
        let tip = git
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "merge",
                &merge_tree,
                &[
                    &git.find_commit(main).unwrap(),
                    &git.find_commit(side).unwrap(),
                ],
            )
            .unwrap();
        let output = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(output.path()).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        assert_eq!(
            importer
                .import_graph_stream(
                    &mut repo,
                    tip,
                    GitStreamLimits::new(2, 32 * 1024 * 1024).unwrap()
                )
                .unwrap()
                .commits,
            3
        );
        assert_eq!(
            repo.get_file_content(".github/dependabot.yml").unwrap(),
            Some(b"version: 2\n".to_vec())
        );
        assert_eq!(
            repo.get_file_content("side.txt").unwrap(),
            Some(b"side\n".to_vec())
        );
        assert!(repo.checked_git_sha(&side.to_string()).unwrap().is_none());
        let hash = repo.checked_git_sha(&tip.to_string()).unwrap().unwrap();
        let change = repo.load_change(&hash).unwrap();
        assert!(
            matches!(change.origin(),atomic_core::change::ChangeOrigin::GitSynthesized { parents, .. } if parents.len()==2)
        );
    }

    #[test]
    fn direct_stream_sibling_retirement_has_deterministic_hashes() {
        let input = tempfile::tempdir().unwrap();
        let git = GitRepository::init_bare(input.path()).unwrap();
        let sig = git2::Signature::new(
            "Stream",
            "stream@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let mut previous = None;
        for step in 0..3 {
            let mut files = Files::new();
            if step != 1 {
                files.insert("left/a".into(), (b"a\n".to_vec(), 0o100644));
                files.insert("right/b".into(), (b"b\n".to_vec(), 0o100644));
            }
            let tree = git.find_tree(tree(&git, &files, "")).unwrap();
            let parent = previous.map(|oid| git.find_commit(oid).unwrap());
            previous = Some(
                git.commit(
                    Some("refs/heads/main"),
                    &sig,
                    &sig,
                    &format!("step {step}"),
                    &tree,
                    &parent.iter().collect::<Vec<_>>(),
                )
                .unwrap(),
            );
        }
        let mut state = None;
        for batch in [1, 2, 100] {
            let output = tempfile::tempdir().unwrap();
            let mut repo = Repository::init(output.path()).unwrap();
            let importer = ParallelImporter::new(
                &git,
                ParallelImportOptions {
                    graph_only: true,
                    target_view: repo.current_view().into(),
                    ..Default::default()
                },
            );
            let outcome = importer
                .import_graph_stream(
                    &mut repo,
                    previous.unwrap(),
                    GitStreamLimits::new(batch, 32 * 1024 * 1024).unwrap(),
                )
                .unwrap();
            if let Some(expected) = state {
                assert_eq!(outcome.state, expected);
            } else {
                state = Some(outcome.state);
            }
        }
    }

    #[test]
    #[ignore = "direct Git-to-graph Hyperfine canary; set ATOMIC_DIRECT_STREAM_GIT and ATOMIC_DIRECT_STREAM_ARTIFACT"]
    fn direct_stream_hyperfine() {
        let git =
            GitRepository::open(std::env::var_os("ATOMIC_DIRECT_STREAM_GIT").unwrap()).unwrap();
        let artifact = PathBuf::from(std::env::var_os("ATOMIC_DIRECT_STREAM_ARTIFACT").unwrap());
        fs::create_dir_all(&artifact).unwrap();
        assert!(!artifact.join("repo").exists());
        let mut repo = Repository::init(artifact.join("repo")).unwrap();
        let importer = ParallelImporter::new(
            &git,
            ParallelImportOptions {
                graph_only: true,
                target_view: repo.current_view().into(),
                ..Default::default()
            },
        );
        let all = importer
            .collect_commit_oids_from_tip(
                &git,
                Oid::from_str("c5161524980522320c7d3425e714cc0965a869a5").unwrap(),
            )
            .unwrap();
        let count: usize = std::env::var("ATOMIC_DIRECT_STREAM_COMMITS")
            .unwrap_or_else(|_| "100".into())
            .parse()
            .unwrap();
        assert!(count > 0 && count <= all.len());
        let started = Instant::now();
        let result = importer.import_graph_stream(
            &mut repo,
            all[count - 1],
            GitStreamLimits::new(100, 32 * 1024 * 1024).unwrap(),
        );
        let elapsed = started.elapsed().as_secs_f64();
        let mut report = serde_json::json!({"requested":count,"elapsed_seconds":elapsed,"direct":true,"ordinary_recorder_trap_enabled":true,"source_preparation_pass":false,"graph_only":true,"historical_verification":false,"post_checks":false,"materialized":false,"build_label":std::env::var("ATOMIC_DIRECT_STREAM_BUILD_LABEL").unwrap_or_else(|_|"unoptimized test".into()),"result":result.as_ref().ok().map(|outcome| serde_json::json!({"commits":outcome.commits,"batches":outcome.batches,"state":outcome.state.to_base32(),"private_publish_seconds":outcome.private_publish_seconds,"final_proof_promotion_seconds":outcome.final_proof_promotion_seconds})),"error":result.as_ref().err().map(ToString::to_string)});
        fs::write(
            artifact.join("direct-result.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("DIRECT STREAM {report}");
        result.unwrap();
        assert_eq!(fs::read_dir(repo.root()).unwrap().count(), 1);
        drop(repo);
        let repo = Repository::open(artifact.join("repo")).unwrap();
        assert_eq!(repo.effective_history(None).unwrap().len(), count);
        for oid in &all[..count] {
            let hash = repo.checked_git_sha(&oid.to_string()).unwrap().unwrap();
            assert!(repo.load_change(&hash).unwrap().file_ops().is_empty());
        }
        let wc = repo.require_working_copy_id().unwrap();
        repo.materialize(wc).unwrap();
        assert!(repo
            .status(wc, atomic_repository::StatusOptions::default())
            .unwrap()
            .is_clean());
        report["post_checks"] = serde_json::json!(true);
        report["materialized"] = serde_json::json!(true);
        fs::write(
            artifact.join("direct-result.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("DIRECT STREAM POST CHECKS {report}");
    }
}
