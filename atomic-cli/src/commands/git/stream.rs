//! Direct first-parent Git ODB -> in-memory graph compiler -> private batches.
//! No ordinary recorder, prepared-source repository, replay spool or checkout.

use super::*;
use atomic_core::change::InodeAttr;
use atomic_repository::{
    GitGraphImportSession, GitStreamLimits, GitStreamOutcome, PreparedGitGraphChange,
};

// Applies to each commit's input blobs and to compiled private batch payloads,
// not total process memory. Keep room for large initial asset commits.
const DEFAULT_STREAM_BYTE_LIMIT: usize = 128 * 1024 * 1024;

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
    if bytes > limit {
        return Err(git_error(format!("direct stream commit {} needs {bytes} input bytes, above {limit}; increase the explicit bound", commit.id())));
    }
    Ok(())
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
                    path.clone(),
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
                path: file.path.clone(),
                value: InodeAttr::Mode(expected.mode()),
            });
            hunks.push(GraphOp::SetAttr {
                inode,
                path: file.path.clone(),
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
                    .checked_sub(extract_filename(path).len() as u64)
                    .ok_or_else(|| git_error("invalid stream directory name span"))?,
            );
            let flags = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
            hunks.push(GraphOp::DirDel {
                path: path.clone(),
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
        let prepared =
            PreparedGitGraphChange::new(compiled, origin, foreign_commit_facts(parsed)?, unhashed)
                .map_err(CliError::from)?;
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
    pub(crate) fn import_graph_stream_prevalidated(
        &self,
        repo: &mut Repository,
        plan: ProspectiveImportPlan,
    ) -> CliResult<ImportStats> {
        let tip = plan
            .commits
            .last()
            .ok_or_else(|| git_error("direct stream needs a nonempty history"))?
            .parsed
            .git_sha
            .clone();
        let total = plan.commits.len();
        let merges = plan
            .commits
            .iter()
            .filter(|commit| commit.parsed.is_merge)
            .count();
        let empty = plan
            .commits
            .iter()
            .filter(|commit| commit.parsed.is_empty && !commit.parsed.is_merge)
            .count();
        let files = plan
            .commits
            .iter()
            .map(|commit| commit.parsed.files.len())
            .sum();
        // Preserve existing command preflight/adoption, release its historical
        // payloads here, and never run a recorder-backed preparation pass.
        drop(plan);
        let started = Instant::now();
        let outcome = self.import_graph_stream(
            repo,
            Oid::from_str(&tip).map_err(|e| git_error(e.to_string()))?,
            GitStreamLimits::new(100, DEFAULT_STREAM_BYTE_LIMIT).map_err(CliError::from)?,
        )?;
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
    pub(crate) fn validate_graph_stream_input(&self, tip: Oid) -> CliResult<()> {
        self.graph_stream_inputs(tip).map(|_| ())
    }

    fn graph_stream_inputs(
        &self,
        tip: Oid,
    ) -> CliResult<(GitRepository, ConversionPolicy, Vec<Oid>)> {
        let git = self.open_git_repo()?;
        for reference in git
            .references()
            .map_err(|error| git_error(error.to_string()))?
        {
            let reference = reference.map_err(|error| git_error(error.to_string()))?;
            if reference.name_bytes().starts_with(b"refs/atomic/bindings/") {
                return Err(git_error("direct stream does not restore Atomic binding transport; use ordinary import explicitly"));
            }
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
                return Err(git_error("direct stream refuses Atomic provenance/binding claims; use ordinary import explicitly"));
            }
            let tree = commit.tree().map_err(|e| git_error(e.to_string()))?;
            // Share the ordinary importer's kind/mode validation. The parser
            // already carries raw Git names through the reversible escape;
            // there is no stream-specific character or UTF-8 restriction.
            validate_import_tree_capabilities(&git, &tree, "")?;
        }
        Ok((git, policy, ordered))
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
        let mut session = GitGraphImportSession::begin(repo, limits).map_err(CliError::from)?;
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
        session.finish(&project, &proof).map_err(CliError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            for batch in [1, 100] {
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
                let outcome = importer
                    .import_graph_stream(
                        &mut repo,
                        *tip,
                        GitStreamLimits::new(batch, 32 * 1024 * 1024).unwrap(),
                    )
                    .unwrap_or_else(|error| panic!("tip {tip}, batch {batch}: {error}"));
                if let Some(state) = state {
                    assert_eq!(outcome.state, state, "batch size changed identity at {tip}");
                } else {
                    state = Some(outcome.state);
                }
                let source = git.find_commit(*tip).unwrap().tree_id();
                let actual = repo.project_tree(repo.current_view(), &policy).unwrap();
                assert_eq!(
                    actual.git.root.as_bytes(),
                    source.as_bytes(),
                    "raw Git tree bytes must survive at {tip}"
                );
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
        let edit = raw_commit(&git, &files, Some(root));
        files.insert(
            b"d\xffr/recreated file".to_vec(),
            (b"again\n".to_vec(), 0o100755),
        );
        let tip = raw_commit(&git, &files, Some(edit));
        assert_raw_history(&git, &[root, edit, tip]);
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
    fn direct_stream_default_budget_accepts_commits_above_32_mib() {
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
                GitStreamLimits::new(100, DEFAULT_STREAM_BYTE_LIMIT).unwrap(),
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
