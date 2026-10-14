//! Authenticated managed commit-time capture (CB-12A, RFC §10.3 / §11.1).
//!
//! A `pre-commit` hook installed by `atomic git bridge enable` writes one
//! capture per active managed agent session while the commit is forming. The
//! capture proves, at commit time: exact HEAD, the primary index tree/digest,
//! the working-copy identity, the session and turn, and the conversion policy
//! slot. A per-session MAC (keyed Blake3) binds the fields together.
//!
//! # What a verified capture does NOT prove
//!
//! A verified capture is necessary but not sufficient for the
//! `ManagedGitCommitCaptured` classification: that requires the exact
//! baseline→index and index→worktree reassembly (RFC §10.3.2), which is not
//! implemented. Until it is, a verified capture binds the transition as
//! evidence (`RepositoryOperations { capture }`) AND the session stays
//! DURABLY INCOMPLETE with observed-operation-only attribution per the
//! owner-APPROVED RFC §19 Q2 policy (allow-as-incomplete, 2026-09-14,
//! superseding the 2026-09-13 deferral). No exact attribution and no
//! approximate split is performed here.
//!
//! # Evidence quality
//!
//! The MAC key lives in the session JSON beside the evidence. The MAC detects
//! tampering and cross-session forgery; it is not content correctness and not
//! a defense against a determined local writer (RFC §11: hooks improve
//! evidence, never replace authoritative observation).

use std::io::Write;
use std::path::{Path, PathBuf};

use atomic_core::types::Base32;

use crate::error::AgentResult;
use crate::turn::session::AgentSession;

/// Capture schema version.
pub const CAPTURE_VERSION: u32 = 1;

/// One commit-time capture bound by the session MAC.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedCommitCapture {
    pub version: u32,
    /// Session the capture was written for.
    pub session_id: String,
    /// The managed turn in flight when the commit formed.
    pub turn: u32,
    /// Working-copy identity at commit time (base32 ULID).
    pub working_copy: String,
    /// Exact HEAD at commit time.
    pub head_oid: Option<String>,
    pub head_symref: Option<String>,
    /// Primary index digest at commit time (base32).
    pub index_digest: Option<String>,
    /// Primary index tree at commit time, when readable.
    pub index_tree: Option<String>,
    /// Git's reported in-progress operation state.
    pub repository_state: String,
    /// Present Git sequence-operation markers.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Conversion policy fingerprint. Always `None` today: the Phase 4
    /// manifest engine has not computed one (RFC §19 Q2 is owner-APPROVED
    /// as allow-as-incomplete, 2026-09-14 — the fallback binds without a
    /// policy fingerprint until the manifest engine ships).
    pub conversion_policy: Option<String>,
    /// Snapshot change covering the staged tree at commit time, when one
    /// exists. `None` until the pre-commit snapshot path ships.
    pub snapshot: Option<String>,
    /// Wall-clock capture time (RFC 3339; excluded from MAC-free equality).
    pub at_rfc3339: String,
    /// Wall-clock capture time (Unix seconds).
    pub at_unix: i64,
    /// Keyed-Blake3 MAC over the canonical JSON of every other field.
    pub mac: String,
}

/// Where one session's captures live: `<sessions>/<id>/captures/`.
pub fn capture_dir(sessions_dir: &Path, session_id: &str) -> AgentResult<PathBuf> {
    crate::turn::session::validate_session_id(session_id)?;
    Ok(sessions_dir.join(session_id).join("captures"))
}

/// Path of the capture written for one turn of one session.
///
/// Legacy/live path. Attempts (review R9) live at
/// `turn-{turn}.attempt-{n}.json` and are created exclusively — a retried
/// commit appends a new attempt instead of overwriting the previous
/// attempt's evidence.
pub fn capture_path(sessions_dir: &Path, session_id: &str, turn: u32) -> AgentResult<PathBuf> {
    Ok(capture_dir(sessions_dir, session_id)?.join(format!("turn-{turn}.json")))
}

fn attempt_capture_path(
    sessions_dir: &Path,
    session_id: &str,
    turn: u32,
    attempt: u32,
) -> AgentResult<PathBuf> {
    Ok(capture_dir(sessions_dir, session_id)?.join(format!("turn-{turn}.attempt-{attempt}.json")))
}

/// Every stored capture for one turn, oldest first (legacy single file
/// first, then per-attempt files in attempt order).
fn capture_files(sessions_dir: &Path, session_id: &str, turn: u32) -> AgentResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let legacy = capture_path(sessions_dir, session_id, turn)?;
    if legacy.exists() {
        files.push(legacy);
    }
    let prefix = format!("turn-{turn}.attempt-");
    if let Ok(entries) = std::fs::read_dir(capture_dir(sessions_dir, session_id)?) {
        let mut attempts: Vec<(u32, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let rest = name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".json")?
                    .to_string();
                rest.parse::<u32>()
                    .ok()
                    .map(|attempt| (attempt, entry.path()))
            })
            .collect();
        attempts.sort_by_key(|(attempt, _)| *attempt);
        files.extend(attempts.into_iter().map(|(_, path)| path));
    }
    Ok(files)
}

/// The most recent capture file for one turn (the latest commit attempt),
/// when any exists.
fn latest_capture_file(
    sessions_dir: &Path,
    session_id: &str,
    turn: u32,
) -> AgentResult<Option<PathBuf>> {
    Ok(capture_files(sessions_dir, session_id, turn)?.pop())
}

/// Whether any capture evidence exists for one turn of one session.
pub fn has_capture(sessions_dir: &Path, session_id: &str, turn: u32) -> bool {
    capture_files(sessions_dir, session_id, turn)
        .map(|files| !files.is_empty())
        .unwrap_or(false)
}

impl ManagedCommitCapture {
    /// The canonical MAC input: the capture's JSON with `mac` cleared.
    ///
    /// JSON with sorted field order (serde_json preserves struct order) is
    /// deterministic for this struct, so the MAC binds exactly these bytes.
    fn mac_input(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.mac = String::new();
        serde_json::to_vec(&unsigned).expect("capture canonical JSON")
    }

    /// Compute the MAC under `key_hex` (64 hex chars → 32 bytes).
    fn mac(key_hex: &str, input: &[u8]) -> String {
        let key = mac_key_bytes(key_hex);
        let mut hasher = blake3::Hasher::new_keyed(&key);
        hasher.update(input);
        hasher.finalize().to_hex().to_string()
    }

    /// Sign this capture in place with the session's MAC key.
    pub fn sign(&mut self, key_hex: &str) {
        let input = self.mac_input();
        self.mac = Self::mac(key_hex, &input);
    }

    /// Verify the MAC. Any edited field — including a swapped capture from
    /// another session or turn — fails.
    pub fn verify(&self, key_hex: &str) -> bool {
        let input = self.mac_input();
        Self::mac(key_hex, &input) == self.mac
    }

    /// Encode for durable storage (one JSON document per file).
    pub fn to_json(&self) -> AgentResult<Vec<u8>> {
        serde_json::to_vec_pretty(self)
            .map(|mut bytes| {
                bytes.push(b'\n');
                bytes
            })
            .map_err(|e| crate::error::AgentError::RecordFailed {
                session_id: self.session_id.clone(),
                turn_number: self.turn,
                reason: format!("Failed to encode commit capture: {}", e),
            })
    }

    /// Decode a stored capture.
    pub fn from_json(bytes: &[u8]) -> AgentResult<Self> {
        serde_json::from_slice(bytes).map_err(|e| crate::error::AgentError::CaptureInvalid {
            reason: format!("JSON parse error: {}", e),
        })
    }
}

/// Decode the hex MAC key into exactly 32 bytes.
fn mac_key_bytes(key_hex: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let mut byte = 0u8;
    let mut filled = 0usize;
    for (index, character) in key_hex.chars().enumerate() {
        let nibble = character.to_digit(16).unwrap_or(0) as u8;
        if index % 2 == 0 {
            byte = nibble << 4;
        } else if filled < 32 {
            key[filled] = byte | nibble;
            filled += 1;
        }
    }
    key
}

/// Build a capture from the session and the exact git observation token.
///
/// `conversion_policy` and `snapshot` stay `None` (documented above); the
/// caller signs the result with [`AgentSession::ensure_mac_key`].
pub fn build_capture(
    session: &AgentSession,
    turn: u32,
    working_copy: String,
    token: &atomic_repository::GitObservationToken,
) -> ManagedCommitCapture {
    let (head_oid, head_symref) = match &token.head {
        atomic_repository::GitHeadObservation::Attached { symref, oid } => {
            (Some(oid.clone()), Some(symref.clone()))
        }
        atomic_repository::GitHeadObservation::Detached { oid } => (Some(oid.clone()), None),
        atomic_repository::GitHeadObservation::Unborn { symref }
        | atomic_repository::GitHeadObservation::MissingTarget { symref } => {
            (None, Some(symref.clone()))
        }
    };
    ManagedCommitCapture {
        version: CAPTURE_VERSION,
        session_id: session.session_id.clone(),
        turn,
        working_copy,
        head_oid,
        head_symref,
        index_digest: Some(token.index_digest.to_base32()),
        index_tree: token.index_tree.clone(),
        repository_state: token.repository_state.clone(),
        markers: token
            .markers
            .iter()
            .map(|marker| format!("{marker:?}"))
            .collect(),
        conversion_policy: None,
        snapshot: None,
        at_rfc3339: chrono::Utc::now().to_rfc3339(),
        at_unix: chrono::Utc::now().timestamp(),
        mac: String::new(),
    }
}

/// Write the capture for one turn of one session, signing it first.
///
/// Review R9: attempts are create-only. A retried commit within the same
/// turn writes `turn-N.attempt-2.json` and so on instead of overwriting the
/// previous attempt's evidence; earlier attempts stay durable for review.
pub fn write_capture(
    sessions_dir: &Path,
    session: &mut AgentSession,
    turn: u32,
    working_copy: String,
    token: &atomic_repository::GitObservationToken,
) -> AgentResult<PathBuf> {
    let key = session.ensure_mac_key();
    let mut capture = build_capture(session, turn, working_copy, token);
    capture.sign(&key);
    let dir = capture_dir(sessions_dir, &session.session_id)?;
    std::fs::create_dir_all(&dir)?;
    // Find the first attempt slot that does not exist yet, exclusively.
    for attempt in 1..=64u32 {
        let path = attempt_capture_path(sessions_dir, &session.session_id, turn, attempt)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(&capture.to_json()?)?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(crate::error::AgentError::RecordFailed {
        session_id: session.session_id.clone(),
        turn_number: turn,
        reason: "too many commit attempts without a turn boundary (64)".to_string(),
    })
}

/// Load and fully verify the capture for one turn of one session.
///
/// The capture binds commit-time state, not post-commit state: HEAD at
/// pre-commit time is the PARENT of the commit being formed, and the primary
/// index tree is the tree the commit will carry. Verification therefore
/// checks:
///
/// - MAC authentication (any edited field fails);
/// - the capture binds this session, turn and working-copy identity;
/// - `capture.head_oid` equals the parent OID (the turn-start HEAD);
/// - `capture.index_tree` equals the committed tree (the turn-end HEAD tree);
/// - the capture was taken inside the turn window (not replayed from an
///   earlier turn under a copied filename).
///
/// Returns `Err` when a capture file exists but fails any binding; returns
/// `Ok(None)` when no capture exists (hook bypassed, removed, or
/// `--no-verify`).
#[allow(clippy::too_many_arguments)]
pub fn verify_capture(
    sessions_dir: &Path,
    session: &AgentSession,
    turn: u32,
    expected_working_copy: &str,
    expected_parent_oid: Option<&str>,
    expected_committed_tree: Option<&str>,
    turn_started_at: i64,
) -> AgentResult<Option<ManagedCommitCapture>> {
    // Verify the LATEST attempt (the last commit attempt of the turn);
    // earlier attempts stay on disk as durable evidence.
    let Some(path) = latest_capture_file(sessions_dir, &session.session_id, turn)? else {
        return Ok(None);
    };
    let bytes = std::fs::read(&path).map_err(|error| crate::error::AgentError::CaptureInvalid {
        reason: format!("cannot read capture {}: {}", path.display(), error),
    })?;
    let capture = ManagedCommitCapture::from_json(&bytes)?;
    if capture.version != CAPTURE_VERSION {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture schema version {} is not supported (expected {CAPTURE_VERSION})",
                capture.version
            ),
        });
    }

    // The durable session JSON is the MAC-key authority: the pre-commit hook
    // persisted the key it signed with. Fall back to the in-memory session
    // for freshly-created keys not yet saved.
    let key = {
        let store = crate::turn::session::SessionStore::new(sessions_dir)?;
        store
            .load(&session.session_id)?
            .and_then(|stored| stored.mac_key)
            .or_else(|| session.mac_key.clone())
    };
    let key = key.ok_or_else(|| crate::error::AgentError::CaptureInvalid {
        reason: "session has no MAC key; capture cannot authenticate".to_string(),
    })?;
    if !capture.verify(&key) {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: "capture MAC verification failed (tampered or forged)".to_string(),
        });
    }
    if capture.session_id != session.session_id {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture binds session '{}' not '{}'",
                capture.session_id, session.session_id
            ),
        });
    }
    if capture.turn != turn {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture binds turn {} not {} (replayed or stale)",
                capture.turn, turn
            ),
        });
    }
    if capture.working_copy != expected_working_copy {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture binds working copy '{}' not '{}'",
                capture.working_copy, expected_working_copy
            ),
        });
    }
    if capture.head_oid.as_deref() != expected_parent_oid {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture binds parent HEAD {:?} not {:?} (stale or edited after commit)",
                capture.head_oid, expected_parent_oid
            ),
        });
    }
    match (capture.index_tree.as_deref(), expected_committed_tree) {
        (Some(captured), Some(committed)) if captured == committed => {}
        _ => {
            return Err(crate::error::AgentError::CaptureInvalid {
                reason: format!(
                    "capture index tree {:?} does not prove the committed tree {:?}",
                    capture.index_tree, expected_committed_tree
                ),
            });
        }
    }
    // Turn-window freshness with a generous same-machine clock skew on both
    // sides (review R9: a future-dated capture is fabricated, not fresh).
    let now_unix = chrono::Utc::now().timestamp();
    if capture.at_unix < turn_started_at.saturating_sub(60) {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture taken at {} predates turn start {} (stale)",
                capture.at_unix, turn_started_at
            ),
        });
    }
    if capture.at_unix > now_unix.saturating_add(300) {
        return Err(crate::error::AgentError::CaptureInvalid {
            reason: format!(
                "capture taken at {} is future-dated past {} (fabricated)",
                capture.at_unix, now_unix
            ),
        });
    }
    Ok(Some(capture))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::session::SessionStore;

    fn sample_token() -> atomic_repository::GitObservationToken {
        atomic_repository::GitObservationToken {
            head: atomic_repository::GitHeadObservation::Attached {
                symref: "refs/heads/main".to_string(),
                oid: "a".repeat(40),
            },
            head_tree: Some("tree-head".to_string()),
            index_digest: atomic_core::types::Hash::of(b"index"),
            index_tree: Some("tree-index".to_string()),
            index_stages: Vec::new(),
            index_locked: false,
            repository_state: String::new(),
            markers: Vec::new(),
        }
    }

    fn make_session(id: &str) -> AgentSession {
        AgentSession::new(id, "claude-code", "Claude Code")
    }

    #[test]
    fn written_capture_verifies_and_binds_the_turn() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-1");
        store.save(&session).unwrap();

        let path = write_capture(
            dir.path(),
            &mut session,
            3,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        // Persist the generated MAC key (the CLI hook does this after every
        // capture) — or the consumer cannot authenticate.
        store.save(&session).unwrap();
        let persisted = store.load("sess-cap-1").unwrap().unwrap();
        assert!(persisted.mac_key.is_some());
        assert_eq!(persisted.mac_key, session.mac_key);

        let verified = verify_capture(
            dir.path(),
            &persisted,
            3,
            "wc",
            Some(&"a".repeat(40)),
            Some("tree-index"),
            0,
        )
        .unwrap()
        .expect("capture exists");
        assert_eq!(verified.turn, 3);
        assert_eq!(verified.session_id, "sess-cap-1");
        assert_eq!(verified.conversion_policy, None);
        assert_eq!(verified.snapshot, None);
        assert!(path.exists());
    }

    #[test]
    fn missing_capture_is_none_not_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = make_session("sess-cap-2");
        let verified = verify_capture(
            dir.path(),
            &session,
            1,
            "wc",
            Some(&"b".repeat(40)),
            Some("tree-index"),
            0,
        )
        .unwrap();
        assert!(verified.is_none(), "hook bypassed: no capture, no error");
    }

    #[test]
    fn tampered_capture_fails_authentication() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-3");
        store.save(&session).unwrap();
        write_capture(
            dir.path(),
            &mut session,
            2,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        store.save(&session).unwrap();

        // Tamper with a covered field.
        let path = latest_capture_file(dir.path(), "sess-cap-3", 2)
            .unwrap()
            .expect("capture was written");
        let mut capture = ManagedCommitCapture::from_json(&std::fs::read(&path).unwrap()).unwrap();
        capture.head_oid = Some("f".repeat(40));
        std::fs::write(&path, capture.to_json().unwrap()).unwrap();

        let persisted = store.load("sess-cap-3").unwrap().unwrap();
        let error = verify_capture(
            dir.path(),
            &persisted,
            2,
            "wc",
            Some(&"f".repeat(40)),
            Some("tree-index"),
            0,
        )
        .expect_err("tampered capture must fail");
        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn replayed_capture_from_another_turn_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-4");
        store.save(&session).unwrap();
        write_capture(
            dir.path(),
            &mut session,
            1,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        store.save(&session).unwrap();

        let persisted = store.load("sess-cap-4").unwrap().unwrap();
        // The same turn works...
        verify_capture(
            dir.path(),
            &persisted,
            1,
            "wc",
            Some(&"a".repeat(40)),
            Some("tree-index"),
            0,
        )
        .unwrap()
        .expect("turn match");
        // ...but a capture copied to another turn's path is a replay: the
        // per-turn binding must fail even though the MAC itself is intact.
        std::fs::copy(
            latest_capture_file(dir.path(), "sess-cap-4", 1)
                .unwrap()
                .unwrap(),
            attempt_capture_path(dir.path(), "sess-cap-4", 2, 1).unwrap(),
        )
        .unwrap();
        let error = verify_capture(
            dir.path(),
            &persisted,
            2,
            "wc",
            Some(&"a".repeat(40)),
            Some("tree-index"),
            0,
        )
        .expect_err("replayed capture must fail");
        assert!(
            error.to_string().contains("replayed"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn foreign_session_capture_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-5");
        store.save(&session).unwrap();
        write_capture(
            dir.path(),
            &mut session,
            1,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        store.save(&session).unwrap();

        let impostor = make_session("sess-cap-6");
        // The capture copied into the impostor's path must not authenticate
        // under the impostor's own MAC key.
        std::fs::create_dir_all(capture_dir(dir.path(), "sess-cap-6").unwrap()).unwrap();
        std::fs::copy(
            latest_capture_file(dir.path(), "sess-cap-5", 1)
                .unwrap()
                .unwrap(),
            attempt_capture_path(dir.path(), "sess-cap-6", 1, 1).unwrap(),
        )
        .unwrap();
        assert!(
            verify_capture(
                dir.path(),
                &impostor,
                1,
                "wc",
                Some(&"a".repeat(40)),
                Some("tree-index"),
                0,
            )
            .is_err(),
            "another session's MAC key must not authenticate the capture"
        );
    }

    /// Review R9: a retried commit within the same turn must not overwrite
    /// the previous attempt's evidence — attempts are create-only files and
    /// verification reads the latest attempt.
    #[test]
    fn retried_commit_preserves_earlier_attempt_evidence() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-7");
        store.save(&session).unwrap();

        write_capture(
            dir.path(),
            &mut session,
            1,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        write_capture(
            dir.path(),
            &mut session,
            1,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();

        let files = capture_files(dir.path(), "sess-cap-7", 1).unwrap();
        assert_eq!(
            files.len(),
            2,
            "the first attempt's evidence must survive a second write"
        );
        assert!(has_capture(dir.path(), "sess-cap-7", 1));
        store.save(&session).unwrap();
        let persisted = store.load("sess-cap-7").unwrap().unwrap();
        let verified = verify_capture(
            dir.path(),
            &persisted,
            1,
            "wc",
            Some(&"a".repeat(40)),
            Some("tree-index"),
            0,
        )
        .unwrap()
        .expect("latest attempt verifies");
        assert_eq!(verified.turn, 1);
    }

    /// Review R9: versioned evidence and future-dated rows are refused —
    /// verification bounds the schema version and the capture time on both
    /// sides of the window.
    #[test]
    fn future_dated_capture_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let mut session = make_session("sess-cap-8");
        store.save(&session).unwrap();
        write_capture(
            dir.path(),
            &mut session,
            1,
            "wc".to_string(),
            &sample_token(),
        )
        .unwrap();
        store.save(&session).unwrap();

        let key = session.mac_key.clone().unwrap();
        let path = latest_capture_file(dir.path(), "sess-cap-8", 1)
            .unwrap()
            .unwrap();
        let mut capture = ManagedCommitCapture::from_json(&std::fs::read(&path).unwrap()).unwrap();

        // A future-dated but correctly-signed capture is fabrication, not
        // freshness.
        capture.at_unix = chrono::Utc::now().timestamp() + 10_000;
        capture.sign(&key);
        std::fs::write(&path, capture.to_json().unwrap()).unwrap();

        let persisted = store.load("sess-cap-8").unwrap().unwrap();
        let error = verify_capture(
            dir.path(),
            &persisted,
            1,
            "wc",
            Some(&"a".repeat(40)),
            Some("tree-index"),
            0,
        )
        .expect_err("future-dated capture must fail");
        assert!(
            error.to_string().contains("future-dated"),
            "unexpected: {error}"
        );
    }
}
