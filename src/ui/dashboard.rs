//! Tree-row model shared by the full-screen dashboard and the narrow
//! sidebar, plus the row renderers.
//!
//! Row building is pure (state + scan + ui-state in, Vec<Row> out) so it's
//! unit-testable without a terminal.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::state::VagState;
use crate::types::{AgentKind, SessionKey, SessionMeta};
use crate::ui::editbuf::{EditBuf, LineId, Mode};
use crate::ui::icons::Icons;
use crate::ui::theme::Theme;

/// Session turn status shown as a badge, computed by the app from the
/// per-runtime turn trackers + external registries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Badge {
    #[default]
    None,
    /// A command is in flight (BadgeInfo::dur = working for how long).
    Working,
    /// The turn finished while the user wasn't viewing the session
    /// (dur = how long ago); cleared on view.
    DoneUnread,
    /// Open in vag, waiting for input, already seen.
    Idle,
    /// Child process exited but the pane is still open.
    Exited,
    /// Running outside vag (claude live registry); dur = working for how
    /// long, when its transcript is being appended.
    External,
}

impl Badge {
    fn glyph(&self, icons: &Icons) -> (&'static str, Color) {
        match self {
            Badge::None => ("", Color::Reset),
            // Working is animated (see SPINNER, shared by both icon sets);
            // this static glyph is only a fallback.
            Badge::Working => ("●", Color::Green),
            Badge::DoneUnread => (icons.badge_done_unread, Color::Cyan),
            Badge::Idle => (icons.badge_idle, Color::DarkGray),
            Badge::Exited => (icons.badge_exited, Color::Red),
            Badge::External => (icons.badge_external, Color::Magenta),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Row {
    /// The "+ new session" button row pinned above everything.
    NewSession,
    /// A blank breathing line between the buttons and the tree. Never
    /// selectable: move_cursor steps over it, row actions can't hit it.
    Spacer,
    Folder {
        id: String,
        depth: usize,
        name: String,
        collapsed: bool,
        session_count: usize,
        default_dir: Option<String>,
        /// The project a scoped folder belongs to (repo dirname), set only
        /// when viewing UNSCOPED so global vs project folders read apart.
        scope_label: Option<String>,
    },
    /// The pseudo-folder for unassigned sessions.
    Inbox { count: usize, collapsed: bool },
    /// One configured `[[remotes]]` machine, always shown (even empty — the
    /// group itself is the discoverability). Members are that machine's
    /// unfoldered remote sessions; foldered ones stay in their folders.
    Machine {
        name: String,
        host: String,
        count: usize,
        collapsed: bool,
    },
    Session {
        key: SessionKey,
        depth: usize,
        /// Index into the app's session list; None for provisional runtimes
        /// (codex id not discovered yet) which have no scan entry.
        meta_idx: Option<usize>,
    },
    /// Placeholder shown under an expanded folder that holds nothing, so an
    /// empty folder reads as "opened but empty" rather than a dead row.
    /// `folder` = the enclosing folder id (for new-session context).
    Empty {
        depth: usize,
        folder: Option<String>,
    },
}

impl Row {
    pub fn session_key(&self) -> Option<&SessionKey> {
        match self {
            Row::Session { key, .. } => Some(key),
            _ => None,
        }
    }

    pub fn folder_id(&self) -> Option<&str> {
        match self {
            Row::Folder { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn machine_name(&self) -> Option<&str> {
        match self {
            Row::Machine { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// The session-color palette offered by the picker (state accepts any of
/// these names, or a raw "#rrggbb" typed into state.json by hand).
pub const SESSION_PALETTE: [&str; 8] = [
    "red", "orange", "yellow", "green", "cyan", "blue", "magenta", "pink",
];

/// Palette name or `#rrggbb` → terminal color. None for unknown text, so a
/// hand-edited state file can never break rendering.
pub fn parse_session_color(s: &str) -> Option<Color> {
    match s.trim().to_ascii_lowercase().as_str() {
        "red" => Some(Color::Red),
        "orange" => Some(Color::Rgb(0xff, 0x87, 0x37)),
        "yellow" => Some(Color::Yellow),
        "green" => Some(Color::Green),
        "cyan" => Some(Color::Cyan),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "pink" => Some(Color::Rgb(0xff, 0x87, 0xaf)),
        hex => {
            let hex = hex.strip_prefix('#')?;
            if hex.len() != 6 {
                return None;
            }
            let n = u32::from_str_radix(hex, 16).ok()?;
            Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
        }
    }
}

/// A session's configured accent color, if any (and parseable).
pub fn session_color(state: &VagState, key: &SessionKey) -> Option<Color> {
    state
        .session(key)?
        .color
        .as_deref()
        .and_then(parse_session_color)
}

pub const INBOX_ID: &str = "\u{0}inbox"; // collapse-set key for the pseudo-folder

/// Collapse-set key for a machine group (NUL-prefixed like INBOX_ID so it
/// can never collide with a real folder id).
pub fn machine_collapse_key(name: &str) -> String {
    format!("\u{0}machine:{name}")
}

/// Build visible rows. `provisional` = open runtimes with no scan entry yet
/// (requested/state labels when known, otherwise "(starting…)").
/// `machines` = configured `[[remotes]]` as (name, host); each gets an
/// always-visible group (scope-exempt like remote sessions) holding that
/// machine's unfoldered remote sessions.
/// `scope` = only show sessions whose cwd is inside this root (git-repo
/// scoping), and only folders that contain such sessions or are bound there.
#[allow(clippy::too_many_arguments)]
pub fn build_rows(
    state: &VagState,
    sessions: &[SessionMeta],
    provisional: &[SessionKey],
    machines: &[(String, String)],
    collapsed: &HashSet<String>,
    filter: Option<&str>,
    show_hidden: bool,
    show_archived: bool,
    scope: Option<&std::path::Path>,
) -> Vec<Row> {
    // filter mode: flat list of matching sessions, no folders
    if let Some(q) = filter {
        let q = q.to_lowercase();
        let mut rows: Vec<Row> = sessions
            .iter()
            .enumerate()
            .filter(|(_, m)| visible(state, m, show_hidden, show_archived, scope))
            .filter(|(_, m)| {
                q.is_empty()
                    || display_title(state, m).to_lowercase().contains(&q)
                    || m.project_label().to_lowercase().contains(&q)
            })
            .map(|(i, m)| Row::Session {
                key: m.key.clone(),
                depth: 0,
                meta_idx: Some(i),
            })
            .collect();
        // Open meta-less panes are reachability handles, not ordinary search
        // results. Keep them visible under any filter until discovery gives
        // them searchable metadata.
        rows.extend(provisional.iter().cloned().map(|key| Row::Session {
            key,
            depth: 0,
            meta_idx: None,
        }));
        return rows;
    }

    // group session indices by folder id (validated against existing
    // folders); unfoldered remote sessions of a configured machine group
    // under that machine instead of the Inbox.
    let mut by_folder: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    let mut by_machine: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in sessions.iter().enumerate() {
        if !visible(state, m, show_hidden, show_archived, scope) {
            continue;
        }
        let folder = state
            .session(&m.key)
            .and_then(|r| r.folder.clone())
            .filter(|id| state.folder(id).is_some());
        if folder.is_none()
            && let Some(machine) = state
                .session(&m.key)
                .and_then(|r| r.remote.as_deref())
                .and_then(|rname| machines.iter().find(|(n, _)| n == rname))
        {
            by_machine.entry(machine.0.as_str()).or_default().push(i);
            continue;
        }
        by_folder.entry(folder).or_default().push(i);
    }

    // A spacer under the button keeps the tree from reading as one dense
    // block with the header and the + row (filter mode stays flat/dense).
    let mut rows = vec![Row::NewSession, Row::Spacer];
    // User folders on top — a newly created folder sits above the Inbox even
    // when empty, so organizing feels immediate.
    push_folder_level(
        state, sessions, &by_folder, collapsed, None, 0, scope, &mut rows,
    );
    // Machine groups next. ALWAYS shown — an empty group is what teaches that
    // the machine exists.
    for (name, host) in machines {
        let members = by_machine.get(name.as_str()).cloned().unwrap_or_default();
        let is_collapsed = collapsed.contains(&machine_collapse_key(name));
        rows.push(Row::Machine {
            name: name.clone(),
            host: host.clone(),
            count: members.len(),
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            for i in members {
                rows.push(Row::Session {
                    key: sessions[i].key.clone(),
                    depth: 1,
                    meta_idx: Some(i),
                });
            }
        }
    }
    // Inbox last: the unfiled catch-all. Shown when it has sessions, or when
    // there are no folders at all (so an empty tree still has an anchor).
    let inbox = by_folder.get(&None).cloned().unwrap_or_default();
    let inbox_collapsed = collapsed.contains(INBOX_ID);
    let inbox_count = inbox.len() + provisional.len();
    if inbox_count > 0 || state.folders.is_empty() {
        rows.push(Row::Inbox {
            count: inbox_count,
            collapsed: inbox_collapsed,
        });
        if !inbox_collapsed {
            for key in provisional {
                rows.push(Row::Session {
                    key: key.clone(),
                    depth: 1,
                    meta_idx: None,
                });
            }
            for i in inbox {
                rows.push(Row::Session {
                    key: sessions[i].key.clone(),
                    depth: 1,
                    meta_idx: Some(i),
                });
            }
        }
    }
    rows
}

fn visible(
    state: &VagState,
    m: &SessionMeta,
    show_hidden: bool,
    show_archived: bool,
    scope: Option<&std::path::Path>,
) -> bool {
    if !show_hidden && state.session(&m.key).map(|r| r.hidden).unwrap_or(false) {
        return false;
    }
    if !show_archived && m.archived {
        return false;
    }
    // Remote sessions carry a remote cwd that can never sit under a local
    // repo root — scoping must not make them vanish.
    if let Some(root) = scope
        && !m.cwd.starts_with(root)
        && state
            .session(&m.key)
            .and_then(|r| r.remote.as_ref())
            .is_none()
    {
        return false;
    }
    true
}

/// Under a repo filter, a folder earns a row when it BELONGS to that repo
/// (a project folder — always visible there, even empty), or when it's a
/// global/other folder whose subtree holds in-scope sessions or is bound to
/// a directory inside the scope (or has a visible descendant). Empty global
/// folders surface only in the unfiltered view.
fn folder_in_scope(
    state: &VagState,
    by_folder: &HashMap<Option<String>, Vec<usize>>,
    id: &str,
    scope: &std::path::Path,
) -> bool {
    if state
        .folder(id)
        .and_then(|f| f.scope.as_deref())
        .is_some_and(|s| s == scope)
    {
        return true;
    }
    if count_recursive(state, by_folder, id) > 0 {
        return true;
    }
    if state
        .folder(id)
        .and_then(|f| f.default_dir.as_ref())
        .map(|d| d.starts_with(scope))
        .unwrap_or(false)
    {
        return true;
    }
    state
        .children_of(Some(id))
        .iter()
        .any(|f| folder_in_scope(state, by_folder, &f.id, scope))
}

#[allow(clippy::too_many_arguments)]
fn push_folder_level(
    state: &VagState,
    sessions: &[SessionMeta],
    by_folder: &HashMap<Option<String>, Vec<usize>>,
    collapsed: &HashSet<String>,
    parent: Option<&str>,
    depth: usize,
    scope: Option<&std::path::Path>,
    rows: &mut Vec<Row>,
) {
    for f in state.children_of(parent) {
        if let Some(root) = scope
            && !folder_in_scope(state, by_folder, &f.id, root)
        {
            continue;
        }
        let members = by_folder
            .get(&Some(f.id.clone()))
            .cloned()
            .unwrap_or_default();
        let is_collapsed = collapsed.contains(&f.id);
        rows.push(Row::Folder {
            id: f.id.clone(),
            depth,
            name: f.name.clone(),
            collapsed: is_collapsed,
            session_count: count_recursive(state, by_folder, &f.id),
            default_dir: f.default_dir.as_ref().map(|p| p.display().to_string()),
            scope_label: match scope {
                // filtered view: scoping is implied, no label noise
                Some(_) => None,
                None => f
                    .scope
                    .as_deref()
                    .and_then(|s| s.file_name().map(|n| n.to_string_lossy().into_owned())),
            },
        });
        if !is_collapsed {
            let before = rows.len();
            for i in members {
                rows.push(Row::Session {
                    key: sessions[i].key.clone(),
                    depth: depth + 1,
                    meta_idx: Some(i),
                });
            }
            push_folder_level(
                state,
                sessions,
                by_folder,
                collapsed,
                Some(&f.id),
                depth + 1,
                scope,
                rows,
            );
            // Nothing rendered under an expanded folder → an empty-state row
            // (also covers a folder whose only children were scope-pruned).
            if rows.len() == before {
                rows.push(Row::Empty {
                    depth: depth + 1,
                    folder: Some(f.id.clone()),
                });
            }
        }
    }
}

fn count_recursive(
    state: &VagState,
    by_folder: &HashMap<Option<String>, Vec<usize>>,
    id: &str,
) -> usize {
    let own = by_folder
        .get(&Some(id.to_string()))
        .map(|v| v.len())
        .unwrap_or(0);
    own + state
        .children_of(Some(id))
        .iter()
        .map(|f| count_recursive(state, by_folder, &f.id))
        .sum::<usize>()
}

pub fn display_title(state: &VagState, m: &SessionMeta) -> String {
    if let Some(name) = state_name(state, &m.key) {
        return name;
    }
    m.display_title()
}

/// Vag-owned name for rows that may not have a SessionMeta yet (fresh known
/// ids, early SQLite resolution, and remote synthetic sessions).
pub fn state_name(state: &VagState, key: &SessionKey) -> Option<String> {
    state
        .session(key)
        .and_then(|r| r.name_override.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

pub fn meta_less_title(key: &SessionKey) -> String {
    if key.id.starts_with("pending-") {
        "(starting…)".to_string()
    } else {
        format!("{} session", key.agent.label())
    }
}

pub fn rel_time(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = t else { return "".into() };
    let d = now.signed_duration_since(t);
    let s = d.num_seconds();
    if s < 0 {
        return "now".into();
    }
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m", s / 60);
    }
    if s < 86_400 {
        return format!("{}h", s / 3600);
    }
    format!("{}d", s / 86_400)
}

/// Badge plus the live metadata rendered next to it. `dur` meaning depends
/// on kind: Working/External = working for; DoneUnread = finished ago.
#[derive(Debug, Clone, Copy, Default)]
pub struct BadgeInfo {
    pub kind: Badge,
    pub dur: Option<std::time::Duration>,
}

/// Braille spinner shown while a session is actively producing output.
/// The app advances `RowCtx::spin_frame` on its tick cadence.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Compact duration for "working 4m32s"-style labels. Seconds precision up
/// to an hour: turn durations are what users watch.
pub fn fmt_work_dur(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m{:02}s", s / 60, s % 60);
    }
    if s < 86_400 {
        return format!("{}h{:02}m", s / 3600, (s % 3600) / 60);
    }
    format!("{}d", s / 86_400)
}

/// Everything the renderers need from the app about one row.
pub struct RowCtx<'a> {
    pub state: &'a VagState,
    pub sessions: &'a [SessionMeta],
    pub badges: &'a HashMap<SessionKey, BadgeInfo>,
    pub now: DateTime<Utc>,
    pub active: Option<&'a SessionKey>,
    pub open_order: &'a [SessionKey],
    /// Animation frame counter (app tick / 100ms); only Busy rows animate.
    pub spin_frame: usize,
    /// Resolved glyph set (ascii/nerd), chosen once by the app from config.
    pub icons: &'a Icons,
    /// Display titles for provisional runtimes with no scan entry (requested
    /// agent names or shell labels); fallback is "(starting…)".
    pub provisional_labels: &'a HashMap<SessionKey, String>,
    /// Active color theme: ALL chrome text in the tree (buttons, folder
    /// names, project labels, timestamps, highlights) keys off this — never
    /// hardcode a chrome color in a row renderer.
    pub theme: Theme,
}

fn session_line(
    ctx: &RowCtx,
    key: &SessionKey,
    meta_idx: Option<usize>,
    depth: usize,
    narrow: bool,
    width: usize,
) -> Line<'static> {
    // +1: groups/buttons share the base padding; sessions sit one deeper.
    let indent = "  ".repeat(depth + 1);
    let info = ctx.badges.get(key).copied().unwrap_or_default();
    let (bg, bc) = match info.kind {
        // Working sessions animate; the glyph cycles with the app tick.
        Badge::Working => (SPINNER[ctx.spin_frame % SPINNER.len()], Color::Green),
        other => other.glyph(ctx.icons),
    };
    // Turn indicator: "⠹ working 4m32s" / "● done 2m" / "▲ working 3m"
    let indicator = match (info.kind, info.dur) {
        (Badge::Working, Some(d)) => Some(format!("working {}", fmt_work_dur(d))),
        (Badge::DoneUnread, Some(d)) => Some(format!("done {}", fmt_work_dur(d))),
        (Badge::External, Some(d)) => Some(format!("working {}", fmt_work_dur(d))),
        _ => None,
    };
    let unread = info.kind == Badge::DoneUnread;
    let (title, project, time, hidden, archived) = match meta_idx {
        Some(i) => {
            let m = &ctx.sessions[i];
            (
                display_title(ctx.state, m),
                m.project_label(),
                rel_time(m.last_activity, ctx.now),
                ctx.state.session(key).map(|r| r.hidden).unwrap_or(false),
                m.archived,
            )
        }
        None => (
            state_name(ctx.state, key)
                .or_else(|| ctx.provisional_labels.get(key).cloned())
                .unwrap_or_else(|| meta_less_title(key)),
            String::new(),
            String::new(),
            false,
            false,
        ),
    };
    let agent_icon = Span::styled(
        format!("{} ", ctx.icons.agent(key.agent)),
        Style::new().fg(match key.agent {
            AgentKind::Claude => Color::LightYellow,
            AgentKind::Codex => Color::LightBlue,
            AgentKind::Shell => Color::LightGreen,
        }),
    );
    let quick = ctx
        .open_order
        .iter()
        .position(|k| k == key)
        .filter(|i| *i < 9)
        .map(|i| format!("{} ", i + 1))
        .unwrap_or_default();

    let mut spans = vec![Span::raw(indent), agent_icon];
    if !quick.is_empty() {
        spans.push(Span::styled(quick, Style::new().fg(ctx.theme.dim)));
    }
    let mut tstyle = Style::new();
    if let Some(c) = session_color(ctx.state, key) {
        tstyle = tstyle.fg(c);
    }
    if Some(key) == ctx.active || unread {
        tstyle = tstyle.add_modifier(Modifier::BOLD);
    }
    if hidden || archived {
        tstyle = tstyle.fg(ctx.theme.dim);
    }
    if narrow {
        // Compact live indicator: "⠹ 4m32s" / "● 2m" (dur only, no words).
        let compact = match (info.kind, info.dur) {
            (Badge::Working | Badge::DoneUnread | Badge::External, Some(d)) => {
                Some(fmt_work_dur(d))
            }
            _ => None,
        };
        let ind_width = compact.as_ref().map(|d| d.chars().count() + 3).unwrap_or(2);
        spans.push(Span::styled(
            truncate(&title, width.saturating_sub(depth * 2 + 4 + ind_width)),
            tstyle,
        ));
        if !bg.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(bg.to_string(), Style::new().fg(bc)));
            if let Some(d) = compact {
                spans.push(Span::styled(format!(" {d}"), Style::new().fg(bc)));
            }
        }
    } else {
        spans.push(Span::styled(truncate(&title, 48), tstyle));
        let remote = ctx.state.session(key).and_then(|r| r.remote.clone());
        if let Some(rname) = remote {
            // Remote marker + name where the project label goes:
            // "@gpu-box" (ascii sigils glue) / "󰅟 gpu-box" (nerd glyphs
            // breathe).
            let sep = if ctx.icons.remote.is_ascii() { "" } else { " " };
            spans.push(Span::styled(
                format!("  {}{sep}{rname}", ctx.icons.remote),
                Style::new().fg(ctx.theme.info),
            ));
        } else {
            spans.push(Span::styled(
                format!("  {project}"),
                Style::new().fg(ctx.theme.info),
            ));
        }
        if archived {
            spans.push(Span::styled("  [archived]", Style::new().fg(ctx.theme.dim)));
        }
        if hidden {
            spans.push(Span::styled("  [hidden]", Style::new().fg(ctx.theme.dim)));
        }
        if let Some(ind) = &indicator {
            // A live turn indicator replaces the last-activity age.
            spans.push(Span::raw("  "));
            spans.push(Span::styled(bg.to_string(), Style::new().fg(bc)));
            spans.push(Span::styled(format!(" {ind}"), Style::new().fg(bc)));
        } else {
            spans.push(Span::styled(
                format!("  {time}"),
                Style::new().fg(ctx.theme.dim),
            ));
            if !bg.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(bg.to_string(), Style::new().fg(bc)));
            }
        }
    }
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
fn folder_line(
    icons: &Icons,
    th: &Theme,
    depth: usize,
    name: &str,
    collapsed: bool,
    count: usize,
    default_dir: Option<&str>,
    narrow: bool,
) -> Line<'static> {
    folder_line_with_marker(
        icons,
        th,
        depth,
        name,
        collapsed,
        count,
        default_dir,
        narrow,
        None,
    )
}

/// `marker` replaces the collapse arrow (the Inbox's nerd glyph stands
/// alone instead of stacking a folder arrow next to it).
#[allow(clippy::too_many_arguments)]
fn folder_line_with_marker(
    icons: &Icons,
    th: &Theme,
    depth: usize,
    name: &str,
    collapsed: bool,
    count: usize,
    default_dir: Option<&str>,
    narrow: bool,
    marker: Option<&str>,
) -> Line<'static> {
    // Base "  " lines every group/button up at the same left padding as
    // the "+ new session" row; sessions render one level deeper.
    let indent = "  ".repeat(depth + 1);
    let arrow = marker.unwrap_or(if collapsed {
        icons.folder_collapsed
    } else {
        icons.folder_expanded
    });
    let mut spans = vec![
        Span::raw(indent),
        Span::styled(
            format!("{arrow} {name}"),
            Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" ({count})"), Style::new().fg(th.dim)),
    ];
    if !narrow && let Some(d) = default_dir {
        spans.push(Span::styled(format!("  ⇒ {d}"), Style::new().fg(th.dim)));
    }
    Line::from(spans)
}

/// Machine group header: collapse arrow (like folders) + remote glyph +
/// name + member count, the ssh host dimmed in wide mode, and a dim
/// teach-the-keys hint while the group is empty.
fn machine_line(
    icons: &Icons,
    th: &Theme,
    name: &str,
    host: &str,
    count: usize,
    collapsed: bool,
    narrow: bool,
) -> Line<'static> {
    let arrow = if collapsed {
        icons.folder_collapsed
    } else {
        icons.folder_expanded
    };
    let sep = if icons.remote.is_ascii() { "" } else { " " };
    let mut spans = vec![
        Span::raw("  "), // group base padding (aligns with + new session)
        Span::styled(
            format!("{arrow} {}{sep}{name}", icons.remote),
            Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" ({count})"), Style::new().fg(th.dim)),
    ];
    if !narrow {
        spans.push(Span::styled(format!("  {host}"), Style::new().fg(th.info)));
    }
    if count == 0 {
        spans.push(Span::styled(
            "  (n: new session · s: shell)",
            Style::new().fg(th.dim),
        ));
    }
    Line::from(spans)
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A dim full-width hairline: separates the chrome (header above, pinned
/// settings below) from the scrolling tree so the regions read apart.
pub fn rule_line(th: &Theme, width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::new().fg(th.dim),
    ))
}

/// The pinned "⚙ settings" footer line. It renders OUTSIDE the scrolling
/// list (its own 1-line region at the bottom of the tree), so it never
/// steals a viewport slot from sessions: the list scrolls above it and the
/// button stays put. The cursor reaches it one step PAST the last row
/// (`j` at the end, or End) — that position is the app's settings sentinel.
pub fn settings_line(
    icons: &Icons,
    th: &Theme,
    selected: bool,
    width: u16,
    key: char,
) -> Line<'static> {
    let mut line = Line::from(Span::styled(
        format!("  {} settings ({key})", icons.settings),
        Style::new().fg(th.dim),
    ));
    if selected {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        let fill = (width as usize).saturating_sub(used);
        if fill > 0 {
            line.push_span(Span::raw(" ".repeat(fill)));
        }
        line = line.style(Style::new().bg(th.sel));
    }
    line
}

/// Render the row list into `area` with cursor highlighting and scrolling.
/// Used for both the dashboard body and the sidebar body (`narrow`).
pub fn render_rows(
    f: &mut Frame,
    area: Rect,
    rows: &[Row],
    cursor: usize,
    ctx: &RowCtx,
    narrow: bool,
    tree_focused: bool,
) {
    let th = &ctx.theme;
    if area.height == 0 {
        return;
    }
    let visible = area.height as usize;
    let top = cursor
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.len().saturating_sub(visible.min(rows.len())));
    // keep cursor roughly centered when scrolling long lists
    let top = if rows.len() > visible {
        cursor.saturating_sub(visible / 2).min(rows.len() - visible)
    } else {
        top
    };
    let mut lines = Vec::with_capacity(visible);
    for (i, row) in rows.iter().enumerate().skip(top).take(visible) {
        let mut line = match row {
            Row::NewSession => Line::from(vec![Span::styled(
                format!("  {} new session", ctx.icons.new_session),
                Style::new().fg(th.accent),
            )]),
            Row::Spacer => Line::raw(""),
            Row::Folder {
                id: _,
                depth,
                name,
                collapsed,
                session_count,
                default_dir,
                scope_label,
            } => {
                let mut line = folder_line(
                    ctx.icons,
                    th,
                    *depth,
                    name,
                    *collapsed,
                    *session_count,
                    default_dir.as_deref(),
                    narrow,
                );
                // Global view: mark project folders with their repo so it's
                // clear why they appear/disappear with the g filter.
                if let Some(repo) = scope_label
                    && !narrow
                {
                    line.push_span(Span::styled(format!("  ⌂ {repo}"), Style::new().fg(th.dim)));
                }
                line
            }
            Row::Inbox { count, collapsed } => {
                // Nerd: the inbox glyph stands in for the folder arrow (one
                // icon, not two). Ascii has no inbox glyph — the arrow does
                // the job exactly as before.
                let marker = (!ctx.icons.inbox.is_empty()).then_some(ctx.icons.inbox);
                folder_line_with_marker(
                    ctx.icons, th, 0, "Inbox", *collapsed, *count, None, narrow, marker,
                )
            }
            Row::Machine {
                name,
                host,
                count,
                collapsed,
            } => machine_line(ctx.icons, th, name, host, *count, *collapsed, narrow),
            Row::Session {
                key,
                depth,
                meta_idx,
            } => session_line(ctx, key, *meta_idx, *depth, narrow, area.width as usize),
            Row::Empty { depth, .. } => Line::from(vec![Span::styled(
                format!("{}(empty — n: new session here)", "  ".repeat(*depth + 1)),
                Style::new().fg(th.dim).add_modifier(Modifier::ITALIC),
            )]),
        };
        if i == cursor {
            // Solid bar, NOT REVERSED: reversing flips every colored span
            // (agent icon, quick number, badge) into a mismatched background
            // patch. A bg keeps each span's own text color on one surface.
            let style = if tree_focused {
                Style::new().bg(th.sel)
            } else {
                Style::new().bg(th.surface)
            };
            // Pad to the full row width so the highlight is a bar across
            // the pane, not a box hugging the text.
            let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            let fill = (area.width as usize).saturating_sub(used);
            if fill > 0 {
                line.push_span(Span::raw(" ".repeat(fill)));
            }
            line = line.style(style);
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Render the edit-mode buffer where the tree rows normally paint: one line
/// per EditLine (indented by depth), folder lines cyan, readonly lines dark
/// gray, and the cursor cell reversed. Scrolls with the same rough-centering
/// as `render_rows`.
pub fn render_editbuf(f: &mut Frame, area: Rect, buf: &EditBuf, th: &Theme) {
    if area.height == 0 {
        return;
    }
    let lines = buf.lines();
    // cursor() yields (row, byte offset clamped to a char boundary)
    let (crow, cbyte) = buf.cursor();
    let visible = area.height as usize;
    let top = if lines.len() > visible {
        crow.saturating_sub(visible / 2).min(lines.len() - visible)
    } else {
        0
    };
    let mut out: Vec<Line> = Vec::with_capacity(visible);
    if lines.is_empty() {
        // Everything was dd-ed: keep a visible cursor cell so `o`/`p` have
        // an anchor the user can see.
        out.push(Line::from(Span::styled(
            " ",
            Style::new().add_modifier(Modifier::REVERSED),
        )));
    }
    for (i, l) in lines.iter().enumerate().skip(top).take(visible) {
        let style = if l.readonly {
            Style::new().fg(th.dim)
        } else if matches!(l.id, LineId::Folder(_)) {
            Style::new().fg(th.accent)
        } else {
            Style::new()
        };
        let indent = "  ".repeat(l.depth);
        if i == crow {
            let before = &l.text[..cbyte];
            let cur = l.text[cbyte..].chars().next();
            let after = cur
                .map(|c| &l.text[cbyte + c.len_utf8()..])
                .unwrap_or_default();
            out.push(Line::from(vec![
                Span::raw(indent),
                Span::styled(before.to_string(), style),
                Span::styled(
                    // Past end of line (Insert mode): a reversed space.
                    cur.map(String::from).unwrap_or_else(|| " ".into()),
                    style.add_modifier(Modifier::REVERSED),
                ),
                Span::styled(after.to_string(), style),
            ]));
        } else {
            out.push(Line::from(vec![
                Span::raw(indent),
                Span::styled(l.text.clone(), style),
            ]));
        }
    }
    f.render_widget(Paragraph::new(out), area);
}

/// Footer/mode line shown instead of the normal hints while edit mode is
/// active (vim-style).
pub fn editbuf_footer_line(buf: &EditBuf) -> Line<'static> {
    match buf.mode() {
        Mode::Insert => Line::from(Span::styled(
            " -- INSERT --",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Mode::Cmdline(cmd) => Line::from(vec![
            Span::raw(format!(" :{cmd}")),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ]),
        Mode::Normal => Line::from(Span::styled(
            " EDIT (:w save · :q quit)",
            Style::new().fg(Color::DarkGray),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VagState;
    use std::path::PathBuf;

    fn meta(agent: AgentKind, id: &str, title: &str) -> SessionMeta {
        SessionMeta {
            key: SessionKey::new(agent, id),
            title: Some(title.to_string()),
            preview: None,
            cwd: PathBuf::from("/tmp/proj"),
            created: None,
            last_user_activity: None,
            last_activity: None,
            archived: false,
            source_path: PathBuf::from("/tmp/x.jsonl"),
            git_branch: None,
        }
    }

    #[test]
    fn rows_group_by_folder_and_inbox() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        let s1 = meta(AgentKind::Claude, "aaa", "one");
        let s2 = meta(AgentKind::Codex, "bbb", "two");
        st.set_session_folder(&s1.key, Some(&fid)).unwrap();
        let sessions = vec![s1, s2];
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        // + new session, spacer, folder(work) + s1, then Inbox(1) + s2
        assert!(matches!(rows[0], Row::NewSession));
        assert!(matches!(rows[1], Row::Spacer));
        assert!(matches!(&rows[2], Row::Folder { name, .. } if name == "work"));
        assert!(matches!(&rows[3], Row::Session { key, .. } if key.id == "aaa"));
        assert!(matches!(rows[4], Row::Inbox { count: 1, .. }));
        assert!(matches!(&rows[5], Row::Session { key, .. } if key.id == "bbb"));
    }

    #[test]
    fn empty_folder_shows_placeholder_when_expanded() {
        let mut st = VagState::default();
        let fid = st.create_folder("empties", None).unwrap();
        let rows = build_rows(
            &st,
            &[],
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        // + new session, spacer, folder(empties), its empty placeholder
        assert!(matches!(rows[0], Row::NewSession));
        assert!(matches!(&rows[2], Row::Folder { name, .. } if name == "empties"));
        assert!(
            matches!(&rows[3], Row::Empty { folder: Some(f), depth: 1 } if *f == fid),
            "expanded empty folder gets a placeholder: {rows:?}"
        );
        // no Inbox row: folders exist and there are no unfiled sessions
        assert!(!rows.iter().any(|r| matches!(r, Row::Inbox { .. })));
        // collapsed → no placeholder
        let mut collapsed = HashSet::new();
        collapsed.insert(fid.clone());
        let rows = build_rows(&st, &[], &[], &[], &collapsed, None, false, false, None);
        assert!(!rows.iter().any(|r| matches!(r, Row::Empty { .. })));
    }

    #[test]
    fn project_folders_show_under_their_scope_globals_only_unfiltered() {
        let mut st = VagState::default();
        let repo = PathBuf::from("/repo");
        let project = st
            .create_folder_scoped("proj-work", None, Some(repo.clone()))
            .unwrap();
        let global = st.create_folder("global-work", None).unwrap();
        // subfolder of a project folder inherits its scope regardless of view
        let child = st.create_folder("sub", Some(&project)).unwrap();
        assert_eq!(
            st.folder(&child).unwrap().scope.as_deref(),
            Some(repo.as_path())
        );

        // FILTERED: empty project folder (and its child) visible; empty
        // global folder hidden.
        let rows = build_rows(
            &st,
            &[],
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            Some(repo.as_path()),
        );
        let folders: Vec<&str> = rows.iter().filter_map(|r| r.folder_id()).collect();
        assert!(
            folders.contains(&project.as_str()),
            "project folder under scope: {rows:?}"
        );
        assert!(
            folders.contains(&child.as_str()),
            "inherited-scope child too"
        );
        assert!(
            !folders.contains(&global.as_str()),
            "empty global hidden under scope"
        );
        // no scope labels in the filtered view
        assert!(rows.iter().all(|r| !matches!(
            r,
            Row::Folder {
                scope_label: Some(_),
                ..
            }
        )));

        // UNFILTERED: everything visible; project folder carries its label.
        let rows = build_rows(
            &st,
            &[],
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        let folders: Vec<&str> = rows.iter().filter_map(|r| r.folder_id()).collect();
        assert!(folders.contains(&project.as_str()));
        assert!(folders.contains(&global.as_str()));
        assert!(rows.iter().any(|r| matches!(
            r,
            Row::Folder { id, scope_label: Some(l), .. } if *id == project && l == "repo"
        )));
        assert!(rows.iter().any(|r| matches!(
            r,
            Row::Folder { id, scope_label: None, .. } if *id == global
        )));
    }

    #[test]
    fn collapse_hides_members_but_counts() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        let s1 = meta(AgentKind::Claude, "aaa", "one");
        st.set_session_folder(&s1.key, Some(&fid)).unwrap();
        let sessions = vec![s1];
        let mut collapsed = HashSet::new();
        collapsed.insert(fid.clone());
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &collapsed,
            None,
            false,
            false,
            None,
        );
        assert_eq!(rows.len(), 3); // + new session, spacer, folder
        assert!(matches!(
            &rows[2],
            Row::Folder {
                session_count: 1,
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn filter_flattens_and_matches() {
        let st = VagState::default();
        let pending = SessionKey::new(AgentKind::Codex, "pending-open");
        let sessions = vec![
            meta(AgentKind::Claude, "aaa", "fix auth bug"),
            meta(AgentKind::Codex, "bbb", "write docs"),
        ];
        let rows = build_rows(
            &st,
            &sessions,
            std::slice::from_ref(&pending),
            &[],
            &HashSet::new(),
            Some("auth"),
            false,
            false,
            None,
        );
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Row::Session { key, .. } if key.id == "aaa"));
        assert!(matches!(&rows[1], Row::Session { key, meta_idx: None, .. } if key == &pending));
    }

    #[test]
    fn hidden_filtered_unless_shown() {
        let mut st = VagState::default();
        let s1 = meta(AgentKind::Claude, "aaa", "one");
        st.session_mut(&s1.key).hidden = true;
        let sessions = vec![s1];
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        assert!(rows.iter().all(|r| r.session_key().is_none()));
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            true,
            false,
            None,
        );
        assert!(rows.iter().any(|r| r.session_key().is_some()));
    }

    #[test]
    fn dangling_folder_assignment_falls_to_inbox() {
        let mut st = VagState::default();
        let s1 = meta(AgentKind::Claude, "aaa", "one");
        st.session_mut(&s1.key).folder = Some("gone".into());
        let sessions = vec![s1];
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        assert!(matches!(rows[2], Row::Inbox { count: 1, .. }));
    }

    #[test]
    fn scope_filters_sessions_and_prunes_folders() {
        let mut st = VagState::default();
        let in_repo = st.create_folder("repo-work", None).unwrap();
        let elsewhere = st.create_folder("other", None).unwrap();
        let bound = st.create_folder("bound-empty", None).unwrap();
        st.set_folder_default_dir(&bound, Some(PathBuf::from("/repo/sub")))
            .unwrap();

        let mut s1 = meta(AgentKind::Claude, "aaa", "in repo");
        s1.cwd = PathBuf::from("/repo");
        let mut s2 = meta(AgentKind::Codex, "bbb", "in repo subdir");
        s2.cwd = PathBuf::from("/repo/crates/x");
        let mut s3 = meta(AgentKind::Claude, "ccc", "outside");
        s3.cwd = PathBuf::from("/elsewhere");
        st.set_session_folder(&s1.key, Some(&in_repo)).unwrap();
        st.set_session_folder(&s3.key, Some(&elsewhere)).unwrap();
        let sessions = vec![s1, s2, s3];

        let scope = PathBuf::from("/repo");
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            Some(&scope),
        );
        let keys: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.session_key().map(|k| k.id.as_str()))
            .collect();
        assert_eq!(keys, vec!["aaa", "bbb"]); // folder s1 (top), inbox s2; s3 gone
        let folders: Vec<&str> = rows.iter().filter_map(|r| r.folder_id()).collect();
        // folder with in-repo session + folder bound into the repo; "other" pruned
        assert!(folders.contains(&in_repo.as_str()));
        assert!(folders.contains(&bound.as_str()));
        assert!(!folders.contains(&elsewhere.as_str()));
        // unscoped shows everything again
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        assert!(
            rows.iter()
                .filter_map(|r| r.session_key())
                .any(|k| k.id == "ccc")
        );
    }

    #[test]
    fn remote_sessions_exempt_from_scope_filtering() {
        let mut st = VagState::default();
        let mut remote = meta(AgentKind::Claude, "rrr", "on the box");
        remote.cwd = PathBuf::from("~/work"); // remote path, never under scope
        st.session_mut(&remote.key).remote = Some("gpu-box".into());
        let mut local_out = meta(AgentKind::Codex, "lll", "elsewhere");
        local_out.cwd = PathBuf::from("/elsewhere");
        let sessions = vec![remote, local_out];

        let scope = PathBuf::from("/repo");
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            Some(&scope),
        );
        let keys: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.session_key().map(|k| k.id.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec!["rrr"],
            "remote session survives scoping; out-of-scope local one doesn't"
        );
    }

    #[test]
    fn session_line_marks_remote_sessions_instead_of_project() {
        let mut st = VagState::default();
        let mut m = meta(AgentKind::Claude, "rrr", "on the box");
        m.cwd = PathBuf::from("~/work");
        st.session_mut(&m.key).remote = Some("gpu-box".into());
        let sessions = vec![m, meta(AgentKind::Codex, "lll", "local one")];
        let badges = HashMap::new();
        let labels = HashMap::new();
        let now = Utc::now();
        let mut ctx = RowCtx {
            state: &st,
            sessions: &sessions,
            badges: &badges,
            now,
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::TRANSPARENT,
        };
        let l = session_line(&ctx, &sessions[0].key, Some(0), 1, false, 80);
        let t = line_text(&l);
        assert!(t.contains("  @gpu-box"), "ascii remote marker glues: {t:?}");
        assert!(!t.contains("work"), "project label replaced: {t:?}");
        // nerd glyphs get a breathing space
        ctx.icons = &Icons::NERD;
        let l = session_line(&ctx, &sessions[0].key, Some(0), 1, false, 80);
        let t = line_text(&l);
        assert!(
            t.contains(&format!("  {} gpu-box", Icons::NERD.remote)),
            "{t:?}"
        );
        // local sessions keep the project label
        ctx.icons = &Icons::ASCII;
        let l = session_line(&ctx, &sessions[1].key, Some(1), 1, false, 80);
        let t = line_text(&l);
        assert!(t.contains("  proj"), "local project label intact: {t:?}");
        assert!(!t.contains('@'), "{t:?}");
    }

    #[test]
    fn provisional_rows_render_before_inbox_sessions() {
        let st = VagState::default();
        let prov = SessionKey::new(AgentKind::Claude, "pending-xyz");
        let rows = build_rows(
            &st,
            &[],
            std::slice::from_ref(&prov),
            &[],
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        assert!(matches!(rows[0], Row::NewSession));
        assert!(matches!(rows[2], Row::Inbox { count: 1, .. }));
        assert!(matches!(&rows[3], Row::Session { key, meta_idx: None, .. } if *key == prov));
    }

    fn machines(names: &[(&str, &str)]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|(n, h)| (n.to_string(), h.to_string()))
            .collect()
    }

    #[test]
    fn machine_groups_render_between_folders_and_inbox_even_empty() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        let s1 = meta(AgentKind::Claude, "aaa", "foldered");
        st.set_session_folder(&s1.key, Some(&fid)).unwrap();
        let sessions = vec![s1, meta(AgentKind::Codex, "bbb", "inboxed")];
        let ms = machines(&[("gpu", "user@gpu.example")]);
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &ms,
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        // + new session, spacer, folder + aaa, machine(gpu), Inbox + bbb
        assert!(matches!(rows[0], Row::NewSession));
        assert!(matches!(&rows[2], Row::Folder { name, .. } if name == "work"));
        assert!(matches!(&rows[3], Row::Session { key, .. } if key.id == "aaa"));
        match &rows[4] {
            Row::Machine {
                name,
                host,
                count,
                collapsed,
            } => {
                assert_eq!(name, "gpu");
                assert_eq!(host, "user@gpu.example");
                assert_eq!(*count, 0, "no unfoldered remote sessions yet");
                assert!(!collapsed);
            }
            other => panic!("expected machine row, got {other:?}"),
        }
        assert!(matches!(rows[5], Row::Inbox { count: 1, .. }));
        assert!(matches!(&rows[6], Row::Session { key, .. } if key.id == "bbb"));

        // …and the empty group survives repo scoping (scope-exempt).
        let scope = PathBuf::from("/repo");
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &ms,
            &HashSet::new(),
            None,
            false,
            false,
            Some(&scope),
        );
        assert!(
            rows.iter().any(|r| r.machine_name() == Some("gpu")),
            "machine group visible under scope"
        );
    }

    #[test]
    fn machine_members_are_unfoldered_remote_sessions_only() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        // Unfoldered remote session → under its machine, NOT the Inbox.
        let free = meta(AgentKind::Claude, "rrr", "on the box");
        st.session_mut(&free.key).remote = Some("gpu".into());
        // Foldered remote session → stays in its folder, NOT duplicated.
        let held = meta(AgentKind::Claude, "fff", "foldered remote");
        st.session_mut(&held.key).remote = Some("gpu".into());
        st.set_session_folder(&held.key, Some(&fid)).unwrap();
        // Remote of a machine no longer configured → falls back to Inbox.
        let orphan = meta(AgentKind::Codex, "ooo", "orphaned");
        st.session_mut(&orphan.key).remote = Some("gone-box".into());
        let sessions = vec![free, held, orphan];
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &machines(&[("gpu", "user@gpu.example")]),
            &HashSet::new(),
            None,
            false,
            false,
            None,
        );
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.session_key().map(|k| k.id.as_str()))
            .collect();
        assert_eq!(ids, vec!["fff", "rrr", "ooo"], "folder, machine, inbox");
        assert!(matches!(&rows[2], Row::Folder { name, .. } if name == "work"));
        assert!(
            matches!(&rows[4], Row::Machine { name, count: 1, .. } if name == "gpu"),
            "foldered remote not double-counted: {rows:?}"
        );
        assert_eq!(
            ids.iter().filter(|id| **id == "fff").count(),
            1,
            "foldered remote session appears exactly once"
        );
    }

    #[test]
    fn machine_collapse_hides_members_but_counts() {
        let mut st = VagState::default();
        let m1 = meta(AgentKind::Claude, "rrr", "on the box");
        st.session_mut(&m1.key).remote = Some("gpu".into());
        let sessions = vec![m1];
        let mut collapsed = HashSet::new();
        collapsed.insert(machine_collapse_key("gpu"));
        let rows = build_rows(
            &st,
            &sessions,
            &[],
            &machines(&[("gpu", "user@gpu.example")]),
            &collapsed,
            None,
            false,
            false,
            None,
        );
        assert!(rows.iter().all(|r| r.session_key().is_none()));
        assert!(rows.iter().any(|r| matches!(
            r,
            Row::Machine {
                count: 1,
                collapsed: true,
                ..
            }
        )));
        // The collapse key can never collide with a folder id or INBOX_ID.
        assert!(machine_collapse_key("inbox").starts_with('\u{0}'));
        assert_ne!(machine_collapse_key("inbox"), INBOX_ID);
    }

    #[test]
    fn machine_line_hints_keys_only_while_empty() {
        let l = machine_line(
            &Icons::ASCII,
            &Theme::TRANSPARENT,
            "gpu",
            "user@gpu",
            0,
            false,
            false,
        );
        assert_eq!(
            line_text(&l),
            "  ▾ @gpu (0)  user@gpu  (n: new session · s: shell)"
        );
        let l = machine_line(
            &Icons::ASCII,
            &Theme::TRANSPARENT,
            "gpu",
            "user@gpu",
            2,
            false,
            false,
        );
        assert_eq!(line_text(&l), "  ▾ @gpu (2)  user@gpu");
        // narrow (sidebar) mode drops the host; collapsed flips the arrow
        let l = machine_line(
            &Icons::ASCII,
            &Theme::TRANSPARENT,
            "gpu",
            "user@gpu",
            2,
            true,
            true,
        );
        assert_eq!(line_text(&l), "  ▸ @gpu (2)");
        // nerd remote glyph gets a breathing space like session rows
        let l = machine_line(
            &Icons::NERD,
            &Theme::TRANSPARENT,
            "gpu",
            "user@gpu",
            1,
            false,
            true,
        );
        assert_eq!(
            line_text(&l),
            format!(
                "  {} {} gpu (1)",
                Icons::NERD.folder_expanded,
                Icons::NERD.remote
            )
        );
    }

    #[test]
    fn provisional_labels_replace_the_starting_placeholder() {
        let mut st = VagState::default();
        let key = SessionKey::new(AgentKind::Shell, "shell-abc123");
        let resolved = SessionKey::new(AgentKind::Codex, "real-id");
        st.session_mut(&resolved).name_override = Some("named early".into());
        let badges = HashMap::new();
        let mut labels = HashMap::new();
        labels.insert(key.clone(), "shell @ gpu".to_string());
        let ctx = RowCtx {
            state: &st,
            sessions: &[],
            badges: &badges,
            now: Utc::now(),
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::TRANSPARENT,
        };
        let t = line_text(&session_line(&ctx, &key, None, 1, false, 80));
        assert!(t.contains("shell @ gpu"), "{t:?}");
        assert!(t.starts_with("    $ "), "shell glyph leads: {t:?}");
        // unlabelled provisional rows keep the historical placeholder
        let other = SessionKey::new(AgentKind::Codex, "pending-xyz");
        let t = line_text(&session_line(&ctx, &other, None, 1, false, 80));
        assert!(t.contains("(starting…)"), "{t:?}");
        let t = line_text(&session_line(&ctx, &resolved, None, 1, false, 80));
        assert!(t.contains("named early"), "{t:?}");
        let unnamed = SessionKey::new(AgentKind::Codex, "real-unnamed");
        let t = line_text(&session_line(&ctx, &unnamed, None, 1, false, 80));
        assert!(t.contains("codex session"), "{t:?}");
    }

    #[test]
    fn session_color_parsing() {
        assert_eq!(parse_session_color("red"), Some(Color::Red));
        assert_eq!(
            parse_session_color(" Pink "),
            Some(Color::Rgb(0xff, 0x87, 0xaf))
        );
        assert_eq!(
            parse_session_color("#1a2b3c"),
            Some(Color::Rgb(0x1a, 0x2b, 0x3c))
        );
        assert_eq!(parse_session_color("#12345"), None);
        assert_eq!(parse_session_color("chartreuse-ish"), None);
        // every palette entry must parse (picker renders swatches from it)
        for name in SESSION_PALETTE {
            assert!(parse_session_color(name).is_some(), "{name}");
        }
        // state lookup path
        let mut st = VagState::default();
        let k = SessionKey::new(AgentKind::Claude, "aaa");
        st.session_mut(&k).color = Some("blue".into());
        assert_eq!(session_color(&st, &k), Some(Color::Blue));
        st.session_mut(&k).color = Some("junk".into());
        assert_eq!(session_color(&st, &k), None);
    }

    #[test]
    fn rel_time_formats() {
        let now = Utc::now();
        assert_eq!(
            rel_time(Some(now - chrono::Duration::seconds(30)), now),
            "30s"
        );
        assert_eq!(
            rel_time(Some(now - chrono::Duration::minutes(5)), now),
            "5m"
        );
        assert_eq!(rel_time(Some(now - chrono::Duration::hours(3)), now), "3h");
        assert_eq!(rel_time(Some(now - chrono::Duration::days(2)), now), "2d");
        assert_eq!(rel_time(None, now), "");
    }

    #[test]
    fn editbuf_footer_reflects_mode() {
        use crate::ui::editbuf::EditLine;
        use crate::ui::input::Key;
        let mut buf = EditBuf::new(vec![EditLine {
            id: LineId::Session(SessionKey::new(AgentKind::Claude, "a")),
            text: "alpha".into(),
            depth: 0,
            readonly: false,
            copied: false,
        }]);
        let text = |l: &Line| -> String { l.spans.iter().map(|s| s.content.clone()).collect() };
        assert_eq!(
            text(&editbuf_footer_line(&buf)),
            " EDIT (:w save · :q quit)"
        );
        buf.handle_key(&Key::Char('i'));
        assert_eq!(text(&editbuf_footer_line(&buf)), " -- INSERT --");
        buf.handle_key(&Key::Esc);
        buf.handle_key(&Key::Char(':'));
        buf.handle_key(&Key::Char('w'));
        assert_eq!(text(&editbuf_footer_line(&buf)), " :w ");
    }

    #[test]
    fn badge_glyphs_follow_the_icon_set() {
        assert_eq!(Badge::DoneUnread.glyph(&Icons::ASCII), ("●", Color::Cyan));
        assert_eq!(Badge::Idle.glyph(&Icons::ASCII).0, "◌");
        assert_eq!(Badge::Exited.glyph(&Icons::ASCII).0, "✚");
        assert_eq!(Badge::External.glyph(&Icons::ASCII).0, "▲");
        assert_eq!(Badge::None.glyph(&Icons::NERD).0, "");
        assert_eq!(Badge::DoneUnread.glyph(&Icons::NERD).0, "\u{F0E0}");
        assert_eq!(Badge::Exited.glyph(&Icons::NERD).0, "\u{F068C}");
        assert_eq!(Badge::External.glyph(&Icons::NERD).0, "\u{F08E}");
        // Working animates via SPINNER in both sets; the fallback is fixed.
        assert_eq!(Badge::Working.glyph(&Icons::NERD).0, "●");
    }

    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.clone()).collect()
    }

    #[test]
    fn folder_line_uses_icon_arrows() {
        let l = folder_line(
            &Icons::ASCII,
            &Theme::TRANSPARENT,
            1,
            "work",
            true,
            2,
            None,
            false,
        );
        assert_eq!(line_text(&l), "    ▸ work (2)");
        let l = folder_line(
            &Icons::ASCII,
            &Theme::TRANSPARENT,
            0,
            "work",
            false,
            2,
            Some("/tmp"),
            false,
        );
        assert_eq!(line_text(&l), "  ▾ work (2)  ⇒ /tmp");
        let l = folder_line(
            &Icons::NERD,
            &Theme::TRANSPARENT,
            0,
            "work",
            true,
            1,
            None,
            true,
        );
        assert_eq!(line_text(&l), "  \u{F07B} work (1)");
        let l = folder_line(
            &Icons::NERD,
            &Theme::TRANSPARENT,
            0,
            "work",
            false,
            1,
            None,
            true,
        );
        assert_eq!(line_text(&l), "  \u{F07C} work (1)");
    }

    #[test]
    fn session_line_uses_icon_agent_marks() {
        let st = VagState::default();
        let sessions = vec![meta(AgentKind::Claude, "aaa", "one")];
        let badges = HashMap::new();
        let labels = HashMap::new();
        let now = Utc::now();
        let mut ctx = RowCtx {
            state: &st,
            sessions: &sessions,
            badges: &badges,
            now,
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::TRANSPARENT,
        };
        let l = session_line(&ctx, &sessions[0].key, Some(0), 1, false, 80);
        assert!(
            line_text(&l).starts_with("    ✳ one"),
            "ascii stays pixel-identical: {:?}",
            line_text(&l)
        );
        ctx.icons = &Icons::NERD;
        let l = session_line(&ctx, &sessions[0].key, Some(0), 1, false, 80);
        assert!(line_text(&l).starts_with("    \u{F0674} one"));
    }

    #[test]
    fn narrow_indicator_separates_glyph_from_duration() {
        let st = VagState::default();
        let sessions = vec![meta(AgentKind::Claude, "aaa", "one")];
        let mut badges = HashMap::new();
        badges.insert(
            sessions[0].key.clone(),
            BadgeInfo {
                kind: Badge::Working,
                dur: Some(std::time::Duration::from_secs(41)),
            },
        );
        let labels = HashMap::new();
        let ctx = RowCtx {
            state: &st,
            sessions: &sessions,
            badges: &badges,
            now: Utc::now(),
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::TRANSPARENT,
        };
        let l = session_line(&ctx, &sessions[0].key, Some(0), 0, true, 34);
        let text = line_text(&l);
        assert!(
            text.contains(&format!("{} 41s", SPINNER[0])),
            "glyph and duration need a gap: {text:?}"
        );
    }

    #[test]
    fn list_scrolls_while_pinned_settings_stays_visible() {
        // 15 sessions, a 10-line tree: the rows region gets 9 lines and
        // scrolls; the settings footer owns the 10th and never moves.
        let st = VagState::default();
        let sessions: Vec<SessionMeta> = (0..15)
            .map(|i| {
                meta(
                    AgentKind::Claude,
                    &format!("s{i:02}"),
                    &format!("sess{i:02}"),
                )
            })
            .collect();
        let badges = HashMap::new();
        let labels = HashMap::new();
        let ctx = RowCtx {
            state: &st,
            sessions: &sessions,
            badges: &badges,
            now: Utc::now(),
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::NIGHT,
        };
        let rows: Vec<Row> = sessions
            .iter()
            .enumerate()
            .map(|(i, m)| Row::Session {
                key: m.key.clone(),
                depth: 0,
                meta_idx: Some(i),
            })
            .collect();
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        // cursor one PAST the rows = the settings sentinel: list shows its
        // tail (the newest sessions stay reachable), settings line below.
        term.draw(|f| {
            let [list, set] = ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Min(1),
                ratatui::layout::Constraint::Length(1),
            ])
            .areas(Rect::new(0, 0, 40, 10));
            render_rows(f, list, &rows, rows.len(), &ctx, true, true);
            f.render_widget(
                Paragraph::new(settings_line(
                    &Icons::ASCII,
                    &Theme::NIGHT,
                    true,
                    set.width,
                    ',',
                )),
                set,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let line = |y: u16| {
            (0..40u16)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        // settings pinned on the last line, highlighted (sentinel selected)
        assert!(line(9).contains("⚙ settings"), "{}", line(9));
        assert_eq!(buf[(3, 9)].bg, Theme::NIGHT.sel);
        // the 9-line list scrolled to its tail: last session visible, the
        // first scrolled away — settings never consumed a list slot
        let visible: String = (0..9).map(line).collect();
        assert!(visible.contains("sess14"), "tail visible: {visible}");
        assert!(!visible.contains("sess00"), "head scrolled off: {visible}");
        // no in-list row claims the cursor bar (the sentinel is outside)
        for y in 0..9u16 {
            assert_ne!(buf[(39, y)].bg, Theme::NIGHT.sel, "line {y}");
        }
    }

    #[test]
    fn cursor_row_is_a_solid_bar_not_reversed() {
        // REVERSED flips colored spans (agent icon, quick number, badge)
        // into mismatched background patches; the highlight must be a plain
        // bg that keeps span foregrounds.
        let st = VagState::default();
        let sessions = vec![meta(AgentKind::Claude, "aaa", "one")];
        let badges = HashMap::new();
        let labels = HashMap::new();
        let ctx = RowCtx {
            state: &st,
            sessions: &sessions,
            badges: &badges,
            now: Utc::now(),
            active: None,
            open_order: &[],
            spin_frame: 0,
            icons: &Icons::ASCII,
            provisional_labels: &labels,
            theme: Theme::NIGHT,
        };
        let rows = vec![Row::Session {
            key: sessions[0].key.clone(),
            depth: 0,
            meta_idx: Some(0),
        }];
        let sel = Theme::NIGHT.sel;
        let backend = ratatui::backend::TestBackend::new(40, 3);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            render_rows(f, Rect::new(0, 0, 40, 3), &rows, 0, &ctx, true, true);
        })
        .unwrap();
        let buf = term.backend().buffer();
        for x in 0..40 {
            let cell = &buf[(x, 0)];
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "cell {x} reversed"
            );
            assert_eq!(cell.bg, sel, "cell {x} must sit on the selection bar");
        }
        // Spans keep their own colors: the agent icon stays yellow.
        let icon_x = (0..40)
            .find(|&x| buf[(x, 0)].symbol() == "✳")
            .expect("agent icon rendered");
        assert_eq!(buf[(icon_x, 0)].fg, Color::LightYellow);
    }

    #[test]
    fn work_dur_formats() {
        use std::time::Duration;
        assert_eq!(fmt_work_dur(Duration::from_secs(14)), "14s");
        assert_eq!(fmt_work_dur(Duration::from_secs(14 * 60 + 2)), "14m02s");
        assert_eq!(
            fmt_work_dur(Duration::from_secs(3 * 3600 + 12 * 60)),
            "3h12m"
        );
        assert_eq!(fmt_work_dur(Duration::from_secs(2 * 86_400)), "2d");
    }
}
