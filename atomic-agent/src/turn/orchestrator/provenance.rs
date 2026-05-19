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

use std::path::{Path, PathBuf};

use crate::event::TurnEvent;
use crate::provenance::accumulator::ProvenanceAccumulator;
use crate::record::TurnRecordOutcome;
use crate::turn::session::AgentSession;

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
            if e.kind() == std::io::ErrorKind::WouldBlock {
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
            if e.kind() == std::io::ErrorKind::WouldBlock {
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

        self.with_accumulator(session_id, |acc| {
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

            // Save to the repository
            match atomic_repository::Repository::open(&self.repo_root) {
                Ok(repo) => match repo.save_provenance_graph(&graph) {
                    Ok(hash) => {
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
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to save provenance graph for session {}: {}",
                            session_id,
                            e,
                        );
                    }
                },
                Err(e) => {
                    log::warn!(
                        "Could not open repository to save provenance graph \
                         for session {}: {}",
                        session_id,
                        e,
                    );
                }
            }

            true // always save — we appended the patch proposal node
        });
    }
}
