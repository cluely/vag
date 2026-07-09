//! Core shared types. These are the stable contracts between discovery,
//! state, actions, runtime and UI — do not change shapes without updating
//! all consumers.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    /// Ephemeral terminal pane (local `$SHELL` or `ssh <host>`): no agent
    /// CLI, never discovered by scans, never persisted, never resumable.
    Shell,
}

impl AgentKind {
    pub fn label(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Shell => "shell",
        }
    }

    /// Short glyph shown in lists.
    pub fn icon(&self) -> &'static str {
        match self {
            AgentKind::Claude => "✳",
            AgentKind::Codex => "◆",
            AgentKind::Shell => "$",
        }
    }
}

impl std::str::FromStr for AgentKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            "shell" => Ok(AgentKind::Shell),
            _ => Err(()),
        }
    }
}

/// Stable identity of a session across scans. Serialized as `"<agent>:<id>"`
/// when used as a map key in vag's own state file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub agent: AgentKind,
    pub id: String,
}

impl SessionKey {
    pub fn new(agent: AgentKind, id: impl Into<String>) -> Self {
        SessionKey {
            agent,
            id: id.into(),
        }
    }

    pub fn to_key_string(&self) -> String {
        format!("{}:{}", self.agent.label(), self.id)
    }

    #[allow(dead_code)] // inverse of to_key_string; used by tests today
    pub fn parse(s: &str) -> Option<Self> {
        let (agent, id) = s.split_once(':')?;
        let agent: AgentKind = agent.parse().ok()?;
        if id.is_empty() {
            return None;
        }
        Some(SessionKey {
            agent,
            id: id.to_string(),
        })
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_key_string())
    }
}

/// Unified session metadata produced by the discovery backends.
///
/// Invariants the backends must uphold:
/// - `cwd` is the session's original working root, recovered from store
///   *content* (never from encoded directory names). Required for resume.
/// - `source_path` is the transcript/rollout file; its existence is the
///   definition of "the session exists".
/// - Parsing is defensive: both storage formats are officially internal and
///   drift between CLI releases. Unknown record types are ignored, all fields
///   treated as optional.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub key: SessionKey,
    /// Best-effort human title from the agent's own store (claude:
    /// custom-title > ai-title; codex: sqlite title / thread_name).
    pub title: Option<String>,
    /// Snippet of the first user prompt (fallback display + search text).
    pub preview: Option<String>,
    pub cwd: PathBuf,
    /// Shown in the pane titlebar ("created 3h ago").
    pub created: Option<DateTime<Utc>>,
    /// When the USER last sent a message to this session (claude: last
    /// genuine user record in the transcript tail; codex: history.jsonl).
    /// Rows sort by this — unlike `last_activity` it doesn't advance while
    /// an agent streams output, so running sessions don't leapfrog.
    pub last_user_activity: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    /// Codex-native archived flag (claude sessions: always false).
    pub archived: bool,
    #[allow(dead_code)] // data model; not yet surfaced in the UI
    pub source_path: PathBuf,
    /// Shown in the pane titlebar ("⎇ main").
    pub git_branch: Option<String>,
}

impl SessionMeta {
    /// Display title with fallbacks; never empty.
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_deref() {
            let t = t.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
        if let Some(p) = self.preview.as_deref() {
            let p = p.trim().replace(['\n', '\r'], " ");
            if !p.is_empty() {
                let mut s: String = p.chars().take(60).collect();
                if p.chars().count() > 60 {
                    s.push('…');
                }
                return s;
            }
        }
        format!("({})", &self.key.id[..self.key.id.len().min(8)])
    }

    /// Short project label: last path component of cwd.
    pub fn project_label(&self) -> String {
        self.cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_roundtrip() {
        let k = SessionKey::new(AgentKind::Claude, "39212683-afb1");
        assert_eq!(k.to_key_string(), "claude:39212683-afb1");
        assert_eq!(SessionKey::parse("claude:39212683-afb1"), Some(k));
        // codex uuids contain no ':' but ids with ':' still parse (split_once)
        assert_eq!(SessionKey::parse("codex:"), None);
        assert_eq!(SessionKey::parse("gemini:x"), None);
        assert_eq!(SessionKey::parse("claude"), None);
    }

    #[test]
    fn shell_kind_parse_label_roundtrip() {
        assert_eq!(AgentKind::Shell.label(), "shell");
        assert_eq!(AgentKind::Shell.icon(), "$");
        assert_eq!("shell".parse(), Ok(AgentKind::Shell));
        assert_eq!(
            AgentKind::Shell.label().parse::<AgentKind>(),
            Ok(AgentKind::Shell)
        );
        // Shell panes aren't persisted, but state keys must still parse.
        let k = SessionKey::new(AgentKind::Shell, "39212683-afb1");
        assert_eq!(k.to_key_string(), "shell:39212683-afb1");
        assert_eq!(SessionKey::parse("shell:39212683-afb1"), Some(k));
    }
}
