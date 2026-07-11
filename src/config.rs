//! User configuration: `$XDG_CONFIG_HOME/vag/config.toml` (default
//! `~/.config/vag/config.toml` on all Unix platforms — deliberately XDG even
//! on macOS, like lazygit's XDG mode).
//!
//! All sections and fields are optional in the TOML file; unknown keys are
//! ignored (serde default, no deny_unknown_fields) so configs survive
//! version skew in both directions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::types::AgentKind;

/// A ctrl+letter chord, stored as its control byte (0x01..=0x1a).
///
/// Restricted to ctrl+letter so the byte-oriented input path can scan for a
/// single byte. ctrl-i/j/m (tab/newline/enter aliases) are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetachKey(pub u8);

impl DetachKey {
    pub const DEFAULT: DetachKey = DetachKey(0x11); // ctrl-q

    /// Parse "ctrl-q" / "ctrl+q" / "C-q" (case-insensitive letter).
    pub fn parse(s: &str) -> Option<DetachKey> {
        let s = s.trim().to_ascii_lowercase();
        let letter = s
            .strip_prefix("ctrl-")
            .or_else(|| s.strip_prefix("ctrl+"))
            .or_else(|| s.strip_prefix("c-"))?;
        let mut chars = letter.chars();
        let c = chars.next()?;
        if chars.next().is_some() || !c.is_ascii_lowercase() {
            return None;
        }
        // Reject chords whose control byte collides with common keys.
        if matches!(c, 'i' | 'j' | 'm') {
            return None;
        }
        Some(DetachKey(c as u8 - b'a' + 1))
    }

    pub fn byte(&self) -> u8 {
        self.0
    }

    /// e.g. "ctrl-q"
    pub fn label(&self) -> String {
        format!("ctrl-{}", (b'a' + self.0 - 1) as char)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub keys: KeysConfig,
    pub agents: AgentsConfig,
    pub ui: UiConfig,
    pub behavior: BehaviorConfig,
    pub diff: DiffConfig,
    /// Per-key color overrides applied on top of `ui.theme` (palette names
    /// or "#rrggbb").
    pub theme: ThemeOverrides,
    /// SSH machines sessions can be created on ("cloud vs local"). Empty =
    /// the new-session flow skips the location step entirely.
    pub remotes: Vec<RemoteConfig>,
}

/// `[diff]` table: the per-session diff view.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    /// Render diff bodies through [delta](https://github.com/dandavison/delta)
    /// when it's on PATH — syntax highlighting and the user's own delta
    /// config for free. Missing binary or a failed run falls back to the
    /// builtin renderer silently; the file tree/scoping are vag's either way.
    pub use_delta: bool,
    /// Extra args appended to every delta invocation (e.g.
    /// ["--side-by-side"]). vag always passes --paging=never, --width and
    /// --file-style=omit; later args win, so these can override styling.
    pub delta_args: Vec<String>,
}

impl Default for DiffConfig {
    fn default() -> Self {
        DiffConfig {
            use_delta: true,
            delta_args: vec![],
        }
    }
}

/// `[theme]` table: any subset of keys, layered over the named base theme.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ThemeOverrides {
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub surface: Option<String>,
    pub sidebar_bg: Option<String>,
    pub sel: Option<String>,
    pub dim: Option<String>,
    pub accent: Option<String>,
    pub info: Option<String>,
    pub pane_fg: Option<String>,
    pub pane_bg: Option<String>,
}

/// One `[[remotes]]` entry.
///
/// ```toml
/// [[remotes]]
/// name = "gpu-box"            # shown in the UI
/// host = "user@10.0.0.5"      # anything `ssh` accepts (incl. config aliases)
/// default_dir = "~/work"       # optional: prefill for new sessions
/// # claude_command = "claude"  # optional: binary path on the remote
/// # codex_command = "codex"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemoteConfig {
    pub name: String,
    pub host: String,
    pub default_dir: Option<String>,
    /// Remote binary overrides; empty = the agent's default name.
    pub claude_command: String,
    pub codex_command: String,
}

impl RemoteConfig {
    pub fn command_for(&self, agent: AgentKind) -> String {
        let c = match agent {
            AgentKind::Claude => &self.claude_command,
            AgentKind::Codex => &self.codex_command,
            // Shell panes have no agent CLI — spawning is built by the UI
            // layer and never asks; a plain POSIX shell is the safe default.
            AgentKind::Shell => return "sh".to_string(),
        };
        if c.is_empty() {
            agent.label().to_string()
        } else {
            c.clone()
        }
    }
}

/// Every rebindable single-character tree command. Navigation (j/k/h/l,
/// arrows, space, enter, esc, `/`, 1..9) is fixed — those chars are
/// RESERVED and refused as bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Quit,
    Help,
    NewSession,
    NewFolder,
    Fork,
    EditMode,
    MoveSession,
    Rename,
    AddMachine,
    Shell,
    BindDir,
    Color,
    Hide,
    ShowHidden,
    Scope,
    Archive,
    Delete,
    CloseRuntime,
    Zoom,
    /// Toggle the active session's diff tab (agent PTY ⇄ git diff view).
    DiffView,
    /// Open the settings page (also reachable via the pinned ⚙ row).
    Settings,
}

impl KeyAction {
    pub const ALL: [KeyAction; 21] = [
        KeyAction::Quit,
        KeyAction::Help,
        KeyAction::NewSession,
        KeyAction::NewFolder,
        KeyAction::Fork,
        KeyAction::EditMode,
        KeyAction::MoveSession,
        KeyAction::Rename,
        KeyAction::AddMachine,
        KeyAction::Shell,
        KeyAction::BindDir,
        KeyAction::Color,
        KeyAction::Hide,
        KeyAction::ShowHidden,
        KeyAction::Scope,
        KeyAction::Archive,
        KeyAction::Delete,
        KeyAction::CloseRuntime,
        KeyAction::Zoom,
        KeyAction::DiffView,
        KeyAction::Settings,
    ];

    /// The TOML key under `[keys]`.
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::Quit => "quit",
            KeyAction::Help => "help",
            KeyAction::NewSession => "new_session",
            KeyAction::NewFolder => "new_folder",
            KeyAction::Fork => "fork",
            KeyAction::EditMode => "edit",
            KeyAction::MoveSession => "move",
            KeyAction::Rename => "rename",
            KeyAction::AddMachine => "add_machine",
            KeyAction::Shell => "shell",
            KeyAction::BindDir => "bind_dir",
            KeyAction::Color => "color",
            KeyAction::Hide => "hide",
            KeyAction::ShowHidden => "show_hidden",
            KeyAction::Scope => "scope",
            KeyAction::Archive => "archive",
            KeyAction::Delete => "delete",
            KeyAction::CloseRuntime => "close",
            KeyAction::Zoom => "zoom",
            KeyAction::DiffView => "diff",
            KeyAction::Settings => "settings",
        }
    }

    /// Human label for the settings page / help.
    pub fn title(self) -> &'static str {
        match self {
            KeyAction::Quit => "quit",
            KeyAction::Help => "help",
            KeyAction::NewSession => "new session",
            KeyAction::NewFolder => "new folder",
            KeyAction::Fork => "fork session",
            KeyAction::EditMode => "edit mode (vim tree)",
            KeyAction::MoveSession => "move to folder",
            KeyAction::Rename => "rename",
            KeyAction::AddMachine => "add machine (ssh)",
            KeyAction::Shell => "shell pane",
            KeyAction::BindDir => "bind folder dir",
            KeyAction::Color => "session color",
            KeyAction::Hide => "hide / unhide",
            KeyAction::ShowHidden => "show hidden",
            KeyAction::Scope => "repo scope toggle",
            KeyAction::Archive => "archive (codex)",
            KeyAction::Delete => "delete",
            KeyAction::CloseRuntime => "close process",
            KeyAction::Zoom => "zoom full-screen",
            KeyAction::DiffView => "diff view (agent ⇄ diff tab)",
            KeyAction::Settings => "open settings",
        }
    }

    pub fn default_char(self) -> char {
        match self {
            KeyAction::Quit => 'q',
            KeyAction::Help => '?',
            KeyAction::NewSession => 'n',
            KeyAction::NewFolder => 'N',
            KeyAction::Fork => 'F',
            KeyAction::EditMode => 'e',
            KeyAction::MoveSession => 'm',
            KeyAction::Rename => 'r',
            KeyAction::AddMachine => 'R',
            KeyAction::Shell => 's',
            KeyAction::BindDir => 'b',
            KeyAction::Color => 'c',
            KeyAction::Hide => 'd',
            KeyAction::ShowHidden => 'H',
            KeyAction::Scope => 'g',
            KeyAction::Archive => 'A',
            KeyAction::Delete => 'x',
            KeyAction::CloseRuntime => 'w',
            KeyAction::Zoom => 'z',
            KeyAction::DiffView => 'D',
            KeyAction::Settings => ',',
        }
    }

    fn by_name(name: &str) -> Option<KeyAction> {
        KeyAction::ALL.iter().copied().find(|a| a.name() == name)
    }

    fn index(self) -> usize {
        KeyAction::ALL.iter().position(|a| *a == self).unwrap()
    }
}

/// Every rebindable ctrl-chord command (a separate keyspace from the plain
/// single-char `KeyAction`s above — ctrl chords are reserved bytes scanned
/// out of the raw pty stream, not parsed `Key` chars).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlAction {
    Detach,
    ToggleSidebar,
    FocusTree,
    FocusPane,
    /// Flip the active session between its agent PTY and its diff view —
    /// the one diff key that must work while the pane has focus (plain
    /// chars are forwarded to the child there).
    ToggleDiff,
}

impl CtrlAction {
    pub const ALL: [CtrlAction; 5] = [
        CtrlAction::Detach,
        CtrlAction::ToggleSidebar,
        CtrlAction::FocusTree,
        CtrlAction::FocusPane,
        CtrlAction::ToggleDiff,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CtrlAction::Detach => "detach",
            CtrlAction::ToggleSidebar => "toggle_sidebar",
            CtrlAction::FocusTree => "focus_tree",
            CtrlAction::FocusPane => "focus_pane",
            CtrlAction::ToggleDiff => "toggle_diff",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            CtrlAction::Detach => "detach (pane -> tree)",
            CtrlAction::ToggleSidebar => "toggle sidebar (in pane)",
            CtrlAction::FocusTree => "focus tree (from pane)",
            CtrlAction::FocusPane => "focus active session (from tree)",
            CtrlAction::ToggleDiff => "toggle diff view (in pane)",
        }
    }

    /// Default chord for this action. Bytes are ASCII Ctrl-Q/Ctrl-E/Ctrl-H/
    /// Ctrl-L respectively, matching the `c as u8 - b'a' + 1` formula
    /// `DetachKey::parse` already uses (Ctrl-E = 0x05, Ctrl-H = 0x08,
    /// Ctrl-L = 0x0c).
    pub fn default_key(self) -> DetachKey {
        match self {
            CtrlAction::Detach => DetachKey::DEFAULT,
            CtrlAction::ToggleSidebar => DetachKey(0x05),
            CtrlAction::FocusTree => DetachKey(0x08),
            CtrlAction::FocusPane => DetachKey(0x0c),
            // Ctrl-G: free in both agents' composers and mnemonic for git.
            CtrlAction::ToggleDiff => DetachKey(0x07),
        }
    }

    fn by_name(name: &str) -> Option<CtrlAction> {
        CtrlAction::ALL.iter().copied().find(|a| a.name() == name)
    }
}

/// `[keys]` table: the detach chord plus one single-char binding per tree
/// command. Values are chars stored per action; unknown keys in the file
/// are ignored, invalid/colliding values warn and fall back (never a hard
/// error — a typo can't take the tool down).
#[derive(Debug, Clone)]
pub struct KeysConfig {
    /// The single reserved hotkey while a session pane has focus.
    pub detach: DetachKey,
    /// Toggle the sidebar's visibility while a session pane has focus
    /// (runtime-only view toggle; does not touch ui.tree).
    pub toggle_sidebar: DetachKey,
    /// Switch focus back to the tree while a session pane has focus (a
    /// plain alias, no double-press escape hatch).
    pub focus_tree: DetachKey,
    /// Switch focus to the active session's pane while the tree has focus.
    pub focus_pane: DetachKey,
    /// Flip the active session between agent PTY and diff view while the
    /// pane has focus.
    pub toggle_diff: DetachKey,
    chars: [char; KeyAction::ALL.len()],
}

impl KeysConfig {
    /// Chars the fixed navigation layer owns: never bindable.
    pub const RESERVED: [char; 6] = ['j', 'k', 'h', 'l', ' ', '/'];

    pub fn is_reserved(c: char) -> bool {
        Self::RESERVED.contains(&c) || c.is_ascii_digit()
    }

    /// A single printable, non-reserved char; None otherwise.
    pub fn parse_binding(s: &str) -> Option<char> {
        let mut it = s.chars();
        let c = it.next()?;
        if it.next().is_some() || c.is_control() || c.is_whitespace() || Self::is_reserved(c) {
            return None;
        }
        Some(c)
    }

    pub fn get(&self, a: KeyAction) -> char {
        self.chars[a.index()]
    }

    pub fn set(&mut self, a: KeyAction, c: char) {
        self.chars[a.index()] = c;
    }

    /// The action bound to `c` (first in ALL order on a collision — which
    /// sanitize() prevents for loaded configs).
    pub fn action_for(&self, c: char) -> Option<KeyAction> {
        KeyAction::ALL
            .iter()
            .copied()
            .find(|a| self.chars[a.index()] == c)
    }

    /// The binding `c` would collide with, excluding `a` itself.
    pub fn collision(&self, a: KeyAction, c: char) -> Option<KeyAction> {
        self.action_for(c).filter(|other| *other != a)
    }

    /// All four ctrl-chord bindings, in `CtrlAction::ALL` order.
    pub fn ctrl_bindings(&self) -> [(CtrlAction, DetachKey); CtrlAction::ALL.len()] {
        CtrlAction::ALL.map(|a| (a, self.get_ctrl(a)))
    }

    pub fn get_ctrl(&self, a: CtrlAction) -> DetachKey {
        match a {
            CtrlAction::Detach => self.detach,
            CtrlAction::ToggleSidebar => self.toggle_sidebar,
            CtrlAction::FocusTree => self.focus_tree,
            CtrlAction::FocusPane => self.focus_pane,
            CtrlAction::ToggleDiff => self.toggle_diff,
        }
    }

    pub fn set_ctrl(&mut self, a: CtrlAction, k: DetachKey) {
        match a {
            CtrlAction::Detach => self.detach = k,
            CtrlAction::ToggleSidebar => self.toggle_sidebar = k,
            CtrlAction::FocusTree => self.focus_tree = k,
            CtrlAction::FocusPane => self.focus_pane = k,
            CtrlAction::ToggleDiff => self.toggle_diff = k,
        }
    }

    /// The ctrl action `k` would collide with, excluding `a` itself.
    pub fn ctrl_collision(&self, a: CtrlAction, k: DetachKey) -> Option<CtrlAction> {
        self.ctrl_bindings()
            .into_iter()
            .find(|(other, bound)| *other != a && *bound == k)
            .map(|(other, _)| other)
    }

    /// Drop reserved/duplicate bindings back to their defaults (warn on
    /// stderr). Earlier actions in ALL order keep the contested char.
    fn sanitize(&mut self) {
        let mut seen: Vec<char> = Vec::new();
        for a in KeyAction::ALL {
            let c = self.get(a);
            if Self::is_reserved(c) || seen.contains(&c) {
                eprintln!(
                    "vag: keys.{} = `{c}` is {}; using `{}`",
                    a.name(),
                    if Self::is_reserved(c) {
                        "reserved for navigation"
                    } else {
                        "already bound"
                    },
                    a.default_char()
                );
                self.set(a, a.default_char());
            }
            seen.push(self.get(a));
        }
        let mut seen_ctrl: Vec<DetachKey> = Vec::new();
        for a in CtrlAction::ALL {
            let k = self.get_ctrl(a);
            if seen_ctrl.contains(&k) {
                let def = a.default_key();
                eprintln!(
                    "vag: keys.{} = {} is already bound; using {}",
                    a.name(),
                    k.label(),
                    def.label()
                );
                self.set_ctrl(a, def);
            }
            seen_ctrl.push(self.get_ctrl(a));
        }
    }
}

impl Serialize for KeysConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(CtrlAction::ALL.len() + KeyAction::ALL.len()))?;
        for a in CtrlAction::ALL {
            map.serialize_entry(a.name(), &self.get_ctrl(a).label())?;
        }
        for a in KeyAction::ALL {
            map.serialize_entry(a.name(), &self.get(a).to_string())?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for KeysConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = std::collections::BTreeMap::<String, String>::deserialize(d)?;
        let mut k = KeysConfig::default();
        for (key, val) in &raw {
            if let Some(a) = CtrlAction::by_name(key) {
                let bound = DetachKey::parse(val).unwrap_or_else(|| {
                    eprintln!(
                        "vag: invalid keys.{key} `{val}` (want ctrl-<letter>); using {}",
                        a.default_key().label()
                    );
                    a.default_key()
                });
                k.set_ctrl(a, bound);
            } else if let Some(a) = KeyAction::by_name(key) {
                match Self::parse_binding(val) {
                    Some(c) => k.set(a, c),
                    None => eprintln!(
                        "vag: invalid keys.{key} `{val}` (want one non-reserved char); using `{}`",
                        a.default_char()
                    ),
                }
            }
            // unknown keys: ignored (forward compat, like every section)
        }
        k.sanitize();
        Ok(k)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentsConfig {
    pub claude: AgentConfig,
    pub codex: AgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentConfig {
    /// Binary name or path. Empty string means "use the built-in default".
    pub command: String,
    /// Extra args appended to every spawn of this agent.
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub sidebar_width: u16,
    /// How the session tree appears while a session is open: a persistent
    /// sidebar, or an oil.nvim-style floating window toggled by the detach
    /// key.
    pub tree: TreeMode,
    /// Icon set: `ascii` (universal glyphs, default), `nerd` (Nerd Font
    /// glyphs), or `auto` (nerd when the terminal likely has one).
    /// Overridable per run: `vag --icons nerd` or VAG_ICONS=nerd.
    pub icons: IconMode,
    /// Session pane chrome: `titlebar` (borderless, tmux-style full-width
    /// title bar on the top line — the default) or `border` (bordered box).
    pub pane: PaneStyle,
    /// Start the tree in nvim edit mode by default.
    pub edit_default: bool,
    /// Color theme: "night" (solid dark, the default), "mocha", "gruvbox",
    /// or "transparent" (no background — the terminal shows through).
    /// Fine-tune any key via the [theme] table. Per run: --theme/VAG_THEME.
    pub theme: String,
    /// Mouse support: wheel scrolls the pane's scrollback (or the tree),
    /// clicks focus the pane; children that enable mouse reporting get the
    /// events forwarded. Costs native drag-selection (use Shift+drag).
    pub mouse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PaneStyle {
    Border,
    #[default]
    Titlebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconMode {
    Nerd,
    #[default]
    Ascii,
    Auto,
}

impl IconMode {
    /// Resolve Auto against the environment: modern terminals that commonly
    /// ship with Nerd-Font-patched setups, or an explicit NERD_FONT env.
    pub fn use_nerd(self) -> bool {
        match self {
            IconMode::Nerd => true,
            IconMode::Ascii => false,
            IconMode::Auto => {
                if std::env::var_os("NERD_FONT").is_some()
                    || std::env::var_os("USE_NERD_FONT").is_some()
                {
                    return true;
                }
                let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
                matches!(
                    tp.as_str(),
                    "WezTerm" | "kitty" | "ghostty" | "Alacritty" | "iTerm.app"
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TreeMode {
    #[default]
    Sidebar,
    Float,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Scope the tree to the current git repo by default when vag is
    /// launched inside one (g toggles at runtime).
    pub repo_scope: bool,
    pub show_hidden: bool,
    /// Show codex threads with thread_source == 'automation' etc.
    pub codex_show_automation: bool,
    /// Overrides for the agents' data directories (rarely needed; the env
    /// vars CLAUDE_CONFIG_DIR / CODEX_HOME are respected without config).
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        BehaviorConfig {
            repo_scope: true,
            show_hidden: false,
            codex_show_automation: false,
            claude_config_dir: None,
            codex_home: None,
        }
    }
}

impl Default for KeysConfig {
    fn default() -> Self {
        let mut chars = [' '; KeyAction::ALL.len()];
        for a in KeyAction::ALL {
            chars[a.index()] = a.default_char();
        }
        KeysConfig {
            detach: CtrlAction::Detach.default_key(),
            toggle_sidebar: CtrlAction::ToggleSidebar.default_key(),
            focus_tree: CtrlAction::FocusTree.default_key(),
            focus_pane: CtrlAction::FocusPane.default_key(),
            toggle_diff: CtrlAction::ToggleDiff.default_key(),
            chars,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            sidebar_width: 34,
            tree: TreeMode::Sidebar,
            icons: IconMode::Ascii,
            pane: PaneStyle::Titlebar,
            edit_default: false,
            theme: "night".into(),
            mouse: true,
        }
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg_dir(env_var: &str, home_fallback: &str) -> PathBuf {
    match std::env::var_os(env_var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir().join(home_fallback),
    }
}

impl Config {
    /// Load from config_path(). Missing file → defaults. Malformed TOML → Err.
    pub fn load() -> Result<Config> {
        let path = Self::config_path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("malformed config at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading config at {}", path.display()));
            }
        };
        // Malformed remotes are dropped loudly rather than crashing the UI
        // later ("seamless": a bad entry can't take the whole tool down).
        cfg.remotes.retain(|r| {
            let ok = !r.name.trim().is_empty() && !r.host.trim().is_empty();
            if !ok {
                eprintln!("vag: ignoring [[remotes]] entry with empty name/host");
            }
            ok
        });
        // Per-run overrides (also how `vag --icons/--tree/--edit` reach
        // the TUI; usable directly as env vars too).
        if let Ok(v) = std::env::var("VAG_ICONS") {
            match v.to_ascii_lowercase().as_str() {
                "nerd" => cfg.ui.icons = IconMode::Nerd,
                "ascii" => cfg.ui.icons = IconMode::Ascii,
                "auto" => cfg.ui.icons = IconMode::Auto,
                other => eprintln!("vag: ignoring invalid VAG_ICONS `{other}`"),
            }
        }
        if let Ok(v) = std::env::var("VAG_TREE") {
            match v.to_ascii_lowercase().as_str() {
                "sidebar" => cfg.ui.tree = TreeMode::Sidebar,
                "float" => cfg.ui.tree = TreeMode::Float,
                other => eprintln!("vag: ignoring invalid VAG_TREE `{other}`"),
            }
        }
        if let Ok(v) = std::env::var("VAG_THEME") {
            cfg.ui.theme = v;
        }
        if let Ok(v) = std::env::var("VAG_PANE") {
            match v.to_ascii_lowercase().as_str() {
                "border" => cfg.ui.pane = PaneStyle::Border,
                "titlebar" => cfg.ui.pane = PaneStyle::Titlebar,
                other => eprintln!("vag: ignoring invalid VAG_PANE `{other}`"),
            }
        }
        if let Ok(v) = std::env::var("VAG_EDIT") {
            match v.as_str() {
                "1" | "true" => cfg.ui.edit_default = true,
                "0" | "false" => cfg.ui.edit_default = false,
                other => eprintln!("vag: ignoring invalid VAG_EDIT `{other}`"),
            }
        }
        Ok(cfg)
    }

    pub fn config_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("vag")
            .join("config.toml")
    }

    /// vag's own data dir (state.json lives here).
    pub fn data_dir() -> PathBuf {
        xdg_dir("XDG_DATA_HOME", ".local/share").join("vag")
    }

    /// Claude Code data root: behavior.claude_config_dir > $CLAUDE_CONFIG_DIR > ~/.claude
    pub fn claude_dir(&self) -> PathBuf {
        if let Some(d) = &self.behavior.claude_config_dir {
            return d.clone();
        }
        match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home_dir().join(".claude"),
        }
    }

    /// Codex data root: behavior.codex_home > $CODEX_HOME > ~/.codex
    pub fn codex_home(&self) -> PathBuf {
        if let Some(d) = &self.behavior.codex_home {
            return d.clone();
        }
        match std::env::var_os("CODEX_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home_dir().join(".codex"),
        }
    }

    pub fn remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }

    /// Resolved command + standing extra args for an agent.
    pub fn command_for(&self, agent: AgentKind) -> (String, Vec<String>) {
        let ac = match agent {
            AgentKind::Claude => &self.agents.claude,
            AgentKind::Codex => &self.agents.codex,
            // Shell panes have no agent CLI — spawning is built by the UI
            // layer and never asks; a plain POSIX shell is the safe default.
            AgentKind::Shell => return ("sh".to_string(), Vec::new()),
        };
        let cmd = if ac.command.is_empty() {
            agent.label().to_string()
        } else {
            ac.command.clone()
        };
        (cmd, ac.extra_args.clone())
    }
}

// ---------------------------------------------------------------------------
// Config-file editing (`vag remote add/remove` and the in-app add-remote
// flow). Edits go through toml_edit so the user's comments, ordering and
// formatting elsewhere in the file survive byte-identically; writes are
// atomic (tmp + rename, same dir — the state.rs pattern).

/// Append a `[[remotes]]` entry to the config file at `path`, creating the
/// file (and parent dirs) when missing. Errors on an empty name/host or a
/// duplicate name. Optional fields are written only when set: `default_dir`
/// when Some, the `*_command` overrides when non-empty.
pub fn add_remote_to_file(path: &Path, r: &RemoteConfig) -> Result<()> {
    let name = r.name.trim();
    let host = r.host.trim();
    if name.is_empty() {
        bail!("remote name must not be empty");
    }
    if host.is_empty() {
        bail!("remote host must not be empty");
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading config at {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("malformed config at {}", path.display()))?;
    let dup = doc
        .get("remotes")
        .and_then(|i| i.as_array_of_tables())
        .is_some_and(|aot| {
            aot.iter()
                .any(|t| t.get("name").and_then(|i| i.as_str()) == Some(name))
        });
    if dup {
        bail!(
            "remote `{name}` already exists in {} — pick another name or run: vag remote remove {name}",
            path.display()
        );
    }
    let mut t = toml_edit::Table::new();
    t["name"] = toml_edit::value(name);
    t["host"] = toml_edit::value(host);
    if let Some(d) = &r.default_dir {
        t["default_dir"] = toml_edit::value(d.as_str());
    }
    if !r.claude_command.is_empty() {
        t["claude_command"] = toml_edit::value(r.claude_command.as_str());
    }
    if !r.codex_command.is_empty() {
        t["codex_command"] = toml_edit::value(r.codex_command.as_str());
    }
    doc.entry("remotes")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ))
        .as_array_of_tables_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`remotes` in {} is not an array of tables ([[remotes]])",
                path.display()
            )
        })?
        .push(t);
    write_atomic(path, doc.to_string().as_bytes())
}

/// Remove every `[[remotes]]` entry named `name` from the config file.
/// Ok(false) when the file, the remotes array, or a matching entry doesn't
/// exist (nothing is written); everything else stays byte-identical.
pub fn remove_remote_from_file(path: &Path, name: &str) -> Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading config at {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("malformed config at {}", path.display()))?;
    let Some(aot) = doc
        .get_mut("remotes")
        .and_then(|i| i.as_array_of_tables_mut())
    else {
        return Ok(false);
    };
    let before = aot.len();
    aot.retain(|t| t.get("name").and_then(|i| i.as_str()) != Some(name));
    if aot.len() == before {
        return Ok(false);
    }
    write_atomic(path, doc.to_string().as_bytes())?;
    Ok(true)
}

/// Set `[section] key = value` in the config file (the in-app settings
/// page's persistence). Creates the file and/or section when missing;
/// everything else — comments, ordering, formatting — survives untouched.
pub fn set_config_item<V: Into<toml_edit::Value>>(
    path: &Path,
    section: &str,
    key: &str,
    value: V,
) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading config at {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("malformed config at {}", path.display()))?;
    let tbl = doc
        .entry(section)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`{section}` in {} is not a table", path.display()))?;
    tbl[key] = toml_edit::value(value);
    write_atomic(path, doc.to_string().as_bytes())
}

/// Atomic write (tmp + rename, same dir), creating parent dirs — mirrors
/// state.rs so a crash can never truncate the user's config.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config path has no file name: {}", path.display()))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let write_and_rename = (|| -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_and_rename.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_and_rename.with_context(|| format!("writing config to {}", path.display()))
}

/// Host aliases from `~/.ssh/config`, offered as suggestions when adding a
/// remote. Glob patterns (`*`/`?`) and negations are skipped; the list is
/// deduped and sorted. Missing/unreadable file → empty.
#[allow(dead_code)] // consumed by the in-app add-remote flow
pub fn ssh_config_aliases() -> Vec<String> {
    ssh_config_aliases_from(&home_dir().join(".ssh").join("config"))
}

pub(crate) fn ssh_config_aliases_from(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // ssh_config accepts both `Keyword args` and `Keyword=args`;
        // keywords are case-insensitive.
        let line = line.replacen('=', " ", 1);
        let mut tokens = line.split_whitespace();
        if !tokens
            .next()
            .is_some_and(|k| k.eq_ignore_ascii_case("host"))
        {
            continue;
        }
        for alias in tokens {
            // Patterns aren't connectable names.
            if alias.contains(['*', '?']) || alias.starts_with('!') {
                continue;
            }
            out.push(alias.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_config_defaults_lenient_parse_and_sanitize() {
        // defaults: every action maps both ways
        let k = KeysConfig::default();
        assert_eq!(k.get(KeyAction::NewSession), 'n');
        assert_eq!(k.get(KeyAction::Settings), ',');
        assert_eq!(k.action_for(','), Some(KeyAction::Settings));
        assert_eq!(k.action_for('F'), Some(KeyAction::Fork));
        assert_eq!(k.action_for('l'), None, "navigation chars stay unmapped");

        // custom bindings parse; invalid/reserved/colliding fall back
        let cfg: Config = toml::from_str(
            r#"
            [keys]
            detach = "ctrl-a"
            new_session = "o"
            fork = "j"       # reserved → default F
            hide = "o"       # collides with new_session → default d
            zoom = "zz"      # not one char → default z
            future_key = "y" # unknown → ignored
            "#,
        )
        .unwrap();
        assert_eq!(cfg.keys.detach.label(), "ctrl-a");
        assert_eq!(cfg.keys.get(KeyAction::NewSession), 'o');
        assert_eq!(cfg.keys.get(KeyAction::Fork), 'F');
        assert_eq!(cfg.keys.get(KeyAction::Hide), 'd');
        assert_eq!(cfg.keys.get(KeyAction::Zoom), 'z');
        assert_eq!(cfg.keys.action_for('o'), Some(KeyAction::NewSession));
        assert_eq!(cfg.keys.action_for('n'), None, "rebound char released");

        // collision detection excludes the action being rebound
        let mut k = KeysConfig::default();
        assert_eq!(
            k.collision(KeyAction::Fork, 'n'),
            Some(KeyAction::NewSession)
        );
        assert_eq!(k.collision(KeyAction::Fork, 'F'), None);
        k.set(KeyAction::Fork, 'f');
        assert_eq!(k.action_for('f'), Some(KeyAction::Fork));

        // binding parser: printable single non-reserved chars only
        assert_eq!(KeysConfig::parse_binding("f"), Some('f'));
        assert_eq!(KeysConfig::parse_binding("?"), Some('?'));
        assert_eq!(KeysConfig::parse_binding("j"), None);
        assert_eq!(KeysConfig::parse_binding("5"), None);
        assert_eq!(KeysConfig::parse_binding(" "), None);
        assert_eq!(KeysConfig::parse_binding("ab"), None);
    }

    #[test]
    fn ctrl_action_defaults_and_collision_sanitize() {
        // defaults: each of the 4 ctrl chords matches CtrlAction::default_key
        let k = KeysConfig::default();
        for a in CtrlAction::ALL {
            assert_eq!(k.get_ctrl(a), a.default_key());
        }
        assert_eq!(k.get_ctrl(CtrlAction::Detach).label(), "ctrl-q");
        assert_eq!(k.get_ctrl(CtrlAction::ToggleSidebar).label(), "ctrl-e");
        assert_eq!(k.get_ctrl(CtrlAction::FocusTree).label(), "ctrl-h");
        assert_eq!(k.get_ctrl(CtrlAction::FocusPane).label(), "ctrl-l");

        // custom bindings parse; a deliberate collision with detach's
        // default falls back to focus_tree's own default (later in ALL
        // order keeps the fallback, mirroring the char-collision sanitize).
        let cfg: Config = toml::from_str(
            r#"
            [keys]
            toggle_sidebar = "ctrl-e"
            focus_tree = "ctrl-q"
            focus_pane = "ctrl-l"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.keys.get_ctrl(CtrlAction::ToggleSidebar).label(),
            "ctrl-e"
        );
        assert_eq!(
            cfg.keys.get_ctrl(CtrlAction::FocusTree),
            CtrlAction::FocusTree.default_key(),
            "colliding with detach's default falls back to its own default"
        );
        assert_eq!(cfg.keys.get_ctrl(CtrlAction::FocusPane).label(), "ctrl-l");
        assert_eq!(cfg.keys.detach, DetachKey::DEFAULT);

        // collision detection excludes the action being rebound
        let k = KeysConfig::default();
        assert_eq!(
            k.ctrl_collision(CtrlAction::FocusTree, DetachKey::DEFAULT),
            Some(CtrlAction::Detach)
        );
        assert_eq!(
            k.ctrl_collision(CtrlAction::Detach, DetachKey::DEFAULT),
            None
        );
    }

    #[test]
    fn set_config_item_creates_and_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // creates file + section from nothing
        set_config_item(&path, "ui", "theme", "gruvbox").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[ui]") && text.contains("theme = \"gruvbox\""));

        // comments on OTHER lines, unrelated sections and formatting
        // survive an update (only the replaced key's own decor is rebuilt)
        std::fs::write(
            &path,
            "# my config\n[ui]\ntheme = \"gruvbox\"\nsidebar_width = 40  # wide\n\n[keys]\ndetach = \"ctrl-a\"\n",
        )
        .unwrap();
        set_config_item(&path, "ui", "theme", "night").unwrap();
        set_config_item(&path, "keys", "fork", "f").unwrap();
        set_config_item(&path, "ui", "edit_default", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# my config\n"), "{text}");
        assert!(text.contains("theme = \"night\""), "{text}");
        assert!(text.contains("sidebar_width = 40  # wide"), "{text}");
        assert!(text.contains("detach = \"ctrl-a\""), "{text}");
        assert!(text.contains("fork = \"f\""), "{text}");
        assert!(text.contains("edit_default = true"), "{text}");
        // and the result round-trips through the real loader
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.ui.theme, "night");
        assert_eq!(cfg.ui.sidebar_width, 40);
        assert_eq!(cfg.keys.get(KeyAction::Fork), 'f');
        assert!(cfg.ui.edit_default);
    }

    #[test]
    fn detach_key_parsing() {
        assert_eq!(DetachKey::parse("ctrl-q"), Some(DetachKey(0x11)));
        assert_eq!(DetachKey::parse("Ctrl+A"), Some(DetachKey(0x01)));
        assert_eq!(DetachKey::parse("C-z"), Some(DetachKey(0x1a)));
        assert_eq!(DetachKey::parse("ctrl-m"), None); // enter alias
        assert_eq!(DetachKey::parse("ctrl-1"), None);
        assert_eq!(DetachKey::parse("q"), None);
        assert_eq!(DetachKey::parse("ctrl-qq"), None);
        assert_eq!(DetachKey(0x11).label(), "ctrl-q");
    }

    #[test]
    fn config_parses_partial_and_unknown() {
        let cfg: Config = toml::from_str(
            r#"
            [keys]
            detach = "ctrl-g"
            [agents.claude]
            command = "/opt/homebrew/bin/claude"
            [behavior]
            codex_show_automation = true
            future_unknown_key = "ignored"
            [future_section]
            x = 1
            "#,
        )
        .unwrap();
        assert_eq!(cfg.keys.detach, DetachKey(0x07));
        assert_eq!(cfg.agents.claude.command, "/opt/homebrew/bin/claude");
        assert!(cfg.agents.codex.command.is_empty());
        assert!(cfg.behavior.codex_show_automation);
        assert_eq!(cfg.ui.sidebar_width, 34);
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.keys.detach, DetachKey::DEFAULT);
        let (cmd, args) = cfg.command_for(AgentKind::Codex);
        assert_eq!(cmd, "codex");
        assert!(args.is_empty());
    }

    #[test]
    fn invalid_detach_falls_back() {
        let cfg: Config = toml::from_str("[keys]\ndetach = \"super-x\"\n").unwrap();
        assert_eq!(cfg.keys.detach, DetachKey::DEFAULT);
    }

    // --- remote file editing -------------------------------------------------

    fn tmp_config(dir: &tempfile::TempDir) -> PathBuf {
        // Parent dir intentionally missing: add must create it.
        dir.path().join("vag").join("config.toml")
    }

    fn gpu() -> RemoteConfig {
        RemoteConfig {
            name: "gpu".into(),
            host: "user@10.0.0.5".into(),
            default_dir: Some("~/work".into()),
            claude_command: String::new(),
            codex_command: String::new(),
        }
    }

    #[test]
    fn add_remote_creates_missing_file_and_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        add_remote_to_file(&path, &gpu()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.remotes.len(), 1);
        assert_eq!(cfg.remotes[0].name, "gpu");
        assert_eq!(cfg.remotes[0].host, "user@10.0.0.5");
        assert_eq!(cfg.remotes[0].default_dir.as_deref(), Some("~/work"));
        // Unset optionals are omitted from the file entirely.
        assert!(!text.contains("claude_command"), "{text}");
        assert!(!text.contains("codex_command"), "{text}");

        let mut full = gpu();
        full.name = "cpu".into();
        full.default_dir = None;
        full.claude_command = "/opt/claude".into();
        add_remote_to_file(&path, &full).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.remotes.len(), 2);
        assert_eq!(cfg.remotes[1].name, "cpu");
        assert_eq!(cfg.remotes[1].default_dir, None);
        assert_eq!(cfg.remotes[1].claude_command, "/opt/claude");
        let cpu_entry = text.split("[[remotes]]").nth(2).unwrap();
        assert!(!cpu_entry.contains("default_dir"), "{text}");
    }

    #[test]
    fn add_remote_preserves_existing_bytes_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        let original = "# my vag config — hands off, tools!\n\
             [keys]\n\
             detach   =   \"ctrl-g\"     # odd spacing, kept\n\
             \n\
               [ui]   # indented section header\n\
               sidebar_width = 40\n\
             \n\
             [[remotes]]\n\
             name = \"old\" # comment on the entry\n\
             host = \"user@old\"\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();

        add_remote_to_file(&path, &gpu()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(original),
            "existing bytes must be untouched:\n{text}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.remotes.len(), 2);
        assert_eq!(cfg.remotes[1].name, "gpu");
        assert_eq!(cfg.keys.detach, DetachKey::parse("ctrl-g").unwrap());
    }

    #[test]
    fn add_then_remove_roundtrips_byte_identically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        let original = "# comment up top\n\n[behavior]\nrepo_scope = false # trailing comment\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();

        add_remote_to_file(&path, &gpu()).unwrap();
        assert!(remove_remote_from_file(&path, "gpu").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn add_remote_duplicate_name_errs_and_leaves_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        add_remote_to_file(&path, &gpu()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let mut dup = gpu();
        dup.host = "other@host".into();
        let err = add_remote_to_file(&path, &dup).unwrap_err().to_string();
        assert!(
            err.contains("gpu") && err.contains("already exists"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn add_remote_rejects_empty_name_or_host() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        let mut r = gpu();
        r.name = "   ".into();
        assert!(add_remote_to_file(&path, &r).is_err());
        let mut r = gpu();
        r.host = String::new();
        assert!(add_remote_to_file(&path, &r).is_err());
        assert!(!path.exists(), "validation failures must not create files");
    }

    #[test]
    fn remove_remote_existing_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp_config(&tmp);
        let original = "# machines\n\n\
             [[remotes]]\n\
             name = \"a\"\n\
             host = \"user@a\"\n\
             \n\
             [[remotes]]\n\
             name = \"b\"\n\
             host = \"user@b\"\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();

        // Absent name: Ok(false), file byte-identical.
        assert!(!remove_remote_from_file(&path, "nope").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        // Existing: entry `b` disappears, everything else byte-identical.
        assert!(remove_remote_from_file(&path, "b").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.remotes.len(), 1);
        assert_eq!(cfg.remotes[0].name, "a");
        assert!(text.starts_with("# machines\n\n[[remotes]]\nname = \"a\"\nhost = \"user@a\"\n"));
        assert!(!text.contains("user@b"));

        // Missing file / missing remotes array: Ok(false).
        assert!(!remove_remote_from_file(&tmp.path().join("nothing.toml"), "a").unwrap());
        let bare = tmp.path().join("bare.toml");
        std::fs::write(&bare, "[ui]\nsidebar_width = 20\n").unwrap();
        assert!(!remove_remote_from_file(&bare, "a").unwrap());
    }

    #[test]
    fn ssh_config_aliases_parse_dedupe_sort_and_skip_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ssh_config");
        std::fs::write(
            &p,
            "# global stuff\n\
             Host github.com work-* release?box\n\
             \tHostName github.com\n\
             \tUser git\n\
             \n\
             Host dev prod dev\n\
             Host *\n\
             host lowercase\n\
             Host = eqform\n\
             Host !negated ok\n\
             Match host something\n",
        )
        .unwrap();
        assert_eq!(
            ssh_config_aliases_from(&p),
            ["dev", "eqform", "github.com", "lowercase", "ok", "prod"]
        );
        assert!(ssh_config_aliases_from(&tmp.path().join("missing")).is_empty());
    }
}
