//! Direct first-parent Git ODB -> in-memory graph compiler -> private batches.
//! No ordinary recorder, prepared-source repository, replay spool or checkout.

use super::*;
use atomic_core::change::InodeAttr;
use atomic_repository::{
    GitGraphImportSession, GitStreamLimits, GitStreamOutcome, PreparedGitGraphChange,
};

#[path = "rolling_dashboard.rs"]
mod rolling_dashboard;

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

fn input_budget(git: &GitRepository, commit: &git2::Commit<'_>, limit: usize) -> ShimResult<()> {
    let bytes = input_blob_bytes(git, commit)?;
    if bytes > limit {
        return Err(git_error(format!("direct stream commit {} needs {bytes} input bytes, above {limit}; increase the explicit bound", commit.id())));
    }
    Ok(())
}

fn input_blob_bytes(git: &GitRepository, commit: &git2::Commit<'_>) -> ShimResult<usize> {
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
}

impl Compiler {
    fn compile(
        &mut self,
        parsed: &ParsedCommit,
        policy: &ConversionPolicy,
        boundaries: &[&'static str],
        mut provenance: serde_json::Value,
    ) -> ShimResult<PreparedGitGraphChange> {
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
        let compiled = Change::new(header, hunks, change.contents, deps);
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let parents = parsed
            .parent_oids
            .iter()
            .map(|oid| git_object_id(algorithm, *oid))
            .collect::<ShimResult<Vec<_>>>()?;
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
        .map_err(ShimError::from)?;
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
        let prepared = PreparedGitGraphChange::new(compiled, origin, facts, unhashed)
            .map_err(ShimError::from)?;
        let hash = prepared.hash();
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
    ) -> ShimResult<ProspectiveImportPlan> {
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
    ) -> ShimResult<ImportStats> {
        let stream = plan
            .stream
            .ok_or_else(|| git_error("missing compact stream preflight"))?;
        let total = stream.ordered.len();
        let (merges, empty, files) = (stream.merges, stream.empty, stream.files);
        let started = Instant::now();
        let outcome =
            self.import_graph_stream_rolling(repo, stream.tip, limits, &stream.ordered)?;
        print_info(&format!("Direct graph stream: {} commits in {} private batches; final tip verified (historical states not independently verified)", outcome.commits, outcome.batches));
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
    fn graph_stream_inputs(
        &self,
        tip: Oid,
    ) -> ShimResult<(GitRepository, ConversionPolicy, Vec<Oid>)> {
        let git = self.open_git_repo()?;
        if crate::commands::import::has_binding_transport(&git)? {
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
    ) -> ShimResult<GitStreamOutcome> {
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
        let mut session = GitGraphImportSession::begin(repo, limits).map_err(ShimError::from)?;
        let enabled = std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_some_and(|term| term != "dumb");
        let mut dashboard = rolling_dashboard::Dashboard::start(500, limits.bytes(), enabled);
        dashboard.update(|state| {
            state.total = ordered.len();
            state.checked = ordered.len();
        });
        let mut compiler = Compiler::default();
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
                    session.publish(&batch).map_err(ShimError::from)?;
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
                session.publish(&batch).map_err(ShimError::from)?;
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
    ) -> ShimResult<GitStreamOutcome> {
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
        let mut session = GitGraphImportSession::begin(repo, limits).map_err(ShimError::from)?;
        let mut compiler = Compiler::default();
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
                session.publish(&batch).map_err(ShimError::from)?;
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
            session.publish(&batch).map_err(ShimError::from)?;
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
) -> ShimResult<GitStreamOutcome> {
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
    session.finish(&project, &proof).map_err(ShimError::from)
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
    ) -> ShimResult<()> {
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
    ) -> ShimResult<()> {
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
    ) -> ShimResult<(GitStreamOutcome, usize, usize, usize, usize, f64)> {
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
        let limits = GitStreamLimits::new(batch_size, 32 * 1024 * 1024).map_err(ShimError::from)?;

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

        let mut session = GitGraphImportSession::begin(repo, limits).map_err(ShimError::from)?;
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
                    session.publish(&batch).map_err(ShimError::from)?;
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
                session.publish(&batch).map_err(ShimError::from)?;
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
        let outcome = session.finish(&project, &proof).map_err(ShimError::from)?;
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
