//! Session discovery across agent backends. Each backend scans its agent's
//! own storage defensively (formats are officially internal) and produces
//! unified [`crate::types::SessionMeta`].
//!
//! Backends must never write to the agents' data directories.

pub mod claude;
pub mod codex;

use crate::config::Config;
use crate::types::{AgentKind, SessionMeta};

#[derive(Debug, Default)]
pub struct ScanResult {
    pub sessions: Vec<SessionMeta>,
    /// Human-readable, non-fatal problems (shown in a status line, not fatal:
    /// one backend failing must not hide the other's sessions).
    pub warnings: Vec<String>,
    /// Backends that contributed nothing because they errored. Consumers must
    /// not treat their sessions as gone (e.g. gc grace stamping).
    pub failed_agents: Vec<AgentKind>,
}

impl ScanResult {
    /// A scan where every backend failed (or the scan thread itself died) —
    /// the result carries no information about which sessions still exist.
    pub fn total_failure(&self) -> bool {
        self.failed_agents.contains(&AgentKind::Claude)
            && self.failed_agents.contains(&AgentKind::Codex)
    }
}

/// Scan both backends. Never fails outright; per-backend errors become
/// warnings and that backend contributes zero sessions.
pub fn scan_all(cfg: &Config) -> ScanResult {
    let mut out = ScanResult::default();
    match claude::scan(cfg) {
        Ok(mut s) => out.sessions.append(&mut s),
        Err(e) => {
            out.warnings.push(format!("claude scan: {e:#}"));
            out.failed_agents.push(AgentKind::Claude);
        }
    }
    match codex::scan(cfg) {
        Ok(mut s) => out.sessions.append(&mut s),
        Err(e) => {
            out.warnings.push(format!("codex scan: {e:#}"));
            out.failed_agents.push(AgentKind::Codex);
        }
    }
    // Order by when the USER last sent a message (stable while agents
    // stream — running sessions must not leapfrog on every scan), falling
    // back to transcript activity for never-prompted sessions.
    out.sessions
        .sort_by(|a, b| sort_ts(b).cmp(&sort_ts(a)).then_with(|| a.key.cmp(&b.key)));
    out
}

/// The row-ordering timestamp: last message sent, else last activity.
pub fn sort_ts(m: &SessionMeta) -> Option<chrono::DateTime<chrono::Utc>> {
    m.last_user_activity.or(m.last_activity)
}
