//! Provenance graph helpers and attestation creation for the turn orchestrator.
//!
//! Contains methods for:
//! - Loading/saving the provenance accumulator
//! - Ingesting Sherpa JSONL trace files
//! - Saving turn provenance graphs
//! - Injecting reasoning/thinking blocks
//! - Creating session attestations
//!
//! # Concurrency
//!
//! Each hook event (session-start, user-prompt-submit, post-tool, stop)
//! spawns a separate process.  Multiple events can fire close together,
//! causing concurrent access to the accumulator's `graph.json`.  All
//! load → mutate → save cycles are serialized through an advisory file
//! lock (`{session_dir}/graph.lock`) to prevent corruption.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::event::TurnEvent;
use crate::provenance::accumulator::{build_tool_detail, ProvenanceAccumulator};
use crate::provenance::classify::summarize_tool_call;
use crate::record::TurnRecordOutcome;
use crate::transcript;
use crate::turn::session::AgentSession;
use atomic_core::change::session::SessionTodo;

use super::{truncate_prompt, TurnOrchestrator};

/// Filename for the advisory lock that serializes accumulator access.
const LOCK_FILENAME: &str = "graph.lock";

impl TurnOrchestrator {
    /// Get the session directory for provenance graph storage.
    ///
    /// Returns `{sessions_dir}/{session_id}/` — a subdirectory alongside
    /// the session's JSON file. The `ProvenanceAccumulator` stores its
    /// `graph.json` here.
    pub(crate) fn session_graph_dir(&self, session_id: &str) -> PathBuf {
        self.session_store.sessions_dir().join(session_id)
    }

    /// Execute `f` while holding an exclusive file lock on the session's
    /// accumulator.  The lock file is `{session_dir}/graph.lock`.
    ///
    /// The callback receives a mutable `ProvenanceAccumulator`.  If `f`
    /// returns `true`, the accumulator is saved back to disk before the
    /// lock is released.  If `f` returns `false`, the accumulator is
    /// discarded (read-only access).
    ///
    /// Best-effort: returns `None` if the lock or load fails.
    fn with_accumulator<F>(&self, session_id: &str, f: F) -> Option<ProvenanceAccumulator>
    where
        F: FnOnce(&mut ProvenanceAccumulator) -> bool,
    {
        use fs2::FileExt;

        let dir = self.session_graph_dir(session_id);

        // Ensure the directory exists before creating the lock file.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("Failed to create session dir for {}: {}", session_id, e,);
            return None;
        }

        // Open (or create) the lock file and acquire an exclusive lock.
        let lock_path = dir.join(LOCK_FILENAME);
        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Failed to open lock file for session {}: {}", session_id, e,);
                return None;
            }
        };

        if let Err(e) = lock_file.try_lock_exclusive() {
            if super::is_lock_contended(&e) {
                log::warn!(
                    "Provenance accumulator for session {} is already locked; skipping best-effort provenance update",
                    session_id,
                );
            } else {
                log::warn!("Failed to acquire lock for session {}: {}", session_id, e,);
            }
            return None;
        }

        // Load (or create) the accumulator while holding the lock.
        let mut acc = match ProvenanceAccumulator::load_or_create(&dir, session_id) {
            Ok(a) => a,
            Err(e) => {
                log::warn!(
                    "Failed to load provenance accumulator for {}: {}",
                    session_id,
                    e,
                );
                let _ = lock_file.unlock();
                return None;
            }
        };

        // Run the callback.
        let should_save = f(&mut acc);

        // Save if the callback mutated the accumulator.
        if should_save {
            if let Err(e) = acc.save(&dir) {
                log::warn!(
                    "Failed to save provenance accumulator for {}: {}",
                    session_id,
                    e,
                );
            }
        }

        // Release the lock (also released on drop, but be explicit).
        let _ = lock_file.unlock();

        Some(acc)
    }

    /// Load the provenance accumulator for a session (read-only, locked).
    ///
    /// Best-effort: returns `None` on failure (logged, never fatal).
    pub(crate) fn load_accumulator(&self, session_id: &str) -> Option<ProvenanceAccumulator> {
        self.with_accumulator(session_id, |_| false)
    }

    /// Save the provenance accumulator for a session.
    ///
    /// Best-effort: failures are logged but never fatal.
    pub(crate) fn save_accumulator(&self, session_id: &str, acc: &ProvenanceAccumulator) {
        // We can't reuse with_accumulator here because we already have
        // the accumulator in hand.  Acquire the lock, write, release.
        use fs2::FileExt;

        let dir = self.session_graph_dir(session_id);
        let _ = std::fs::create_dir_all(&dir);
        let lock_path = dir.join(LOCK_FILENAME);

        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Failed to open lock file for session {}: {}", session_id, e);
                return;
            }
        };

        if let Err(e) = lock_file.try_lock_exclusive() {
            if super::is_lock_contended(&e) {
                log::warn!(
                    "Provenance accumulator for session {} is already locked; skipping save",
                    session_id,
                );
            } else {
                log::warn!("Failed to acquire lock for session {}: {}", session_id, e);
            }
            return;
        }

        if let Err(e) = acc.save(&dir) {
            log::warn!(
                "Failed to save provenance accumulator for {}: {}",
                session_id,
                e,
            );
        }

        let _ = lock_file.unlock();
    }

    /// Inject reasoning/thinking blocks from the stop event into the
    /// ProvenanceAccumulator as Decision nodes.
    ///
    /// The OpenCode plugin sends `reasoning_text` (blocks separated by
    /// `\n---\n`) and `reasoning_signature` (last block's Anthropic signature)
    /// in the `stop` payload. Each block becomes a Decision node in the
    /// provenance graph with the full thinking text and signature in its detail.
    pub(crate) fn inject_reasoning_nodes(&mut self, session_id: &str, event: &TurnEvent) {
        let raw = match event.raw_json.as_ref() {
            Some(r) => r,
            None => return,
        };

        let timestamp = event.timestamp.timestamp();

        self.with_accumulator(session_id, |acc| {
            let mut block_count = 0;

            // Prefer the structured `reasoning_blocks` array when available.
            if let Some(blocks_arr) = raw.get("reasoning_blocks").and_then(|v| v.as_array()) {
                for block in blocks_arr {
                    let text = match block.get("text").and_then(|v| v.as_str()) {
                        Some(t) if !t.trim().is_empty() => t.trim(),
                        _ => continue,
                    };
                    let duration_ms = block.get("duration_ms").and_then(|v| v.as_u64());
                    let signature = block.get("signature").and_then(|v| v.as_str());

                    acc.append_reasoning(text, duration_ms, signature, timestamp);
                    block_count += 1;
                }
            } else {
                // Fallback: split concatenated reasoning_text on "\n---\n"
                let reasoning_text = match raw.get("reasoning_text").and_then(|v| v.as_str()) {
                    Some(t) if !t.is_empty() => t,
                    _ => return false,
                };
                let reasoning_signature = raw.get("reasoning_signature").and_then(|v| v.as_str());

                let blocks: Vec<&str> = reasoning_text
                    .split("\n---\n")
                    .filter(|b| !b.trim().is_empty())
                    .collect();

                let total = blocks.len();
                for (i, block) in blocks.iter().enumerate() {
                    let sig = if i == total - 1 {
                        reasoning_signature
                    } else {
                        None
                    };
                    acc.append_reasoning(block.trim(), None, sig, timestamp);
                    block_count += 1;
                }
            }

            if block_count > 0 {
                log::info!(
                    "Injected {} reasoning node{} into provenance for session {}",
                    block_count,
                    if block_count == 1 { "" } else { "s" },
                    session_id,
                );
            }

            block_count > 0
        });
    }

    /// Inject an LLM response node into the ProvenanceAccumulator.
    ///
    /// The agent's closing message for the turn — what it *concluded*, as
    /// opposed to the tool-derived nodes that capture what it *did*.
    /// Sources, in priority order:
    ///
    /// 1. The stop payload, when the agent sends the response there
    ///    (`last_assistant_message` for codex/grok, `prompt_response` for
    ///    gemini-cli, `response` for plugins that supply it).
    /// 2. The last assistant entry of the session transcript (claude-code:
    ///    the Stop hook carries `transcript_path`, applied to the session
    ///    before recording).
    ///
    /// Agents with neither source available get no node.
    pub(crate) fn inject_response_node(
        &mut self,
        session_id: &str,
        session: &AgentSession,
        event: &TurnEvent,
    ) {
        let raw = event.raw_json.as_ref();
        let from_payload = |key: &str| -> Option<String> {
            raw.and_then(|r| r.get(key))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let response = from_payload("last_assistant_message")
            .or_else(|| from_payload("prompt_response"))
            .or_else(|| from_payload("response"))
            .or_else(|| {
                let path = session.transcript_path.as_ref()?;
                let data = std::fs::read(path).ok()?;
                transcript::last_assistant_text(
                    &data,
                    transcript::format_for_agent(&session.agent_name),
                )
            });

        let Some(response) = response else {
            return;
        };

        let timestamp = event.timestamp.timestamp();
        self.with_accumulator(session_id, |acc| {
            acc.append_llm_response(&response, timestamp);
            true
        });
    }

    /// OpenCode: recover transcript, reasoning, and response from
    /// OpenCode's local SQLite store and fold them into the session and
    /// the stop payload before recording.
    ///
    /// OpenCode writes no transcript file, so its sessions never carry a
    /// `transcript_path`, and thin plugin versions forward only
    /// session/model metadata. Without this step an OpenCode turn could
    /// never carry `agent_turn` data, reasoning, or an `llm_response`
    /// node. The recovered data lands exactly where the existing pipeline
    /// reads it:
    ///
    /// - the transcript is synthesized into the session directory and set
    ///   as `session.transcript_path` (consumed by
    ///   `build_unhashed_turn_data` via the `opencode` condense format);
    /// - the turn's reasoning/response/token/cost fields are injected
    ///   into `event.raw_json` only when the plugin didn't send them
    ///   (consumed by `build_turn_provenance`, `inject_reasoning_nodes`,
    ///   and `inject_response_node`).
    ///
    /// Best-effort: any failure leaves the turn recording what the plugin
    /// sent.
    pub(crate) fn enrich_opencode_turn(&self, session: &mut AgentSession, event: &mut TurnEvent) {
        if session.agent_name != "opencode" {
            return;
        }

        let Some(data) = transcript::opencode::read_turn(&session.session_id, &self.repo_root)
        else {
            return;
        };

        // 1. Transcript file → transcript_path (unblocks agent_turn).
        // Rewritten on every turn (whole-session record); a plugin-supplied
        // transcript path is never clobbered.
        if !data.transcript_jsonl.is_empty() {
            let dir = self.session_graph_dir(&session.session_id);
            let synthesized = dir.join("opencode-transcript.jsonl");
            let ours = session.transcript_path.is_none()
                || session.transcript_path.as_deref() == Some(synthesized.as_path());
            if ours {
                let _ = std::fs::create_dir_all(&dir);
                match std::fs::write(&synthesized, &data.transcript_jsonl) {
                    Ok(()) => session.set_transcript_path(&synthesized),
                    Err(e) => log::warn!(
                        "Failed to write opencode transcript for session {}: {}",
                        session.session_id,
                        e
                    ),
                }
            }
        }

        // 2. Stop-payload fields the plugin didn't send.
        let Some(raw) = event.raw_json.as_mut() else {
            return;
        };
        let Some(obj) = raw.as_object_mut() else {
            return;
        };

        if !obj.contains_key("reasoning_blocks")
            && !obj.contains_key("reasoning_text")
            && !data.reasoning_blocks.is_empty()
        {
            let blocks: Vec<serde_json::Value> = data
                .reasoning_blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "text": b.text,
                        "duration_ms": b.duration_ms,
                    })
                })
                .collect();
            obj.insert(
                "reasoning_blocks".to_string(),
                serde_json::Value::Array(blocks),
            );
        }

        if let Some(response) = &data.response {
            if !obj.contains_key("last_assistant_message") && !obj.contains_key("response") {
                obj.insert("response".to_string(), serde_json::json!(response));
            }
        }

        let have_tokens = obj.contains_key("input_tokens") || obj.contains_key("output_tokens");
        if !have_tokens
            && (data.input_tokens > 0 || data.output_tokens > 0 || data.reasoning_tokens > 0)
        {
            obj.insert(
                "input_tokens".to_string(),
                serde_json::json!(data.input_tokens),
            );
            obj.insert(
                "output_tokens".to_string(),
                serde_json::json!(data.output_tokens),
            );
            obj.insert(
                "reasoning_tokens".to_string(),
                serde_json::json!(data.reasoning_tokens),
            );
            obj.insert(
                "cache_read_tokens".to_string(),
                serde_json::json!(data.cache_read_tokens),
            );
            obj.insert(
                "cache_write_tokens".to_string(),
                serde_json::json!(data.cache_write_tokens),
            );
        }

        if !obj.contains_key("cost_usd") && data.cost_usd > 0.0 {
            obj.insert("cost_usd".to_string(), serde_json::json!(data.cost_usd));
        }
        if !obj.contains_key("finish_reason") {
            if let Some(reason) = &data.finish_reason {
                obj.insert("finish_reason".to_string(), serde_json::json!(reason));
            }
        }
        if !obj.contains_key("step_count") && data.step_count > 0 {
            obj.insert("step_count".to_string(), serde_json::json!(data.step_count));
        }

        // 3. Enrich the session graph's tool nodes. Thin plugin payloads carry
        //    no tool input/output, so nodes recorded at hook time hold bare
        //    summaries ("Execute bash"). The store's tool parts carry the
        //    command, file and output under the same call id — rewrite the
        //    summary and detail so the graph reads as rich as the rich-plugin
        //    path. Runs over ALL nodes, so one enriched turn repairs earlier
        //    thin turns of the same session.
        if !data.tool_parts.is_empty() {
            let by_call: HashMap<&str, &transcript::opencode::ToolPart> = data
                .tool_parts
                .iter()
                .map(|tp| (tp.call_id.as_str(), tp))
                .collect();

            self.with_accumulator(&session.session_id, |acc| {
                let mut changed = false;
                for node in acc.nodes.iter_mut() {
                    let Some(call_id) = node.tool_call_id.as_deref() else {
                        continue;
                    };
                    let Some(tp) = by_call.get(call_id) else {
                        continue;
                    };
                    let tool_name = node.tool_name.as_deref().unwrap_or(tp.tool.as_str());
                    let input = tp.input.as_ref();
                    let output = tp.output.as_deref();
                    let status = tp.status.as_deref();

                    let summary = summarize_tool_call(tool_name, node.kind, input, output, status);
                    if summary != node.summary {
                        node.summary = summary;
                        changed = true;
                    }
                    if let Some(detail) =
                        build_tool_detail(node.kind, tool_name, input, output, status)
                    {
                        node.detail = Some(detail);
                        changed = true;
                    }
                }
                changed
            });
        }

        log::info!(
            "Recovered opencode turn data from local store for session {} \
             ({} reasoning block{}, response: {}, {} step{})",
            session.session_id,
            data.reasoning_blocks.len(),
            if data.reasoning_blocks.len() == 1 {
                ""
            } else {
                "s"
            },
            data.response.is_some(),
            data.step_count,
            if data.step_count == 1 { "" } else { "s" },
        );
    }

    /// Read a Sherpa JSONL trace file and create provenance nodes for
    /// every record, preserving the full agent-trace + Sherpa extension data.
    ///
    /// Returns `true` if at least one record was successfully ingested.
    pub(crate) fn ingest_sherpa_trace(&self, session_id: &str, trace_path: &Path) -> bool {
        use crate::provenance::types::NodeKind;

        let content = match std::fs::read_to_string(trace_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "sherpa trace: failed to read {}: {}",
                    trace_path.display(),
                    e
                );
                return false;
            }
        };

        let mut ingested_any = false;

        self.with_accumulator(session_id, |acc| {
            let mut ingested = 0u32;
            for (line_no, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }

                let record: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("sherpa trace: line {} parse error: {}", line_no + 1, e);
                        continue;
                    }
                };

                let dev_atomic = &record["metadata"]["dev.atomic"];
                let record_type = dev_atomic["record_type"].as_str().unwrap_or("unknown");
                let timestamp = record["timestamp"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

                let (kind, summary) = match record_type {
                    "intent" => (
                        NodeKind::Goal,
                        dev_atomic["intent_title"]
                            .as_str()
                            .unwrap_or("intent")
                            .to_string(),
                    ),
                    "commitment" => {
                        let file = record["files"]
                            .as_array()
                            .and_then(|f| f.first())
                            .and_then(|f| f["path"].as_str())
                            .unwrap_or("");
                        (NodeKind::Commitment, format!("wrote {}", file))
                    }
                    "execution" => (
                        NodeKind::Execution,
                        dev_atomic["command"]
                            .as_str()
                            .unwrap_or("command")
                            .to_string(),
                    ),
                    "todo" => {
                        let tid = dev_atomic["todo_id"].as_str().unwrap_or("");
                        let content_str = dev_atomic["content"].as_str().unwrap_or("");
                        (NodeKind::Todo, format!("[{}] {}", tid, content_str))
                    }
                    "todo_status" => {
                        let tid = dev_atomic["todo_id"].as_str().unwrap_or("");
                        let from = dev_atomic["from_status"].as_str().unwrap_or("");
                        let to = dev_atomic["to_status"].as_str().unwrap_or("");
                        (
                            NodeKind::TodoStatusChange,
                            format!("{}: {} → {}", tid, from, to),
                        )
                    }
                    "phase_transition" => {
                        let from = dev_atomic["from_phase"].as_str().unwrap_or("");
                        let to = dev_atomic["to_phase"].as_str().unwrap_or("");
                        (NodeKind::PhaseTransition, format!("{} → {}", from, to))
                    }
                    "lesson" => (
                        NodeKind::Lesson,
                        dev_atomic["label"].as_str().unwrap_or("lesson").to_string(),
                    ),
                    "llm_response" => (
                        NodeKind::LlmResponse,
                        truncate_prompt(
                            dev_atomic["reply"].as_str().unwrap_or("llm response"),
                            200,
                        ),
                    ),
                    "verification" => (
                        NodeKind::Verification,
                        dev_atomic["summary"]
                            .as_str()
                            .unwrap_or("verification")
                            .to_string(),
                    ),
                    "human_gate" => {
                        let resolution = dev_atomic["resolution"].as_str().unwrap_or("");
                        (
                            NodeKind::HumanGateResolution,
                            format!("resolution: {}", resolution),
                        )
                    }
                    _ => {
                        log::debug!(
                            "sherpa trace: skipping unknown record_type '{}'",
                            record_type
                        );
                        continue;
                    }
                };

                acc.append_raw_node(kind, timestamp, &summary, Some(dev_atomic.clone()));
                ingested += 1;
            }

            if ingested > 0 {
                log::info!(
                    "sherpa trace: ingested {} records from {}",
                    ingested,
                    trace_path.display()
                );
                ingested_any = true;
            }

            ingested > 0
        });

        ingested_any
    }

    /// Save the provenance graph for a recorded turn.
    ///
    /// Appends a patch proposal node to the accumulator, converts to a
    /// content-addressed `ProvenanceGraph`, and saves it to the repository.
    /// The accumulator's `last_provenance_hash` is updated so subsequent
    /// graphs chain correctly via `previous`.
    ///
    /// Best-effort: all failures are logged but never block the session.
    pub(crate) fn save_turn_provenance(
        &self,
        session_id: &str,
        session: &AgentSession,
        outcome: &TurnRecordOutcome,
        event: &TurnEvent,
    ) {
        use atomic_core::types::Base32;

        let plan_id = session
            .managed_run
            .as_ref()
            .and_then(|run| run.work_item_id.clone());
        let ledger_turn = session.turn_count.saturating_sub(1);
        let todos = extract_turn_todos(event, session_id, ledger_turn);

        self.with_accumulator(session_id, |acc| {
            for todo in &todos {
                acc.append_todo_snapshot(todo, event.timestamp.timestamp());
            }

            // Append a patch proposal node for the recorded change
            let change_hash_base32 = outcome.hash.to_base32();
            acc.append_patch_proposal(
                &change_hash_base32,
                outcome.recorded_file_list(),
                event.timestamp.timestamp(),
            );

            // Convert the accumulated graph to a content-addressed ProvenanceGraph
            let change_hashes = vec![outcome.hash];
            let mut graph = acc.to_provenance_graph(
                &session.agent_name,
                &session.agent_display_name,
                &session.agent_vendor,
                &change_hashes,
            );

            // Set the Sherpa profile if this is a Sherpa session.
            if session.agent_name == "sherpa" {
                graph.profile = Some("sherpa-trace/1.0.0".to_string());
            }
            graph.plan_id = plan_id.clone();
            graph.todos = todos.clone();

            // Save the provenance graph in two phases:
            //
            // Phase 1 (non-blocking): Write the content-addressed file to
            // disk via ChangeStore.  This is pure filesystem I/O and never
            // contends on the redb write lock.
            //
            // Phase 2 (best-effort): Open the repository and register the
            // provenance in the pristine database (change deps, session
            // tables).  We use Repository::open_existing() which calls
            // Pristine::open_existing() — this skips the table-init write
            // transaction so it never blocks on the redb write lock.
            // Failure is still treated as non-fatal; the provenance chain
            // remains intact and the DB metadata can be rebuilt later.

            // Resolve the *canonical* changes dir. In a sandbox `repo_root`
            // has no graph of its own — the pointer file names the canonical
            // repository. Joining `.atomic` onto `repo_root` would write the
            // graph into a throwaway dir inside the sandbox, so it would be
            // absent from the real graph whenever the best-effort Phase 2
            // below loses the redb write-lock race with a concurrent agent.
            // `canonical_dot_dir` follows the pointer without opening redb, so
            // Phase 1 stays lock-free.
            let changes_dir =
                match atomic_repository::Repository::canonical_dot_dir(&self.repo_root) {
                    Ok(dot_dir) => dot_dir.join("changes"),
                    Err(e) => {
                        log::warn!(
                            "Could not resolve canonical changes dir to save \
                             provenance graph for session {}: {}",
                            session_id,
                            e,
                        );
                        return true;
                    }
                };

            // Phase 1: Filesystem write — never blocks on redb.
            let hash = match atomic_repository::ChangeStore::new(
                changes_dir,
                atomic_repository::DEFAULT_CACHE_CAPACITY,
            ) {
                Ok(store) => match store.save_provenance_graph(&graph) {
                    Ok(h) => h,
                    Err(e) => {
                        log::warn!(
                            "Failed to write provenance graph to disk for \
                             session {}: {}",
                            session_id,
                            e,
                        );
                        return true;
                    }
                },
                Err(e) => {
                    log::warn!(
                        "Could not open change store to save provenance \
                         graph for session {}: {}",
                        session_id,
                        e,
                    );
                    return true;
                }
            };

            acc.set_last_provenance_hash(hash.to_base32());
            log::info!(
                "Saved provenance graph {} for session {} \
                 ({} nodes, {} edges, {} changes)",
                hash.to_base32(),
                session_id,
                graph.node_count(),
                graph.edge_count(),
                graph.change_count(),
            );

            // Phase 2: Database registration — best-effort.
            // We use open_existing() which skips the table-init write
            // transaction, so it won't block on the redb write lock.
            // Failure here is still non-fatal: the provenance file is on
            // disk and the DB metadata (EXTERNAL/INTERNAL mapping, DEPS,
            // session tables) can be populated on a subsequent open or
            // explicit rebuild.
            match atomic_repository::Repository::open_existing(&self.repo_root) {
                Ok(repo) => {
                    if let Err(e) = repo.save_provenance_graph(&graph) {
                        log::warn!(
                            "Provenance graph {} saved to disk but database \
                             registration failed for session {}: {}",
                            hash.to_base32(),
                            session_id,
                            e,
                        );
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Provenance graph {} saved to disk but could not \
                         open repository for database registration \
                         (session {}): {}",
                        hash.to_base32(),
                        session_id,
                        e,
                    );
                }
            }

            true // always save — we appended the patch proposal node
        });
    }
}

/// Extract the generic end-of-turn todo snapshot supplied by current agent
/// hooks. Preserve an upstream stable `id` when present. Agents that omit IDs
/// get a turn-local snapshot identity; this preserves the ledger faithfully
/// without falsely claiming cross-turn lifecycle continuity.
fn extract_turn_todos(event: &TurnEvent, session_id: &str, turn_number: u32) -> Vec<SessionTodo> {
    event
        .raw_json
        .as_ref()
        .and_then(|raw| raw.get("todos"))
        .and_then(serde_json::Value::as_array)
        .map(|todos| {
            todos
                .iter()
                .enumerate()
                .filter_map(|(index, value)| {
                    let content = value.get("content")?.as_str()?.to_string();
                    let id = value
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("session:{}/turn:{}/todo:{}", session_id, turn_number, index)
                        });
                    Some(SessionTodo {
                        id,
                        content,
                        status: value
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("pending")
                            .to_string(),
                        priority: value
                            .get("priority")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("medium")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod generic_context_tests {
    use super::*;
    use crate::event::HookType;

    #[test]
    fn extracts_explicit_and_turn_local_todo_ids() {
        let event = TurnEvent::new("sess-1", HookType::TurnEnd).with_raw_json(serde_json::json!({
            "todos": [
                {"id": "todo-7", "content": "Keep ID", "status": "in_progress", "priority": "high"},
                {"content": "Snapshot only", "status": "pending", "priority": "medium"}
            ]
        }));

        let todos = extract_turn_todos(&event, "sess-1", 3);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].id, "todo-7");
        assert_eq!(todos[1].id, "session:sess-1/turn:3/todo:1");
        assert_eq!(todos[0].status, "in_progress");
    }
}
