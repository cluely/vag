//! Claude Code session discovery.
//!
//! Storage (claude 2.1.x, format officially internal — parse defensively):
//! - Sessions: `<claude_dir>/projects/<encoded-cwd>/<uuid>.jsonl`, filename
//!   (minus extension) == session id. ONLY files directly inside a project
//!   dir are sessions; `<uuid>/` subdirectories hold subagent sidechains,
//!   workflows and tool-results — never descend into them.
//! - Dir-name encoding is LOSSY (`/`, `.`, `_`, `-` all become `-`): the
//!   real cwd must be read from a `"cwd"` field inside the jsonl records.
//!   The FIRST line is often a timestamp-less sidecar (`mode`,
//!   `permission-mode`, `file-history-snapshot`, `queue-operation`) without
//!   cwd — scan forward until a record carries one.
//! - Titles: sidecar records `{"type":"custom-title","customTitle":...}`
//!   (user-set, wins) and `{"type":"ai-title","aiTitle":...}` (auto,
//!   re-emitted over time — LAST occurrence wins for both). Files may
//!   predate both (legacy first-line `{"type":"summary","summary":...}` is a
//!   further fallback). Both windows collect titles and merge as
//!   tail custom > head custom > tail ai > head ai > legacy summary: a
//!   custom-title set once early would otherwise be displaced by the
//!   ai-titles re-emitted near EOF. (A title landing only between HEAD_MAX
//!   and len-TAIL_MAX remains a documented window tradeoff.)
//! - Preview: first record with `"type":"user"`, `isMeta` absent/false,
//!   whose message content doesn't start with `<command-name>`,
//!   `<local-command-caveat>` or `<system-reminder>`. Content may be a plain
//!   string or an array of `{type:"text",text:...}` blocks.
//! - Timestamps: message records carry ISO8601 `timestamp`. last_activity =
//!   last timestamped record (mtime is bumped by content-free appends; use
//!   mtime only as a cache key / cheap fallback). created = first
//!   timestamped record.
//! - `sessions-index.json` in a project dir is a lazily-created, often-stale
//!   cache — usable as an accelerator ONLY after stat()ing each fullPath.
//!   Correctness must not depend on it (this module ignores it entirely).
//!
//! PERFORMANCE CONTRACT: session files reach 30MB. Never read a whole file:
//! read up to `HEAD_MAX` bytes from the start (cwd, created, preview, legacy
//! summary, fallback titles) and up to `TAIL_MAX` bytes from the end
//! (titles, last_activity), splitting on newlines and tolerating a partial
//! first line in the tail chunk. Maintain an in-memory cache keyed by
//! (path, mtime, len) so re-scans are cheap.
//!
//! Real-store caveat (verified on 2.1.197 data): sessions started via queued
//! prompts can open with a SINGLE user record hundreds of KB long (pasted
//! content), pushing the first `cwd` past HEAD_MAX — a plain 128KB chunk
//! loses ~7% of real sessions. When the head chunk yields no cwd, a bounded
//! streaming fallback re-scans line-by-line up to `HEAD_STREAM_MAX`; typical
//! files never take that path.
//!
//! Cache choice: a module-level `OnceLock<Mutex<HashMap<PathBuf, _>>>` —
//! keeps the public `parse_session_file(&Path, &str)` signature from the
//! skeleton, lives for the process, and is bounded by the number of session
//! files (entries are overwritten in place when (mtime, len) changes, never
//! accumulated). `None` parse results are cached too, so empty/garbage files
//! don't get re-read every scan.
//!
//! Also here: the live-process registry `<claude_dir>/sessions/<pid>.json`
//! (`{pid, sessionId, cwd, name, ...}` — NO status field; files go stale
//! after crashes). Liveness = `kill(pid, 0) == 0` AND the entry's pid file
//! name matches. Used for "running outside vag" badges and for fork-id
//! discovery.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::Config;
use crate::types::{AgentKind, SessionKey, SessionMeta};

pub const HEAD_MAX: u64 = 128 * 1024;
pub const TAIL_MAX: u64 = 64 * 1024;

/// Hard cap for the streaming head fallback (giant first user record, see
/// module docs). Only reached by files whose first HEAD_MAX bytes contain no
/// cwd; still a fraction of a 30MB worst-case transcript.
const HEAD_STREAM_MAX: u64 = 4 * 1024 * 1024;

/// Previews are display + search text; cap so a giant first prompt (pasted
/// logs etc.) doesn't bloat every SessionMeta.
const PREVIEW_MAX_CHARS: usize = 200;

/// Registry files are tiny; refuse to slurp something absurd.
const REGISTRY_MAX_LEN: u64 = 256 * 1024;

/// Scan all claude sessions. Missing `projects/` dir → Ok(vec![]).
/// Per-file parse failures are skipped silently (defensive), per-dir IO
/// errors are skipped; only a completely unreadable root is an Err.
pub fn scan(cfg: &Config) -> Result<Vec<SessionMeta>> {
    let root = cfg.claude_dir().join("projects");
    let project_dirs = match fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    // Prompt-history overlay: the transcript tail can miss the last genuine
    // prompt when a single turn produced >TAIL_MAX of output; history.jsonl
    // (one line per typed prompt, kept indefinitely) fills the gap.
    let history = history_last_prompt(&cfg.claude_dir().join("history.jsonl"));

    let mut out = Vec::new();
    for proj in project_dirs.flatten() {
        let Ok(ptype) = proj.file_type() else {
            continue;
        };
        let pdir = proj.path();
        // Follow symlinked project dirs, skip plain files.
        if ptype.is_symlink() {
            if !pdir.is_dir() {
                continue;
            }
        } else if !ptype.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&pdir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Never descend into `<uuid>/` subdirectories (sidechains etc.).
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue; // sessions-index.json, .DS_Store, ...
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(mut meta) = parse_session_file(&path, id) {
                if let Some(ts) = history.get(&meta.key.id) {
                    meta.last_user_activity = meta.last_user_activity.max(Some(*ts));
                }
                out.push(meta);
            }
        }
    }
    Ok(out)
}

/// A claude process currently registered in `<claude_dir>/sessions/`.
#[derive(Debug, Clone)]
pub struct RunningClaude {
    #[allow(dead_code)] // future UI: process badges/tooltips
    pub pid: u32,
    pub session_id: String,
    #[allow(dead_code)] // future UI: process badges/tooltips
    pub cwd: Option<PathBuf>,
    #[allow(dead_code)] // future UI: process badges/tooltips
    pub name: Option<String>,
    /// When the process started (registry `startedAt`, unix millis). None
    /// when missing/unparseable — never trust `procStart` (timezone
    /// pitfalls). Not surfaced in the UI yet (turn tracking replaced
    /// uptime badges); kept for future tooltips.
    #[allow(dead_code)]
    pub started: Option<DateTime<Utc>>,
}

/// Live entries only (stale pid files filtered via kill(pid,0)).
pub fn running_sessions(cfg: &Config) -> Vec<RunningClaude> {
    let dir = cfg.claude_dir().join("sessions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if entry
            .metadata()
            .map(|m| m.len() > REGISTRY_MAX_LEN)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<RawRunning>(&text) else {
            continue;
        };
        let Some(pid) = raw.pid else { continue };
        // Stale/foreign-file defense: the registry file must be named after
        // the pid it claims.
        if stem != pid.to_string() {
            continue;
        }
        if pid == 0 || pid > i32::MAX as u64 {
            continue;
        }
        let Some(session_id) = raw.session_id.filter(|s| !s.is_empty()) else {
            continue;
        };
        if !pid_alive(pid as i32) {
            continue;
        }
        out.push(RunningClaude {
            pid: pid as u32,
            session_id,
            cwd: raw.cwd.filter(|c| !c.is_empty()).map(PathBuf::from),
            name: raw.name.filter(|n| !n.is_empty()),
            started: raw.started_at.as_ref().and_then(parse_started_at),
        });
    }
    out
}

fn pid_alive(pid: i32) -> bool {
    // Contract: liveness is kill(pid, 0) == 0 only. Never parse procStart
    // (UTC there vs local in `ps lstart` — timezone pitfalls).
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Parse a single session transcript (head+tail strategy above).
/// Returns None when the file yields no usable records at all (e.g. empty
/// file, non-session jsonl — concretely: no `cwd` recoverable, which also
/// means it can't be resumed). `id` is the filename stem.
pub fn parse_session_file(path: &Path, id: &str) -> Option<SessionMeta> {
    let md = fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let len = md.len();
    let mtime = md.modified().ok()?;

    if let Some(cached) = cache_lookup(path, mtime, len) {
        return cached;
    }
    let meta = parse_uncached(path, id, len, mtime);
    cache_store(path, mtime, len, meta.clone());
    meta
}

// ---------------------------------------------------------------------------
// cache

struct CacheEntry {
    mtime: SystemTime,
    len: u64,
    meta: Option<SessionMeta>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Some(result) on a (mtime, len) match; None means "must parse".
fn cache_lookup(path: &Path, mtime: SystemTime, len: u64) -> Option<Option<SessionMeta>> {
    let map = cache().lock().unwrap_or_else(|p| p.into_inner());
    let e = map.get(path)?;
    (e.mtime == mtime && e.len == len).then(|| e.meta.clone())
}

fn cache_store(path: &Path, mtime: SystemTime, len: u64, meta: Option<SessionMeta>) {
    let mut map = cache().lock().unwrap_or_else(|p| p.into_inner());
    map.insert(path.to_path_buf(), CacheEntry { mtime, len, meta });
}

// ---------------------------------------------------------------------------
// record shapes (every field optional; unknown fields ignored)

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    git_branch: Option<String>,
    is_meta: Option<bool>,
    message: Option<RawMessage>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    summary: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMessage {
    content: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawRunning {
    pid: Option<u64>,
    session_id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    /// Process start. Observed as unix epoch millis (number) on 2.1.197;
    /// accept an RFC3339 string too in case the format shifts.
    started_at: Option<serde_json::Value>,
}

fn parse_started_at(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    match v {
        serde_json::Value::Number(n) => {
            let ms = n.as_i64()?;
            // Sanity: epoch millis for any plausible date are > 10^12;
            // reject seconds-scale or negative values rather than showing
            // a 50-year uptime.
            if !(1_000_000_000_000..=10_000_000_000_000).contains(&ms) {
                return None;
            }
            DateTime::from_timestamp_millis(ms)
        }
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// head + tail parse

fn parse_uncached(path: &Path, id: &str, len: u64, mtime: SystemTime) -> Option<SessionMeta> {
    if len == 0 {
        return None;
    }

    let mut file = File::open(path).ok()?;
    let mut head_buf = Vec::with_capacity(len.min(HEAD_MAX) as usize);
    file.by_ref()
        .take(HEAD_MAX)
        .read_to_end(&mut head_buf)
        .ok()?;
    let head_truncated = len > HEAD_MAX;

    let head_lines = split_lines(&head_buf, false, head_truncated);
    let mut head = scan_head(&head_lines);
    if head.cwd.is_none() && head_truncated {
        // Rare path: a giant early line hid the first cwd-bearing record
        // beyond the chunk. Stream from the start, bounded.
        if let Some(streamed) = scan_head_streaming(&mut file) {
            head = streamed;
        }
    }

    // Separate tail read only when the head chunk didn't cover the file.
    let tail_buf: Option<Vec<u8>> = if head_truncated {
        let mut buf = Vec::with_capacity(TAIL_MAX as usize);
        file.seek(SeekFrom::End(-(TAIL_MAX as i64))).ok()?;
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    } else {
        None
    };

    let tail = match &tail_buf {
        Some(buf) => scan_tail(&split_lines(buf, true, false)),
        // Head chunk is the whole file; reverse-scan the same lines.
        None => scan_tail(&head_lines),
    };

    // A session we can't recover a cwd for can't be displayed usefully or
    // resumed — treat as "no usable records" (also covers foreign jsonl).
    let cwd = head.cwd?;

    // Title merge across both windows: a user-set custom-title may only
    // exist early (named once, then >TAIL_MAX of transcript follows) while
    // ai-titles are re-emitted near EOF — without the head fallbacks the
    // tail ai-title would silently displace the user's name.
    let title = tail
        .custom_title
        .or(head.custom_title)
        .or(tail.ai_title)
        .or(head.ai_title)
        .or(head.legacy_summary);
    let last_activity = tail
        .last_activity
        .or(head.created)
        .or_else(|| Some(DateTime::<Utc>::from(mtime)));

    Some(SessionMeta {
        key: SessionKey::new(AgentKind::Claude, id),
        title,
        preview: head.preview,
        cwd,
        created: head.created,
        last_activity,
        last_user_activity: tail.last_user,
        archived: false,
        source_path: path.to_path_buf(),
        git_branch: head.git_branch,
    })
}

/// Last-typed-prompt time per session id from `history.jsonl` lines like
/// `{"display":…,"timestamp":<ms>,"project":…,"sessionId":…}`. Bounded tail
/// read; missing/garbled file → empty map (defensive).
fn history_last_prompt(path: &Path) -> HashMap<String, DateTime<Utc>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Line {
        session_id: Option<String>,
        timestamp: Option<i64>, // unix millis
    }
    let mut out = HashMap::new();
    for line in tail_lines(path, HISTORY_TAIL_MAX) {
        let Ok(l) = serde_json::from_slice::<Line>(&line) else {
            continue;
        };
        let (Some(id), Some(ms)) = (l.session_id, l.timestamp) else {
            continue;
        };
        if let Some(ts) = DateTime::from_timestamp_millis(ms) {
            let e = out.entry(id).or_insert(ts);
            if ts > *e {
                *e = ts;
            }
        }
    }
    out
}

/// Complete lines from the last `max` bytes of a file (partial first line
/// dropped when truncated). Shared by the history overlays.
pub(crate) fn tail_lines(path: &Path, max: u64) -> Vec<Vec<u8>> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let truncated = len > max;
    if truncated && file.seek(SeekFrom::End(-(max as i64))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(len.min(max) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    split_lines(&buf, truncated, false)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

pub(crate) const HISTORY_TAIL_MAX: u64 = 512 * 1024;

/// Split a chunk into complete lines. `drop_first`/`drop_last` discard the
/// partial fragment at a truncated chunk boundary.
fn split_lines(buf: &[u8], drop_first: bool, drop_last: bool) -> Vec<&[u8]> {
    let mut segs: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
    if drop_last {
        segs.pop();
    }
    if drop_first && !segs.is_empty() {
        segs.remove(0);
    }
    segs.into_iter()
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .collect()
}

#[derive(Default)]
struct HeadInfo {
    cwd: Option<PathBuf>,
    created: Option<DateTime<Utc>>,
    preview: Option<String>,
    legacy_summary: Option<String>,
    git_branch: Option<String>,
    // Fallback titles for records that fell out of the tail window.
    custom_title: Option<String>,
    ai_title: Option<String>,
}

impl HeadInfo {
    fn is_complete(&self) -> bool {
        self.cwd.is_some()
            && self.created.is_some()
            && self.preview.is_some()
            && self.git_branch.is_some()
    }

    fn absorb(&mut self, rec: RawRecord) {
        if self.cwd.is_none()
            && let Some(c) = rec.cwd.as_deref().filter(|c| !c.is_empty())
        {
            self.cwd = Some(PathBuf::from(c));
        }
        if self.created.is_none() {
            self.created = rec.timestamp.as_deref().and_then(parse_ts);
        }
        if self.git_branch.is_none() {
            self.git_branch = rec.git_branch.filter(|b| !b.is_empty());
        }
        match rec.kind.as_deref() {
            Some("summary") if self.legacy_summary.is_none() => {
                self.legacy_summary = rec.summary.filter(|s| !s.trim().is_empty());
            }
            // Unconditional overwrite: the LAST occurrence in the head
            // window wins, matching the tail rule.
            Some("custom-title") => {
                if let Some(t) = rec.custom_title.filter(|s| !s.trim().is_empty()) {
                    self.custom_title = Some(t);
                }
            }
            Some("ai-title") => {
                if let Some(t) = rec.ai_title.filter(|s| !s.trim().is_empty()) {
                    self.ai_title = Some(t);
                }
            }
            Some("user") => {
                if self.preview.is_none()
                    && !rec.is_meta.unwrap_or(false)
                    && let Some(text) = rec
                        .message
                        .as_ref()
                        .and_then(|m| content_text(m.content.as_ref()))
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && !is_command_noise(trimmed) {
                        self.preview = Some(truncate_chars(trimmed, PREVIEW_MAX_CHARS));
                    }
                }
            }
            _ => {}
        }
    }
}

fn scan_head(lines: &[&[u8]]) -> HeadInfo {
    let mut info = HeadInfo::default();
    for line in lines {
        // Past completeness of the core fields, keep walking the window for
        // title records only (LAST occurrence must win, and they may sit
        // before len-TAIL_MAX in big files); the byte pre-filter avoids
        // json-parsing every remaining (potentially huge) line.
        if info.is_complete()
            && !contains_bytes(line, b"\"custom-title\"")
            && !contains_bytes(line, b"\"ai-title\"")
        {
            continue;
        }
        if let Ok(rec) = serde_json::from_slice::<RawRecord>(line) {
            info.absorb(rec);
        }
    }
    info
}

/// Bounded line-streaming head scan for files whose first HEAD_MAX bytes
/// carry no cwd (giant queued-prompt first record). Reads on past HEAD_MAX
/// only while the cwd is still missing, and never past HEAD_STREAM_MAX.
fn scan_head_streaming(file: &mut File) -> Option<HeadInfo> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut info = HeadInfo::default();
    let mut line: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut streamed: u64 = 0;
    loop {
        if info.is_complete()
            || (info.cwd.is_some() && streamed >= HEAD_MAX)
            || streamed >= HEAD_STREAM_MAX
        {
            break;
        }
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) => streamed += n as u64,
        }
        let mut l = line.as_slice();
        l = l.strip_suffix(b"\n").unwrap_or(l);
        l = l.strip_suffix(b"\r").unwrap_or(l);
        if l.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_slice::<RawRecord>(l) {
            info.absorb(rec);
        }
    }
    Some(info)
}

#[derive(Default)]
struct TailInfo {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_activity: Option<DateTime<Utc>>,
    /// Timestamp of the last GENUINE user message (not is_meta, not a
    /// tool_result-only record): the row-ordering anchor that doesn't move
    /// while the agent streams.
    last_user: Option<DateTime<Utc>>,
}

/// Reverse scan so the LAST title of each kind wins; cheap byte-substring
/// pre-filters keep us from json-parsing every (potentially huge) line.
fn scan_tail(lines: &[&[u8]]) -> TailInfo {
    let mut t = TailInfo::default();
    for line in lines.iter().rev() {
        if t.custom_title.is_some() && t.last_activity.is_some() && t.last_user.is_some() {
            break;
        }
        let want_custom = t.custom_title.is_none() && contains_bytes(line, b"\"custom-title\"");
        let want_ai = t.custom_title.is_none()
            && t.ai_title.is_none()
            && contains_bytes(line, b"\"ai-title\"");
        let want_ts = t.last_activity.is_none() && contains_bytes(line, b"\"timestamp\"");
        let want_user = t.last_user.is_none() && contains_bytes(line, b"\"user\"");
        if !(want_custom || want_ai || want_ts || want_user) {
            continue;
        }
        let Ok(rec) = serde_json::from_slice::<RawRecord>(line) else {
            continue;
        };
        match rec.kind.as_deref() {
            Some("custom-title") if t.custom_title.is_none() => {
                t.custom_title = rec.custom_title.filter(|s| !s.trim().is_empty());
            }
            Some("ai-title") if t.ai_title.is_none() => {
                t.ai_title = rec.ai_title.filter(|s| !s.trim().is_empty());
            }
            // Genuine prompt = user record, not injected meta, with real
            // text content (tool_result-only arrays yield None).
            Some("user")
                if t.last_user.is_none()
                    && !rec.is_meta.unwrap_or(false)
                    && rec
                        .message
                        .as_ref()
                        .and_then(|m| content_text(m.content.as_ref()))
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false) =>
            {
                t.last_user = rec.timestamp.as_deref().and_then(parse_ts);
            }
            _ => {}
        }
        if t.last_activity.is_none() {
            t.last_activity = rec.timestamp.as_deref().and_then(parse_ts);
        }
    }
    t
}

/// First text found: plain string content, or the first non-empty
/// `{type:"text"}` block of an array (tool_result-only arrays yield None).
fn content_text(content: Option<&serde_json::Value>) -> Option<String> {
    match content? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            for it in items {
                if it.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = it.get("text").and_then(|t| t.as_str())
                    && !text.trim().is_empty()
                {
                    return Some(text.to_string());
                }
            }
            None
        }
        _ => None,
    }
}

fn is_command_noise(text: &str) -> bool {
    text.starts_with("<command-name>")
        || text.starts_with("<local-command-caveat>")
        || text.starts_with("<system-reminder>")
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    const ID: &str = "39212683-afb1-4576-8884-6869c73ba4f9";

    fn cfg_for(root: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.behavior.claude_config_dir = Some(root.to_path_buf());
        cfg
    }

    fn project_dir(root: &Path, name: &str) -> PathBuf {
        let d = root.join("projects").join(name);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_session(dir: &Path, id: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn user_msg(cwd: &str, ts: &str, content: serde_json::Value) -> String {
        json!({
            "parentUuid": null, "isSidechain": false, "userType": "external",
            "cwd": cwd, "sessionId": ID, "version": "2.1.197", "type": "user",
            "message": {"role": "user", "content": content},
            "uuid": "11111111-1111-4111-8111-111111111111",
            "timestamp": ts, "gitBranch": "main"
        })
        .to_string()
    }

    fn assistant_msg(cwd: &str, ts: &str, text: &str) -> String {
        json!({
            "parentUuid": "11111111-1111-4111-8111-111111111111", "isSidechain": false,
            "cwd": cwd, "sessionId": ID, "version": "2.1.197", "type": "assistant",
            "message": {"role": "assistant",
                        "content": [{"type": "text", "text": text}],
                        "usage": {"input_tokens": 10, "output_tokens": 5}},
            "uuid": "22222222-2222-4222-8222-222222222222",
            "timestamp": ts, "gitBranch": "main"
        })
        .to_string()
    }

    fn mode_sidecar() -> String {
        json!({"type": "mode", "mode": "normal", "sessionId": ID}).to_string()
    }

    fn ai_title(t: &str) -> String {
        json!({"type": "ai-title", "aiTitle": t, "sessionId": ID}).to_string()
    }

    fn custom_title(t: &str) -> String {
        json!({"type": "custom-title", "customTitle": t, "sessionId": ID}).to_string()
    }

    #[test]
    fn typical_session() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "-Users-x-proj");
        let path = write_session(
            &proj,
            ID,
            &[
                mode_sidecar(),
                user_msg(
                    "/Users/x/proj",
                    "2026-07-01T12:00:00.000Z",
                    json!("fix the bug"),
                ),
                assistant_msg("/Users/x/proj", "2026-07-01T12:00:05.000Z", "on it"),
                ai_title("Fix auth bug"),
            ],
        );
        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.key, SessionKey::new(AgentKind::Claude, ID));
        assert_eq!(m.cwd, PathBuf::from("/Users/x/proj"));
        assert_eq!(m.title.as_deref(), Some("Fix auth bug"));
        assert_eq!(m.preview.as_deref(), Some("fix the bug"));
        assert_eq!(m.created.unwrap().to_rfc3339(), "2026-07-01T12:00:00+00:00");
        assert_eq!(
            m.last_activity.unwrap().to_rfc3339(),
            "2026-07-01T12:00:05+00:00"
        );
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert!(!m.archived);
        assert_eq!(m.source_path, path);
    }

    #[test]
    fn custom_title_beats_ai_title() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let path = write_session(
            &proj,
            ID,
            &[
                user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hi")),
                custom_title("auth work"),
                ai_title("Fix auth bug"), // later, but custom still wins
            ],
        );
        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.title.as_deref(), Some("auth work"));
    }

    #[test]
    fn last_ai_title_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let path = write_session(
            &proj,
            ID,
            &[
                user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hi")),
                ai_title("First title"),
                assistant_msg("/w", "2026-07-01T12:01:00.000Z", "ok"),
                ai_title("Second title"),
            ],
        );
        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.title.as_deref(), Some("Second title"));
    }

    #[test]
    fn legacy_summary_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let path = write_session(
            &proj,
            ID,
            &[
                json!({"type": "summary", "summary": "Old style title",
                       "leafUuid": "33333333-3333-4333-8333-333333333333"})
                .to_string(),
                user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hello")),
            ],
        );
        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.title.as_deref(), Some("Old style title"));
    }

    #[test]
    fn preview_skips_meta_and_command_noise_and_handles_arrays() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let mut meta_line: serde_json::Value = serde_json::from_str(&user_msg(
            "/w",
            "2026-07-01T12:00:00.000Z",
            json!("meta stuff"),
        ))
        .unwrap();
        meta_line["isMeta"] = json!(true);
        let path = write_session(
            &proj,
            ID,
            &[
                meta_line.to_string(),
                user_msg(
                    "/w",
                    "2026-07-01T12:00:01.000Z",
                    json!("<command-name>/clear</command-name>"),
                ),
                user_msg(
                    "/w",
                    "2026-07-01T12:00:02.000Z",
                    json!("<local-command-caveat>stdout follows</local-command-caveat>"),
                ),
                user_msg(
                    "/w",
                    "2026-07-01T12:00:03.000Z",
                    json!("<system-reminder>noise</system-reminder>"),
                ),
                // tool_result-only array: no text → not a preview
                user_msg(
                    "/w",
                    "2026-07-01T12:00:04.000Z",
                    json!([{"type": "tool_result", "tool_use_id": "t1", "content": "42"}]),
                ),
                user_msg(
                    "/w",
                    "2026-07-01T12:00:05.000Z",
                    json!([{"type": "text", "text": "real question"}]),
                ),
            ],
        );
        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.preview.as_deref(), Some("real question"));
    }

    #[test]
    fn empty_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let path = proj.join(format!("{ID}.jsonl"));
        fs::write(&path, "").unwrap();
        assert!(parse_session_file(&path, ID).is_none());
        assert!(scan(&cfg_for(tmp.path())).unwrap().is_empty());
    }

    #[test]
    fn garbage_and_cwdless_files_are_none() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let path = proj.join(format!("{ID}.jsonl"));
        fs::write(
            &path,
            "not json at all\n{\"type\":\"mode\",\"mode\":\"normal\"}\n",
        )
        .unwrap();
        assert!(parse_session_file(&path, ID).is_none());
    }

    #[test]
    fn big_file_head_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");

        let mut lines = vec![
            mode_sidecar(),
            user_msg("/only/in/head", "2026-07-01T09:00:00.000Z", json!("start")),
        ];
        padding_lines(&mut lines);
        lines.push(custom_title("only in tail"));
        lines.push(
            json!({"type": "assistant", "sessionId": ID,
                   "message": {"role": "assistant",
                               "content": [{"type": "text", "text": "bye"}]},
                   "timestamp": "2026-07-01T11:00:00.000Z"})
            .to_string(),
        );
        let path = write_session(&proj, ID, &lines);

        let len = fs::metadata(&path).unwrap().len();
        assert!(
            len > HEAD_MAX + TAIL_MAX,
            "fixture must exceed both windows, got {len}"
        );

        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.cwd, PathBuf::from("/only/in/head"));
        assert_eq!(m.title.as_deref(), Some("only in tail"));
        assert_eq!(m.preview.as_deref(), Some("start"));
        assert_eq!(m.created.unwrap().to_rfc3339(), "2026-07-01T09:00:00+00:00");
        assert_eq!(
            m.last_activity.unwrap().to_rfc3339(),
            "2026-07-01T11:00:00+00:00"
        );
    }

    /// ~300KB of cwd-less, title-less padding lines pushing the fixture past
    /// HEAD_MAX + TAIL_MAX so head and tail are disjoint windows.
    fn padding_lines(lines: &mut Vec<String>) {
        let pad = "x".repeat(1024);
        for _ in 0..300 {
            lines.push(
                json!({"type": "assistant", "sessionId": ID,
                       "message": {"role": "assistant",
                                   "content": [{"type": "text", "text": pad}]},
                       "timestamp": "2026-07-01T10:00:00.000Z"})
                .to_string(),
            );
        }
    }

    #[test]
    fn custom_title_in_head_window_beats_tail_ai_title() {
        // The user names the session once early (inside the head window,
        // before len-TAIL_MAX) and never again; ai-titles are re-emitted
        // near EOF. The user's name must still win — and the LAST
        // custom-title within the head window is the one kept.
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let mut lines = vec![
            mode_sidecar(),
            user_msg("/w", "2026-07-01T09:00:00.000Z", json!("start")),
            custom_title("draft name"),
            custom_title("my session"),
        ];
        padding_lines(&mut lines);
        lines.push(ai_title("Auto generated title"));
        let path = write_session(&proj, ID, &lines);
        let len = fs::metadata(&path).unwrap().len();
        assert!(
            len > HEAD_MAX + TAIL_MAX,
            "fixture must exceed both windows, got {len}"
        );

        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.title.as_deref(), Some("my session"));
    }

    #[test]
    fn ai_title_only_in_head_window_is_used() {
        // No titles in the tail window and no legacy summary: an ai-title
        // that only ever appeared early still beats having no title at all.
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let mut lines = vec![
            mode_sidecar(),
            user_msg("/w", "2026-07-01T09:00:00.000Z", json!("start")),
            ai_title("Early auto title"),
        ];
        padding_lines(&mut lines);
        let path = write_session(&proj, ID, &lines);
        let len = fs::metadata(&path).unwrap().len();
        assert!(
            len > HEAD_MAX + TAIL_MAX,
            "fixture must exceed both windows, got {len}"
        );

        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.title.as_deref(), Some("Early auto title"));
    }

    #[test]
    fn giant_first_user_record_still_yields_cwd() {
        // Mirrors real queued-prompt sessions: tiny sidecars, then ONE user
        // line far bigger than HEAD_MAX whose top-level cwd serializes AFTER
        // the message body (hand-built JSON to control key order).
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let pad = "y".repeat(200 * 1024);
        let giant = format!(
            concat!(
                r#"{{"parentUuid":null,"isSidechain":false,"userType":"external","#,
                r#""sessionId":"{id}","type":"user","#,
                r#""message":{{"role":"user","content":"{pad}"}},"#,
                r#""cwd":"/deep/cwd","uuid":"44444444-4444-4444-8444-444444444444","#,
                r#""timestamp":"2026-07-01T12:00:01.000Z","gitBranch":"main"}}"#
            ),
            id = ID,
            pad = pad
        );
        let path = write_session(
            &proj,
            ID,
            &[
                json!({"type": "queue-operation", "operation": "enqueue", "sessionId": ID,
                       "timestamp": "2026-07-01T12:00:00.000Z"})
                .to_string(),
                giant,
                assistant_msg("/deep/cwd", "2026-07-01T12:00:05.000Z", "ok"),
            ],
        );
        assert!(fs::metadata(&path).unwrap().len() > HEAD_MAX);

        let m = parse_session_file(&path, ID).unwrap();
        assert_eq!(m.cwd, PathBuf::from("/deep/cwd"));
        assert_eq!(m.created.unwrap().to_rfc3339(), "2026-07-01T12:00:00+00:00");
        assert_eq!(
            m.preview.as_deref().map(|p| p.len()),
            Some(PREVIEW_MAX_CHARS)
        );
        assert!(m.preview.unwrap().starts_with("yyyy"));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert_eq!(
            m.last_activity.unwrap().to_rfc3339(),
            "2026-07-01T12:00:05+00:00"
        );
    }

    #[test]
    fn subdirectories_are_not_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        write_session(
            &proj,
            ID,
            &[user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hi"))],
        );
        // uuid subdir with an agent transcript that would parse fine
        let sub = proj.join("d4ab0360-4f48-46a6-84da-390c21fdb1a1");
        fs::create_dir(&sub).unwrap();
        write_session(
            &sub,
            "agent-abc123",
            &[user_msg(
                "/w",
                "2026-07-01T12:00:00.000Z",
                json!("sidechain"),
            )],
        );

        let sessions = scan(&cfg_for(tmp.path())).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key.id, ID);
    }

    #[test]
    fn non_jsonl_noise_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        write_session(
            &proj,
            ID,
            &[user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hi"))],
        );
        fs::write(proj.join("sessions-index.json"), r#"{"entries":[]}"#).unwrap();
        fs::write(proj.join(".DS_Store"), [0u8, 1, 2, 3]).unwrap();
        fs::create_dir(proj.join("memory")).unwrap();
        fs::write(proj.join("memory").join("MEMORY.md"), "# notes").unwrap();

        let sessions = scan(&cfg_for(tmp.path())).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key.id, ID);
    }

    #[test]
    fn scan_maps_multiple_project_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = project_dir(tmp.path(), "-Users-x-alpha");
        let p2 = project_dir(tmp.path(), "-Users-x-beta");
        write_session(
            &p1,
            "aaaaaaaa-0000-4000-8000-000000000001",
            &[user_msg(
                "/Users/x/alpha",
                "2026-07-01T12:00:00.000Z",
                json!("one"),
            )],
        );
        write_session(
            &p2,
            "bbbbbbbb-0000-4000-8000-000000000002",
            &[user_msg(
                "/Users/x/beta",
                "2026-07-02T12:00:00.000Z",
                json!("two"),
            )],
        );

        let mut sessions = scan(&cfg_for(tmp.path())).unwrap();
        sessions.sort_by(|a, b| a.key.id.cmp(&b.key.id));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].cwd, PathBuf::from("/Users/x/alpha"));
        assert_eq!(sessions[1].cwd, PathBuf::from("/Users/x/beta"));
    }

    #[test]
    fn missing_projects_dir_is_empty_ok() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan(&cfg_for(tmp.path())).unwrap().is_empty());
    }

    #[test]
    fn cache_hit_keyed_on_path_mtime_len() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = project_dir(tmp.path(), "p");
        let mk = |title: &str| {
            [
                user_msg("/w", "2026-07-01T12:00:00.000Z", json!("hi")),
                ai_title(title),
                String::new(),
            ]
            .join("\n")
        };
        let path = proj.join(format!("{ID}.jsonl"));
        // Fixed mtime avoids fs timestamp-granularity flakiness.
        let fixed = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
        let set_mtime = |t: SystemTime| {
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(t)
                .unwrap()
        };

        fs::write(&path, mk("AAAA")).unwrap();
        set_mtime(fixed);
        assert_eq!(
            parse_session_file(&path, ID).unwrap().title.as_deref(),
            Some("AAAA")
        );

        // Same byte length + same mtime → cache hit, old title returned.
        fs::write(&path, mk("BBBB")).unwrap();
        set_mtime(fixed);
        assert_eq!(
            parse_session_file(&path, ID).unwrap().title.as_deref(),
            Some("AAAA")
        );

        // Bump mtime → cache miss, fresh parse.
        set_mtime(fixed + std::time::Duration::from_secs(2));
        assert_eq!(
            parse_session_file(&path, ID).unwrap().title.as_deref(),
            Some("BBBB")
        );
    }

    #[test]
    fn running_sessions_filters_dead_and_mismatched_pids() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        let entry = |pid: u64| {
            json!({"pid": pid, "sessionId": ID, "cwd": "/w", "startedAt": 1783488369403u64,
                   "procStart": "Wed Jul  8 05:26:08 2026", "version": "2.1.197",
                   "peerProtocol": 1, "kind": "interactive", "entrypoint": "cli",
                   "name": "vibe-aggregator-99", "nameSource": "derived"})
            .to_string()
        };
        // Live: our own pid.
        fs::write(dir.join(format!("{me}.json")), entry(me as u64)).unwrap();
        // Dead pid.
        fs::write(dir.join("999999999.json"), entry(999_999_999)).unwrap();
        // Filename doesn't match claimed pid → stale/foreign, filtered.
        fs::write(dir.join("123.json"), entry(me as u64)).unwrap();
        // Garbage file → skipped.
        fs::write(dir.join("777.json"), "{oops").unwrap();

        let running = running_sessions(&cfg_for(tmp.path()));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].pid, me);
        assert_eq!(running[0].session_id, ID);
        assert_eq!(running[0].cwd.as_deref(), Some(Path::new("/w")));
        assert_eq!(running[0].name.as_deref(), Some("vibe-aggregator-99"));
        assert_eq!(
            running[0].started,
            DateTime::from_timestamp_millis(1_783_488_369_403)
        );
    }

    #[test]
    fn started_at_accepts_millis_and_rfc3339_rejects_garbage() {
        use serde_json::json;
        // real 2.1.197 format: epoch millis
        assert_eq!(
            parse_started_at(&json!(1_783_488_369_403i64)),
            DateTime::from_timestamp_millis(1_783_488_369_403)
        );
        // rfc3339 fallback
        assert_eq!(
            parse_started_at(&json!("2026-07-08T05:00:00.000Z")),
            Some(Utc.with_ymd_and_hms(2026, 7, 8, 5, 0, 0).unwrap())
        );
        // seconds-scale, negative, or non-scalar values are rejected
        assert_eq!(parse_started_at(&json!(1_783_488_369i64)), None);
        assert_eq!(parse_started_at(&json!(-5)), None);
        assert_eq!(parse_started_at(&json!({"nested": true})), None);
    }

    #[test]
    fn running_sessions_tolerates_missing_or_bad_started_at() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        fs::write(
            dir.join(format!("{me}.json")),
            json!({"pid": me, "sessionId": ID, "startedAt": "yesterday-ish"}).to_string(),
        )
        .unwrap();
        let running = running_sessions(&cfg_for(tmp.path()));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].started, None);
    }

    #[test]
    fn running_sessions_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(running_sessions(&cfg_for(tmp.path())).is_empty());
    }

    /// Read-only sanity check against the real store. Run manually:
    /// `cargo test discovery::claude -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_store_smoke() {
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        let sessions = scan(&cfg).expect("real-store scan should not error");
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let again = scan(&cfg).expect("re-scan");
        let warm = t1.elapsed();
        let running = running_sessions(&cfg);
        println!(
            "real store: {} sessions, cold {:?}, warm {:?}, {} running",
            sessions.len(),
            cold,
            warm,
            running.len()
        );
        assert_eq!(sessions.len(), again.len());
        for s in &sessions {
            assert!(s.cwd.is_absolute(), "cwd not absolute: {:?}", s.cwd);
            assert!(!s.key.id.is_empty());
        }
        assert!(cold.as_secs_f64() < 2.0, "cold scan too slow: {cold:?}");
    }
}
