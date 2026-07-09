//! Codex CLI session discovery.
//!
//! Primary path — the sqlite index:
//! - `<codex_home>/state_*.sqlite` (currently `state_5.sqlite`; the filename
//!   version bumps on schema changes — glob and pick the highest N, keep the
//!   jsonl fallback working if the schema surprises us).
//! - The db is WAL-mode and a plain read-only open FAILS while codex holds
//!   it. NEVER open the live file directly: snapshot-copy `state_N.sqlite`
//!   plus `-wal`/`-shm` siblings (if present) into a temp dir, then open the
//!   copy read-only.
//! - Table `threads` columns (verify with PRAGMA table_info, treat all as
//!   optional): id, rollout_path (absolute), cwd, title, preview,
//!   first_user_message, created_at/updated_at (unix seconds; *_ms variants
//!   too), archived (0/1), thread_source ('user'/'automation'/...),
//!   has_user_event, git_branch, source.
//! - NOISE FILTER (critical): unless cfg.behavior.codex_show_automation,
//!   keep only rows with thread_source in ('user','') OR has_user_event=1
//!   (some machines have 90%+ automation threads).
//! - Verify each rollout_path still exists (stat) before emitting; archived
//!   rows keep archived=true (their file lives in
//!   `<codex_home>/archived_sessions/`).
//!
//! Fallback path (sqlite missing/unreadable/schema-mismatch):
//! - Walk `<codex_home>/sessions/YYYY/MM/DD/rollout-*.jsonl` (also match
//!   `*.jsonl.zst` — compression is feature-flagged upstream; for .zst emit
//!   the session with title/preview None rather than decoding).
//! - Line 1 is `{"timestamp","type":"session_meta","payload":{...}}` with
//!   payload.id (canonical; `session_id` is the legacy mirror), cwd, git.
//!   First `{"type":"event_msg","payload":{"type":"user_message",
//!   "message":...}}` = preview. All fields optional, unknown types skipped.
//! - Session names: `<codex_home>/session_index.jsonl` lines
//!   `{"id","thread_name","updated_at"}` — LAST entry per id wins.
//! - Archived: same filename glob in `<codex_home>/archived_sessions/`
//!   (flat dir), archived=true.
//! - The noise filter applies here too, on payload.thread_source alone:
//!   automation rollouts contain an injected user_message, so message
//!   presence cannot stand in for sqlite's has_user_event (verified on real
//!   data). Undecodable .zst files are always kept.
//!
//! The id in filenames is a UUIDv7 also present in line 1; prefer the
//! in-file id, fall back to the filename segment after the timestamp.
//! last_activity: sqlite updated_at, else file mtime (rollouts are
//! append-in-place so mtime is honest here). created: created_at, else the
//! line-1 timestamp (UTC), else the filename timestamp (LOCAL time!).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::config::Config;
use crate::types::{AgentKind, SessionKey, SessionMeta};

/// Bounded head-read of a rollout file: line 1 can carry huge
/// base_instructions and files run to tens of MB — never slurp.
const HEAD_MAX_BYTES: u64 = 256 * 1024;
const HEAD_MAX_LINES: usize = 200;
/// session_index.jsonl is small (one line per rename), cap for paranoia.
const INDEX_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Previews are display + search text, not transcripts.
const PREVIEW_MAX_CHARS: usize = 240;
/// sessions/YYYY/MM/DD is depth 4; leave slack, bound symlink cycles.
const WALK_MAX_DEPTH: usize = 8;

/// Scan codex sessions: sqlite snapshot first, jsonl walk as fallback.
/// Missing `<codex_home>` → Ok(vec![]).
pub fn scan(cfg: &Config) -> Result<Vec<SessionMeta>> {
    let home = cfg.codex_home();
    if !home.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = match find_state_db(&home) {
        Some(db) => match scan_sqlite(&db, cfg.behavior.codex_show_automation) {
            Ok(sessions) => sessions,
            Err(e) => {
                eprintln!(
                    "vag: codex index {} unreadable ({e:#}); falling back to rollout walk",
                    db.display()
                );
                scan_rollouts(cfg)?
            }
        },
        None => scan_rollouts(cfg)?,
    };
    // "Last message sent" overlay for row ordering: history.jsonl has one
    // line per typed prompt ({session_id, ts seconds, text}).
    let history = history_last_prompt(&home.join("history.jsonl"));
    for m in &mut sessions {
        if let Some(ts) = history.get(&m.key.id) {
            m.last_user_activity = m.last_user_activity.max(Some(*ts));
        }
    }
    Ok(sessions)
}

/// Last-typed-prompt time per session id from codex's history.jsonl.
fn history_last_prompt(path: &Path) -> HashMap<String, DateTime<Utc>> {
    #[derive(serde::Deserialize)]
    struct Line {
        session_id: Option<String>,
        ts: Option<i64>, // unix seconds
    }
    let mut out = HashMap::new();
    for line in
        crate::discovery::claude::tail_lines(path, crate::discovery::claude::HISTORY_TAIL_MAX)
    {
        let Ok(l) = serde_json::from_slice::<Line>(&line) else {
            continue;
        };
        let (Some(id), Some(s)) = (l.session_id, l.ts) else {
            continue;
        };
        if let Some(ts) = DateTime::from_timestamp(s, 0) {
            let e = out.entry(id).or_insert(ts);
            if ts > *e {
                *e = ts;
            }
        }
    }
    out
}

/// Force the jsonl-walk path (exposed for tests and as a recovery escape
/// hatch surfaced in warnings when sqlite fails).
pub fn scan_rollouts(cfg: &Config) -> Result<Vec<SessionMeta>> {
    let home = cfg.codex_home();
    if !home.is_dir() {
        return Ok(Vec::new());
    }
    let mut walk = Walk {
        names: load_session_index(&home),
        show_automation: cfg.behavior.codex_show_automation,
        seen: HashSet::new(),
        out: Vec::new(),
    };
    walk.dir(&home.join("sessions"), false, 0);
    walk.dir(&home.join("archived_sessions"), true, 0);
    Ok(walk.out)
}

// ---------------------------------------------------------------------------
// sqlite path
// ---------------------------------------------------------------------------

/// Highest-N `state_N.sqlite` under `home`, if any.
fn find_state_db(home: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(home).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(n) = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(bn, _)| n > *bn) {
            best = Some((n, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

fn scan_sqlite(db: &Path, show_automation: bool) -> Result<Vec<SessionMeta>> {
    let tmp = tempfile::tempdir().context("creating snapshot temp dir")?;
    let snapshot = snapshot_db(db, tmp.path())?;
    let rows = query_threads(&snapshot)?;
    Ok(rows
        .into_iter()
        .filter(|r| show_automation || !is_noise(r))
        .filter_map(thread_row_to_meta)
        .collect())
}

/// Copy the db plus `-wal`/`-shm` siblings (when present) into `dest_dir`.
/// Returns the path of the copy — the only file we ever hand to sqlite.
fn snapshot_db(db: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let file_name = db.file_name().context("state db path has no file name")?;
    let dest = dest_dir.join(file_name);
    fs::copy(db, &dest).with_context(|| format!("snapshotting {}", db.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut src = db.as_os_str().to_owned();
        src.push(suffix);
        let src = PathBuf::from(src);
        if src.exists() {
            let mut side = dest.as_os_str().to_owned();
            side.push(suffix);
            fs::copy(&src, PathBuf::from(side))
                .with_context(|| format!("snapshotting {}", src.display()))?;
        }
    }
    Ok(dest)
}

/// One row of `threads`, every field optional (schema drifts across
/// releases; PRAGMA table_info decides what we even SELECT).
#[derive(Debug, Default)]
struct ThreadRow {
    id: Option<String>,
    rollout_path: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    preview: Option<String>,
    first_user_message: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
    archived: Option<i64>,
    thread_source: Option<String>,
    has_user_event: Option<i64>,
    git_branch: Option<String>,
}

/// Open a *snapshot copy* and read the threads table. Read-write open on
/// purpose: the copy is private and sqlite needs write access to recover a
/// copied `-wal` (a strict read-only open of a WAL snapshot can fail).
fn query_threads(snapshot: &Path) -> Result<Vec<ThreadRow>> {
    let conn = Connection::open_with_flags(
        snapshot,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening snapshot {}", snapshot.display()))?;

    let mut have: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(threads)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if let Ok(name) = row.get::<_, String>(1) {
                have.insert(name);
            }
        }
    }
    if !have.contains("id") || !have.contains("rollout_path") {
        anyhow::bail!("threads table missing id/rollout_path (schema drift or absent table)");
    }

    const WANTED: [&str; 14] = [
        "id",
        "rollout_path",
        "cwd",
        "title",
        "preview",
        "first_user_message",
        "created_at",
        "updated_at",
        "created_at_ms",
        "updated_at_ms",
        "archived",
        "thread_source",
        "has_user_event",
        "git_branch",
    ];
    let cols: Vec<&str> = WANTED
        .iter()
        .copied()
        .filter(|c| have.contains(*c))
        .collect();
    let idx: HashMap<&str, usize> = cols.iter().enumerate().map(|(i, c)| (*c, i)).collect();
    // `cols` only ever holds our own identifiers — no injection surface.
    let sql = format!("SELECT {} FROM threads", cols.join(", "));

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let s = |c: &str| idx.get(c).and_then(|&i| str_at(row, i));
        let n = |c: &str| idx.get(c).and_then(|&i| int_at(row, i));
        out.push(ThreadRow {
            id: s("id"),
            rollout_path: s("rollout_path"),
            cwd: s("cwd"),
            title: s("title"),
            preview: s("preview"),
            first_user_message: s("first_user_message"),
            created_at: n("created_at"),
            updated_at: n("updated_at"),
            created_at_ms: n("created_at_ms"),
            updated_at_ms: n("updated_at_ms"),
            archived: n("archived"),
            thread_source: s("thread_source"),
            has_user_event: n("has_user_event"),
            git_branch: s("git_branch"),
        });
    }
    Ok(out)
}

/// Text → trimmed non-empty string ('' column defaults collapse to None).
fn str_at(row: &rusqlite::Row<'_>, i: usize) -> Option<String> {
    match row.get_ref(i) {
        Ok(ValueRef::Text(b)) => {
            let s = String::from_utf8_lossy(b);
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        _ => None,
    }
}

fn int_at(row: &rusqlite::Row<'_>, i: usize) -> Option<i64> {
    match row.get_ref(i) {
        Ok(ValueRef::Integer(v)) => Some(v),
        Ok(ValueRef::Real(v)) => Some(v as i64),
        Ok(ValueRef::Text(b)) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// Automation/subagent noise. NULL and '' thread_source are pre-migration
/// user threads (str_at already collapses '' to None).
fn is_noise(row: &ThreadRow) -> bool {
    let user_source = matches!(row.thread_source.as_deref(), None | Some("user"));
    !user_source && row.has_user_event.unwrap_or(0) == 0
}

fn thread_row_to_meta(row: ThreadRow) -> Option<SessionMeta> {
    let id = row.id?;
    let rollout_path = PathBuf::from(row.rollout_path?);
    // Session gone (codex delete / manual cleanup) → the row is stale cache.
    let fs_meta = fs::metadata(&rollout_path).ok()?;
    let mtime = fs_meta.modified().ok().map(DateTime::<Utc>::from);

    let created = row
        .created_at_ms
        .filter(|&v| v > 0)
        .and_then(DateTime::from_timestamp_millis)
        .or_else(|| row.created_at.and_then(ts_lenient));
    let last_activity = row
        .updated_at_ms
        .filter(|&v| v > 0)
        .and_then(DateTime::from_timestamp_millis)
        .or_else(|| row.updated_at.and_then(ts_lenient))
        .or(mtime);

    Some(SessionMeta {
        key: SessionKey::new(AgentKind::Codex, id),
        last_user_activity: None, // overlaid from history.jsonl in scan()
        title: row.title,
        preview: row.preview.or(row.first_user_message).map(|s| clip(&s)),
        cwd: row.cwd.map(PathBuf::from).unwrap_or_default(),
        created,
        last_activity,
        archived: row.archived.unwrap_or(0) != 0,
        source_path: rollout_path,
        git_branch: row.git_branch,
    })
}

/// Unix seconds → UTC; values too large to be seconds are treated as millis
/// (defends against a *_ms value landing in the seconds column).
fn ts_lenient(v: i64) -> Option<DateTime<Utc>> {
    if v <= 0 {
        return None;
    }
    if v > 100_000_000_000 {
        DateTime::from_timestamp_millis(v)
    } else {
        DateTime::from_timestamp(v, 0)
    }
}

fn clip(s: &str) -> String {
    let mut out: String = s.chars().take(PREVIEW_MAX_CHARS).collect();
    if s.chars().nth(PREVIEW_MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// jsonl fallback path
// ---------------------------------------------------------------------------

struct Walk {
    names: HashMap<String, String>,
    show_automation: bool,
    seen: HashSet<String>,
    out: Vec<SessionMeta>,
}

impl Walk {
    fn dir(&mut self, dir: &Path, archived: bool, depth: usize) {
        if depth > WALK_MAX_DEPTH {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            // file_type() doesn't follow symlinks — cycles get skipped.
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                self.dir(&path, archived, depth + 1);
            } else if ft.is_file()
                && let Some(meta) = self.rollout_to_meta(&path, archived)
                && self.seen.insert(meta.key.id.clone())
            {
                self.out.push(meta);
            }
        }
    }

    fn rollout_to_meta(&self, path: &Path, archived: bool) -> Option<SessionMeta> {
        let file_name = path.file_name()?.to_str()?;
        let parsed = parse_rollout_filename(file_name)?;
        let mtime = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(DateTime::<Utc>::from);

        let (head, preview) = if parsed.zst {
            (None, None) // never decode; unknown thread_source → always kept
        } else {
            read_rollout_head(path)
        };
        let head = head.unwrap_or_default();

        // Noise filter on thread_source alone: automation rollouts contain
        // an *injected* user_message, so unlike the sqlite path there is no
        // has_user_event signal to rescue human-joined automation threads.
        let user_source = matches!(
            head.thread_source.as_deref(),
            None | Some("") | Some("user")
        );
        if !self.show_automation && !user_source {
            return None;
        }

        let id = head.id.or(parsed.id)?;
        Some(SessionMeta {
            last_user_activity: None, // overlaid from history.jsonl in scan()
            title: self.names.get(&id).cloned(),
            preview: preview.as_deref().map(clip),
            key: SessionKey::new(AgentKind::Codex, id),
            cwd: head.cwd.map(PathBuf::from).unwrap_or_default(),
            // In-file timestamp is UTC (unambiguous); filename is local time.
            created: head.timestamp.or(parsed.created),
            last_activity: mtime,
            archived,
            source_path: path.to_path_buf(),
            git_branch: head.git_branch,
        })
    }
}

#[derive(Debug)]
struct FilenameInfo {
    id: Option<String>,
    created: Option<DateTime<Utc>>,
    zst: bool,
}

/// `rollout-2026-07-03T19-24-41-<uuid>.jsonl[.zst]` → parts. None means
/// "not a rollout file at all"; a matching prefix/suffix with a garbled
/// middle still yields a best-effort id.
fn parse_rollout_filename(name: &str) -> Option<FilenameInfo> {
    let (stem, zst) = if let Some(s) = name.strip_suffix(".jsonl.zst") {
        (s, true)
    } else if let Some(s) = name.strip_suffix(".jsonl") {
        (s, false)
    } else {
        return None;
    };
    let stem = stem.strip_prefix("rollout-")?;

    // "YYYY-MM-DDTHH-MM-SS" is 19 bytes, then '-', then the uuid.
    let (ts, id) = if stem.len() > 20 && stem.as_bytes()[19] == b'-' {
        (stem.get(..19), stem.get(20..))
    } else {
        (None, Some(stem))
    };
    let created = ts
        .and_then(|t| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H-%M-%S").ok())
        // Filename timestamps are LOCAL wall-clock time.
        .and_then(|n| Local.from_local_datetime(&n).earliest())
        .map(|d| d.with_timezone(&Utc));
    let id = id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(FilenameInfo { id, created, zst })
}

#[derive(Debug, Default)]
struct HeadInfo {
    id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    git_branch: Option<String>,
    thread_source: Option<String>,
}

/// Bounded scan of the file head: session_meta fields + first user_message.
fn read_rollout_head(path: &Path) -> (Option<HeadInfo>, Option<String>) {
    let Ok(file) = File::open(path) else {
        return (None, None);
    };
    let mut reader = BufReader::new(file.take(HEAD_MAX_BYTES));
    let mut buf = Vec::new();
    let mut head: Option<HeadInfo> = None;
    let mut preview: Option<String> = None;

    for _ in 0..HEAD_MAX_LINES {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        // Malformed / truncated-at-cap lines are skipped, not fatal.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let payload = v.get("payload");
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") if head.is_none() => {
                let g = |k: &str| {
                    payload
                        .and_then(|p| p.get(k))
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                };
                head = Some(HeadInfo {
                    // `id` is canonical; `session_id` the legacy mirror.
                    id: g("id")
                        .or_else(|| g("session_id"))
                        .filter(|s| !s.is_empty()),
                    cwd: g("cwd").filter(|s| !s.is_empty()),
                    timestamp: g("timestamp")
                        .or_else(|| {
                            v.get("timestamp")
                                .and_then(|t| t.as_str())
                                .map(str::to_string)
                        })
                        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    git_branch: payload
                        .and_then(|p| p.get("git"))
                        .and_then(|g| g.get("branch"))
                        .and_then(|b| b.as_str())
                        .map(str::to_string),
                    thread_source: g("thread_source"),
                });
            }
            Some("event_msg") if preview.is_none() => {
                if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str())
                    == Some("user_message")
                    && let Some(msg) = payload
                        .and_then(|p| p.get("message"))
                        .and_then(|m| m.as_str())
                {
                    let msg = msg.trim();
                    if !msg.is_empty() {
                        preview = Some(msg.to_string());
                    }
                }
            }
            _ => {}
        }
        if head.is_some() && preview.is_some() {
            break;
        }
    }
    (head, preview)
}

/// `session_index.jsonl`: `{"id","thread_name","updated_at"}` per line,
/// last entry per id wins. Missing/garbled file → empty map.
fn load_session_index(home: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(file) = File::open(home.join("session_index.jsonl")) else {
        return map;
    };
    let mut reader = BufReader::new(file.take(INDEX_MAX_BYTES));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default();
        let name = v
            .get("thread_name")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim();
        if !id.is_empty() && !name.is_empty() {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        cfg: Config,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        fs::create_dir_all(&home).unwrap();
        let mut cfg = Config::default();
        cfg.behavior.codex_home = Some(home.clone());
        Fixture {
            _tmp: tmp,
            home,
            cfg,
        }
    }

    /// Synthetic uuid — NEVER real session data.
    fn uid(n: u32) -> String {
        format!("019f0000-0000-7000-8000-{n:012}")
    }

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn meta_line(id: &str, cwd: &str, ts: &str) -> String {
        meta_line_src(id, cwd, ts, "user")
    }

    fn meta_line_src(id: &str, cwd: &str, ts: &str, thread_source: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}","timestamp":"{ts}","cwd":"{cwd}","originator":"codex_cli_rs","cli_version":"0.142.5","source":"cli","git":{{"commit_hash":"abc","branch":"main"}},"thread_source":"{thread_source}"}}}}"#
        )
    }

    fn user_msg_line(text: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-07-03T23:24:50.000Z","type":"event_msg","payload":{{"type":"user_message","message":"{text}"}}}}"#
        )
    }

    fn rollout_path(dir: &Path, ts: &str, id: &str) -> PathBuf {
        dir.join(format!("rollout-{ts}-{id}.jsonl"))
    }

    const FULL_SCHEMA: &str = "CREATE TABLE threads (
        id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT,
        preview TEXT, first_user_message TEXT, created_at INTEGER,
        updated_at INTEGER, archived INTEGER, thread_source TEXT,
        has_user_event INTEGER, git_branch TEXT, source TEXT)";

    #[allow(clippy::too_many_arguments)]
    fn insert_thread(
        conn: &Connection,
        id: &str,
        rollout_path: &Path,
        cwd: &str,
        title: Option<&str>,
        thread_source: Option<&str>,
        has_user_event: i64,
        archived: i64,
    ) {
        conn.execute(
            "INSERT INTO threads (id, rollout_path, cwd, title, preview,
             first_user_message, created_at, updated_at, archived,
             thread_source, has_user_event, git_branch, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                id,
                rollout_path.to_str().unwrap(),
                cwd,
                title,
                "preview text",
                "first user msg",
                1_782_400_000_i64,
                1_782_500_000_i64,
                archived,
                thread_source,
                has_user_event,
                "main",
                "cli",
            ],
        )
        .unwrap();
    }

    /// Existing rollout file so the stat check passes.
    fn touch_rollout(fx: &Fixture, id: &str, archived: bool) -> PathBuf {
        let dir = if archived {
            fx.home.join("archived_sessions")
        } else {
            fx.home.join("sessions/2026/07/03")
        };
        let path = rollout_path(&dir, "2026-07-03T19-24-41", id);
        write_file(&path, &meta_line(id, "/proj/x", "2026-07-03T23:24:41.019Z"));
        path
    }

    fn ids(sessions: &[SessionMeta]) -> Vec<&str> {
        let mut v: Vec<&str> = sessions.iter().map(|s| s.key.id.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn sqlite_happy_path_filters_noise_and_keeps_archived() {
        let fx = fixture();
        let conn = Connection::open(fx.home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(FULL_SCHEMA).unwrap();

        let user = uid(1);
        let auto = uid(2);
        let auto_with_user = uid(3);
        let arch = uid(4);
        let legacy_null = uid(5);
        for (id, archived) in [
            (&user, false),
            (&auto, false),
            (&auto_with_user, false),
            (&legacy_null, false),
            (&arch, true),
        ] {
            let p = touch_rollout(&fx, id, archived);
            let (src, hue, arc) = match id {
                i if i == &user => (Some("user"), 1, 0),
                i if i == &auto => (Some("automation"), 0, 0),
                i if i == &auto_with_user => (Some("automation"), 1, 0),
                i if i == &legacy_null => (None, 0, 0),
                _ => (Some("user"), 1, 1),
            };
            insert_thread(&conn, id, &p, "/proj/x", Some("My title"), src, hue, arc);
        }
        drop(conn);

        let sessions = scan(&fx.cfg).unwrap();
        assert_eq!(ids(&sessions), {
            let mut v = vec![
                user.as_str(),
                auto_with_user.as_str(),
                arch.as_str(),
                legacy_null.as_str(),
            ];
            v.sort();
            v
        });
        let s = sessions.iter().find(|s| s.key.id == user).unwrap();
        assert_eq!(s.title.as_deref(), Some("My title"));
        assert_eq!(s.preview.as_deref(), Some("preview text"));
        assert_eq!(s.cwd, PathBuf::from("/proj/x"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert!(!s.archived);
        assert_eq!(
            s.created.unwrap(),
            DateTime::from_timestamp(1_782_400_000, 0).unwrap()
        );
        assert_eq!(
            s.last_activity.unwrap(),
            DateTime::from_timestamp(1_782_500_000, 0).unwrap()
        );
        assert!(sessions.iter().find(|s| s.key.id == arch).unwrap().archived);

        // codex_show_automation=true includes the automation thread too.
        let mut cfg = fx.cfg.clone();
        cfg.behavior.codex_show_automation = true;
        assert_eq!(scan(&cfg).unwrap().len(), 5);
    }

    #[test]
    fn sqlite_row_with_deleted_rollout_is_dropped() {
        let fx = fixture();
        let conn = Connection::open(fx.home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(FULL_SCHEMA).unwrap();
        let alive = uid(1);
        let gone = uid(2);
        let p = touch_rollout(&fx, &alive, false);
        insert_thread(&conn, &alive, &p, "/proj/x", None, Some("user"), 1, 0);
        insert_thread(
            &conn,
            &gone,
            &fx.home.join("sessions/2026/07/03/nonexistent.jsonl"),
            "/proj/x",
            None,
            Some("user"),
            1,
            0,
        );
        drop(conn);

        assert_eq!(ids(&scan(&fx.cfg).unwrap()), vec![alive.as_str()]);
    }

    #[test]
    fn highest_numbered_state_db_wins() {
        // "12" < "5" lexically — the glob must compare numerically.
        let fx = fixture();
        let old_id = uid(1);
        let new_id = uid(2);
        for (db, id) in [("state_5.sqlite", &old_id), ("state_12.sqlite", &new_id)] {
            let conn = Connection::open(fx.home.join(db)).unwrap();
            conn.execute_batch(FULL_SCHEMA).unwrap();
            let p = touch_rollout(&fx, id, false);
            insert_thread(&conn, id, &p, "/proj/x", None, Some("user"), 1, 0);
        }
        assert_eq!(ids(&scan(&fx.cfg).unwrap()), vec![new_id.as_str()]);
    }

    #[test]
    fn original_db_is_never_opened_directly() {
        // Hold an EXCLUSIVE lock on the live db: any direct sqlite open of it
        // fails with SQLITE_BUSY, while a file-level snapshot copy still
        // works. No sessions/ dir exists, so a silent fallback would return
        // zero sessions and fail the assert below.
        let fx = fixture();
        let db_path = fx.home.join("state_5.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(FULL_SCHEMA).unwrap();
        let id = uid(1);
        let rp = fx
            .home
            .join(format!("rollout-2026-07-03T19-24-41-{id}.jsonl"));
        write_file(&rp, "{}\n"); // exists for the stat check; content unused
        insert_thread(&conn, &id, &rp, "/proj/x", None, Some("user"), 1, 0);
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let sessions = scan(&fx.cfg).unwrap();
        assert_eq!(ids(&sessions), vec![id.as_str()]);
        drop(conn);

        // And the snapshot helper itself targets the temp copy.
        let tmp = tempfile::tempdir().unwrap();
        let copy = snapshot_db(&db_path, tmp.path()).unwrap();
        assert_ne!(copy, db_path);
        assert!(copy.starts_with(tmp.path()));
    }

    #[test]
    fn wal_sidecar_files_are_snapshotted() {
        // Rows living only in the -wal (not yet checkpointed) must be seen.
        let fx = fixture();
        let conn = Connection::open(fx.home.join("state_5.sqlite")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(FULL_SCHEMA).unwrap();
        let id = uid(1);
        let p = touch_rollout(&fx, &id, false);
        insert_thread(&conn, &id, &p, "/proj/x", None, Some("user"), 1, 0);
        assert!(fx.home.join("state_5.sqlite-wal").exists());

        // Keep `conn` open so close-time checkpointing can't hide the bug.
        let sessions = scan(&fx.cfg).unwrap();
        assert_eq!(ids(&sessions), vec![id.as_str()]);
        drop(conn);
    }

    #[test]
    fn sqlite_missing_falls_back_to_rollout_walk() {
        let fx = fixture();
        let day = fx.home.join("sessions/2026/07/03");
        let named = uid(1);
        let bare = uid(2);
        let zst = uid(3);
        let arch = uid(4);
        let legacy = uid(5);

        // Full session: meta + junk + unknown types + user message.
        write_file(
            &rollout_path(&day, "2026-07-03T19-24-41", &named),
            &format!(
                "{}\nnot json at all\n{}\n{}\n{}\n",
                meta_line(&named, "/proj/alpha", "2026-07-03T23:24:41.019Z"),
                r#"{"timestamp":"t","type":"future_record","payload":{"x":1}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"not a preview"}}"#,
                user_msg_line("fix the login bug")
            ),
        );
        // Only a session_meta line — still emitted.
        write_file(
            &rollout_path(&day, "2026-07-03T20-00-00", &bare),
            &format!(
                "{}\n",
                meta_line(&bare, "/proj/beta", "2026-07-04T00:00:00.000Z")
            ),
        );
        // Legacy session_id field instead of id.
        write_file(
            &day.join(format!("rollout-2026-07-03T21-00-00-{legacy}.jsonl")),
            &format!(
                "{}\n",
                r#"{"timestamp":"2026-07-04T01:00:00.000Z","type":"session_meta","payload":{"session_id":"LEGACY-ID","cwd":"/proj/legacy"}}"#
            ),
        );
        // Compressed: emitted bare, never decoded (zstd magic + junk).
        fs::write(
            day.join(format!("rollout-2026-07-03T22-00-00-{zst}.jsonl.zst")),
            b"\x28\xb5\x2f\xfd junk",
        )
        .unwrap();
        // Archived flat dir.
        write_file(
            &fx.home
                .join("archived_sessions")
                .join(format!("rollout-2026-07-01T10-00-00-{arch}.jsonl")),
            &format!(
                "{}\n",
                meta_line(&arch, "/proj/old", "2026-07-01T14:00:00.000Z")
            ),
        );
        // Non-rollout noise is ignored.
        write_file(&day.join("notes.txt"), "hello");
        // Names: last entry per id wins; malformed lines skipped.
        write_file(
            &fx.home.join("session_index.jsonl"),
            &format!(
                "{}\ngarbage line\n{}\n",
                format_args!(
                    r#"{{"id":"{named}","thread_name":"Old name","updated_at":"2026-07-01T00:00:00Z"}}"#
                ),
                format_args!(
                    r#"{{"id":"{named}","thread_name":"Fix login","updated_at":"2026-07-03T00:00:00Z"}}"#
                ),
            ),
        );

        let sessions = scan(&fx.cfg).unwrap();
        let mut expect = vec![
            named.as_str(),
            bare.as_str(),
            "LEGACY-ID",
            zst.as_str(),
            arch.as_str(),
        ];
        expect.sort();
        assert_eq!(ids(&sessions), expect);

        let s = sessions.iter().find(|s| s.key.id == named).unwrap();
        assert_eq!(s.title.as_deref(), Some("Fix login"));
        assert_eq!(s.preview.as_deref(), Some("fix the login bug"));
        assert_eq!(s.cwd, PathBuf::from("/proj/alpha"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        // In-file UTC timestamp preferred over the local filename one.
        assert_eq!(
            s.created.unwrap(),
            DateTime::parse_from_rfc3339("2026-07-03T23:24:41.019Z").unwrap()
        );
        assert!(s.last_activity.is_some());
        assert!(!s.archived);

        let b = sessions.iter().find(|s| s.key.id == bare).unwrap();
        assert!(b.preview.is_none());
        assert!(b.title.is_none());

        let z = sessions.iter().find(|s| s.key.id == zst).unwrap();
        assert!(z.title.is_none() && z.preview.is_none());
        assert_eq!(z.cwd, PathBuf::new());
        // Filename timestamp (local wall clock) as created fallback.
        let expected_local = Local
            .from_local_datetime(
                &NaiveDateTime::parse_from_str("2026-07-03T22-00-00", "%Y-%m-%dT%H-%M-%S").unwrap(),
            )
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(z.created.unwrap(), expected_local);

        assert!(sessions.iter().find(|s| s.key.id == arch).unwrap().archived);
    }

    #[test]
    fn fallback_filters_automation_threads() {
        let fx = fixture();
        let day = fx.home.join("sessions/2026/07/03");
        // Automation rollouts contain an *injected* user_message — it must
        // NOT rescue them from the filter (verified against real data).
        let auto = uid(1);
        let user = uid(2);
        write_file(
            &rollout_path(&day, "2026-07-03T10-00-00", &auto),
            &format!(
                "{}\n{}\n",
                meta_line_src(&auto, "/proj/x", "2026-07-03T14:00:00.000Z", "automation"),
                user_msg_line("injected automation prompt")
            ),
        );
        write_file(
            &rollout_path(&day, "2026-07-03T12-00-00", &user),
            &format!(
                "{}\n",
                meta_line(&user, "/proj/x", "2026-07-03T16:00:00.000Z")
            ),
        );

        assert_eq!(ids(&scan_rollouts(&fx.cfg).unwrap()), vec![user.as_str()]);

        let mut cfg = fx.cfg.clone();
        cfg.behavior.codex_show_automation = true;
        assert_eq!(scan_rollouts(&cfg).unwrap().len(), 2);
    }

    #[test]
    fn sqlite_schema_mismatch_falls_back() {
        let fx = fixture();
        // A threads table missing the columns we depend on…
        let conn = Connection::open(fx.home.join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads (foo TEXT)")
            .unwrap();
        drop(conn);
        // …but rollouts on disk are still discovered.
        let id = uid(1);
        write_file(
            &rollout_path(
                &fx.home.join("sessions/2026/07/03"),
                "2026-07-03T19-24-41",
                &id,
            ),
            &format!(
                "{}\n",
                meta_line(&id, "/proj/x", "2026-07-03T23:24:41.019Z")
            ),
        );
        assert_eq!(ids(&scan(&fx.cfg).unwrap()), vec![id.as_str()]);

        // Same when the file isn't sqlite at all.
        fs::write(fx.home.join("state_5.sqlite"), "not a database").unwrap();
        assert_eq!(ids(&scan(&fx.cfg).unwrap()), vec![id.as_str()]);
    }

    #[test]
    fn missing_home_is_empty() {
        let mut cfg = Config::default();
        cfg.behavior.codex_home = Some(PathBuf::from("/nonexistent/vag-test-codex-home"));
        assert!(scan(&cfg).unwrap().is_empty());
        assert!(scan_rollouts(&cfg).unwrap().is_empty());
    }

    #[test]
    fn filename_parsing() {
        let ok = parse_rollout_filename("rollout-2026-07-03T19-24-41-abc-def.jsonl").unwrap();
        assert_eq!(ok.id.as_deref(), Some("abc-def"));
        assert!(!ok.zst && ok.created.is_some());

        let z = parse_rollout_filename("rollout-2026-07-03T19-24-41-abc.jsonl.zst").unwrap();
        assert!(z.zst);

        // Garbled middle: no timestamp, best-effort id.
        let odd = parse_rollout_filename("rollout-weird.jsonl").unwrap();
        assert_eq!(odd.id.as_deref(), Some("weird"));
        assert!(odd.created.is_none());

        assert!(parse_rollout_filename("notes.txt").is_none());
        assert!(parse_rollout_filename("other-2026-07-03T19-24-41-abc.jsonl").is_none());
    }

    #[test]
    fn timestamp_leniency() {
        assert_eq!(ts_lenient(0), None);
        assert_eq!(ts_lenient(-5), None);
        let secs = ts_lenient(1_782_498_711).unwrap();
        assert_eq!(secs, DateTime::from_timestamp(1_782_498_711, 0).unwrap());
        // Millis accidentally stored in a seconds column.
        let ms = ts_lenient(1_782_498_711_000).unwrap();
        assert_eq!(ms, secs);
    }

    /// Manual smoke test against the real ~/.codex (read-only). Run with:
    /// cargo test discovery::codex -- --ignored --nocapture
    #[test]
    #[ignore = "reads the real ~/.codex; run manually"]
    fn real_codex_home_smoke() {
        let cfg = Config::default();
        if !cfg.codex_home().is_dir() {
            return;
        }
        let t0 = std::time::Instant::now();
        let sessions = scan(&cfg).unwrap();
        let dt = t0.elapsed();
        let t1 = std::time::Instant::now();
        let rollouts = scan_rollouts(&cfg).unwrap();
        let dt_fb = t1.elapsed();
        let mut cfg_all = cfg.clone();
        cfg_all.behavior.codex_show_automation = true;
        let all = scan_rollouts(&cfg_all).unwrap();
        println!(
            "real ~/.codex: sqlite path {} sessions in {dt:?}; fallback walk {} \
             ({} with automation) in {dt_fb:?}",
            sessions.len(),
            rollouts.len(),
            all.len()
        );
        for s in sessions.iter().take(3) {
            // Titles only — never print transcript content.
            println!(
                "  {} archived={} title={:?}",
                s.key,
                s.archived,
                s.title.is_some()
            );
        }
    }
}
