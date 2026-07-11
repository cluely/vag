//! Provider-native lifecycle events for agent sessions hosted by vag.
//!
//! Claude Code exposes lifecycle hooks. For local Claude children we add a
//! session-only `--settings` layer whose command hooks invoke vag's hidden
//! `_agent-event` subcommand. The helper forwards a compact, allowlisted
//! event over a private Unix datagram socket owned by the parent TUI.
//!
//! Codex's interactive TUI exposes the attention states we need through its
//! native terminal notifications. [`add_codex_tui_notifications`] forces
//! those notifications to OSC 9 for the child process; the runtime recognizes
//! that dedicated sequence without confusing ordinary terminal bells.
//!
//! Neither path scrapes rendered terminal text. If hooks/notifications are
//! unavailable, the existing PTY-activity heuristic remains the fallback.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::types::{AgentKind, SessionKey};

// ExitPlanMode includes the complete plan in `tool_input`. Keep the helper
// bounded without dropping realistic long plans before we select the tiny
// allowlisted subset that crosses the socket.
const EVENT_STDIN_MAX: u64 = 4 * 1024 * 1024;
const EVENT_PACKET_MAX: usize = 16 * 1024;

const EVENT_SUBCOMMAND: &str = "_agent-event";
const CODEX_NOTIFICATIONS: &str =
    "tui.notifications=[\"agent-turn-complete\",\"approval-requested\",\"plan-mode-prompt\"]";
const CODEX_NOTIFICATION_METHOD: &str = "tui.notification_method=\"osc9\"";
const CODEX_NOTIFICATION_CONDITION: &str = "tui.notification_condition=\"always\"";

/// Why an agent is currently blocked on a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedsInputKind {
    /// Provider proved a wait but did not expose a more specific reason.
    Input,
    Question,
    Approval,
    PlanApproval,
    Elicitation,
    /// The turn ended normally and the composer is ready for another prompt.
    NextPrompt,
}

impl NeedsInputKind {
    pub fn label(self) -> &'static str {
        match self {
            NeedsInputKind::Input => "input needed",
            NeedsInputKind::Question => "answer needed",
            NeedsInputKind::Approval => "approval needed",
            NeedsInputKind::PlanApproval => "plan approval",
            NeedsInputKind::Elicitation => "input requested",
            NeedsInputKind::NextPrompt => "ready",
        }
    }

    /// Compact but explicit sidebar wording. A glyph alone is too ambiguous
    /// in the narrow tree, while the full labels unnecessarily crowd titles.
    pub fn short_label(self) -> &'static str {
        match self {
            NeedsInputKind::Input | NeedsInputKind::Elicitation => "input",
            NeedsInputKind::Question => "answer",
            NeedsInputKind::Approval => "approval",
            NeedsInputKind::PlanApproval => "plan",
            NeedsInputKind::NextPrompt => "ready",
        }
    }
}

/// Semantic lifecycle transition consumed by the per-runtime activity state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventKind {
    NeedsInput {
        kind: NeedsInputKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    InputResolved {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    TurnStarted,
    /// The current agent turn reached the normal composer. Unlike
    /// `NeedsInput`, this restores the historical Done/Unread behavior and
    /// does not claim that a question or approval is pending.
    TurnCompleted,
    /// The provider session itself is ending. This is deliberately distinct
    /// from `InputResolved`: an elicitation result resumes agent work, while
    /// SessionEnd must leave the runtime idle until its process exits.
    SessionEnded,
}

/// Normalize the bounded OSC 9 message emitted by Codex's native TUI
/// notification backend. Approval/plan messages have provider-owned fixed
/// prefixes; every other non-empty message is the agent-turn preview (or the
/// literal "Agent turn complete" fallback) because vag explicitly enables
/// only these three notification types for the child.
pub fn normalize_codex_notification(message: &str) -> Option<AgentEventKind> {
    if message.is_empty() {
        return None;
    }
    if message.starts_with("Plan mode prompt:") {
        return Some(AgentEventKind::NeedsInput {
            kind: NeedsInputKind::PlanApproval,
            request_id: None,
        });
    }
    if message.starts_with("Approval requested:")
        || message.starts_with("Approval requested by ")
        || message.starts_with("Codex wants to edit ")
    {
        return Some(AgentEventKind::NeedsInput {
            kind: NeedsInputKind::Approval,
            request_id: None,
        });
    }
    Some(AgentEventKind::TurnCompleted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    /// Wall-clock order marker generated in the hook process. This lets the
    /// UI discard a needs-input packet that was queued before a newer submit
    /// or cancel boundary but drained afterwards.
    pub observed_at_unix_nanos: u64,
    pub kind: AgentEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireEvent {
    secret: String,
    runtime: String,
    observed_at_unix_nanos: u64,
    #[serde(flatten)]
    kind: AgentEventKind,
}

/// One routed native event drained by the TUI on its existing 100ms tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedAgentEvent {
    pub key: SessionKey,
    pub event: AgentEvent,
}

/// Private, nonblocking Unix-datagram listener for one vag process.
pub struct AgentEventListener {
    socket: UnixDatagram,
    endpoint: PathBuf,
    secret: String,
    // Owns (and removes) the private 0700 directory + socket path.
    _dir: tempfile::TempDir,
}

impl AgentEventListener {
    pub fn bind() -> io::Result<Self> {
        // `/tmp` keeps the AF_UNIX path below macOS's short sun_path limit;
        // tempfile makes the containing directory private and collision-free.
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = tempfile::Builder::new()
            .prefix("vag-events-")
            .tempdir_in(&base)?;
        let endpoint = dir.path().join("events.sock");
        let socket = UnixDatagram::bind(&endpoint)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            endpoint,
            secret: uuid::Uuid::new_v4().simple().to_string(),
            _dir: dir,
        })
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Drain all complete packets currently queued. Malformed, unauthenticated
    /// and unknown-runtime packets are ignored: native events only improve UI
    /// state and must never be able to break the main loop.
    pub fn drain(&self) -> Vec<RoutedAgentEvent> {
        let mut out = Vec::new();
        let mut buf = vec![0_u8; EVENT_PACKET_MAX];
        loop {
            match self.socket.recv(&mut buf) {
                Ok(n) => {
                    let Ok(wire) = serde_json::from_slice::<WireEvent>(&buf[..n]) else {
                        continue;
                    };
                    if wire.secret != self.secret {
                        continue;
                    }
                    let Some(key) = SessionKey::parse(&wire.runtime) else {
                        continue;
                    };
                    out.push(RoutedAgentEvent {
                        key,
                        event: AgentEvent {
                            observed_at_unix_nanos: wire.observed_at_unix_nanos,
                            kind: wire.kind,
                        },
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        out
    }
}

/// Hidden hook client. It intentionally never returns an error: a stale
/// socket, malformed hook payload, or already-exited parent must not affect
/// Claude's turn. It also forwards no prompt, cwd, transcript, or tool input.
pub fn emit_from_hook(args: &[String]) {
    let [endpoint, secret, runtime] = args else {
        return;
    };
    if SessionKey::parse(runtime).is_none() {
        return;
    }

    let mut bytes = Vec::new();
    let mut stdin = io::stdin().take(EVENT_STDIN_MAX + 1);
    if stdin.read_to_end(&mut bytes).is_err() || bytes.len() as u64 > EVENT_STDIN_MAX {
        return;
    }
    let Ok(input) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let Some(kind) = normalize_claude_hook(&input) else {
        return;
    };
    let wire = WireEvent {
        secret: secret.clone(),
        runtime: runtime.clone(),
        observed_at_unix_nanos: now_unix_nanos(),
        kind,
    };
    let Ok(packet) = serde_json::to_vec(&wire) else {
        return;
    };
    if packet.len() > EVENT_PACKET_MAX {
        return;
    }
    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    let _ = socket.send_to(&packet, endpoint);
}

pub fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

/// Only add provider flags to the real CLI (or a path named like it). A
/// custom wrapper with a different basename retains the heuristic fallback
/// rather than receiving flags it may not understand.
pub fn is_native_cli(program: &str, agent: AgentKind) -> bool {
    let Some(name) = Path::new(program).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == agent.label() || name == format!("{}.exe", agent.label())
}

/// Configure a Codex TUI child to emit native attention events as OSC 9.
/// Appending makes vag's per-child override win over standing user args while
/// leaving the user's persisted config untouched.
pub fn add_codex_tui_notifications(args: &mut Vec<String>) {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let options = &args[..at];
    if options.iter().any(|a| a == CODEX_NOTIFICATIONS)
        && options.iter().any(|a| a == CODEX_NOTIFICATION_METHOD)
        && options.iter().any(|a| a == CODEX_NOTIFICATION_CONDITION)
    {
        return;
    }
    let injected = [
        "-c",
        CODEX_NOTIFICATIONS,
        "-c",
        CODEX_NOTIFICATION_METHOD,
        "-c",
        CODEX_NOTIFICATION_CONDITION,
    ]
    .map(str::to_string);
    // A literal `--` ends Codex option parsing. Keep the per-child settings
    // in the option prefix so they cannot become prompt text.
    args.splice(at..at, injected);
}

/// Add Claude lifecycle hooks as a temporary CLI settings layer. Existing
/// `--settings` values are parsed and merged into one layer so an explicit
/// per-run user setting is never silently replaced.
pub fn instrument_claude(
    args: &mut Vec<String>,
    cwd: &Path,
    helper: &Path,
    endpoint: &Path,
    secret: &str,
    runtime: &SessionKey,
) -> Result<PathBuf> {
    let command = helper
        .to_str()
        .ok_or_else(|| anyhow!("vag executable path is not valid UTF-8"))?;
    let handler = json!({
        "type": "command",
        "command": command,
        "args": [
            EVENT_SUBCOMMAND,
            endpoint.to_string_lossy(),
            secret,
            runtime.to_key_string(),
        ],
        "timeout": 5,
    });
    let group = |matcher: Option<&str>| {
        let mut value = Map::new();
        if let Some(matcher) = matcher {
            value.insert("matcher".into(), Value::String(matcher.into()));
        }
        value.insert("hooks".into(), Value::Array(vec![handler.clone()]));
        Value::Object(value)
    };
    // Deliberately exclude PreToolUse, PermissionRequest, Elicitation, and
    // Stop: all can be blocked/answered by another parallel hook. Their
    // corresponding Notification events fire only when UI attention is
    // actually required, avoiding a semantic latch on a prompt never shown.
    let settings = json!({
        "hooks": {
            "UserPromptSubmit": [group(None)],
            "Notification": [group(Some(
                "permission_prompt|idle_prompt|elicitation_dialog|elicitation_complete|elicitation_response"
            ))],
            "ElicitationResult": [group(None)],
            "StopFailure": [group(None)],
            "SessionEnd": [group(None)],
        }
    });
    let (instrumented, settings_path) = merge_claude_settings(args, cwd, settings, endpoint)?;
    *args = instrumented;
    Ok(settings_path)
}

fn merge_claude_settings(
    args: &[String],
    cwd: &Path,
    injected: Value,
    endpoint: &Path,
) -> Result<(Vec<String>, PathBuf)> {
    let mut retained = Vec::with_capacity(args.len() + 2);
    let mut merged = Value::Object(Map::new());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            retained.extend_from_slice(&args[i..]);
            break;
        }
        if args[i] == "--settings" {
            let Some(value) = args.get(i + 1) else {
                bail!("existing --settings flag has no value");
            };
            let parsed = read_settings_value(value, cwd)
                .with_context(|| format!("reading existing Claude --settings value `{value}`"))?;
            merge_json(&mut merged, parsed);
            i += 2;
            continue;
        }
        if let Some(value) = args[i].strip_prefix("--settings=") {
            let parsed = read_settings_value(value, cwd)
                .with_context(|| format!("reading existing Claude --settings value `{value}`"))?;
            merge_json(&mut merged, parsed);
            i += 1;
            continue;
        }
        retained.push(args[i].clone());
        i += 1;
    }
    merge_json(&mut merged, injected);
    let encoded = serde_json::to_string(&merged).context("serializing Claude hook settings")?;
    let event_dir = endpoint
        .parent()
        .ok_or_else(|| anyhow!("native-event socket has no parent directory"))?;
    let settings_path = event_dir.join(format!(
        "claude-settings-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    let mut settings_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&settings_path)
        .with_context(|| {
            format!(
                "creating private Claude settings file {}",
                settings_path.display()
            )
        })?;
    settings_file
        .write_all(encoded.as_bytes())
        .context("writing private Claude hook settings")?;
    let settings_arg = settings_path
        .to_str()
        .ok_or_else(|| anyhow!("Claude settings path is not valid UTF-8"))?
        .to_string();
    // Keep the global option before a literal `--` separator if one exists.
    let at = retained
        .iter()
        .position(|a| a == "--")
        .unwrap_or(retained.len());
    retained.splice(at..at, ["--settings".into(), settings_arg]);
    Ok((retained, settings_path))
}

fn read_settings_value(raw: &str, cwd: &Path) -> Result<Value> {
    let trimmed = raw.trim();
    let value: Value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed).context("parsing inline JSON")?
    } else {
        let path = expand_tilde(Path::new(trimmed));
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading settings file {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing settings file {}", path.display()))?
    };
    if !value.is_object() {
        bail!("Claude settings must be a JSON object");
    }
    Ok(value)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Claude settings merge semantics: objects deep-merge and array sources
/// concatenate/deduplicate; higher-precedence scalar values replace lower.
fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(overlay)) => {
            for value in overlay {
                if !base.contains(&value) {
                    base.push(value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn normalize_claude_hook(input: &Value) -> Option<AgentEventKind> {
    let event = input.get("hook_event_name")?.as_str()?;
    let request_id = input
        .get("tool_use_id")
        .or_else(|| input.get("elicitation_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    match event {
        // SessionStart also fires for resume/clear/compact. None of those
        // proves that a user turn is in flight, so only a submitted prompt
        // establishes the native working phase.
        "SessionStart" => None,
        "UserPromptSubmit" => Some(AgentEventKind::TurnStarted),
        "ElicitationResult" => Some(AgentEventKind::InputResolved { request_id }),
        "Notification" => match input.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") => Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::Approval,
                request_id,
            }),
            // Claude documents idle_prompt as the completed turn waiting at
            // the normal composer, not an active question.
            Some("idle_prompt") => Some(AgentEventKind::TurnCompleted),
            Some("elicitation_dialog") => Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::Elicitation,
                request_id,
            }),
            Some("elicitation_complete" | "elicitation_response") => {
                Some(AgentEventKind::InputResolved { request_id })
            }
            _ => None,
        },
        // Stop hooks are blockable. Another hook may make Claude continue,
        // so only the post-fact idle notification is authoritative.
        "Stop" => None,
        // StopFailure fires instead of Stop after an API/auth/rate-limit
        // error; the composer is ready even though the turn failed.
        "StopFailure" => Some(AgentEventKind::NeedsInput {
            kind: NeedsInputKind::NextPrompt,
            request_id: None,
        }),
        "SessionEnd" => Some(AgentEventKind::SessionEnded),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str) -> Value {
        json!({"hook_event_name": event})
    }

    #[test]
    fn claude_hook_mapping_is_narrow_and_reason_aware() {
        let mut v = hook("PreToolUse");
        v["tool_name"] = json!("AskUserQuestion");
        assert_eq!(normalize_claude_hook(&v), None);

        let mut v = hook("PermissionRequest");
        v["tool_name"] = json!("Bash");
        assert_eq!(normalize_claude_hook(&v), None);

        let mut v = hook("Notification");
        v["notification_type"] = json!("permission_prompt");
        assert!(matches!(
            normalize_claude_hook(&v),
            Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::Approval,
                ..
            })
        ));

        v["notification_type"] = json!("idle_prompt");
        assert_eq!(
            normalize_claude_hook(&v),
            Some(AgentEventKind::TurnCompleted)
        );
        v["notification_type"] = json!("elicitation_dialog");
        assert!(matches!(
            normalize_claude_hook(&v),
            Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::Elicitation,
                ..
            })
        ));
        v["notification_type"] = json!("auth_success");
        assert_eq!(normalize_claude_hook(&v), None);

        assert_eq!(
            normalize_claude_hook(&hook("UserPromptSubmit")),
            Some(AgentEventKind::TurnStarted)
        );
        assert_eq!(normalize_claude_hook(&hook("SessionStart")), None);
        assert_eq!(
            normalize_claude_hook(&hook("SessionEnd")),
            Some(AgentEventKind::SessionEnded)
        );
        assert_eq!(normalize_claude_hook(&json!({})), None);
    }

    #[test]
    fn codex_osc9_payload_distinguishes_completion_approval_and_plan() {
        assert_eq!(
            normalize_codex_notification("Finished the requested refactor."),
            Some(AgentEventKind::TurnCompleted)
        );
        assert_eq!(
            normalize_codex_notification("Agent turn complete"),
            Some(AgentEventKind::TurnCompleted)
        );
        for message in [
            "Approval requested: cargo test",
            "Approval requested by github",
            "Codex wants to edit src/main.rs",
        ] {
            assert!(matches!(
                normalize_codex_notification(message),
                Some(AgentEventKind::NeedsInput {
                    kind: NeedsInputKind::Approval,
                    ..
                })
            ));
        }
        assert!(matches!(
            normalize_codex_notification("Plan mode prompt: Implement this plan?"),
            Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::PlanApproval,
                ..
            })
        ));
        assert_eq!(normalize_codex_notification(""), None);
    }

    #[test]
    fn blockable_tool_events_are_ignored_and_elicitation_result_resolves() {
        for name in ["PostToolUse", "PostToolUseFailure"] {
            let v = json!({
                "hook_event_name": name,
                "tool_name": "AskUserQuestion",
                "tool_use_id": "q1",
            });
            assert_eq!(normalize_claude_hook(&v), None);
        }
        assert!(matches!(
            normalize_claude_hook(&hook("ElicitationResult")),
            Some(AgentEventKind::InputResolved { .. })
        ));
    }

    #[test]
    fn generated_claude_settings_merge_existing_cli_layer() {
        let runtime = SessionKey::new(AgentKind::Claude, "abc");
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        fs::write(
            &file,
            r#"{"permissions":{"allow":["Read"]},"hooks":{"Stop":[{"hooks":[]}]}}"#,
        )
        .unwrap();
        let mut args = vec![
            "--verbose".into(),
            "--settings".into(),
            file.to_string_lossy().into_owned(),
        ];
        let private_settings = instrument_claude(
            &mut args,
            tmp.path(),
            Path::new("/opt/vag"),
            &tmp.path().join("events.sock"),
            "secret",
            &runtime,
        )
        .unwrap();
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        let at = args.iter().position(|a| a == "--settings").unwrap();
        let settings_path = Path::new(&args[at + 1]);
        assert_eq!(settings_path, private_settings);
        let merged: Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(merged["permissions"]["allow"], json!(["Read"]));
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(merged["hooks"]["SessionStart"].is_null());
        assert!(merged["hooks"]["PreToolUse"].is_null());
        assert!(merged["hooks"]["PermissionRequest"].is_null());
        assert_eq!(
            merged["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|idle_prompt|elicitation_dialog|elicitation_complete|elicitation_response"
        );
        let handler = &merged["hooks"]["StopFailure"][0]["hooks"][0];
        assert_eq!(handler["command"], "/opt/vag");
        assert_eq!(handler["args"][0], EVENT_SUBCOMMAND);
        assert_eq!(handler["args"][3], "claude:abc");
        assert!(
            !args.iter().any(|arg| arg.contains("secret")),
            "socket secret must stay out of process arguments"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(settings_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn malformed_existing_settings_are_not_replaced() {
        let original = vec!["--settings".into(), "{broken".into()];
        let mut args = original.clone();
        assert!(
            instrument_claude(
                &mut args,
                Path::new("/tmp"),
                Path::new("/opt/vag"),
                Path::new("/tmp/events.sock"),
                "secret",
                &SessionKey::new(AgentKind::Claude, "abc"),
            )
            .is_err()
        );
        assert_eq!(args, original);
    }

    #[test]
    fn blockable_stop_is_not_misread_as_ready_but_stop_failure_is() {
        assert_eq!(normalize_claude_hook(&hook("Stop")), None);
        assert!(matches!(
            normalize_claude_hook(&hook("StopFailure")),
            Some(AgentEventKind::NeedsInput {
                kind: NeedsInputKind::NextPrompt,
                ..
            })
        ));
    }

    #[test]
    fn codex_notification_overrides_are_last_and_idempotent() {
        let mut args = vec!["-c".into(), "tui.notifications=false".into()];
        add_codex_tui_notifications(&mut args);
        assert_eq!(
            &args[args.len() - 6..],
            [
                "-c",
                CODEX_NOTIFICATIONS,
                "-c",
                CODEX_NOTIFICATION_METHOD,
                "-c",
                CODEX_NOTIFICATION_CONDITION,
            ]
        );
        let once = args.clone();
        add_codex_tui_notifications(&mut args);
        assert_eq!(args, once);
    }

    #[test]
    fn codex_notification_overrides_stay_before_option_separator() {
        let mut args = vec!["resume".into(), "abc".into(), "--".into(), "prompt".into()];
        add_codex_tui_notifications(&mut args);
        let separator = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &args[separator - 6..separator],
            [
                "-c",
                CODEX_NOTIFICATIONS,
                "-c",
                CODEX_NOTIFICATION_METHOD,
                "-c",
                CODEX_NOTIFICATION_CONDITION,
            ]
        );
        assert_eq!(&args[separator..], ["--", "prompt"]);
    }

    #[test]
    fn native_cli_detection_does_not_instrument_arbitrary_wrappers() {
        assert!(is_native_cli("claude", AgentKind::Claude));
        assert!(is_native_cli("/opt/bin/codex", AgentKind::Codex));
        assert!(is_native_cli("C:/tools/codex.exe", AgentKind::Codex));
        assert!(!is_native_cli("/bin/cat", AgentKind::Codex));
        assert!(!is_native_cli("codex-wrapper", AgentKind::Codex));
    }

    #[test]
    fn private_socket_round_trip_rejects_wrong_secret() {
        let listener = match AgentEventListener::bind() {
            Ok(listener) => listener,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Some hermetic test sandboxes disallow AF_UNIX bind. The
                // production constructor has the same graceful fallback;
                // normal macOS/Linux CI exercises the round trip.
                return;
            }
            Err(e) => panic!("binding native-event socket: {e}"),
        };
        let send = |secret: &str, runtime: &str| {
            let socket = UnixDatagram::unbound().unwrap();
            let packet = serde_json::to_vec(&WireEvent {
                secret: secret.into(),
                runtime: runtime.into(),
                observed_at_unix_nanos: 42,
                kind: AgentEventKind::NeedsInput {
                    kind: NeedsInputKind::Question,
                    request_id: Some("q".into()),
                },
            })
            .unwrap();
            socket.send_to(&packet, listener.endpoint()).unwrap();
        };
        send("wrong", "claude:abc");
        send(listener.secret(), "not-a-key");
        send(listener.secret(), "claude:abc");
        assert_eq!(
            listener.drain(),
            vec![RoutedAgentEvent {
                key: SessionKey::new(AgentKind::Claude, "abc"),
                event: AgentEvent {
                    observed_at_unix_nanos: 42,
                    kind: AgentEventKind::NeedsInput {
                        kind: NeedsInputKind::Question,
                        request_id: Some("q".into()),
                    },
                },
            }]
        );
    }
}
