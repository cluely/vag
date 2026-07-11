//! Argv builders and post-spawn helpers for session lifecycle actions.
//! All mutations of agent session stores go through the agents' own CLIs —
//! vag never moves/edits their files.
//!
//! CONTRACT (consumed by ui/app.rs):
//! - Working directory is always set on the spawned process itself
//!   (SpawnSpec.cwd) — no `--cd`-style flags. Claude resume REQUIRES child
//!   cwd == the session's original project path (cwd-scoped lookup); codex
//!   resume-by-uuid works from anywhere but adopts the current cwd as its
//!   working root, so it too must be spawned at the stored cwd.
//! - If the stored cwd no longer exists, builders return Err (the UI
//!   surfaces "project directory missing" instead of a confusing child
//!   error).
//! - Claude new sessions pre-assign a uuid via `--session-id` so the folder
//!   mapping can be written before the CLI even starts. Codex has no such
//!   flag: its id is discovered from the early SQLite row, with a rollout
//!   watcher as compatibility fallback.
//! - Fork: claude `--resume <id> --fork-session` (new id discovered via the
//!   live-process registry keyed by our child pid); codex `fork <uuid>`
//!   (UUID-only subcommand; new id via SQLite/rollout discovery).
//! - Env: children get TERM=xterm-256color and COLORTERM=truecolor
//!   overrides (matching what the embedded emulator implements); the rest of
//!   the parent env is inherited by the runtime layer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};

use crate::agent_events::{add_codex_tui_notifications, is_native_cli};
use crate::config::{Config, RemoteConfig};
use crate::types::{AgentKind, SessionMeta};

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra/override environment (name, value) pairs; parent env inherited.
    pub env: Vec<(String, String)>,
}

/// What a new-session spawn tells the caller about identity.
#[derive(Debug, Clone)]
pub enum PendingId {
    /// Claude: id pre-assigned via --session-id.
    Known(String),
    /// Codex: discover the id after spawn (SQLite first, rollout fallback).
    Discover,
}

const CLAUDE_REGISTRY_POLL: Duration = Duration::from_millis(100);
const CODEX_ROLLOUT_POLL: Duration = Duration::from_millis(200);
/// A new thread's own session timestamp should be close to process spawn,
/// even when its SQLite insert or rollout materialization is delayed.
pub const CODEX_ID_START_WINDOW: Duration = Duration::from_secs(120);
/// Rollout files stamped up to this long before `spawned_after` still count
/// (clock slack between our SystemTime sample and the fs timestamp).
const MTIME_SLACK: Duration = Duration::from_secs(2);

fn child_env() -> Vec<(String, String)> {
    vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ]
}

fn ensure_dir_exists(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Ok(()),
        _ => bail!("project directory missing: {}", path.display()),
    }
}

pub fn resume_spec(cfg: &Config, meta: &SessionMeta) -> Result<SpawnSpec> {
    ensure_dir_exists(&meta.cwd)?;
    let (program, extra) = cfg.command_for(meta.key.agent);
    let id = meta.key.id.clone();
    let args = match meta.key.agent {
        // User extras first, so they can never be mistaken for the --resume value.
        AgentKind::Claude => {
            let mut a = extra;
            a.push("--resume".to_string());
            a.push(id);
            a
        }
        // Subcommand first, extras after the positional id.
        AgentKind::Codex => {
            let mut a = vec!["resume".to_string(), id];
            a.extend(extra);
            a
        }
        // Shell panes are ephemeral: nothing is persisted, so there is
        // nothing to resume.
        AgentKind::Shell => bail!("shell panes are ephemeral — nothing to resume"),
    };
    Ok(SpawnSpec {
        program,
        args,
        cwd: meta.cwd.clone(),
        env: child_env(),
    })
}

/// `name` only applies to claude (`-n`); ignored for codex.
pub fn new_session_spec(
    cfg: &Config,
    agent: AgentKind,
    dir: &Path,
    name: Option<&str>,
) -> Result<(SpawnSpec, PendingId)> {
    ensure_dir_exists(dir)?;
    let (program, extra) = cfg.command_for(agent);
    let (args, pending) = match agent {
        AgentKind::Claude => {
            let session_id = uuid::Uuid::new_v4().to_string();
            let mut a = extra;
            a.push("--session-id".to_string());
            a.push(session_id.clone());
            if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
                a.push("-n".to_string());
                a.push(name.to_string());
            }
            (a, PendingId::Known(session_id))
        }
        // Codex has no naming flag; the id is learned from the new rollout.
        AgentKind::Codex => (extra, PendingId::Discover),
        // Shell panes aren't agent sessions; the UI builds their spawn
        // itself (local $SHELL / plain ssh).
        AgentKind::Shell => {
            bail!("shell panes aren't agent sessions — open one from the dashboard")
        }
    };
    let spec = SpawnSpec {
        program,
        args,
        cwd: dir.to_path_buf(),
        env: child_env(),
    };
    Ok((spec, pending))
}

pub fn fork_spec(cfg: &Config, meta: &SessionMeta) -> Result<(SpawnSpec, PendingId)> {
    ensure_dir_exists(&meta.cwd)?;
    let (program, extra) = cfg.command_for(meta.key.agent);
    let id = meta.key.id.clone();
    let args = match meta.key.agent {
        AgentKind::Claude => {
            let mut a = extra;
            a.push("--resume".to_string());
            a.push(id);
            a.push("--fork-session".to_string());
            a
        }
        AgentKind::Codex => {
            let mut a = vec!["fork".to_string(), id];
            a.extend(extra);
            a
        }
        // No transcript to fork from — shell panes have no history.
        AgentKind::Shell => bail!("shell panes can't be forked"),
    };
    let spec = SpawnSpec {
        program,
        args,
        cwd: meta.cwd.clone(),
        env: child_env(),
    };
    // Both agents assign the fork's id themselves — discover it post-spawn.
    Ok((spec, PendingId::Discover))
}

/// Shell out to `codex archive <uuid>` / `codex unarchive <uuid>`
/// (keeps codex's sqlite + Desktop catalog consistent). Claude has no
/// per-session delete/archive CLI — callers use the vag-level hidden flag.
pub fn codex_set_archived(cfg: &Config, id: &str, archived: bool) -> Result<()> {
    let verb = if archived { "archive" } else { "unarchive" };
    codex_cli(cfg, &[verb, id])
}

/// Permanently delete a codex session via its own CLI (`--force` skips the
/// interactive prompt — vag confirms in its own modal; UUID required, which
/// every scanned codex id is). Keeps sqlite + the Desktop catalog in sync.
pub fn codex_delete(cfg: &Config, id: &str) -> Result<()> {
    codex_cli(cfg, &["delete", "--force", id])
}

fn codex_cli(cfg: &Config, args: &[&str]) -> Result<()> {
    let (program, _) = cfg.command_for(AgentKind::Codex);
    // temp_dir as cwd: can't fail because vag's own cwd was deleted.
    let output = std::process::Command::new(&program)
        .args(args)
        .current_dir(std::env::temp_dir())
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => bail!(
            "failed to run `{program} {}`: {e} — is codex installed?",
            args.join(" ")
        ),
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut msg: String = stderr.trim().chars().take(500).collect();
    if msg.is_empty() {
        msg = "(no stderr)".to_string();
    }
    bail!(
        "`{program} {}` failed ({}): {msg}",
        args.join(" "),
        output.status
    )
}

/// Poll `f` until it yields Some or `deadline` elapses; always checks at
/// least once, and never sleeps past the deadline.
fn poll_until<T>(
    deadline: Duration,
    poll: Duration,
    mut f: impl FnMut() -> Option<T>,
) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return None;
        }
        std::thread::sleep(poll.min(deadline - elapsed));
    }
}

/// Poll `<claude_dir>/sessions/<child_pid>.json` for the sessionId of a
/// claude child we spawned (fork / pathological cases where --session-id was
/// not applicable). Registry files whose mtime predates `spawned_after`
/// (minus slack) are ignored: crashes orphan stale `<pid>.json` files, and a
/// recycled pid would otherwise instantly resolve to the OLD file's session
/// id. The real child rewrites its entry (fresh mtime), so polling still
/// converges. Returns None after `deadline`.
pub fn discover_claude_session_id(
    cfg: &Config,
    child_pid: u32,
    spawned_after: SystemTime,
    deadline: Duration,
) -> Option<String> {
    let path = cfg
        .claude_dir()
        .join("sessions")
        .join(format!("{child_pid}.json"));
    let min_mtime = spawned_after
        .checked_sub(MTIME_SLACK)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    poll_until(deadline, CLAUDE_REGISTRY_POLL, || {
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= min_mtime);
        if !fresh {
            return None;
        }
        registry_session_id(&path)
    })
}

fn registry_session_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = v.get("sessionId")?.as_str()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Resolve a newly launched Codex thread. The SQLite index is checked first:
/// recent Codex versions insert the thread there before lazily creating its
/// rollout. The rollout walk remains the compatibility fallback. Returns
/// None after `deadline`; callers may retry with the original spawn time.
#[cfg(test)]
pub fn discover_codex_session_id(
    cfg: &Config,
    cwd: &Path,
    spawned_after: SystemTime,
    deadline: Duration,
) -> Option<String> {
    discover_codex_session_id_excluding(cfg, cwd, spawned_after, deadline, &HashSet::new())
}

/// Same resolver with ids that are already claimed by another live runtime.
/// Collision retries use this to advance to the next eligible SQLite/rollout
/// candidate instead of returning the same id forever.
pub fn discover_codex_session_id_excluding(
    cfg: &Config,
    cwd: &Path,
    spawned_after: SystemTime,
    deadline: Duration,
    excluded_ids: &HashSet<String>,
) -> Option<String> {
    let root = cfg.codex_home().join("sessions");
    let spawned_before = spawned_after
        .checked_add(CODEX_ID_START_WINDOW)
        .unwrap_or(spawned_after);
    let target_forms = cwd_match_forms(cwd);
    // This boundary is sampled before process spawn, so no backwards slack
    // is needed (and admitting pre-launch files can steal an old session).
    let min_created = spawned_after;
    // Skip date dirs older than 2 days: "YYYY/MM/DD" sorts lexicographically.
    let cutoff = chrono::Local::now()
        .date_naive()
        .checked_sub_days(chrono::Days::new(2))
        .map(|d| d.format("%Y/%m/%d").to_string())
        .unwrap_or_default();
    let mut last_index_check: Option<Instant> = None;
    let mut index_checks = 0_u32;
    // Once a real index query succeeds it is authoritative, even when the
    // new CLI row is not present yet. Falling through to a merely temporal
    // rollout match at that point can claim a same-cwd subagent created by
    // another Codex process while this child is still starting.
    let mut index_authoritative = false;
    poll_until(deadline, CODEX_ROLLOUT_POLL, || {
        // Snapshotting a live WAL database is intentionally safer than a
        // direct open, but not free. Check twice around the startup race,
        // then back off while the cheap rollout walk keeps polling.
        let index_interval = if index_checks < 2 {
            Duration::from_millis(400)
        } else {
            Duration::from_secs(5)
        };
        if last_index_check.is_none_or(|last| last.elapsed() >= index_interval) {
            last_index_check = Some(Instant::now());
            index_checks += 1;
            if crate::discovery::codex::has_thread_index(cfg) {
                let source_aware = crate::discovery::codex::thread_index_is_source_aware(cfg);
                match crate::discovery::codex::find_new_cli_thread_id(
                    cfg,
                    cwd,
                    spawned_after,
                    spawned_before,
                    excluded_ids,
                ) {
                    Ok(Some(id)) => return Some(id),
                    Ok(None) if source_aware => index_authoritative = true,
                    Ok(None) => {}
                    // An unreadable/drifted index gives no source evidence;
                    // retain the rollout compatibility fallback.
                    Err(_) => {}
                }
            }
        }
        if index_authoritative {
            return None;
        }
        scan_rollouts_once(
            &root,
            &cutoff,
            &target_forms,
            min_created,
            spawned_before,
            excluded_ids,
        )
        .map(|(_, id)| id)
    })
}

/// One pass over recent date dirs; returns the earliest eligible session
/// start after the launch boundary. Collision retries exclude claimed ids
/// and advance to the next candidate.
fn scan_rollouts_once(
    root: &Path,
    cutoff: &str,
    target_forms: &[String],
    min_created: SystemTime,
    max_started: SystemTime,
    excluded_ids: &HashSet<String>,
) -> Option<(SystemTime, String)> {
    let mut best: Option<(SystemTime, String)> = None;
    let cutoff_year = cutoff.get(..4).unwrap_or("");
    for (year, year_path) in read_subdirs(root) {
        if year.as_str() < cutoff_year {
            continue;
        }
        for (month, month_path) in read_subdirs(&year_path) {
            for (day, day_path) in read_subdirs(&month_path) {
                if format!("{year}/{month}/{day}").as_str() < cutoff {
                    continue;
                }
                scan_day_dir(
                    &day_path,
                    target_forms,
                    min_created,
                    max_started,
                    excluded_ids,
                    &mut best,
                );
            }
        }
    }
    best
}

fn scan_day_dir(
    dir: &Path,
    target_forms: &[String],
    min_created: SystemTime,
    max_started: SystemTime,
    excluded_ids: &HashSet<String>,
    best: &mut Option<(SystemTime, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // .jsonl only: .zst candidates can't be line-read without a decoder,
        // and a rollout this fresh is still being written uncompressed.
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        // Gate on creation time (macOS + modern Linux report it), not mtime:
        // a pre-existing session's rollout gets a fresh mtime on every
        // append, but only a file created after our spawn can belong to the
        // new child. mtime is the fallback where created() is unsupported.
        if !system_time_at_or_after(md.created().unwrap_or(mtime), min_created) {
            continue;
        }
        let Some(line) = read_first_line(&entry.path()) else {
            continue;
        };
        let Some((cand_cwd, id, session_started)) = parse_session_meta_line(&line) else {
            continue;
        };
        if excluded_ids.contains(&id) {
            continue;
        }
        if !cwd_matches(&cand_cwd, target_forms) {
            continue;
        }
        let start_marker = session_started.unwrap_or_else(|| md.created().unwrap_or(mtime));
        if !system_time_in_window(start_marker, min_created, max_started) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(started, _)| start_marker < *started)
        {
            *best = Some((start_marker, id));
        }
    }
}

/// Subdirectory (name, path) pairs; missing/unreadable dir → empty.
fn read_subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

/// Cap for a rollout's line-1 read. Sized like discovery/codex.rs's
/// HEAD_MAX_BYTES (256KB): session_meta line 1 carries base_instructions
/// plus dynamic MCP tool schemas — already >40KB in the wild and growing
/// with config — and a line over the cap never gets a terminating newline
/// inside the window, so it would fail to parse on every poll, forever.
const ROLLOUT_LINE1_MAX: u64 = 256 * 1024;

/// Read only the first line (capped) — rollouts can grow large and line 1
/// may be mid-write; any read/decode failure → None (caller keeps polling).
fn read_first_line(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(ROLLOUT_LINE1_MAX));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(line)
}

/// Extract (cwd, id) from an interactive CLI rollout's session_meta line;
/// defensive — any missing identity piece or explicit non-CLI source → None.
fn parse_session_meta_line(line: &str) -> Option<(String, String, Option<SystemTime>)> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let payload = v.get("payload")?;
    if !rollout_source_is_interactive_cli(payload) {
        return None;
    }
    let cwd = payload.get("cwd").and_then(|c| c.as_str())?.to_string();
    let id = payload
        .get("id")
        .and_then(|i| i.as_str())
        .or_else(|| payload.get("session_id").and_then(|i| i.as_str()))?;
    if id.is_empty() {
        None
    } else {
        let timestamp = payload
            .get("timestamp")
            .or_else(|| v.get("timestamp"))
            .and_then(|t| t.as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| SystemTime::from(t.with_timezone(&chrono::Utc)));
        Some((cwd, id.to_string(), timestamp))
    }
}

/// Legacy session_meta records predate both fields and remain eligible. When
/// modern metadata is explicit, only a user-facing CLI (or its short-lived
/// `unknown` bootstrap state) may identify the child Vag just launched.
fn rollout_source_is_interactive_cli(payload: &serde_json::Value) -> bool {
    let source_ok = match payload.get("source") {
        None => true,
        Some(serde_json::Value::String(source)) => matches!(source.as_str(), "cli" | "unknown"),
        // Structured sources identify subagents; other scalar/null values
        // are modern but provide no evidence of an interactive CLI.
        Some(_) => false,
    };
    let thread_source_ok = match payload.get("thread_source") {
        None => true,
        Some(serde_json::Value::String(source)) => matches!(source.as_str(), "" | "user"),
        Some(_) => false,
    };
    source_ok && thread_source_ok
}

/// Whole-second legacy timestamps compare at second precision; modern
/// subsecond timestamps use the exact launch boundary.
fn system_time_in_window(candidate: SystemTime, start: SystemTime, end: SystemTime) -> bool {
    let epoch = |time: SystemTime| time.duration_since(SystemTime::UNIX_EPOCH).ok();
    let (Some(candidate), Some(start), Some(end)) = (epoch(candidate), epoch(start), epoch(end))
    else {
        return false;
    };
    if candidate.subsec_nanos() == 0 {
        candidate.as_secs() >= start.as_secs() && candidate.as_secs() <= end.as_secs()
    } else {
        candidate >= start && candidate <= end
    }
}

fn system_time_at_or_after(candidate: SystemTime, start: SystemTime) -> bool {
    let epoch = |time: SystemTime| time.duration_since(SystemTime::UNIX_EPOCH).ok();
    let (Some(candidate), Some(start)) = (epoch(candidate), epoch(start)) else {
        return false;
    };
    if candidate.subsec_nanos() == 0 {
        candidate.as_secs() >= start.as_secs()
    } else {
        candidate >= start
    }
}

/// Normalized string forms a path may appear as in a rollout: raw and
/// canonicalized (both trailing-slash-trimmed).
fn cwd_match_forms(p: &Path) -> Vec<String> {
    let mut forms = vec![trim_trailing_slash(&p.to_string_lossy()).to_string()];
    if let Ok(canon) = p.canonicalize() {
        let c = trim_trailing_slash(&canon.to_string_lossy()).to_string();
        if !forms.contains(&c) {
            forms.push(c);
        }
    }
    forms
}

fn cwd_matches(candidate: &str, target_forms: &[String]) -> bool {
    let trimmed = trim_trailing_slash(candidate);
    if target_forms.iter().any(|f| f == trimmed) {
        return true;
    }
    // The rollout may store a pre-symlink-resolution path (or vice versa).
    if let Ok(canon) = Path::new(trimmed).canonicalize() {
        let c = trim_trailing_slash(&canon.to_string_lossy()).to_string();
        return target_forms.contains(&c);
    }
    false
}

fn trim_trailing_slash(s: &str) -> &str {
    let t = s.trim_end_matches('/');
    if t.is_empty() && !s.is_empty() {
        "/"
    } else {
        t
    }
}

/// Best-effort check that an agent CLI is installed & callable; returns the
/// resolved program name or a user-facing error string.
pub fn check_agent_available(
    cfg: &Config,
    agent: AgentKind,
) -> std::result::Result<String, String> {
    if agent == AgentKind::Shell {
        return Err("shell panes don't use an agent CLI".to_string());
    }
    let (cmd, _) = cfg.command_for(agent);
    let label = agent.label();
    if cmd.contains('/') {
        if is_executable(Path::new(&cmd)) {
            Ok(cmd)
        } else {
            Err(format!(
                "`{cmd}` is missing or not executable — install {label} or fix agents.{label}.command in vag's config.toml"
            ))
        }
    } else {
        let on_path = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(&cmd)))
        });
        if on_path {
            Ok(cmd)
        } else {
            Err(format!(
                "`{cmd}` not found on PATH — install {label} (or set agents.{label}.command in vag's config.toml)"
            ))
        }
    }
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(md) if md.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                md.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// SSH remotes ("cloud vs local")
//
// A remote session is the same PTY architecture with `ssh -t` as the child:
// the remote agent's TUI streams through the tunnel into our emulator, and
// ssh's own prompts (passwords, host keys) render in the pane like any
// terminal. Identity model:
// - claude: ids are PRE-ASSIGNED (`--session-id` works over ssh), stored in
//   vag state with {remote, cwd} → fully resumable later.
// - codex: no pre-assignable id and no remote fs to poll → sessions get a
//   synthetic `remote-…` id and are attach-only (resume happens on the box).

/// Synthetic-id marker for remote codex sessions (not resumable from vag).
pub const REMOTE_SYNTHETIC_PREFIX: &str = "remote-";

pub fn is_synthetic_remote_id(id: &str) -> bool {
    id.starts_with(REMOTE_SYNTHETIC_PREFIX)
}

/// POSIX single-quote escaping: safe for any byte sequence except NUL.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Wrap a remote command line in `ssh -t <host> -- sh -lc '…'`.
/// TERM is forwarded by ssh from our env override; COLORTERM is exported in
/// the remote script (ssh does not forward it by default).
fn remote_spec(remote: &RemoteConfig, dir: &str, cmd: &str, args: &[String]) -> SpawnSpec {
    let mut line = format!(
        "{} && export COLORTERM=truecolor && exec {}",
        cd_clause(dir),
        shell_quote(cmd)
    );
    for a in args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    SpawnSpec {
        program: "ssh".into(),
        args: vec![
            "-t".into(),
            remote.host.clone(),
            "--".into(),
            "sh".into(),
            "-lc".into(),
            line,
        ],
        cwd: dirs::home_dir().unwrap_or_else(std::env::temp_dir),
        env: vec![
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ],
    }
}

/// New session on a remote. `dir` is a REMOTE path; `~`-prefixed paths are
/// rewritten to `$HOME`-relative in the generated script (see cd_clause).
pub fn remote_new_session_spec(
    remote: &RemoteConfig,
    agent: AgentKind,
    dir: &str,
    name: Option<&str>,
) -> Result<(SpawnSpec, PendingId)> {
    match agent {
        AgentKind::Claude => {
            let id = uuid::Uuid::new_v4().to_string();
            let mut args = vec!["--session-id".to_string(), id.clone()];
            if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
                args.push("-n".into());
                args.push(n.to_string());
            }
            Ok((
                remote_spec(remote, dir, &remote.command_for(agent), &args),
                PendingId::Known(id),
            ))
        }
        AgentKind::Codex => {
            let command = remote.command_for(agent);
            let mut args = Vec::new();
            if is_native_cli(&command, AgentKind::Codex) {
                add_codex_tui_notifications(&mut args);
            }
            let id = format!(
                "{}{}",
                REMOTE_SYNTHETIC_PREFIX,
                uuid::Uuid::new_v4().simple()
            );
            Ok((
                remote_spec(remote, dir, &command, &args),
                PendingId::Known(id),
            ))
        }
        // Shell panes aren't agent sessions; the UI spawns plain `ssh` for a
        // remote shell itself.
        AgentKind::Shell => {
            bail!("shell panes aren't agent sessions — open one from the dashboard")
        }
    }
}

/// Resume a session on its remote. Codex synthetic ids are attach-only.
pub fn remote_resume_spec(
    remote: &RemoteConfig,
    agent: AgentKind,
    id: &str,
    cwd: &str,
) -> Result<SpawnSpec> {
    if is_synthetic_remote_id(id) {
        bail!(
            "remote codex sessions can't be re-attached from vag — resume it on {} with `codex resume`",
            remote.name
        );
    }
    let command = remote.command_for(agent);
    let mut args = match agent {
        AgentKind::Claude => vec!["--resume".to_string(), id.to_string()],
        AgentKind::Codex => vec!["resume".to_string(), id.to_string()],
        // Shell panes are ephemeral even on remotes — nothing to resume.
        AgentKind::Shell => bail!("shell panes are ephemeral — nothing to resume"),
    };
    if agent == AgentKind::Codex && is_native_cli(&command, AgentKind::Codex) {
        add_codex_tui_notifications(&mut args);
    }
    Ok(remote_spec(remote, cwd, &command, &args))
}

/// `cd` clause with `~` handled: `$HOME` sits OUTSIDE the single quotes so
/// the remote shell expands it, while the rest of the path stays quoted.
fn cd_clause(dir: &str) -> String {
    let d = dir.trim();
    if d.is_empty() || d == "~" {
        return "cd \"$HOME\"".into();
    }
    if let Some(rest) = d.strip_prefix("~/") {
        return format!("cd \"$HOME\"/{}", shell_quote(rest));
    }
    format!("cd {}", shell_quote(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionKey;

    fn remote() -> RemoteConfig {
        RemoteConfig {
            name: "gpu".into(),
            host: "user@gpu.example".into(),
            default_dir: Some("~/work".into()),
            claude_command: String::new(),
            codex_command: "/opt/codex".into(),
        }
    }

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_new_claude_preassigns_id_over_ssh() {
        let (spec, pending) =
            remote_new_session_spec(&remote(), AgentKind::Claude, "~/work/proj", Some("gpu run"))
                .unwrap();
        assert_eq!(spec.program, "ssh");
        assert_eq!(&spec.args[..3], &["-t", "user@gpu.example", "--"]);
        assert_eq!(&spec.args[3..5], &["sh", "-lc"]);
        let script = &spec.args[5];
        assert!(
            script.starts_with("cd \"$HOME\"/'work/proj' && "),
            "{script}"
        );
        assert!(script.contains("exec 'claude' '--session-id'"), "{script}");
        assert!(script.contains("'-n' 'gpu run'"), "{script}");
        let PendingId::Known(id) = pending else {
            panic!()
        };
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert!(script.contains(&id));
    }

    #[test]
    fn remote_new_codex_gets_synthetic_id_and_command_override() {
        let (spec, pending) =
            remote_new_session_spec(&remote(), AgentKind::Codex, "/srv/x", None).unwrap();
        let script = &spec.args[5];
        assert!(script.starts_with("cd '/srv/x' && "), "{script}");
        assert!(script.contains("exec '/opt/codex'"), "{script}");
        assert!(
            script.contains("'tui.notification_method=\"osc9\"'"),
            "{script}"
        );
        assert!(
            script.contains("'tui.notification_condition=\"always\"'"),
            "{script}"
        );
        let PendingId::Known(id) = pending else {
            panic!()
        };
        assert!(is_synthetic_remote_id(&id));
    }

    #[test]
    fn remote_resume_claude_and_synthetic_refusal() {
        let spec = remote_resume_spec(&remote(), AgentKind::Claude, "abc-123", "~").unwrap();
        let script = &spec.args[5];
        assert!(script.starts_with("cd \"$HOME\" && "), "{script}");
        assert!(script.contains("'--resume' 'abc-123'"), "{script}");
        assert!(remote_resume_spec(&remote(), AgentKind::Codex, "remote-xyz", "~").is_err());
    }

    #[test]
    fn shell_kind_is_refused_by_every_builder() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let m = meta_for(AgentKind::Shell, "some-id", tmp.path());
        assert!(resume_spec(&cfg, &m).is_err());
        assert!(fork_spec(&cfg, &m).is_err());
        assert!(new_session_spec(&cfg, AgentKind::Shell, tmp.path(), None).is_err());
        assert!(remote_new_session_spec(&remote(), AgentKind::Shell, "~", None).is_err());
        assert!(remote_resume_spec(&remote(), AgentKind::Shell, "id", "~").is_err());
        let err = check_agent_available(&cfg, AgentKind::Shell).unwrap_err();
        assert!(err.contains("shell"), "{err}");
    }

    #[test]
    fn cd_clause_tilde_variants() {
        assert_eq!(cd_clause("~"), "cd \"$HOME\"");
        assert_eq!(cd_clause(""), "cd \"$HOME\"");
        assert_eq!(cd_clause("~/a b"), "cd \"$HOME\"/'a b'");
        assert_eq!(cd_clause("/x/y"), "cd '/x/y'");
    }

    fn test_cfg(claude_dir: &Path, codex_home: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.behavior.claude_config_dir = Some(claude_dir.to_path_buf());
        cfg.behavior.codex_home = Some(codex_home.to_path_buf());
        cfg
    }

    fn meta_for(agent: AgentKind, id: &str, cwd: &Path) -> SessionMeta {
        SessionMeta {
            key: SessionKey::new(agent, id),
            title: None,
            preview: None,
            cwd: cwd.to_path_buf(),
            created: None,
            last_user_activity: None,
            last_activity: None,
            archived: false,
            source_path: cwd.join("transcript.jsonl"),
            git_branch: None,
        }
    }

    fn strs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    // --- spec builders ------------------------------------------------------

    #[test]
    fn resume_claude_extras_before_resume_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.claude.extra_args = vec!["--verbose".into()];
        let m = meta_for(
            AgentKind::Claude,
            "11111111-2222-4333-8444-555555555555",
            tmp.path(),
        );

        let spec = resume_spec(&cfg, &m).unwrap();
        assert_eq!(spec.program, "claude");
        assert_eq!(
            strs(&spec.args),
            [
                "--verbose",
                "--resume",
                "11111111-2222-4333-8444-555555555555"
            ]
        );
        assert_eq!(spec.cwd, tmp.path());
        assert!(spec.env.contains(&("TERM".into(), "xterm-256color".into())));
        assert!(spec.env.contains(&("COLORTERM".into(), "truecolor".into())));
    }

    #[test]
    fn resume_codex_subcommand_then_id_then_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.extra_args = vec!["--profile".into(), "work".into()];
        let m = meta_for(
            AgentKind::Codex,
            "019f2a4c-0000-7000-8000-000000000000",
            tmp.path(),
        );

        let spec = resume_spec(&cfg, &m).unwrap();
        assert_eq!(spec.program, "codex");
        assert_eq!(
            strs(&spec.args),
            [
                "resume",
                "019f2a4c-0000-7000-8000-000000000000",
                "--profile",
                "work"
            ]
        );
        assert_eq!(spec.cwd, tmp.path());
    }

    #[test]
    fn builders_reject_missing_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let gone = tmp.path().join("does-not-exist");
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let m = meta_for(agent, "some-id", &gone);
            for err in [
                resume_spec(&cfg, &m).unwrap_err(),
                fork_spec(&cfg, &m).unwrap_err(),
                new_session_spec(&cfg, agent, &gone, None).unwrap_err(),
            ] {
                assert!(
                    err.to_string().contains("project directory missing"),
                    "unexpected error: {err}"
                );
            }
        }
    }

    #[test]
    fn missing_cwd_is_a_file_not_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let file = tmp.path().join("plain-file");
        std::fs::write(&file, "x").unwrap();
        let err = new_session_spec(&cfg, AgentKind::Claude, &file, None).unwrap_err();
        assert!(err.to_string().contains("project directory missing"));
    }

    #[test]
    fn new_claude_session_pregenerates_v4_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.claude.extra_args = vec!["--verbose".into()];

        let (spec, pending) =
            new_session_spec(&cfg, AgentKind::Claude, tmp.path(), Some("my session")).unwrap();
        let args = strs(&spec.args);
        let pos = args.iter().position(|a| *a == "--session-id").unwrap();
        let id = args[pos + 1];
        assert!(pos >= 1, "extras must come before --session-id");
        assert_eq!(args[0], "--verbose");

        let parsed = uuid::Uuid::parse_str(id).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(id, parsed.hyphenated().to_string(), "hyphenated lowercase");

        match pending {
            PendingId::Known(k) => assert_eq!(k, id),
            PendingId::Discover => panic!("claude new session must know its id"),
        }
        let npos = args.iter().position(|a| *a == "-n").unwrap();
        assert_eq!(args[npos + 1], "my session");
    }

    #[test]
    fn new_claude_session_omits_name_flag_when_absent_or_blank() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        for name in [None, Some(""), Some("   ")] {
            let (spec, _) = new_session_spec(&cfg, AgentKind::Claude, tmp.path(), name).unwrap();
            assert!(
                !spec.args.iter().any(|a| a == "-n"),
                "no -n for name {name:?}"
            );
        }
    }

    #[test]
    fn new_codex_session_has_no_id_or_name_args() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.extra_args = vec!["--model".into(), "o5".into()];

        let (spec, pending) =
            new_session_spec(&cfg, AgentKind::Codex, tmp.path(), Some("ignored name")).unwrap();
        assert_eq!(strs(&spec.args), ["--model", "o5"]);
        assert!(matches!(pending, PendingId::Discover));
        assert_eq!(spec.cwd, tmp.path());
    }

    #[test]
    fn fork_claude_appends_fork_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.claude.extra_args = vec!["--verbose".into()];
        let m = meta_for(AgentKind::Claude, "abc-id", tmp.path());

        let (spec, pending) = fork_spec(&cfg, &m).unwrap();
        assert_eq!(
            strs(&spec.args),
            ["--verbose", "--resume", "abc-id", "--fork-session"]
        );
        assert!(matches!(pending, PendingId::Discover));
    }

    #[test]
    fn fork_codex_subcommand_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.extra_args = vec!["--flag".into()];
        let m = meta_for(AgentKind::Codex, "019f-uuid", tmp.path());

        let (spec, pending) = fork_spec(&cfg, &m).unwrap();
        assert_eq!(strs(&spec.args), ["fork", "019f-uuid", "--flag"]);
        assert!(matches!(pending, PendingId::Discover));
    }

    // --- codex archive shell-out (fake scripts; never the real CLI) ----------

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn codex_set_archived_success_and_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = write_script(tmp.path(), "codex-ok", "#!/bin/sh\nexit 0\n");
        let fail = write_script(
            tmp.path(),
            "codex-fail",
            "#!/bin/sh\necho \"boom: $1 $2\" >&2\nexit 3\n",
        );

        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.command = ok.to_string_lossy().into_owned();
        codex_set_archived(&cfg, "some-id", true).unwrap();

        cfg.agents.codex.command = fail.to_string_lossy().into_owned();
        let err = codex_set_archived(&cfg, "some-id", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("boom: archive some-id"),
            "stderr surfaced: {err}"
        );
        let err = codex_set_archived(&cfg, "some-id", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("boom: unarchive some-id"),
            "verb switches: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_set_archived_caps_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let noisy = write_script(
            tmp.path(),
            "codex-noisy",
            "#!/bin/sh\nawk 'BEGIN{for(i=0;i<2000;i++)printf \"x\"}' >&2\nexit 1\n",
        );
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.command = noisy.to_string_lossy().into_owned();
        let err = codex_set_archived(&cfg, "id", true)
            .unwrap_err()
            .to_string();
        assert!(err.len() < 700, "stderr capped, got {} chars", err.len());
    }

    #[test]
    fn codex_set_archived_missing_binary_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.codex.command = tmp
            .path()
            .join("definitely-not-codex")
            .to_string_lossy()
            .into_owned();
        assert!(codex_set_archived(&cfg, "id", true).is_err());
    }

    // --- claude id discovery --------------------------------------------------

    #[test]
    fn discover_claude_polls_until_registry_file_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let sessions = tmp.path().join("sessions");
        let file = sessions.join("4242.json");

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(
                &file,
                r#"{"pid":4242,"sessionId":"aaaa1111-bbbb-4ccc-8ddd-eeee22223333","cwd":"/x","name":"t"}"#,
            )
            .unwrap();
        });

        let got = discover_claude_session_id(&cfg, 4242, SystemTime::now(), Duration::from_secs(3));
        handle.join().unwrap();
        assert_eq!(got.as_deref(), Some("aaaa1111-bbbb-4ccc-8ddd-eeee22223333"));
    }

    #[test]
    fn discover_claude_times_out_without_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let start = Instant::now();
        let got =
            discover_claude_session_id(&cfg, 999, SystemTime::now(), Duration::from_millis(50));
        assert!(got.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "respects tiny deadline"
        );
    }

    #[test]
    fn discover_claude_ignores_malformed_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("7.json"), "not json at all {{{").unwrap();
        std::fs::write(sessions.join("8.json"), r#"{"pid":8}"#).unwrap();
        let spawned = SystemTime::now();
        assert!(discover_claude_session_id(&cfg, 7, spawned, Duration::from_millis(30)).is_none());
        assert!(discover_claude_session_id(&cfg, 8, spawned, Duration::from_millis(30)).is_none());
    }

    #[test]
    fn discover_claude_ignores_stale_registry_from_recycled_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join("4242.json");

        // Crash-orphaned entry from a previous pid-4242 claude incarnation.
        std::fs::write(
            &file,
            r#"{"pid":4242,"sessionId":"deadbeef-dead-4dea-8dbe-000000000000","cwd":"/x"}"#,
        )
        .unwrap();
        std::fs::File::options()
            .append(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();

        let spawned_after = SystemTime::now();
        // Only the stale file exists → must not be trusted (times out).
        assert!(
            discover_claude_session_id(&cfg, 4242, spawned_after, Duration::from_millis(80))
                .is_none()
        );

        // The real child rewrites its entry (fresh mtime) → resolves.
        let rewrite = file.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::write(
                &rewrite,
                r#"{"pid":4242,"sessionId":"aaaa1111-bbbb-4ccc-8ddd-eeee22223333","cwd":"/x"}"#,
            )
            .unwrap();
        });
        let got = discover_claude_session_id(&cfg, 4242, spawned_after, Duration::from_secs(3));
        handle.join().unwrap();
        assert_eq!(got.as_deref(), Some("aaaa1111-bbbb-4ccc-8ddd-eeee22223333"));
    }

    // --- codex id discovery -----------------------------------------------------

    fn write_rollout(day_dir: &Path, file: &str, id: &str, cwd: &str, mtime: SystemTime) {
        std::fs::create_dir_all(day_dir).unwrap();
        let path = day_dir.join(file);
        let line1 = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "session_id": id, "cwd": cwd, "cli_version": "0.142.5"}
        });
        std::fs::write(&path, format!("{line1}\n{{\"type\":\"other\"}}\n")).unwrap();
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn today_dir(codex_home: &Path) -> PathBuf {
        codex_home.join("sessions").join(
            chrono::Local::now()
                .date_naive()
                .format("%Y/%m/%d")
                .to_string(),
        )
    }

    #[test]
    fn discover_codex_matches_cwd_and_picks_first_after_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let target_str = target.to_string_lossy().into_owned();

        let now = SystemTime::now();
        let spawned_after = now - Duration::from_secs(60);
        let day = today_dir(tmp.path());
        // Older match, newer match, cwd mismatch, malformed line 1.
        write_rollout(
            &day,
            "rollout-a.jsonl",
            "id-older",
            &target_str,
            now - Duration::from_secs(30),
        );
        write_rollout(&day, "rollout-b.jsonl", "id-newest", &target_str, now);
        write_rollout(
            &day,
            "rollout-c.jsonl",
            "id-elsewhere",
            "/somewhere/else",
            now,
        );
        std::fs::write(day.join("rollout-junk.jsonl"), "not json {{{\n").unwrap();
        // Matching but in a stale date dir (mtime is fresh — dir must be pruned).
        write_rollout(
            &tmp.path().join("sessions/2020/01/01"),
            "rollout-ancient.jsonl",
            "id-ancient",
            &target_str,
            now + Duration::from_secs(5),
        );
        // Matching but stamped long before spawn (beyond the 2s slack):
        // backdating mtime below the birth time also lowers the birth time
        // on APFS/HFS+, so the created()-gate rejects it; elsewhere it still
        // loses the newest-match rule to rollout-b.
        write_rollout(
            &day,
            "rollout-d.jsonl",
            "id-stale",
            &target_str,
            now - Duration::from_secs(200),
        );

        let got =
            discover_codex_session_id(&cfg, &target, spawned_after, Duration::from_millis(100));
        assert_eq!(got.as_deref(), Some("id-older"));
        let got = discover_codex_session_id_excluding(
            &cfg,
            &target,
            spawned_after,
            Duration::from_millis(100),
            &HashSet::from(["id-older".to_string()]),
        );
        assert_eq!(got.as_deref(), Some("id-newest"));
    }

    #[test]
    fn discover_codex_matches_trailing_slash_and_session_id_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let day = today_dir(tmp.path());
        std::fs::create_dir_all(&day).unwrap();

        // cwd stored with a trailing slash, id only under legacy "session_id".
        let line1 = serde_json::json!({
            "type": "session_meta",
            "payload": {"session_id": "id-legacy", "cwd": format!("{}/", target.to_string_lossy())}
        });
        std::fs::write(day.join("rollout-e.jsonl"), format!("{line1}\n")).unwrap();

        let spawned_after = SystemTime::now() - Duration::from_secs(60);
        let got =
            discover_codex_session_id(&cfg, &target, spawned_after, Duration::from_millis(100));
        assert_eq!(got.as_deref(), Some("id-legacy"));
    }

    #[test]
    fn discover_codex_rollout_fallback_rejects_explicit_non_cli_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let day = today_dir(tmp.path());
        std::fs::create_dir_all(&day).unwrap();
        let launched = SystemTime::now() - Duration::from_secs(1);
        let ts = |offset_ms| {
            chrono::DateTime::<chrono::Utc>::from(launched + Duration::from_millis(offset_ms))
                .to_rfc3339()
        };
        let write = |name: &str, payload: serde_json::Value| {
            let line = serde_json::json!({
                "type": "session_meta",
                "payload": payload,
            });
            std::fs::write(day.join(name), format!("{line}\n")).unwrap();
        };
        write(
            "rollout-subagent.jsonl",
            serde_json::json!({
                "id": "id-subagent",
                "cwd": target.to_string_lossy(),
                "timestamp": ts(10),
                "source": {"subagent": {"thread_spawn": {"depth": 1}}},
                "thread_source": "subagent",
            }),
        );
        write(
            "rollout-automation.jsonl",
            serde_json::json!({
                "id": "id-automation",
                "cwd": target.to_string_lossy(),
                "timestamp": ts(20),
                "source": "cli",
                "thread_source": "automation",
            }),
        );
        write(
            "rollout-app.jsonl",
            serde_json::json!({
                "id": "id-app",
                "cwd": target.to_string_lossy(),
                "timestamp": ts(30),
                "source": "vscode",
                "thread_source": "user",
            }),
        );
        write(
            "rollout-cli.jsonl",
            serde_json::json!({
                "id": "id-cli",
                "cwd": target.to_string_lossy(),
                "timestamp": ts(40),
                "source": "unknown",
                "thread_source": "user",
            }),
        );

        let got = discover_codex_session_id_excluding(
            &cfg,
            &target,
            launched,
            Duration::from_millis(100),
            &HashSet::new(),
        );
        assert_eq!(got.as_deref(), Some("id-cli"));
        let got = discover_codex_session_id_excluding(
            &cfg,
            &target,
            launched,
            Duration::from_millis(50),
            &HashSet::from(["id-cli".to_string()]),
        );
        assert!(got.is_none(), "no non-CLI rollout may be claimed");
    }

    #[test]
    fn discover_codex_successful_index_query_suppresses_rollout_race() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, cwd TEXT, created_at_ms INTEGER, source TEXT
            )",
        )
        .unwrap();
        drop(conn);

        // This legacy-looking rollout would be accepted when no usable
        // index exists. A successful empty index query must make us wait for
        // its authoritative CLI row instead of claiming the temporal match.
        let day = today_dir(tmp.path());
        write_rollout(
            &day,
            "rollout-racing.jsonl",
            "id-racing",
            &target.to_string_lossy(),
            SystemTime::now(),
        );
        let got = discover_codex_session_id(
            &cfg,
            &target,
            SystemTime::now() - Duration::from_secs(1),
            Duration::from_millis(50),
        );
        assert!(got.is_none());
    }

    #[test]
    fn discover_codex_legacy_index_keeps_rollout_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, cwd TEXT, created_at_ms INTEGER
            )",
        )
        .unwrap();
        drop(conn);

        let day = today_dir(tmp.path());
        write_rollout(
            &day,
            "rollout-legacy.jsonl",
            "id-from-rollout",
            &target.to_string_lossy(),
            SystemTime::now(),
        );
        let got = discover_codex_session_id(
            &cfg,
            &target,
            SystemTime::now() - Duration::from_secs(1),
            Duration::from_millis(100),
        );
        assert_eq!(got.as_deref(), Some("id-from-rollout"));
    }

    #[test]
    fn discover_codex_rejects_session_started_just_before_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let day = today_dir(tmp.path());
        std::fs::create_dir_all(&day).unwrap();
        let launched = SystemTime::now();
        let before = chrono::DateTime::<chrono::Utc>::from(launched - Duration::from_millis(100));
        let line = serde_json::json!({
            "timestamp": before.to_rfc3339(),
            "type": "session_meta",
            "payload": {
                "id": "id-prelaunch",
                "cwd": target.to_string_lossy(),
            }
        });
        // The file itself is new enough to pass the cheap birth-time gate;
        // its session timestamp proves that it belongs to an earlier launch.
        std::fs::write(day.join("rollout-prelaunch.jsonl"), format!("{line}\n")).unwrap();

        assert!(
            discover_codex_session_id(&cfg, &target, launched, Duration::from_millis(50)).is_none()
        );
    }

    #[test]
    fn discover_codex_ignores_preexisting_rollout_with_fresh_mtime() {
        // Mis-attribution scenario: session A's rollout was CREATED long
        // before our spawn but is append-in-place, so its mtime is the
        // newest in the dir; the genuinely new rollout must win anyway.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let target_str = target.to_string_lossy().into_owned();
        let day = today_dir(tmp.path());
        let now = SystemTime::now();

        // Backdating mtime below the birth time lowers the birth time with
        // it (APFS/HFS+); the append then bumps mtime while created() keeps
        // reporting the old time — a faithful "running session" fixture.
        let old_path = day.join("rollout-old.jsonl");
        write_rollout(
            &day,
            "rollout-old.jsonl",
            "id-running",
            &target_str,
            now - Duration::from_secs(300),
        );
        {
            use std::io::Write;
            let mut f = std::fs::File::options()
                .append(true)
                .open(&old_path)
                .unwrap();
            writeln!(f, "{{\"type\":\"other\"}}").unwrap();
        }
        let old_md = std::fs::metadata(&old_path).unwrap();
        if !old_md
            .created()
            .is_ok_and(|c| c < now - Duration::from_secs(100))
        {
            eprintln!("skipping: fs does not lower birth time with a backdated mtime");
            return;
        }

        // The new child's own rollout is created after the launch boundary.
        // The running rollout still has the tempting fresh append mtime, but
        // its old birth/session time must keep it ineligible.
        write_rollout(
            &day,
            "rollout-new.jsonl",
            "id-new",
            &target_str,
            SystemTime::now(),
        );

        let got = discover_codex_session_id(&cfg, &target, now, Duration::from_millis(200));
        assert_eq!(got.as_deref(), Some("id-new"));
    }

    #[test]
    fn discover_codex_parses_oversized_session_meta_line() {
        // Line 1 > 64KB (the old cap) but under ROLLOUT_LINE1_MAX — real
        // session_meta lines grow with base_instructions / MCP tool schemas.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let day = today_dir(tmp.path());
        std::fs::create_dir_all(&day).unwrap();

        let line1 = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": "id-big",
                "cwd": target.to_string_lossy(),
                "base_instructions": "i".repeat(100 * 1024),
            }
        });
        std::fs::write(day.join("rollout-big.jsonl"), format!("{line1}\n")).unwrap();

        let spawned_after = SystemTime::now() - Duration::from_secs(60);
        let got =
            discover_codex_session_id(&cfg, &target, spawned_after, Duration::from_millis(100));
        assert_eq!(got.as_deref(), Some("id-big"));
    }

    #[test]
    fn discover_codex_times_out_when_nothing_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let start = Instant::now();
        let got = discover_codex_session_id(
            &cfg,
            Path::new("/nope"),
            SystemTime::now(),
            Duration::from_millis(50),
        );
        assert!(got.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn discover_codex_polls_for_late_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path(), tmp.path());
        let target = tmp.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let target_str = target.to_string_lossy().into_owned();
        let day = today_dir(tmp.path());
        let spawned_after = SystemTime::now();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            write_rollout(
                &day,
                "rollout-late.jsonl",
                "id-late",
                &target_str,
                SystemTime::now(),
            );
        });
        let got = discover_codex_session_id(&cfg, &target, spawned_after, Duration::from_secs(3));
        handle.join().unwrap();
        assert_eq!(got.as_deref(), Some("id-late"));
    }

    // --- availability check -----------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn check_agent_available_with_explicit_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = write_script(tmp.path(), "fake-claude", "#!/bin/sh\nexit 0\n");
        let mut cfg = test_cfg(tmp.path(), tmp.path());
        cfg.agents.claude.command = fake.to_string_lossy().into_owned();

        let resolved = check_agent_available(&cfg, AgentKind::Claude).unwrap();
        assert_eq!(resolved, fake.to_string_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn check_agent_available_rejects_missing_and_non_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(tmp.path(), tmp.path());

        cfg.agents.claude.command = tmp
            .path()
            .join("no-such-binary")
            .to_string_lossy()
            .into_owned();
        let err = check_agent_available(&cfg, AgentKind::Claude).unwrap_err();
        assert!(err.contains("claude"), "mentions the agent: {err}");

        let plain = tmp.path().join("not-exec");
        std::fs::write(&plain, "data").unwrap();
        cfg.agents.codex.command = plain.to_string_lossy().into_owned();
        assert!(check_agent_available(&cfg, AgentKind::Codex).is_err());
    }
}
