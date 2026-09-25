//! Hook liveness and failure reporting for `atomic agent status`.
//!
//! Two independent signals, because they answer different questions:
//!
//! - **Health** ([`HookHealth`], `.atomic/hook-health.json`) — written by
//!   [`record_fire`] on every hook dispatch. Answers "are the hooks still
//!   alive?" A hook that is installed but has not fired for hours is broken
//!   in a way that nothing else reports: the shell guard in
//!   `atomic-agent::hooks` ends in `|| true`, so a hook that dies is
//!   indistinguishable from a hook that was never installed.
//! - **Errors** ([`HookErrors`], `.atomic/hook-errors.log`) — appended by the
//!   git-shadow validator and by the agent plugins. Answers "what went
//!   wrong?" This file is unbounded and append-only with no rotation, so
//!   only a bounded tail is ever read.
//!
//! Neither signal subsumes the other: a healthy log says nothing about
//! whether the recorder is running, and a healthy recorder says nothing
//! about what it failed to do earlier.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Version of the `.atomic/hook-health.json` document.
const HEALTH_SCHEMA_VERSION: u32 = 1;

/// How many bytes of `hook-errors.log` tail to read.
///
/// The log is unbounded, so the reader seeks to the end and parses only the
/// last chunk. Large enough to cover a busy day of errors on a repo like
/// `atomic`; small enough that `agent status` stays instant.
const ERROR_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Longest error message retained per entry, in the aggregated report.
const MAX_DETAIL_LEN: usize = 200;

/// Prefix the agent plugins use for session ids in `hook-errors.log`.
///
/// It is the one token the two writers' grammars can be told apart by: the
/// plugin writer emits it as the second field, the git-shadow writer emits
/// its own tag there instead.
const SESSION_ID_PREFIX: &str = "ses_";

// ---------------------------------------------------------------------------
// Health record
// ---------------------------------------------------------------------------

/// How a single hook dispatch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum FireOutcome {
    /// The event dispatched and the recorder committed it.
    Ok,
    /// The event reached the dispatcher but failed.
    Error,
}

impl FireOutcome {
    /// The wire name, used in human output.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FireOutcome::Ok => "ok",
            FireOutcome::Error => "error",
        }
    }
}

/// The most recent dispatch of one verb for one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct VerbHealth {
    /// RFC3339 timestamp of the most recent dispatch, successful or not.
    pub last_fired: String,
    /// Outcome of that most recent dispatch.
    pub outcome: FireOutcome,
    /// Failure detail when `outcome` is `error`; truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Timestamp of the most recent *successful* dispatch, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<String>,
}

/// One agent's hook health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AgentHealth {
    pub display_name: String,
    /// Keyed by hook verb (`session-start`, `before-tool`, …).
    pub verbs: BTreeMap<String, VerbHealth>,
}

/// The whole `.atomic/hook-health.json` document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct HookHealth {
    pub schema_version: u32,
    /// RFC3339 timestamp of the last write.
    pub updated_at: String,
    pub agents: BTreeMap<String, AgentHealth>,
}

impl HookHealth {
    /// An empty document, ready for its first [`HookHealth::record`].
    pub(super) fn empty(now: &str) -> Self {
        Self {
            schema_version: HEALTH_SCHEMA_VERSION,
            updated_at: now.to_string(),
            agents: BTreeMap::new(),
        }
    }

    /// Fold one dispatch into the record, preserving `last_ok` across a
    /// later failure so a hook that has *never* succeeded is
    /// distinguishable from one that worked and then broke.
    pub(super) fn record(
        &mut self,
        agent: &str,
        display_name: &str,
        verb: &str,
        outcome: FireOutcome,
        detail: Option<&str>,
        now: &str,
    ) {
        let entry = self
            .agents
            .entry(agent.to_string())
            .or_insert_with(|| AgentHealth {
                display_name: display_name.to_string(),
                verbs: BTreeMap::new(),
            });
        entry.display_name = display_name.to_string();

        // A failure must not erase the last known-good dispatch: the gap
        // between `last_ok` and `last_fired` is what says "this worked, then
        // broke" rather than "this never worked".
        let last_ok = match outcome {
            FireOutcome::Ok => Some(now.to_string()),
            FireOutcome::Error => entry.verbs.get(verb).and_then(|v| v.last_ok.clone()),
        };

        entry.verbs.insert(
            verb.to_string(),
            VerbHealth {
                last_fired: now.to_string(),
                outcome,
                detail: detail.map(truncate_detail),
                last_ok,
            },
        );

        self.updated_at = now.to_string();
    }

    /// Read the record for a repository, treating any missing or malformed
    /// file as "no health data yet".
    ///
    /// Health is diagnostic, so a corrupt file must never fail the command
    /// that is trying to explain what is wrong.
    pub(super) fn read(repo_root: &Path) -> Option<Self> {
        let path = repo_root.join(".atomic").join("hook-health.json");
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist the record, replacing the file atomically.
    ///
    /// Best-effort: a failure here is dropped rather than propagated, because
    /// the caller is a hook that is already on its way out and must not turn
    /// bookkeeping into a hook failure.
    pub(super) fn write(&self, repo_root: &Path) {
        let Ok(bytes) = serde_json::to_vec_pretty(self) else {
            return;
        };
        crate::commands::agent::write_atomic(
            &repo_root.join(".atomic"),
            "hook-health.json",
            &bytes,
        );
    }
}

/// Record one hook dispatch against the repository's health file.
///
/// Returns without doing anything if the repository is missing, so it is safe
/// to call from a hook that may run outside a workspace.
pub(super) fn record_fire(
    repo_root: &Path,
    agent: &str,
    display_name: &str,
    verb: &str,
    outcome: FireOutcome,
    detail: Option<&str>,
) {
    if !repo_root.join(".atomic").is_dir() {
        return;
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut health = HookHealth::read(repo_root).unwrap_or_else(|| HookHealth::empty(&now));
    health.record(agent, display_name, verb, outcome, detail, &now);
    health.write(repo_root);
}

/// Flatten a health record into one row per agent, for display and JSON.
///
/// Agents with no recorded fires are omitted entirely: absence is already
/// reported by the `hooks_installed` flag, and a row of empty timestamps
/// would read as a second, contradictory answer.
pub(super) fn summarize(health: &HookHealth) -> Vec<AgentHookSummary> {
    let mut out: Vec<AgentHookSummary> = health
        .agents
        .iter()
        .map(|(name, agent)| {
            let last_fired = agent
                .verbs
                .values()
                .map(|v| v.last_fired.as_str())
                .max()
                .map(str::to_string);
            let last_ok = agent
                .verbs
                .values()
                .filter_map(|v| v.last_ok.as_deref())
                .max()
                .map(str::to_string);
            let last_error = agent
                .verbs
                .values()
                .filter(|v| v.outcome == FireOutcome::Error)
                .max_by_key(|v| v.last_fired.as_str())
                .map(|v| v.last_fired.clone());
            let failing: Vec<String> = agent
                .verbs
                .iter()
                .filter(|(_, v)| v.outcome == FireOutcome::Error)
                .map(|(k, _)| k.clone())
                .collect();

            AgentHookSummary {
                name: name.clone(),
                display_name: agent.display_name.clone(),
                last_fired,
                last_ok,
                last_error,
                verbs_recorded: agent.verbs.len(),
                failing_verbs: failing,
            }
        })
        .collect();

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One agent's health, flattened for display.
#[derive(Debug, Clone, Serialize)]
pub(super) struct AgentHookSummary {
    pub name: String,
    pub display_name: String,
    pub last_fired: Option<String>,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
    pub verbs_recorded: usize,
    pub failing_verbs: Vec<String>,
}

impl AgentHookSummary {
    /// A one-line liveness phrase for the human report, e.g.
    /// `last fired 4m ago` or `no successful dispatch`.
    pub(super) fn liveness(&self) -> String {
        match (&self.last_fired, &self.last_ok) {
            (None, _) => "never fired".to_string(),
            (Some(_), None) => "fired, never succeeded".to_string(),
            (Some(fired), Some(ok)) => {
                let age = relative_age(fired, ok);
                format!("last ok {age}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hook-errors.log
// ---------------------------------------------------------------------------

/// One parsed line of `hook-errors.log`.
#[derive(Debug, Clone, Serialize)]
pub(super) struct HookErrorEntry {
    /// RFC3339 timestamp, verbatim — the file's two writers do not agree on
    /// precision, so it is not normalized.
    pub at: String,
    /// The verb, or `shadow-validate:<rule>` for git-shadow entries.
    pub verb: String,
    /// Everything after the verb, flattened to one line and truncated.
    pub message: String,
}

/// A bounded, aggregated read of `hook-errors.log`.
#[derive(Debug, Clone, Serialize)]
pub(super) struct HookErrors {
    /// Number of entries parsed in the window that was read.
    pub total: usize,
    /// Physical lines consumed, including continuation lines folded into an
    /// entry's message. Greater than `total` whenever messages spanned lines.
    pub lines: usize,
    /// Timestamp of the oldest entry in the window, if any.
    pub first_seen: Option<String>,
    /// Timestamp of the newest entry in the window, if any.
    pub last_seen: Option<String>,
    /// Human-readable description of what was read, e.g. `all 636 entries
    /// from 1784 lines` — the distinction matters, because a bounded read
    /// cannot claim the file is only as bad as the window.
    pub window: String,
    /// Error classes with counts, most frequent first.
    pub classes: Vec<ErrorClass>,
    /// The most recent entries, newest first.
    pub recent: Vec<HookErrorEntry>,
}

/// A group of similar errors, counted.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ErrorClass {
    /// Stable-ish signature used to group entries.
    pub signature: String,
    /// A representative message.
    pub example: String,
    pub count: usize,
}

/// Read a bounded tail of `hook-errors.log` and aggregate it.
///
/// Returns `None` when the file is absent. A file that exists but cannot be
/// read or parsed yields an empty report rather than an error, because the
/// point of the report is to explain the situation.
pub(super) fn read_errors(repo_root: &Path) -> Option<HookErrors> {
    let path = repo_root.join(".atomic").join("hook-errors.log");
    let meta = std::fs::metadata(&path).ok()?;
    let file_len = meta.len();
    let mut file = std::fs::File::open(&path).ok()?;

    // Read only the tail; the file is unbounded and never rotated.
    let start = file_len.saturating_sub(ERROR_LOG_TAIL_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);
    // A tail seek can land mid-line, in which case the first line read is a
    // fragment and must be dropped. When the whole file was read (start ==
    // 0) nothing is partial, so the first line is real and must be kept —
    // dropping it would silently lose the oldest entry on every small log.
    let body = if start > 0 {
        match text.find('\n') {
            Some(i) => &text[i + 1..],
            None => "",
        }
    } else {
        &text
    };

    let (entries, lines) = parse_error_log(body);

    Some(aggregate(&entries, lines, start))
}

/// Parse a log into entries, folding continuation lines into their entry.
///
/// The plugin writer appends `message` unescaped, so a stack trace or a
/// multi-paragraph error spans several physical lines. A line that does not
/// start with a timestamp is therefore a continuation of the entry above it,
/// not a separate entry — and dropping it would lose most of the message.
///
/// Returns the entries plus the number of physical lines consumed, so the
/// report can be honest about what fraction of the file it understood.
fn parse_error_log(body: &str) -> (Vec<HookErrorEntry>, usize) {
    let mut entries: Vec<HookErrorEntry> = Vec::new();
    let mut lines = 0usize;

    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;

        match parse_error_line(line) {
            Some(entry) => entries.push(entry),
            None => {
                if let Some(last) = entries.last_mut() {
                    last.message = truncate_detail(&format!("{} {}", last.message, line.trim()));
                }
            }
        }
    }

    (entries, lines)
}

/// Parse one log line into an entry, or `None` if it is not shaped like one.
///
/// Two writers append to this file with different grammars:
///
/// ```text
/// {rfc3339} {session_id} {verb} {message}                     # agent plugins
/// {rfc3339} shadow-validate:{rule} view={view} {message}      # git-shadow validator
/// ```
///
/// The plugin grammar leads with the session id, so the verb is the third
/// field; the shadow grammar puts its own tag there. They are told apart by
/// the session-id prefix, which is the one thing both grammars can be
/// relied on to disagree about.
///
/// A line whose first token is not a timestamp is not an entry of its own;
/// [`parse_error_log`] folds it into the previous one.
fn parse_error_line(line: &str) -> Option<HookErrorEntry> {
    let mut fields = line.split_whitespace();
    let at = fields.next()?;
    if chrono::DateTime::parse_from_rfc3339(at).is_err() {
        return None;
    }

    let second = fields.next().unwrap_or("");

    // Everything after the verb, rebuilt so a multi-word message survives.
    let (verb, rest): (&str, String) = if second.starts_with(SESSION_ID_PREFIX) {
        let verb = fields.next().unwrap_or("unknown");
        (verb, fields.collect::<Vec<_>>().join(" "))
    } else {
        (second, fields.collect::<Vec<_>>().join(" "))
    };

    Some(HookErrorEntry {
        at: at.to_string(),
        verb: if verb.is_empty() {
            "unknown".to_string()
        } else {
            verb.to_string()
        },
        message: truncate_detail(&rest),
    })
}

/// Group entries into error classes and build the report.
fn aggregate(entries: &[HookErrorEntry], lines: usize, start: u64) -> HookErrors {
    // Group on a coarse signature: the error text with digits, hex and
    // paths collapsed, so that "turn 78 is not running" and "turn 92 is not
    // running" are one class rather than hundreds of singletons.
    let mut order: Vec<String> = Vec::new();
    let mut buckets: BTreeMap<String, (usize, String)> = BTreeMap::new();

    for entry in entries {
        let signature = error_signature(&entry.message);
        match buckets.get_mut(&signature) {
            Some((count, _)) => *count += 1,
            None => {
                order.push(signature.clone());
                buckets.insert(signature, (1, entry.message.clone()));
            }
        }
    }

    let mut classes: Vec<ErrorClass> = order
        .into_iter()
        .filter_map(|signature| {
            buckets.get(&signature).map(|(count, example)| ErrorClass {
                signature,
                example: example.clone(),
                count: *count,
            })
        })
        .collect();
    classes.sort_by(|a, b| b.count.cmp(&a.count).then(a.signature.cmp(&b.signature)));

    let mut recent: Vec<HookErrorEntry> = entries.to_vec();
    recent.reverse();

    let window = if start > 0 {
        format!(
            "last {lines} lines, {k} entries (file is {kb} KiB; older entries not read)",
            k = entries.len(),
            kb = (start + 64 * 1024) / 1024,
        )
    } else {
        format!("all {lines} lines, {k} entries", k = entries.len())
    };

    HookErrors {
        total: entries.len(),
        lines,
        first_seen: entries.first().map(|e| e.at.clone()),
        last_seen: entries.last().map(|e| e.at.clone()),
        window,
        classes: classes.into_iter().take(10).collect(),
        recent: recent.into_iter().take(10).collect(),
    }
}

/// Collapse a message to a grouping key.
///
/// Session ids, turn numbers, hashes and paths all vary between occurrences
/// of the same underlying failure. Normalizing them is what turns 546
/// separate lines into one actionable row.
fn error_signature(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            out.push('N');
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push_str(&normalize_token(&word));
        } else {
            out.push(c);
            i += 1;
        }
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Map a bare word to a placeholder when it looks like an identifier.
///
/// Only length and shape are used — a token is not identified by its
/// spelling, so nothing sensitive is retained in the grouping key.
fn normalize_token(word: &str) -> String {
    let has_digit = word.chars().any(|c| c.is_ascii_digit());
    let long = word.len() >= 16;
    if has_digit && long {
        "ID".to_string()
    } else if has_digit && word.chars().all(|c| c.is_ascii_alphanumeric()) {
        "N".to_string()
    } else {
        word.to_ascii_lowercase()
    }
}

fn truncate_detail(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DETAIL_LEN {
        return flat;
    }
    let head: String = flat.chars().take(MAX_DETAIL_LEN).collect();
    format!("{head}...")
}

/// Describe how long ago `then` was, relative to `now`, in coarse units.
fn relative_age(now: &str, then: &str) -> String {
    let (Ok(now), Ok(then)) = (
        chrono::DateTime::parse_from_rfc3339(now),
        chrono::DateTime::parse_from_rfc3339(then),
    ) else {
        return "recently".to_string();
    };
    let secs = (now - then).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn record_then_summarize_reports_liveness() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".atomic")).unwrap();

        let now = "2026-09-25T18:00:00+00:00";
        let mut health = HookHealth::empty(now);
        health.record(
            "opencode",
            "OpenCode",
            "session-start",
            FireOutcome::Ok,
            None,
            now,
        );

        let rows = summarize(&health);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "opencode");
        assert_eq!(rows[0].verbs_recorded, 1);
        assert!(rows[0].failing_verbs.is_empty());
        assert_eq!(rows[0].liveness(), "last ok 0s ago");
    }

    #[test]
    fn a_fresh_failure_keeps_the_earlier_success_visible() {
        let mut health = HookHealth::empty("2026-09-25T17:00:00+00:00");
        health.record(
            "opencode",
            "OpenCode",
            "before-tool",
            FireOutcome::Ok,
            None,
            "2026-09-25T17:00:00+00:00",
        );
        health.record(
            "opencode",
            "OpenCode",
            "before-tool",
            FireOutcome::Error,
            Some("boom"),
            "2026-09-25T18:00:00+00:00",
        );

        let rows = summarize(&health);
        assert_eq!(rows[0].failing_verbs, vec!["before-tool"]);
        // last_fired moved forward, but last_ok stayed behind it — that
        // difference is the whole reason both fields exist.
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("2026-09-25T18:00:00+00:00")
        );
        assert_eq!(
            rows[0].last_ok.as_deref(),
            Some("2026-09-25T17:00:00+00:00")
        );
        assert_eq!(rows[0].liveness(), "last ok 1h ago");
    }

    #[test]
    fn a_hook_that_never_succeeded_is_distinguishable() {
        let mut health = HookHealth::empty("2026-09-25T17:00:00+00:00");
        health.record(
            "opencode",
            "OpenCode",
            "stop",
            FireOutcome::Error,
            Some("no such command"),
            "2026-09-25T17:00:00+00:00",
        );
        let rows = summarize(&health);
        assert_eq!(rows[0].liveness(), "fired, never succeeded");
    }

    #[test]
    fn health_round_trips_through_disk() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".atomic")).unwrap();

        let now = "2026-09-25T18:00:00+00:00";
        let mut health = HookHealth::empty(now);
        health.record(
            "claude-code",
            "Claude Code",
            "stop",
            FireOutcome::Ok,
            None,
            now,
        );
        health.write(root);

        let read = HookHealth::read(root).expect("health file round-trips");
        assert_eq!(read.schema_version, 1);
        assert!(read.agents.contains_key("claude-code"));
    }

    #[test]
    fn missing_or_corrupt_health_reads_as_absent() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".atomic")).unwrap();
        assert!(HookHealth::read(root).is_none());

        std::fs::write(root.join(".atomic/hook-health.json"), b"{not json").unwrap();
        assert!(HookHealth::read(root).is_none());
    }

    #[test]
    fn record_fire_is_a_noop_outside_a_repository() {
        let dir = tmp_repo();
        // No .atomic directory: must not create one, must not panic.
        record_fire(
            dir.path(),
            "opencode",
            "OpenCode",
            "stop",
            FireOutcome::Ok,
            None,
        );
        assert!(!dir.path().join(".atomic").exists());
    }

    #[test]
    fn parses_the_plugin_grammar() {
        let entry = parse_error_line(
            "2026-09-21T23:35:21.635Z ses_f4ee01e5dffeKYy29CIT5h2Lst session.idle Error: boom",
        )
        .expect("parses");
        assert_eq!(entry.at, "2026-09-21T23:35:21.635Z");
        assert_eq!(entry.verb, "session.idle");
        assert_eq!(entry.message, "Error: boom");
    }

    #[test]
    fn parses_the_shadow_grammar() {
        let entry = parse_error_line(
            "2026-09-20T10:00:00Z shadow-validate:drift view=dev 2 files disagree",
        )
        .expect("parses");
        assert_eq!(entry.verb, "shadow-validate:drift");
        assert_eq!(entry.message, "view=dev 2 files disagree");
    }

    #[test]
    fn rejects_lines_that_are_not_entries() {
        assert!(parse_error_line("just some prose").is_none());
        assert!(parse_error_line("").is_none());
    }

    #[test]
    fn continuation_lines_fold_into_their_entry() {
        // The real file: the plugin appends `message` unescaped, so a stack
        // trace spans lines and the report must not read each as its own
        // entry (or drop the useful tail).
        let body = "\
2026-09-15T23:07:37Z ses_abc before-tool exit 128: boom
  at dispatch (hooks.rs:1)
  at record (provenance.rs:2)
2026-09-15T23:07:38Z ses_abc before-tool exit 128: boom
  at dispatch (hooks.rs:1)
  at record (provenance.rs:2)
";
        let (entries, lines) = parse_error_log(body);
        assert_eq!(lines, 6);
        assert_eq!(entries.len(), 2, "continuations are not separate entries");
        assert!(entries[0].message.contains("at record (provenance.rs:2)"));
        // Two identical failures differing only in the timestamp still group.
        let report = aggregate(&entries, lines, 0);
        assert_eq!(report.classes.len(), 1);
        assert_eq!(report.classes[0].count, 2);
    }

    #[test]
    fn a_continuation_before_any_entry_is_dropped() {
        let (entries, lines) = parse_error_log("orphan continuation\nno timestamp here\n");
        assert!(entries.is_empty());
        assert_eq!(lines, 2);
    }

    #[test]
    fn groups_turn_numbers_into_one_class() {
        // The real shape from the atomic repo: 546 lines that differ only in
        // the turn number and the session id.
        let entries: Vec<HookErrorEntry> = (78..90)
            .map(|t| {
                parse_error_line(&format!(
                    "2026-09-15T23:07:37Z ses_abc before-tool exit 128: Provenance turn {t} is not running"
                ))
                .unwrap()
            })
            .collect();

        let report = aggregate(&entries, entries.len(), 0);
        assert_eq!(report.total, 12);
        assert_eq!(
            report.classes.len(),
            1,
            "turn numbers must not fragment the class"
        );
        assert_eq!(report.classes[0].count, 12);
    }

    #[test]
    fn distinct_failures_stay_distinct() {
        let entries = vec![
            parse_error_line("2026-09-15T23:00:00Z ses_abc before-tool turn 78 is not running")
                .unwrap(),
            parse_error_line(
                "2026-09-15T23:00:01Z ses_abc session.idle view changed during a tool",
            )
            .unwrap(),
        ];
        let report = aggregate(&entries, entries.len(), 0);
        assert_eq!(report.classes.len(), 2);
    }

    #[test]
    fn recent_is_newest_first() {
        let entries = vec![
            parse_error_line("2026-09-15T23:00:00Z ses_abc before-tool the older error").unwrap(),
            parse_error_line("2026-09-15T23:00:09Z ses_abc before-tool the newer error").unwrap(),
        ];
        let report = aggregate(&entries, entries.len(), 0);
        assert_eq!(report.recent[0].message, "the newer error");
        assert_eq!(report.recent[1].message, "the older error");
    }

    #[test]
    fn a_bounded_read_says_so_rather_than_claiming_the_whole_file() {
        let entries =
            vec![parse_error_line("2026-09-15T23:00:00Z ses_abc before-tool boom").unwrap()];
        let report = aggregate(&entries, entries.len(), 644_000);
        assert!(report.window.contains("older entries not read"));
    }

    #[test]
    fn error_signature_keeps_words_and_collapses_numbers() {
        assert_eq!(error_signature("turn 78 failed"), "turn N failed");
        assert_eq!(
            error_signature("session ses_f4ee01e5dffeKYy29CIT5h2Lst ended"),
            "session ID ended"
        );
    }

    #[test]
    fn error_signature_terminates_on_punctuation_and_spaces() {
        // Regression: an earlier version peeked without advancing, which
        // looped forever on the first alphabetic run.
        for msg in [
            "exit 128: Internal error: Failed to dispatch hook event",
            "a.b c_d 12 e-f",
            "   leading and trailing   ",
            "",
        ] {
            let _ = error_signature(msg);
        }
    }

    #[test]
    fn a_whole_file_read_keeps_its_first_entry() {
        // Regression: the tail-trim that drops a partial first line after a
        // seek was also applied when the whole file had been read, silently
        // losing the oldest entry on every small log.
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".atomic")).unwrap();
        std::fs::write(
            root.join(".atomic/hook-errors.log"),
            b"2026-09-25T20:00:00Z ses_aaa before-tool first entry\n\
              2026-09-25T20:00:01Z ses_aaa before-tool second entry\n",
        )
        .unwrap();

        let report = read_errors(root).expect("log is read");
        assert_eq!(report.total, 2);
        assert_eq!(report.first_seen.as_deref(), Some("2026-09-25T20:00:00Z"));
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "x".repeat(500);
        let out = truncate_detail(&long);
        assert!(out.chars().count() <= MAX_DETAIL_LEN + 3);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn relative_age_uses_coarse_units() {
        let now = "2026-09-25T18:00:00+00:00";
        assert_eq!(relative_age(now, "2026-09-25T17:59:30+00:00"), "30s ago");
        assert_eq!(relative_age(now, "2026-09-25T17:45:00+00:00"), "15m ago");
        assert_eq!(relative_age(now, "2026-09-25T14:00:00+00:00"), "4h ago");
        assert_eq!(relative_age(now, "2026-09-20T18:00:00+00:00"), "5d ago");
    }
}
