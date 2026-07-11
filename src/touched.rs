//! Incremental "touched files" scanner for agent session transcripts.
//!
//! The per-session diff view needs to know which files an agent edited
//! during THIS vag runtime — not across the session's whole history — so the
//! git diff can be scoped to the agent's actual work. Neither provider
//! exposes that set directly, but both append every tool call to their
//! session transcript, so vag tails the transcript and extracts edit
//! operations from the records it already trusts for discovery.
//!
//! Two constraints shape everything here:
//!
//! - Both transcript formats are officially internal and drift between
//!   releases. Unknown fields and record shapes must be ignored, never
//!   errored on — a parse failure that breaks the UI loop is strictly worse
//!   than a missed file in a best-effort diff scope.
//! - Fork/resume copies prior history verbatim into the new transcript, so
//!   raw records are not evidence of THIS runtime's work. A timestamp gate
//!   (spawn time minus clock-skew tolerance) plus Claude tool_use-id dedup
//!   keeps replayed history out of the touched set.
//!
//! The scanner is polled from the existing UI tick, so reads are strictly
//! incremental: only bytes appended since the previous poll are parsed, and
//! a trailing partial line is carried until the writer completes it.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::types::AgentKind;

/// Records timestamped earlier than `since - SKEW_SECONDS` are treated as
/// replayed history, not this runtime's work. The tolerance absorbs clock
/// skew between vag's spawn timestamp and the provider's record timestamps.
const SKEW_SECONDS: i64 = 120;

/// Upper bound for a carried partial line. A single transcript record can
/// legitimately reach megabytes (whole-file Write inputs), but an unbroken
/// 8 MiB fragment means the line is not worth waiting for: drop it and let
/// its eventual tail fail JSON parsing as an ordinary malformed line.
const CARRY_MAX: usize = 8 * 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

/// Upper bound on bytes consumed per poll. poll() runs on the UI thread's
/// 1s tick; re-anchored scanners (vag restart) start at offset 0 of
/// transcripts that reach 30MB, and one uninterrupted scan of that would
/// visibly hitch every open pane. The backlog amortizes over a few ticks
/// instead. (Tiny in tests so the amortization itself is testable.)
const POLL_BUDGET: u64 = if cfg!(test) { 8 * 1024 } else { 4 * 1024 * 1024 };

/// Claude tool names whose input names a file the agent modified. Read-only
/// tools (Read, Grep, ...) and Bash are deliberately excluded: Bash can touch
/// anything, but its effects show up in git status, not in this scope hint.
const CLAUDE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Accumulated edit evidence for one session transcript.
#[derive(Debug, Clone, Default)]
pub struct TouchedFiles {
    /// Absolute paths the agent edited during this runtime, deduped.
    pub files: BTreeSet<PathBuf>,
    /// Total edit operations observed (monotonic; drives refresh triggers).
    pub edits_seen: u64,
}

/// Incremental tail-follower for one transcript file.
///
/// `poll` never errors: a missing transcript is a no-op (the file appears
/// slightly after spawn) and a shrunk one resets the read offset while
/// keeping already-collected files (they were real edits when observed).
pub struct TranscriptScanner {
    agent: AgentKind,
    path: PathBuf,
    /// `since - SKEW_SECONDS`; records timestamped earlier are skipped.
    cutoff: DateTime<Utc>,
    /// Byte offset of the next unread byte in the transcript.
    offset: u64,
    /// Trailing partial line from the previous poll (writer was mid-line).
    carry: Vec<u8>,
    /// Claude tool_use ids already counted. Fork copies duplicate tool_use
    /// blocks verbatim across session files, so the id — not the record —
    /// is the unit of dedup.
    seen_ids: HashSet<String>,
    touched: TouchedFiles,
}

impl TranscriptScanner {
    /// `since`: the runtime spawn time. Records timestamped before
    /// `since - SKEW` are ignored (fork/resume copies history into the
    /// transcript; old records are not this runtime's work).
    pub fn new(agent: AgentKind, path: PathBuf, since: DateTime<Utc>) -> Self {
        Self {
            agent,
            path,
            cutoff: since - Duration::seconds(SKEW_SECONDS),
            offset: 0,
            carry: Vec::new(),
            seen_ids: HashSet::new(),
            touched: TouchedFiles::default(),
        }
    }

    /// Read any new bytes and parse newly completed lines.
    /// Returns true if new touched files or edits were observed.
    /// Never errors: missing file = no-op (transcripts appear slightly after
    /// spawn); shrunk file = reset offset and rescan (defensive).
    pub fn poll(&mut self) -> bool {
        if self.agent == AgentKind::Shell {
            // Shell panes have no transcript semantics.
            return false;
        }
        let before = (self.touched.edits_seen, self.touched.files.len());
        let Ok(meta) = fs::metadata(&self.path) else {
            return false;
        };
        let len = meta.len();
        if len < self.offset {
            // Truncated or replaced. Rescan from the top: the carry belongs
            // to bytes that no longer exist and the id set must be rebuilt
            // alongside the offset, but collected files stay — they were
            // real edits when observed. edits_seen only ever grows.
            self.offset = 0;
            self.carry.clear();
            self.seen_ids.clear();
        }
        if len > self.offset {
            self.read_new_bytes(len);
        }
        (self.touched.edits_seen, self.touched.files.len()) != before
    }

    pub fn touched(&self) -> &TouchedFiles {
        &self.touched
    }

    /// The transcript being tailed — lets the owner detect a source_path
    /// change (e.g. discovery re-resolving a session) and rebuild.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read `[offset, len)` in bounded chunks, at most [`POLL_BUDGET`] per
    /// call. Capping at the stat'd length keeps offset accounting exact
    /// even if the writer appends mid-read; leftover backlog (and any
    /// mid-read appends) are picked up on later polls. Any I/O error simply
    /// ends this poll — whatever was consumed stays consumed.
    fn read_new_bytes(&mut self, len: u64) {
        let Ok(file) = fs::File::open(&self.path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut remaining = (len - self.offset).min(POLL_BUDGET);
        let mut chunk = vec![0_u8; READ_CHUNK];
        while remaining > 0 {
            let want = chunk.len().min(remaining as usize);
            let read = match reader.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            self.offset += read as u64;
            remaining -= read as u64;
            self.consume(&chunk[..read]);
        }
    }

    /// Append bytes to the carry, then parse every completed line. Only
    /// lines terminated by `\n` are parsed; the remainder is carried because
    /// the writer may be mid-line.
    fn consume(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        if let Some(last_newline) = self.carry.iter().rposition(|&b| b == b'\n') {
            let rest = self.carry.split_off(last_newline + 1);
            let complete = std::mem::replace(&mut self.carry, rest);
            for line in complete.split(|&b| b == b'\n') {
                if !line.is_empty() {
                    self.scan_line(line);
                }
            }
        }
        if self.carry.len() > CARRY_MAX {
            self.carry.clear();
        }
    }

    /// Parse one transcript line. Malformed JSON, non-object records and
    /// missing fields are silently skipped: this runs on the UI tick and
    /// must never be able to break the main loop.
    fn scan_line(&mut self, line: &[u8]) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if !record.is_object() {
            return;
        }
        // Records without a parseable timestamp are skipped for edit
        // extraction — every real tool record has one, and without it the
        // fork/resume history gate cannot be applied.
        let Some(timestamp) = record_timestamp(&record) else {
            return;
        };
        if timestamp < self.cutoff {
            return;
        }
        match self.agent {
            AgentKind::Claude => self.scan_claude(&record),
            AgentKind::Codex => self.scan_codex(&record),
            AgentKind::Shell => {}
        }
    }

    /// Claude: assistant records carry `message.content` arrays; each
    /// `tool_use` block of an edit-family tool names the target file in
    /// `input.file_path` (or `input.notebook_path` for NotebookEdit).
    fn scan_claude(&mut self, record: &Value) {
        let Some(content) = record.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !CLAUDE_EDIT_TOOLS.contains(&name) {
                continue;
            }
            // Every real tool_use block has an id; one without cannot be
            // deduped against fork copies, so it is skipped rather than
            // risking double counts.
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !self.seen_ids.insert(id.to_string()) {
                continue;
            }
            let input = block.get("input");
            let Some(path) = input
                .and_then(|input| input.get("file_path"))
                .and_then(Value::as_str)
                .or_else(|| {
                    input
                        .and_then(|input| input.get("notebook_path"))
                        .and_then(Value::as_str)
                })
            else {
                continue;
            };
            self.insert_edit(Path::new(path));
        }
    }

    /// Codex: `patch_apply_end` events report the applied patch; the keys of
    /// `payload.changes` are the absolute paths it modified. Failed applies
    /// changed nothing and are skipped. Codex has no tool_use-id equivalent,
    /// so replayed duplicates dedup naturally through the path set (while
    /// still counting toward `edits_seen`).
    fn scan_codex(&mut self, record: &Value) {
        let Some(payload) = record.get("payload") else {
            return;
        };
        if payload.get("type").and_then(Value::as_str) != Some("patch_apply_end") {
            return;
        }
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            return;
        }
        let Some(changes) = payload.get("changes").and_then(Value::as_object) else {
            return;
        };
        for path in changes.keys() {
            self.insert_edit(Path::new(path));
        }
    }

    /// Only absolute paths enter the set: both agents record absolute paths
    /// in practice, and a relative one would be ambiguous here (the scanner
    /// does not know the agent's cwd at the time of the edit). Paths are
    /// never canonicalized — the file may already be deleted, and resolving
    /// symlinks would desync the set from git's view of the worktree.
    fn insert_edit(&mut self, path: &Path) {
        if !path.is_absolute() {
            return;
        }
        self.touched.files.insert(path.to_path_buf());
        self.touched.edits_seen += 1;
    }
}

fn record_timestamp(record: &Value) -> Option<DateTime<Utc>> {
    let raw = record.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::OpenOptions;
    use std::io::Write;

    use chrono::SecondsFormat;
    use serde_json::json;

    fn ts(time: DateTime<Utc>) -> String {
        time.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn claude_record(time: DateTime<Utc>, blocks: Vec<Value>) -> String {
        json!({
            "timestamp": ts(time),
            "uuid": "3f6d3f21-0000-4000-8000-000000000001",
            "message": {"role": "assistant", "content": blocks},
        })
        .to_string()
    }

    fn codex_record(time: DateTime<Utc>, success: bool, paths: &[&str]) -> String {
        let mut changes = serde_json::Map::new();
        for path in paths {
            changes.insert((*path).into(), json!({"type": "update"}));
        }
        json!({
            "timestamp": ts(time),
            "type": "event_msg",
            "payload": {
                "type": "patch_apply_end",
                "call_id": "exec-1",
                "success": success,
                "changes": changes,
            },
        })
        .to_string()
    }

    fn append(path: &Path, text: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn files(scanner: &TranscriptScanner) -> Vec<String> {
        scanner
            .touched()
            .files
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn claude_extracts_edit_family_and_ignores_other_tools() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let line = claude_record(
            fresh,
            vec![
                tool_use("t1", "Edit", json!({"file_path": "/w/a.rs", "old_string": "x"})),
                tool_use("t2", "Write", json!({"file_path": "/w/b.rs", "content": "hi"})),
                tool_use("t3", "NotebookEdit", json!({"notebook_path": "/w/c.ipynb"})),
                tool_use("t4", "Bash", json!({"command": "touch /w/never.rs"})),
                tool_use("t5", "Read", json!({"file_path": "/w/read-only.rs"})),
                json!({"type": "text", "text": "done"}),
            ],
        );
        fs::write(&path, format!("{line}\n")).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path.clone(), since);
        assert_eq!(scanner.path(), path.as_path());
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs", "/w/c.ipynb"]);
        assert_eq!(scanner.touched().edits_seen, 3);
        assert!(!scanner.poll(), "no new bytes must report no change");
    }

    #[test]
    fn claude_duplicate_tool_use_id_across_polls_counts_once() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let original = tool_use("m1", "MultiEdit", json!({"file_path": "/w/a.rs"}));
        fs::write(
            &path,
            format!("{}\n", claude_record(fresh, vec![original.clone()])),
        )
        .unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path.clone(), since);
        assert!(scanner.poll());
        assert_eq!(scanner.touched().edits_seen, 1);

        // A fork copies the tool_use block verbatim into the transcript.
        append(&path, &format!("{}\n", claude_record(fresh, vec![original])));
        assert!(!scanner.poll(), "verbatim fork copy must not count again");
        assert_eq!(scanner.touched().edits_seen, 1);

        let other = tool_use("m2", "Edit", json!({"file_path": "/w/b.rs"}));
        append(&path, &format!("{}\n", claude_record(fresh, vec![other])));
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs"]);
        assert_eq!(scanner.touched().edits_seen, 2);
    }

    #[test]
    fn claude_skips_old_malformed_timestampless_and_relative_records() {
        let since = Utc::now();
        let old = since - Duration::seconds(200);
        let in_skew = since - Duration::seconds(60);
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lines = [
            // Older than since - 120s: replayed history, not this runtime.
            claude_record(old, vec![tool_use("t1", "Edit", json!({"file_path": "/w/old.rs"}))]),
            "{ this is not json".to_string(),
            "[1, 2, 3]".to_string(),
            // No timestamp: cannot be gated, skipped.
            json!({"message": {"content": [
                tool_use("t2", "Edit", json!({"file_path": "/w/untimed.rs"}))
            ]}})
            .to_string(),
            // Relative path: ambiguous, ignored.
            claude_record(fresh, vec![tool_use("t3", "Edit", json!({"file_path": "src/rel.rs"}))]),
            // Missing path fields entirely: skipped.
            claude_record(fresh, vec![tool_use("t4", "Write", json!({"content": "x"}))]),
            // Within the skew window: accepted.
            claude_record(in_skew, vec![tool_use("t5", "Edit", json!({"file_path": "/w/skew.rs"}))]),
        ];
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path, since);
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/skew.rs"]);
        assert_eq!(scanner.touched().edits_seen, 1);
    }

    #[test]
    fn codex_accepts_only_successful_patch_apply_end() {
        let since = Utc::now();
        let old = since - Duration::seconds(200);
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            codex_record(fresh, true, &["/w/a.rs", "/w/b.rs"]),
            codex_record(fresh, false, &["/w/failed.rs"]),
            json!({"timestamp": ts(fresh), "type": "event_msg",
                   "payload": {"type": "exec_command_end", "exit_code": 0}})
            .to_string(),
            json!({"timestamp": ts(fresh), "type": "response_item",
                   "payload": {"type": "message"}})
            .to_string(),
            codex_record(old, true, &["/w/history.rs"]),
            // Same path again: the set dedups, but the edit still counts.
            codex_record(fresh, true, &["/w/a.rs"]),
        ];
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Codex, path, since);
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs"]);
        assert_eq!(scanner.touched().edits_seen, 3);
    }

    #[test]
    fn incremental_appends_carry_partial_lines() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let line = |id: &str, file: &str| {
            claude_record(fresh, vec![tool_use(id, "Edit", json!({"file_path": file}))])
        };
        fs::write(&path, format!("{}\n", line("t1", "/w/a.rs"))).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path.clone(), since);
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs"]);

        // Append a complete line plus the first half of the next one: the
        // writer is mid-record, so only the complete line may be parsed.
        let third = line("t3", "/w/c.rs");
        let (head, tail) = third.split_at(third.len() / 2);
        append(&path, &format!("{}\n{head}", line("t2", "/w/b.rs")));
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs"]);
        assert_eq!(scanner.touched().edits_seen, 2);

        // Completing the carried line yields exactly one more edit.
        append(&path, &format!("{tail}\n"));
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs", "/w/c.rs"]);
        assert_eq!(scanner.touched().edits_seen, 3);

        // A dangling fragment with no newline reports nothing.
        append(&path, "{\"half");
        assert!(!scanner.poll());
        assert_eq!(scanner.touched().edits_seen, 3);
    }

    #[test]
    fn truncation_keeps_files_and_rescans_from_the_top() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let line = |id: &str, file: &str| {
            claude_record(fresh, vec![tool_use(id, "Edit", json!({"file_path": file}))])
        };
        fs::write(
            &path,
            format!("{}\n{}\n", line("t1", "/w/a.rs"), line("t2", "/w/b.rs")),
        )
        .unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path.clone(), since);
        assert!(scanner.poll());
        assert_eq!(scanner.touched().files.len(), 2);

        // Shrink to empty: no panic, offset resets, collected files stay.
        fs::write(&path, "").unwrap();
        assert!(!scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs"]);
        assert_eq!(scanner.touched().edits_seen, 2);

        // The reset also cleared seen ids: a reused id in the rewritten file
        // is fresh evidence again, and subsequent appends parse normally.
        append(&path, &format!("{}\n", line("t1", "/w/c.rs")));
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/a.rs", "/w/b.rs", "/w/c.rs"]);
        assert_eq!(scanner.touched().edits_seen, 3);
    }

    #[test]
    fn missing_file_is_a_noop_until_it_appears() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet.jsonl");
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path.clone(), since);
        assert!(!scanner.poll());
        assert!(scanner.touched().files.is_empty());

        let line = claude_record(
            fresh,
            vec![tool_use("t1", "Write", json!({"file_path": "/w/late.rs"}))],
        );
        fs::write(&path, format!("{line}\n")).unwrap();
        assert!(scanner.poll());
        assert_eq!(files(&scanner), ["/w/late.rs"]);
    }

    #[test]
    fn poll_budget_amortizes_a_large_backlog_without_losing_edits() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        // Well past the (test-sized) POLL_BUDGET in one pre-existing file —
        // the restart/re-anchor case where history is scanned from zero.
        let mut text = String::new();
        for i in 0..120 {
            text.push_str(&claude_record(
                fresh,
                vec![tool_use(
                    &format!("t{i}"),
                    "Edit",
                    json!({"file_path": format!("/w/file-{i:03}.rs")}),
                )],
            ));
            text.push('\n');
        }
        assert!(text.len() as u64 > 2 * POLL_BUDGET);
        fs::write(&path, &text).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Claude, path, since);
        assert!(scanner.poll());
        let first = scanner.touched().files.len();
        assert!(
            first > 0 && first < 120,
            "one poll must stop at the budget (got {first})"
        );
        let mut polls = 1;
        while scanner.touched().files.len() < 120 {
            scanner.poll();
            polls += 1;
            assert!(polls < 32, "backlog never drained");
        }
        assert_eq!(scanner.touched().edits_seen, 120);
        assert!(!scanner.poll(), "drained backlog reports no change");
    }

    #[test]
    fn shell_sessions_never_report_edits() {
        let since = Utc::now();
        let fresh = since + Duration::seconds(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let line = claude_record(
            fresh,
            vec![tool_use("t1", "Edit", json!({"file_path": "/w/a.rs"}))],
        );
        fs::write(&path, format!("{line}\n")).unwrap();
        let mut scanner = TranscriptScanner::new(AgentKind::Shell, path, since);
        assert!(!scanner.poll());
        assert!(!scanner.poll());
        assert!(scanner.touched().files.is_empty());
        assert_eq!(scanner.touched().edits_seen, 0);
    }
}
