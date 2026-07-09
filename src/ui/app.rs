//! Main application: terminal lifecycle, event loop, focus model, layouts,
//! and the wiring between discovery, state, actions and runtimes.
//!
//! Threading model: the app logic is single-threaded; helper threads are
//! dumb pumps feeding one crossbeam channel — stdin bytes, SIGWINCH, a
//! 100ms tick, per-runtime events, background scans and id-discovery.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::{Context, Result};
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::{cursor, event as ctevent, execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::actions::{self, PendingId};
use crate::config::{
    Config, CtrlAction, DetachKey, IconMode, KeyAction, PaneStyle, RemoteConfig, TreeMode,
};
use crate::discovery::{self, ScanResult, claude::RunningClaude};
use crate::runtime::{PaneSize, RuntimeEvent, SessionRuntime};
use crate::state::VagState;
use crate::types::{AgentKind, SessionKey, SessionMeta};
use crate::ui::activity::{Activity, Turn};
use crate::ui::dashboard::{self, Badge, BadgeInfo, INBOX_ID, Row, RowCtx, machine_collapse_key};
use crate::ui::editbuf::{EditAction, EditBuf, EditEvent, EditLine, LineId, Mode as EditMode};
use crate::ui::icons::Icons;
use crate::ui::input::{Key, Parser};
use crate::ui::pane;
use crate::ui::prompts::{
    BindTarget, Commit, DirPick, DirTarget, InputKind, LineEdit, LocationChoice, Modal, Outcome,
    SettingId, SettingRow,
};
use crate::ui::theme::Theme;

const NO_AGENTS_MSG: &str = "neither claude nor codex found — run `vag doctor`";

const DOUBLE_DETACH: Duration = Duration::from_millis(400);
const RESCAN_EVERY: Duration = Duration::from_secs(5);
const STATUS_TTL: Duration = Duration::from_secs(5);
const DISCOVER_DEADLINE: Duration = Duration::from_secs(20);
/// A scan that hasn't reported back after this long is presumed lost.
const SCAN_STUCK_AFTER: Duration = Duration::from_secs(60);

enum AppEvent {
    Stdin(Vec<u8>),
    Runtime(SessionKey, RuntimeEvent),
    ScanDone(ScanResult),
    IdResolved {
        provisional: SessionKey,
        resolved: Option<String>,
    },
    ArchiveDone {
        key: SessionKey,
        archived: bool,
        result: Result<(), String>,
    },
    DeleteDone {
        key: SessionKey,
        result: Result<(), String>,
    },
    /// A batch from the directory-picker's background walk. `id` pairs the
    /// batch with the walk that produced it — stale walks are dropped.
    DirScan {
        id: u64,
        dirs: Vec<String>,
        done: bool,
    },
    Resize,
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Pane,
}

/// Context for a spawned runtime whose real session id isn't known yet.
struct PendingCtx {
    folder: Option<String>,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Bracketed-paste tracker for the raw pane-input path.
///
/// Purely observational: every byte is still forwarded to the child
/// verbatim. Its single consumer is the detach-byte scan, which must not
/// fire inside a paste — otherwise the paste is truncated, the child is
/// wedged in paste mode, and the remainder executes as chrome commands.
/// `matched` carries partial-marker progress across stdin chunks (an 8 KiB
/// read can split a marker), so no byte buffering is needed.
#[derive(Debug, Default)]
struct PasteGate {
    in_paste: bool,
    matched: usize,
}

impl PasteGate {
    /// Advance over one input byte; returns true when the byte lies inside
    /// a bracketed paste (the detach byte must then be forwarded, not
    /// intercepted).
    fn feed(&mut self, b: u8) -> bool {
        let marker = if self.in_paste {
            PASTE_END
        } else {
            PASTE_START
        };
        if b == marker[self.matched] {
            self.matched += 1;
            if self.matched == marker.len() {
                self.in_paste = !self.in_paste;
                self.matched = 0;
                // The final marker byte is still paste framing.
                return true;
            }
        } else {
            // ESC is the only byte that can restart a marker match.
            self.matched = usize::from(b == marker[0]);
        }
        self.in_paste
    }

    fn reset(&mut self) {
        self.in_paste = false;
        self.matched = 0;
    }
}

/// Identity of a dashboard row, used to keep the cursor on the same row
/// across background rescans that reorder or insert rows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowAnchor {
    NewSession,
    Spacer,
    Session(SessionKey),
    Folder(String),
    Inbox,
    Machine(String),
    /// The empty-state row under a folder (anchored by that folder's id).
    Empty(Option<String>),
}

fn row_anchor(row: &Row) -> RowAnchor {
    match row {
        Row::NewSession => RowAnchor::NewSession,
        Row::Spacer => RowAnchor::Spacer,
        Row::Session { key, .. } => RowAnchor::Session(key.clone()),
        Row::Folder { id, .. } => RowAnchor::Folder(id.clone()),
        Row::Inbox { .. } => RowAnchor::Inbox,
        Row::Machine { name, .. } => RowAnchor::Machine(name.clone()),
        Row::Empty { folder, .. } => RowAnchor::Empty(folder.clone()),
    }
}

fn locate_row(rows: &[Row], anchor: &RowAnchor) -> Option<usize> {
    rows.iter().position(|r| row_anchor(r) == *anchor)
}

/// Escape sequences re-asserting the input-side terminal modes a zoomed
/// child enabled inside the headless emulator. The real terminal never saw
/// them (the reader thread only tees output while zoomed), so they must be
/// replayed on zoom entry; exit_zoom's reset string turns them all off.
fn zoom_mode_replay(mode: TermMode) -> Vec<u8> {
    const MODES: &[(TermMode, &[u8])] = &[
        (TermMode::BRACKETED_PASTE, b"\x1b[?2004h"),
        (TermMode::FOCUS_IN_OUT, b"\x1b[?1004h"),
        (TermMode::APP_CURSOR, b"\x1b[?1h"),
        (TermMode::MOUSE_REPORT_CLICK, b"\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, b"\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, b"\x1b[?1003h"),
        (TermMode::UTF8_MOUSE, b"\x1b[?1005h"),
        (TermMode::SGR_MOUSE, b"\x1b[?1006h"),
        (TermMode::ALTERNATE_SCROLL, b"\x1b[?1007h"),
    ];
    let mut out = Vec::new();
    for (bit, seq) in MODES {
        if mode.contains(*bit) {
            out.extend_from_slice(seq);
        }
    }
    out
}

type Backend = CrosstermBackend<std::io::Stdout>;

/// RAII terminal state guard — restores the terminal even on panic (a
/// panic hook is installed in `run`).
struct TermGuard;

impl TermGuard {
    fn setup() -> Result<TermGuard> {
        terminal::enable_raw_mode().context("enabling raw mode")?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            ctevent::EnableBracketedPaste,
            cursor::Hide
        )
        .context("terminal setup")?;
        Ok(TermGuard)
    }

    fn restore() {
        let _ = execute!(
            std::io::stdout(),
            ctevent::DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        TermGuard::restore();
    }
}

pub fn run() -> Result<()> {
    let cfg = Config::load()?;
    let state = VagState::load()?;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TermGuard::restore();
        default_hook(info);
    }));

    let _guard = TermGuard::setup()?;
    let mut term =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).context("creating terminal")?;

    let (tx, rx) = unbounded::<AppEvent>();
    spawn_stdin_pump(tx.clone());
    spawn_winch_pump(tx.clone());
    spawn_tick_pump(tx.clone());

    let mut app = App::new(cfg, state, tx);
    app.request_scan();
    app.refresh_external();

    let res = app.main_loop(&mut term, &rx);
    // Kill children before the guard restores the terminal so nothing
    // writes to the raw PTY-less screen afterwards.
    let save_res = app.shutdown();
    // A failed final save must be reported AFTER the guard restores the
    // terminal — the alternate screen would swallow the message.
    drop(_guard);
    if let Err(e) = save_res {
        eprintln!("vag: FAILED to save state: {e:#}");
    }
    res
}

fn spawn_stdin_pump(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(AppEvent::Stdin(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn spawn_winch_pump(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let mut signals = match signal_hook::iterator::Signals::new([libc::SIGWINCH]) {
            Ok(s) => s,
            Err(_) => return,
        };
        for _ in signals.forever() {
            if tx.send(AppEvent::Resize).is_err() {
                break;
            }
        }
    });
}

fn spawn_tick_pump(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

/// Canonical config-file spellings of the ui enums (match their serde
/// lowercase names — what Config::load parses back).
fn icon_mode_name(m: IconMode) -> &'static str {
    match m {
        IconMode::Ascii => "ascii",
        IconMode::Nerd => "nerd",
        IconMode::Auto => "auto",
    }
}

fn pane_style_name(p: PaneStyle) -> &'static str {
    match p {
        PaneStyle::Titlebar => "titlebar",
        PaneStyle::Border => "border",
    }
}

fn tree_mode_name(t: TreeMode) -> &'static str {
    match t {
        TreeMode::Sidebar => "sidebar",
        TreeMode::Float => "float",
    }
}

/// Push a theme's pane colors into the two process-wide slots: the pane
/// painter's default fg/bg and the emulator's OSC 10/11 answers. Called at
/// startup and again on every in-app theme switch (both are RwLocks).
fn apply_theme_globals(theme: &Theme) {
    match theme.pane {
        Some((fr, fgc, fb, br, bgc, bb)) => {
            crate::runtime::set_theme_colors(Some(((fr, fgc, fb), (br, bgc, bb))));
            pane::set_pane_colors(Some((Color::Rgb(fr, fgc, fb), Color::Rgb(br, bgc, bb))));
        }
        None => {
            crate::runtime::set_theme_colors(None);
            pane::set_pane_colors(None);
        }
    }
}

struct App {
    cfg: Config,
    state: VagState,
    tx: Sender<AppEvent>,

    /// Glyph set resolved once from cfg.ui.icons.
    icons: Icons,
    /// (claude, codex) CLI availability; probed at startup and re-probed
    /// whenever the PickAgent modal opens.
    agents_ok: (bool, bool),
    /// Persistent header warning while no agent CLI is installed (scan
    /// warnings are overwritten every rescan; this one must survive them).
    agent_notice: Option<String>,

    sessions: Vec<SessionMeta>,
    meta_idx: HashMap<SessionKey, usize>,
    warnings: Vec<String>,

    rows: Vec<Row>,
    cursor: usize,
    collapsed: HashSet<String>,
    /// Runtime toggle (H), seeded from cfg.behavior.show_hidden.
    show_hidden: bool,
    /// Resolved color theme (ui.theme + [theme] overrides).
    theme: Theme,
    /// Where vag was launched (tilde-shortened) — the sidebar header.
    launch_dir: String,
    /// Root of the git repo vag was launched in, if any.
    scope_root: Option<PathBuf>,
    /// Runtime toggle (g): only show sessions/folders under scope_root.
    /// Defaults on when launched inside a repo.
    scoped: bool,
    filter: Option<String>,
    filter_edit: Option<LineEdit>,

    runtimes: HashMap<SessionKey, SessionRuntime>,
    open_order: Vec<SessionKey>,
    active: Option<SessionKey>,
    exited: HashSet<SessionKey>,
    pending: HashMap<SessionKey, PendingCtx>,
    /// Display titles for provisional runtimes with no scan entry — today
    /// only ephemeral shell panes ("shell @ gpu" / "shell: proj"). Removed
    /// with the runtime; never persisted.
    provisional_labels: HashMap<SessionKey, String>,

    focus: Focus,
    /// Floating tree overlay (ui.tree = "float"): open ⇔ focus == Tree
    /// while a session is active. Focus stays the single source of input
    /// routing; this bool only tells the draw side to paint the overlay.
    tree_float: bool,
    /// Runtime-only sidebar visibility toggle (ctrl-e) while a session pane
    /// has focus. Never persisted — a view toggle, not a config change.
    sidebar_hidden: bool,
    /// Running inside tmux ($TMUX set): the focus keys forward to
    /// `tmux select-pane` at vag's edges (ctrl-h with the tree already
    /// focused = go LEFT out of vag; ctrl-l with nothing to focus = go
    /// RIGHT), the vim-tmux-navigator model.
    tmux_nav: bool,
    /// Edit mode (`e`): the tree as an editable oil.nvim-style buffer. While
    /// Some, all tree-focus input routes to it and it renders wherever the
    /// tree body renders (dashboard, sidebar or float).
    editbuf: Option<EditBuf>,
    zoomed: bool,
    modal: Option<Modal>,
    parser: Parser,
    last_detach: Option<Instant>,
    last_pane_rect: Option<Rect>,
    /// Bracketed-paste framing of the bytes forwarded to the active pane.
    pane_paste: PasteGate,
    /// codex archive/unarchive shell-outs currently running on workers.
    archive_in_flight: HashSet<SessionKey>,
    /// Current directory-picker walk; batches from older walks are dropped.
    dir_scan_id: u64,

    external_claude: HashMap<String, RunningClaude>,
    last_external: Instant,
    last_scan_req: Instant,
    scan_in_flight: bool,

    /// App start, for the busy-spinner animation frame.
    started: Instant,
    /// Last 1Hz repaint of the relative-time labels.
    last_time_paint: Instant,
    /// Turn tracker per open runtime (working / done-unread / idle).
    activity: HashMap<SessionKey, Activity>,
    /// Last user-input write per open runtime (echo-grace for the tracker).
    last_input_at: HashMap<SessionKey, Instant>,
    /// working-since per externally-running claude session id (transcript
    /// mtime freshness observed by refresh_external).
    ext_working: HashMap<String, Instant>,

    /// ui.edit_default: enter edit mode after the first successful scan
    /// (armed once at startup, disarmed on the first attempt).
    edit_default_pending: bool,

    status: Option<(String, Instant)>,
    dirty: bool,
    quit: bool,
}

impl App {
    fn new(cfg: Config, state: VagState, tx: Sender<AppEvent>) -> App {
        let show_hidden = cfg.behavior.show_hidden;
        let icons = Icons::for_mode(cfg.ui.icons);
        let theme = Theme::from_config(&cfg);
        // Prime the pane/emulator defaults BEFORE any runtime spawns so the
        // OSC 10/11 answers agents base their palettes on match the colors
        // the pane actually paints.
        apply_theme_globals(&theme);
        let agents_ok = Self::probe_agents(&cfg);
        let edit_default_pending = cfg.ui.edit_default;
        let cwd = std::env::current_dir().unwrap_or_default();
        let scope_root = detect_git_root(&cwd);
        let launch_dir = tilde_shorten(&cwd);
        // Honor behavior.repo_scope: launched inside a repo, the tree scopes
        // to it by default only when the config says so (g still toggles).
        let scoped = scope_root.is_some() && cfg.behavior.repo_scope;
        App {
            cfg,
            state,
            tx,
            icons,
            theme,
            launch_dir,
            agents_ok,
            agent_notice: (!agents_ok.0 && !agents_ok.1).then(|| NO_AGENTS_MSG.to_string()),
            sessions: vec![],
            meta_idx: HashMap::new(),
            warnings: vec![],
            rows: vec![],
            cursor: 0,
            collapsed: HashSet::new(),
            show_hidden,
            scoped,
            scope_root,
            filter: None,
            filter_edit: None,
            runtimes: HashMap::new(),
            open_order: vec![],
            active: None,
            exited: HashSet::new(),
            pending: HashMap::new(),
            provisional_labels: HashMap::new(),
            focus: Focus::Tree,
            tree_float: false,
            sidebar_hidden: false,
            tmux_nav: std::env::var_os("TMUX").is_some(),
            editbuf: None,
            zoomed: false,
            modal: None,
            parser: Parser::new(),
            last_detach: None,
            last_pane_rect: None,
            pane_paste: PasteGate::default(),
            archive_in_flight: HashSet::new(),
            dir_scan_id: 0,
            external_claude: HashMap::new(),
            last_external: Instant::now(),
            last_scan_req: Instant::now(),
            scan_in_flight: false,
            started: Instant::now(),
            last_time_paint: Instant::now(),
            activity: HashMap::new(),
            last_input_at: HashMap::new(),
            ext_working: HashMap::new(),
            edit_default_pending,
            status: None,
            dirty: true,
            quit: false,
        }
    }

    /// (claude, codex) CLI availability.
    fn probe_agents(cfg: &Config) -> (bool, bool) {
        (
            actions::check_agent_available(cfg, AgentKind::Claude).is_ok(),
            actions::check_agent_available(cfg, AgentKind::Codex).is_ok(),
        )
    }

    fn main_loop(&mut self, term: &mut Terminal<Backend>, rx: &Receiver<AppEvent>) -> Result<()> {
        self.draw(term)?;
        while !self.quit {
            let ev = match rx.recv() {
                Ok(ev) => ev,
                Err(_) => break,
            };
            self.handle(ev, term)?;
            // Drain whatever queued up so redraws coalesce.
            while let Ok(ev) = rx.try_recv() {
                self.handle(ev, term)?;
                if self.quit {
                    break;
                }
            }
            if self.dirty && !self.zoomed {
                self.draw(term)?;
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        // kill() can block a few seconds per child (SIGHUP grace, SIGKILL
        // escalation) — do them in parallel so quitting stays snappy.
        let handles: Vec<_> = self
            .runtimes
            .drain()
            .map(|(_, mut rt)| std::thread::spawn(move || rt.kill()))
            .collect();
        for h in handles {
            let _ = h.join();
        }
        self.state.save().context("saving state on exit")
    }

    // ---------- events ----------

    fn handle(&mut self, ev: AppEvent, term: &mut Terminal<Backend>) -> Result<()> {
        match ev {
            AppEvent::Stdin(bytes) => self.on_stdin(bytes, term)?,
            AppEvent::Runtime(key, rev) => self.on_runtime(key, rev, term)?,
            AppEvent::ScanDone(res) => self.on_scan_done(res),
            AppEvent::IdResolved {
                provisional,
                resolved,
            } => self.on_id_resolved(provisional, resolved),
            AppEvent::ArchiveDone {
                key,
                archived,
                result,
            } => self.on_archive_done(key, archived, result),
            AppEvent::DeleteDone { key, result } => self.on_delete_done(key, result),
            AppEvent::DirScan { id, dirs, done } => self.on_dir_scan(id, dirs, done),
            AppEvent::Resize => self.on_resize(term)?,
            AppEvent::Tick => self.on_tick(term)?,
        }
        Ok(())
    }

    fn on_stdin(&mut self, bytes: Vec<u8>, term: &mut Terminal<Backend>) -> Result<()> {
        if self.zoomed || (self.focus == Focus::Pane && self.modal.is_none()) {
            self.forward_with_detach(&bytes, term)?;
            return Ok(());
        }
        let keys = self.parser.feed(&bytes);
        for k in keys {
            self.on_key(k, term)?;
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    /// Forward raw bytes to the active runtime, intercepting the detach key.
    ///
    /// The detach byte is NOT intercepted inside a bracketed paste — pasted
    /// content must reach the child verbatim (a literal detach byte in a
    /// paste would otherwise truncate it and feed the rest to the chrome).
    /// A double press is completed by on_key's detach arm: the first press
    /// detaches here, the second arrives in tree focus and delivers the
    /// literal byte.
    fn forward_with_detach(&mut self, bytes: &[u8], term: &mut Terminal<Backend>) -> Result<()> {
        let Some(active) = self.active.clone() else {
            self.focus_tree();
            return Ok(());
        };
        let db = self.cfg.keys.detach.byte();
        // ctrl-e (toggle sidebar) and ctrl-h (focus tree) are reserved only
        // in NORMAL split-pane focus: zoom hands the whole terminal to the
        // child (no sidebar concept to steal these for), and toggling the
        // sidebar only means something in TreeMode::Sidebar. Unlike detach,
        // neither has a double-press escape hatch — matches every other
        // reserved chrome key in this app (detach is the sole exception).
        let sidebar_byte = (!self.zoomed && self.cfg.ui.tree == TreeMode::Sidebar)
            .then_some(self.cfg.keys.toggle_sidebar.byte());
        let focus_tree_byte = (!self.zoomed).then_some(self.cfg.keys.focus_tree.byte());
        for (i, &b) in bytes.iter().enumerate() {
            let in_paste = self.pane_paste.feed(b);
            if in_paste {
                continue;
            }
            if b == db {
                if i > 0 {
                    self.write_to(&active, &bytes[..i]);
                }
                self.last_detach = Some(Instant::now());
                self.detach(term)?;
                // detach() always lands in tree focus (and un-zooms), so
                // the remaining bytes are chrome input now.
                let rest = bytes[i + 1..].to_vec();
                if !rest.is_empty() {
                    self.on_stdin(rest, term)?;
                }
                return Ok(());
            }
            if Some(b) == sidebar_byte {
                if i > 0 {
                    self.write_to(&active, &bytes[..i]);
                }
                self.sidebar_hidden = !self.sidebar_hidden;
                // The raw-byte path never marks dirty on its own (on_key
                // does, but we're not in it): without this the layout
                // change only appears on the next unrelated repaint.
                self.dirty = true;
                let rest = bytes[i + 1..].to_vec();
                if !rest.is_empty() {
                    self.on_stdin(rest, term)?;
                }
                return Ok(());
            }
            if Some(b) == focus_tree_byte {
                if i > 0 {
                    self.write_to(&active, &bytes[..i]);
                }
                self.focus_tree();
                self.dirty = true; // same as detach(): repaint the focus move
                let rest = bytes[i + 1..].to_vec();
                if !rest.is_empty() {
                    self.on_stdin(rest, term)?;
                }
                return Ok(());
            }
        }
        if !bytes.is_empty() {
            self.write_to(&active, bytes);
        }
        Ok(())
    }

    fn write_to(&mut self, key: &SessionKey, bytes: &[u8]) {
        if let Some(rt) = self.runtimes.get(key) {
            rt.write_input_filtered(bytes);
            self.last_input_at.insert(key.clone(), Instant::now());
        }
    }

    /// Point the pane at a (possibly different) session; per-pane input
    /// state must not leak across children.
    fn set_active(&mut self, key: Option<SessionKey>) {
        if self.active != key {
            self.pane_paste.reset();
        }
        if key.is_none() {
            // No session behind the overlay: the full dashboard takes the
            // screen and any float state is stale.
            self.tree_float = false;
        }
        self.active = key;
    }

    /// Route input to the tree. In float mode with a session active there
    /// is no persistent sidebar, so tree focus means the float is open —
    /// the two must move together or input would go to an invisible tree.
    fn focus_tree(&mut self) {
        self.focus = Focus::Tree;
        self.tree_float = self.cfg.ui.tree == TreeMode::Float && self.active.is_some();
    }

    /// Route input to the pane; dismisses the float if it was open.
    fn focus_pane(&mut self) {
        self.focus = Focus::Pane;
        self.tree_float = false;
    }

    /// Hand a focus motion that runs off vag's edge to the surrounding tmux
    /// (vim-tmux-navigator model). Outside tmux this is a silent no-op.
    /// Fire-and-forget: never block the UI thread on the spawn.
    fn tmux_select_pane(&self, dir: &str) {
        if !self.tmux_nav {
            return;
        }
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", dir])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    fn detach(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        if self.zoomed {
            self.exit_zoom(term)?;
        }
        self.focus_tree();
        self.dirty = true;
        Ok(())
    }

    fn on_runtime(
        &mut self,
        key: SessionKey,
        ev: RuntimeEvent,
        _term: &mut Terminal<Backend>,
    ) -> Result<()> {
        match ev {
            RuntimeEvent::Wakeup => {
                // Ack first so the runtime can send the next coalesced wakeup.
                if let Some(rt) = self.runtimes.get(&key) {
                    rt.ack_wakeup();
                }
                if self.active.as_ref() == Some(&key) && !self.zoomed {
                    self.dirty = true;
                }
            }
            RuntimeEvent::Exited(status) => {
                self.exited.insert(key.clone());
                if self.zoomed && self.active.as_ref() == Some(&key) {
                    // Child gone while it owned the screen: reclaim it.
                    // (handled on next tick/draw via exit_zoom)
                    self.tx.send(AppEvent::Tick).ok();
                }
                let code = match status {
                    Some(0) | None => String::new(),
                    Some(c) => format!(" (code {c})"),
                };
                self.set_status(format!("{} exited{code}", key.agent.label()));
                self.request_scan();
                self.dirty = true;
            }
            RuntimeEvent::Title | RuntimeEvent::Bell => {
                self.dirty = true;
            }
        }
        Ok(())
    }

    fn on_scan_done(&mut self, res: ScanResult) {
        self.scan_in_flight = false;
        let scan_ok = !res.total_failure();
        // A scan where every backend failed carries no information — keep
        // the previous list rather than blanking the UI and mis-stamping
        // every organized session as missing.
        if !scan_ok && !self.sessions.is_empty() {
            self.warnings = res.warnings;
            self.dirty = true;
            return;
        }
        self.warnings = res.warnings;
        self.sessions = res.sessions;
        // Remote sessions live only in vag state (local scans can't see
        // them): graft synthesized metas in BEFORE the index and rows are
        // built, and re-sort so they interleave like local rows.
        let synth = synthesize_remote_metas(&self.state, &self.sessions);
        self.sessions.extend(synth);
        // Same "last message sent" order discovery::scan_all uses, with one
        // overlay the scan can't know: input typed into an OPEN pane counts
        // immediately (the store lags until the agent persists the prompt).
        let now = Utc::now();
        let key_of = |m: &SessionMeta| {
            let store = discovery::sort_ts(m);
            let live = self
                .last_input_at
                .get(&m.key)
                .filter(|_| self.runtimes.contains_key(&m.key))
                .map(|t| now - chrono::Duration::from_std(t.elapsed()).unwrap_or_default());
            store.max(live)
        };
        self.sessions
            .sort_by(|a, b| key_of(b).cmp(&key_of(a)).then_with(|| a.key.cmp(&b.key)));
        self.meta_idx = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, m)| (m.key.clone(), i))
            .collect();
        // Any pending runtime whose id got scanned is no longer provisional
        // (rescued by on_id_resolved normally; this is belt & braces).
        self.pending.retain(|k, _| !self.meta_idx.contains_key(k));
        let mut present: HashSet<String> = self
            .sessions
            .iter()
            .map(|m| m.key.to_key_string())
            .collect();
        // A backend that errored told us nothing about its sessions: treat
        // its state entries as still present so the 30-day gc clock doesn't
        // start on a transient failure.
        for agent in &res.failed_agents {
            let prefix = format!("{}:", agent.label());
            present.extend(
                self.state
                    .sessions
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .cloned(),
            );
        }
        self.state.gc_missing(&present, Utc::now());
        self.rebuild_rows();
        // ui.edit_default: drop into edit mode once the dashboard first has
        // rows from a successful scan. One-shot, and only when it would be
        // reachable by the `e` key right now (tree focus, no modal, not
        // already editing).
        if self.edit_default_pending && scan_ok {
            self.edit_default_pending = false;
            if !self.rows.is_empty()
                && self.editbuf.is_none()
                && self.modal.is_none()
                && self.focus == Focus::Tree
            {
                self.enter_edit_mode();
            }
        }
        self.dirty = true;
    }

    fn on_id_resolved(&mut self, provisional: SessionKey, resolved: Option<String>) {
        let Some(ctx) = self.pending.remove(&provisional) else {
            return;
        };
        match resolved {
            Some(id) => {
                let real = SessionKey::new(provisional.agent, id);
                if self.runtimes.contains_key(&real) {
                    // The discovered id belongs to a session that is
                    // already open: inserting would drop — and thereby
                    // kill — that live runtime. Treat as a failed
                    // discovery: the pane stays provisional and visible,
                    // and no state is written for the (likely mis-)
                    // resolved id.
                    self.pending.insert(provisional, ctx);
                    self.set_status(
                        "discovered session id collides with an open session — pane kept \
                         provisional"
                            .into(),
                    );
                } else {
                    if let Some(rt) = self.runtimes.remove(&provisional) {
                        self.runtimes.insert(real.clone(), rt);
                    }
                    if let Some(a) = self.activity.remove(&provisional) {
                        self.activity.insert(real.clone(), a);
                    }
                    if let Some(t) = self.last_input_at.remove(&provisional) {
                        self.last_input_at.insert(real.clone(), t);
                    }
                    for k in self.open_order.iter_mut() {
                        if *k == provisional {
                            *k = real.clone();
                        }
                    }
                    if self.exited.remove(&provisional) {
                        self.exited.insert(real.clone());
                    }
                    if self.active.as_ref() == Some(&provisional) {
                        self.active = Some(real.clone());
                    }
                    // A modal opened against the provisional key (e.g. the
                    // close-runtime confirm) must act on the re-keyed
                    // runtime when committed.
                    if let Some(modal) = self.modal.as_mut() {
                        modal.rekey_session(&provisional, &real);
                    }
                    if let Some(folder) = ctx.folder
                        && self.state.folder(&folder).is_some()
                    {
                        let _ = self.state.set_session_folder(&real, Some(&folder));
                    }
                    self.state.session_mut(&real).last_opened = Some(Utc::now());
                    self.persist();
                    self.request_scan();
                }
            }
            None => {
                // Keep the pending entry: the provisional row must stay
                // visible so the still-running child remains reachable
                // and closable.
                self.pending.insert(provisional, ctx);
                self.set_status("couldn't identify the new session id (pane still works)".into());
            }
        }
        self.rebuild_rows();
        self.dirty = true;
    }

    fn on_archive_done(&mut self, key: SessionKey, archived: bool, result: Result<(), String>) {
        self.archive_in_flight.remove(&key);
        match result {
            Ok(()) => {
                self.set_status(if archived {
                    "archived".into()
                } else {
                    "unarchived".into()
                });
                self.request_scan();
            }
            Err(e) => self.set_status(e),
        }
    }

    fn on_resize(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        if self.zoomed {
            if let Some(active) = &self.active
                && let Some(rt) = self.runtimes.get(active)
            {
                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                rt.resize(PaneSize { rows, cols });
            }
            return Ok(());
        }
        term.autoresize().ok();
        // pane runtime resize happens during draw (layout knows the rect)
        self.dirty = true;
        Ok(())
    }

    fn on_tick(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        if let Some(k) = self.parser.flush_pending_esc() {
            if !(self.zoomed || (self.focus == Focus::Pane && self.modal.is_none())) {
                self.on_key(k, term)?;
            } else if let Some(active) = self.active.clone() {
                // a held ESC belongs to the child in pane mode
                self.write_to(&active, &[0x1b]);
            }
        }
        if let Some((_, t)) = &self.status
            && t.elapsed() > STATUS_TTL
        {
            self.status = None;
            self.dirty = true;
        }
        // Reclaim the screen if the zoomed child died.
        if self.zoomed {
            let dead = self
                .active
                .as_ref()
                .and_then(|k| self.runtimes.get(k))
                .map(|rt| !rt.is_running())
                .unwrap_or(true);
            if dead {
                self.exit_zoom(term)?;
                self.set_status("agent exited — returned to vag".into());
            }
        }
        if self.last_external.elapsed() > RESCAN_EVERY {
            self.refresh_external();
            self.dirty = true;
        }
        if !self.scan_in_flight
            && self.last_scan_req.elapsed() > RESCAN_EVERY
            && self.modal.is_none()
            && !self.zoomed
        {
            self.request_scan();
        }
        // Feed the turn trackers; also drives the spinner/badge repaints
        // while a runtime is open.
        if !self.runtimes.is_empty() {
            let now = Instant::now();
            for (k, rt) in &self.runtimes {
                if !rt.is_running() {
                    continue;
                }
                let viewed = self.active.as_ref() == Some(k);
                self.activity.entry(k.clone()).or_default().observe(
                    now,
                    rt.last_output().elapsed(),
                    self.last_input_at.get(k).map(Instant::elapsed),
                    viewed,
                );
            }
            if !self.zoomed {
                self.dirty = true;
            }
        }
        // Relative-time labels tick at 1Hz even when nothing else changes —
        // repaints are diffed by ratatui, so this only touches changed cells.
        if !self.zoomed && self.last_time_paint.elapsed() >= Duration::from_secs(1) {
            self.last_time_paint = Instant::now();
            self.dirty = true;
        }
        // Watchdog: if the scan thread ever fails to report back (it
        // shouldn't — panics are caught — but a stuck flag would freeze
        // rescans forever), clear the flag and let the next tick retry.
        if self.scan_in_flight && self.last_scan_req.elapsed() > SCAN_STUCK_AFTER {
            self.scan_in_flight = false;
            self.set_status("session scan stalled — retrying".into());
        }
        Ok(())
    }

    fn refresh_external(&mut self) {
        self.last_external = Instant::now();
        self.external_claude = discovery::claude::running_sessions(&self.cfg)
            .into_iter()
            .map(|r| (r.session_id.clone(), r))
            .collect();
        // External working detection: a claude that's mid-command appends to
        // its transcript continuously, so a fresh mtime ≈ working. Sampled
        // at this function's 5s cadence; start time is the first fresh
        // observation after a stale one.
        const EXT_FRESH: Duration = Duration::from_secs(12);
        let now = Instant::now();
        let mut working: HashMap<String, Instant> = HashMap::new();
        for id in self.external_claude.keys() {
            // Skip sessions we host ourselves — the PTY tracker owns those.
            let key = SessionKey::new(AgentKind::Claude, id.clone());
            if self.runtimes.contains_key(&key) {
                continue;
            }
            let fresh = self
                .meta_idx
                .get(&key)
                .and_then(|i| self.sessions[*i].source_path.metadata().ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|age| age < EXT_FRESH)
                .unwrap_or(false);
            if fresh {
                let since = self.ext_working.get(id).copied().unwrap_or(now);
                working.insert(id.clone(), since);
            }
        }
        self.ext_working = working;
    }

    fn request_scan(&mut self) {
        if self.scan_in_flight {
            return;
        }
        self.scan_in_flight = true;
        self.last_scan_req = Instant::now();
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // A panicking backend must not wedge scan_in_flight forever or
            // blank the session list: report a total failure instead.
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                discovery::scan_all(&cfg)
            }))
            .unwrap_or_else(|_| ScanResult {
                sessions: vec![],
                warnings: vec!["session scan crashed — keeping previous list".into()],
                failed_agents: vec![AgentKind::Claude, AgentKind::Codex],
            });
            let _ = tx.send(AppEvent::ScanDone(res));
        });
    }

    // ---------- keys (tree / modal) ----------

    fn on_key(&mut self, key: Key, term: &mut Terminal<Backend>) -> Result<()> {
        self.dirty = true;

        let detach_ctrl = (b'a' + self.cfg.keys.detach.byte() - 1) as char;
        let focus_pane_ctrl = (b'a' + self.cfg.keys.focus_pane.byte() - 1) as char;
        let focus_tree_ctrl = (b'a' + self.cfg.keys.focus_tree.byte() - 1) as char;
        // Any other key invalidates the double-press window, so a stale
        // timestamp can never make a later detach press inject a literal
        // byte instead of detaching.
        if !matches!(key, Key::Ctrl(c) if c == detach_ctrl) {
            self.last_detach = None;
        }

        // modal capture
        if let Some(mut modal) = self.modal.take() {
            match modal.handle_key(&key) {
                Outcome::Pending => {
                    // The theme picker previews LIVE: whatever the cursor
                    // rests on is the theme you're looking at.
                    self.preview_picked_theme(&modal);
                    self.modal = Some(modal);
                }
                Outcome::Msg(s) => {
                    // The modal refused the key (e.g. picking an agent that
                    // isn't installed): keep it open, surface the reason.
                    self.modal = Some(modal);
                    self.set_status(s);
                }
                Outcome::Cancel => match modal {
                    // Esc in the theme picker: put the original theme back.
                    Modal::PickTheme {
                        original, from_idx, ..
                    } => {
                        let t = self.theme_for_name(&original);
                        self.apply_theme_live(t);
                        if from_idx != usize::MAX {
                            self.open_settings(from_idx);
                        }
                    }
                    // Esc in the key-capture overlay: back to the page.
                    Modal::CaptureKey { from_idx, .. } => self.open_settings(from_idx),
                    _ => {}
                },
                Outcome::Commit(c) => self.apply_commit(c, term)?,
            }
            return Ok(());
        }

        // Edit mode owns every remaining tree-focus key (including the
        // detach ctrl-key, which the buffer no-ops): leave it with `:q`.
        if self.editbuf.is_some() {
            return self.on_edit_key(key, term);
        }

        // live filter editing
        if let Some(mut edit) = self.filter_edit.take() {
            match key {
                Key::Esc => {
                    self.filter = None;
                    self.rebuild_rows();
                }
                Key::Enter => {
                    self.filter_edit = None;
                    if self
                        .filter
                        .as_deref()
                        .map(|f| f.is_empty())
                        .unwrap_or(false)
                    {
                        self.filter = None;
                        self.rebuild_rows();
                    }
                }
                k => {
                    if edit.handle(&k) {
                        self.filter = Some(edit.buf.clone());
                        self.filter_edit = Some(edit);
                        self.cursor = 0;
                        self.rebuild_rows();
                    } else {
                        self.filter_edit = Some(edit);
                    }
                }
            }
            return Ok(());
        }

        match key {
            Key::Ctrl(c) if c == detach_ctrl && self.active.is_some() => {
                let now = Instant::now();
                if self
                    .last_detach
                    .take()
                    .map(|t| now.duration_since(t) < DOUBLE_DETACH)
                    .unwrap_or(false)
                {
                    // Second press of a double-press: deliver the literal
                    // detach byte to the child (PLAN §3 focus model).
                    let active = self.active.clone().unwrap();
                    self.write_to(&active, &[self.cfg.keys.detach.byte()]);
                }
                self.focus_pane();
            }
            Key::Ctrl('c') => self.request_quit(),
            Key::Down | Key::Char('j') => self.move_cursor(1),
            Key::Up | Key::Char('k') => self.move_cursor(-1),
            Key::PageDown => {
                if self.active.is_some() {
                    self.scroll_active(-20);
                } else {
                    self.move_cursor(10);
                }
            }
            Key::PageUp => {
                if self.active.is_some() {
                    self.scroll_active(20);
                } else {
                    self.move_cursor(-10);
                }
            }
            Key::Home => self.cursor = 0,
            // End jumps PAST the last row: the pinned ⚙ settings footer.
            Key::End => self.cursor = self.rows.len(),
            Key::Enter => self.activate_row(term)?,
            Key::Char(' ') | Key::Char('h') | Key::Left => self.toggle_collapse(),
            Key::Char('l') | Key::Right | Key::Tab if self.active.is_some() => {
                self.focus_pane();
            }
            // ctrl-l alias: focus the ALREADY-active session's pane without
            // opening/activating whatever row the cursor happens to be on
            // (that's `enter`'s job, a different action entirely).
            Key::Ctrl(c) if c == focus_pane_ctrl && self.active.is_some() => {
                self.focus_pane();
            }
            // vim-tmux-navigator edges: nothing left/right of here INSIDE
            // vag, so hand the motion to tmux (no-ops outside tmux).
            // ctrl-l with no session to focus = keep going right.
            Key::Ctrl(c) if c == focus_pane_ctrl => self.tmux_select_pane("-R"),
            // ctrl-h while the tree is focused = keep going left.
            Key::Ctrl(c) if c == focus_tree_ctrl => self.tmux_select_pane("-L"),
            Key::Esc => self.on_esc_tree(),
            Key::Char('/') => {
                self.filter_edit = Some(LineEdit::with_text(self.filter.as_deref().unwrap_or("")));
                self.filter = Some(self.filter.clone().unwrap_or_default());
                self.cursor = 0;
                self.rebuild_rows();
            }
            Key::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if let Some(k) = self.open_order.get(idx).cloned() {
                    self.set_active(Some(k));
                    self.focus_pane();
                }
            }
            // Every remaining char routes through the (rebindable) key map.
            Key::Char(c) => {
                if let Some(a) = self.cfg.keys.action_for(c) {
                    self.run_action(a, term)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Execute a tree command by its action, however it's bound.
    fn run_action(&mut self, a: KeyAction, term: &mut Terminal<Backend>) -> Result<()> {
        match a {
            KeyAction::Quit => self.request_quit(),
            KeyAction::Help => self.modal = Some(Modal::Help),
            KeyAction::NewSession => self.start_new_session(),
            KeyAction::NewFolder => self.start_new_folder(),
            KeyAction::Fork => self.fork_under_cursor(term)?,
            KeyAction::EditMode => self.enter_edit_mode(),
            KeyAction::MoveSession => self.start_move_session(),
            KeyAction::Rename => self.start_rename(),
            KeyAction::AddMachine => self.start_add_machine(),
            KeyAction::Shell => self.start_shell(),
            KeyAction::BindDir => self.bind_dir_or_dashboard(),
            KeyAction::Color => self.start_set_color(),
            KeyAction::Hide => self.toggle_hidden(),
            KeyAction::ShowHidden => self.toggle_show_hidden(),
            KeyAction::Scope => self.toggle_scope(),
            KeyAction::Archive => self.start_archive(),
            KeyAction::Delete => self.start_delete_folder(),
            KeyAction::CloseRuntime => self.start_close_runtime(),
            KeyAction::Zoom => {
                if self.active.is_some() {
                    self.enter_zoom(term)?;
                } else if let Some(k) = self.session_under_cursor() {
                    self.open_session(&k, term)?;
                    if self.active.is_some() {
                        self.enter_zoom(term)?;
                    }
                }
            }
            KeyAction::Settings => self.open_settings(0),
        }
        Ok(())
    }

    /// Esc from tree focus: clear the filter first; then dismiss the float
    /// (float mode); then drop back to the full dashboard (sidebar mode —
    /// in float mode the dashboard is reached with `b` instead, so closing
    /// the float never skips past the pane).
    fn on_esc_tree(&mut self) {
        if self.filter.is_some() {
            self.filter = None;
            self.rebuild_rows();
        } else if self.tree_float {
            self.focus_pane();
        } else if self.active.is_some() {
            self.set_active(None); // back to full dashboard, runtimes live on
        }
    }

    /// `b` binds a default dir when the cursor is on a folder row (its only
    /// historical effect); on any other row it goes back to the full
    /// dashboard — the sole route there in float mode, where Esc is taken
    /// by float dismissal.
    fn bind_dir_or_dashboard(&mut self) {
        if self.folder_under_cursor().is_some() {
            self.start_bind_dir();
        } else if self.active.is_some() {
            self.set_active(None);
        }
    }

    fn request_quit(&mut self) {
        let live = self.runtimes.values().filter(|r| r.is_running()).count();
        if live > 0 {
            self.modal = Some(Modal::Confirm {
                msg: format!("{live} session(s) still running — quit and stop them?"),
                commit: Commit::Quit,
            });
        } else {
            self.quit = true;
        }
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        // rows.len() is one PAST the list: the pinned ⚙ settings footer.
        // j from the last row lands there; the list itself never scrolls
        // for it because it renders outside the row viewport.
        let max = self.rows.len() as i64;
        let dir: i64 = if delta >= 0 { 1 } else { -1 };
        let mut c = (self.cursor as i64 + delta).clamp(0, max);
        // Spacer rows are visual air, not stops: keep going in the same
        // direction (row 0 and the sentinel are never spacers, so this
        // always lands somewhere selectable).
        while (0..max).contains(&c) && matches!(self.rows[c as usize], Row::Spacer) {
            c += dir;
        }
        self.cursor = c.clamp(0, max) as usize;
    }

    /// Cursor parked on the pinned settings footer (one past the rows).
    fn settings_selected(&self) -> bool {
        self.cursor >= self.rows.len()
    }

    fn scroll_active(&mut self, delta: i32) {
        if let Some(active) = &self.active
            && let Some(rt) = self.runtimes.get(active)
        {
            rt.scroll_display(delta);
        }
    }

    fn session_under_cursor(&self) -> Option<SessionKey> {
        self.rows
            .get(self.cursor)
            .and_then(|r| r.session_key().cloned())
    }

    fn folder_under_cursor(&self) -> Option<String> {
        self.rows
            .get(self.cursor)
            .and_then(|r| r.folder_id().map(str::to_string))
    }

    fn machine_under_cursor(&self) -> Option<String> {
        self.rows
            .get(self.cursor)
            .and_then(|r| r.machine_name().map(str::to_string))
    }

    /// The folder context of the cursor row (folder itself, or the session's
    /// folder, or the Inbox → None).
    fn folder_context(&self) -> Option<String> {
        match self.rows.get(self.cursor) {
            Some(Row::Folder { id, .. }) => Some(id.clone()),
            Some(Row::Empty { folder, .. }) => folder.clone(),
            Some(Row::Session { key, .. }) => {
                self.state.session(key).and_then(|r| r.folder.clone())
            }
            _ => None,
        }
    }

    fn activate_row(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        if self.settings_selected() {
            self.open_settings(0);
            return Ok(());
        }
        match self.rows.get(self.cursor).cloned() {
            Some(Row::Spacer) => {}
            Some(Row::NewSession) => self.start_new_session(),
            Some(Row::Session { key, .. }) => self.open_session(&key, term)?,
            Some(Row::Folder { id, .. }) => {
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
                self.rebuild_rows();
            }
            Some(Row::Inbox { .. }) => {
                if !self.collapsed.remove(INBOX_ID) {
                    self.collapsed.insert(INBOX_ID.to_string());
                }
                self.rebuild_rows();
            }
            // Enter on a machine header = start a session THERE (that's the
            // discoverability); space/h still collapse the group.
            Some(Row::Machine { name, .. }) => self.start_new_session_on_machine(name),
            // Enter on an empty folder's placeholder = new session in it.
            Some(Row::Empty { .. }) => self.start_new_session(),
            None => {}
        }
        Ok(())
    }

    fn toggle_collapse(&mut self) {
        match self.rows.get(self.cursor) {
            Some(Row::Folder { id, .. }) => {
                let id = id.clone();
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
                self.rebuild_rows();
            }
            Some(Row::Inbox { .. }) => {
                if !self.collapsed.remove(INBOX_ID) {
                    self.collapsed.insert(INBOX_ID.to_string());
                }
                self.rebuild_rows();
            }
            Some(Row::Machine { name, .. }) => {
                let key = machine_collapse_key(name);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    // ---------- session lifecycle ----------

    fn open_session(&mut self, key: &SessionKey, term: &mut Terminal<Backend>) -> Result<()> {
        if self.runtimes.contains_key(key) {
            self.set_active(Some(key.clone()));
            self.focus_pane();
            return Ok(());
        }
        // Double-attach guard: an unforked second resume of a claude
        // session already running elsewhere interleaves both into one
        // transcript (PLAN §7). Re-read the registry so the check doesn't
        // trust a stale map.
        if key.agent == AgentKind::Claude {
            self.refresh_external();
            if self.external_claude.contains_key(&key.id) {
                self.modal = Some(Modal::Confirm {
                    msg: "This session is open in another terminal — resuming here will \
                          interleave both into one transcript. Open anyway? (consider F to \
                          fork instead)"
                        .into(),
                    commit: Commit::OpenAnyway { key: key.clone() },
                });
                return Ok(());
            }
        }
        self.open_session_unchecked(key, term)
    }

    /// The actual resume/spawn path — reached from open_session after its
    /// guards, or from the OpenAnyway confirm which bypasses them.
    fn open_session_unchecked(
        &mut self,
        key: &SessionKey,
        _term: &mut Terminal<Backend>,
    ) -> Result<()> {
        if self.runtimes.contains_key(key) {
            self.set_active(Some(key.clone()));
            self.focus_pane();
            return Ok(());
        }
        // Remote sessions resume over ssh straight from vag state — no
        // local meta, no local dir/CLI checks apply.
        if self.open_remote_session(key) {
            return Ok(());
        }
        let Some(meta) = self.meta_idx.get(key).map(|i| self.sessions[*i].clone()) else {
            self.set_status("session not found in scan".into());
            return Ok(());
        };
        if let Err(e) = actions::check_agent_available(&self.cfg, key.agent) {
            self.set_status(e);
            return Ok(());
        }
        match actions::resume_spec(&self.cfg, &meta) {
            Ok(spec) => {
                self.spawn_runtime(key.clone(), &spec, None);
                self.state.session_mut(key).last_opened = Some(Utc::now());
                self.persist();
            }
            Err(e) => self.set_status(format!("{e:#}")),
        }
        Ok(())
    }

    /// Resume/attach a session that lives on an ssh remote. Returns true
    /// when the key was a remote session (handled here, successfully or
    /// not); false hands back to the local resume path.
    fn open_remote_session(&mut self, key: &SessionKey) -> bool {
        let Some(rname) = self.state.session(key).and_then(|r| r.remote.clone()) else {
            return false;
        };
        let Some(rc) = self.cfg.remote(&rname).cloned() else {
            self.set_status(format!("remote {rname} no longer configured"));
            return true;
        };
        let cwd = self
            .state
            .session(key)
            .and_then(|r| r.remote_cwd.clone())
            .unwrap_or_else(|| "~".into());
        match actions::remote_resume_spec(&rc, key.agent, &key.id, &cwd) {
            Ok(spec) => {
                self.spawn_runtime(key.clone(), &spec, None);
                self.state.session_mut(key).last_opened = Some(Utc::now());
                self.persist();
            }
            // e.g. synthetic codex ids are attach-only by design.
            Err(e) => self.set_status(format!("{e:#}")),
        }
        true
    }

    fn spawn_runtime(
        &mut self,
        key: SessionKey,
        spec: &actions::SpawnSpec,
        pending: Option<PendingCtx>,
    ) {
        if self.runtimes.contains_key(&key) {
            // Never clobber (and thereby kill) a live runtime via
            // HashMap::insert — switch to it instead.
            self.set_active(Some(key));
            self.focus_pane();
            return;
        }
        let size = self.pane_size();
        let tx = self.tx.clone();
        let events = {
            let tx = tx.clone();
            let (s, r) = unbounded::<(SessionKey, RuntimeEvent)>();
            std::thread::spawn(move || {
                for (k, ev) in r.iter() {
                    if tx.send(AppEvent::Runtime(k, ev)).is_err() {
                        break;
                    }
                }
            });
            s
        };
        match SessionRuntime::spawn(key.clone(), spec, size, events) {
            Ok(rt) => {
                let child_pid = rt.child_pid();
                self.runtimes.insert(key.clone(), rt);
                self.activity.insert(key.clone(), Activity::default());
                self.last_input_at.remove(&key);
                self.exited.remove(&key);
                if !self.open_order.contains(&key) {
                    self.open_order.push(key.clone());
                }
                self.active = Some(key.clone());
                // fresh child: paste framing can't carry over
                self.pane_paste.reset();
                self.focus_pane();
                if let Some(ctx) = pending {
                    self.pending.insert(key.clone(), ctx);
                    self.spawn_discovery(key.clone(), child_pid, spec.cwd.clone());
                }
                self.rebuild_rows();
            }
            Err(e) => self.set_status(format!("spawn failed: {e:#}")),
        }
    }

    fn spawn_discovery(&self, provisional: SessionKey, child_pid: Option<u32>, cwd: PathBuf) {
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let agent = provisional.agent;
        // No slack subtraction here: the discovery fns apply MTIME_SLACK
        // themselves (subtracting on both sides doubled the window).
        let spawned_after = std::time::SystemTime::now();
        std::thread::spawn(move || {
            let resolved = match agent {
                AgentKind::Claude => child_pid.and_then(|pid| {
                    actions::discover_claude_session_id(&cfg, pid, spawned_after, DISCOVER_DEADLINE)
                }),
                AgentKind::Codex => {
                    actions::discover_codex_session_id(&cfg, &cwd, spawned_after, DISCOVER_DEADLINE)
                }
                // Shell panes have no store to discover ids from.
                AgentKind::Shell => None,
            };
            let _ = tx.send(AppEvent::IdResolved {
                provisional,
                resolved,
            });
        });
    }

    fn provisional_key(agent: AgentKind) -> SessionKey {
        let n = uuid::Uuid::new_v4().simple().to_string();
        SessionKey::new(agent, format!("pending-{}", &n[..12]))
    }

    fn fork_under_cursor(&mut self, _term: &mut Terminal<Backend>) -> Result<()> {
        let Some(key) = self.session_under_cursor() else {
            return Ok(());
        };
        match self.fork_session(&key, None) {
            Ok(()) => self.set_status("forked — new session starting".into()),
            Err(e) => self.set_status(e),
        }
        Ok(())
    }

    /// Fork `key` into a provisional runtime. `folder`: Some(target) pins
    /// the fork's folder (edit-mode ForkInto); None inherits the source's.
    fn fork_session(
        &mut self,
        key: &SessionKey,
        folder: Option<Option<String>>,
    ) -> std::result::Result<(), String> {
        if key.agent == AgentKind::Shell {
            return Err("shells are ephemeral — nothing to fork".into());
        }
        if key.id.starts_with("pending-") {
            return Err("session id not known yet — try again shortly".into());
        }
        if self
            .state
            .session(key)
            .and_then(|r| r.remote.as_ref())
            .is_some()
        {
            return Err("fork isn't supported on remote sessions yet".into());
        }
        let Some(meta) = self.meta_idx.get(key).map(|i| self.sessions[*i].clone()) else {
            return Err("session not found in scan".into());
        };
        actions::check_agent_available(&self.cfg, key.agent)?;
        let (spec, _pending) =
            actions::fork_spec(&self.cfg, &meta).map_err(|e| format!("{e:#}"))?;
        let folder =
            folder.unwrap_or_else(|| self.state.session(key).and_then(|r| r.folder.clone()));
        let prov = Self::provisional_key(key.agent);
        self.spawn_runtime(prov, &spec, Some(PendingCtx { folder }));
        Ok(())
    }

    // ---------- edit mode ----------

    /// `e`: snapshot the visible tree into an editable buffer (dashboard,
    /// sidebar and float all render it wherever the tree body renders).
    fn enter_edit_mode(&mut self) {
        if self.editbuf.is_some() {
            return;
        }
        let lines = edit_lines_from_rows(
            &self.rows,
            &self.state,
            &self.sessions,
            &self.provisional_labels,
        );
        // Land the buffer cursor on the row the tree cursor was on (the
        // "+ new session" row has no buffer line; j clamps at the end).
        let target = self.rows[..self.cursor.min(self.rows.len())]
            .iter()
            .filter(|r| !matches!(r, Row::NewSession))
            .count();
        let mut buf = EditBuf::new(lines);
        for _ in 0..target {
            buf.handle_key(&Key::Char('j'));
        }
        self.editbuf = Some(buf);
        self.set_status("edit mode — vim keys, :w saves, :q leaves".into());
    }

    /// All tree-focus input while edit mode is active.
    fn on_edit_key(&mut self, key: Key, term: &mut Terminal<Backend>) -> Result<()> {
        let Some(buf) = self.editbuf.as_mut() else {
            return Ok(());
        };
        let ev = match &key {
            // Bracketed paste: typed into the buffer in Insert mode (the
            // buffer itself ignores Paste), dropped in Normal/Cmdline.
            Key::Paste(s) => {
                let mut ev = EditEvent::None;
                if *buf.mode() == EditMode::Insert {
                    for c in s.chars().filter(|c| !c.is_control()) {
                        ev = buf.handle_key(&Key::Char(c));
                    }
                }
                ev
            }
            k => buf.handle_key(k),
        };
        match ev {
            EditEvent::None => {}
            EditEvent::Message(m) => self.set_status(m),
            EditEvent::Save => self.start_edit_save(false),
            EditEvent::SaveQuit => self.start_edit_save(true),
            EditEvent::Quit => self.editbuf = None,
            EditEvent::OpenSession(k) => {
                self.editbuf = None;
                self.open_session(&k, term)?;
            }
        }
        Ok(())
    }

    /// `:w` / `:wq`: diff the buffer and put the action list behind a
    /// confirm modal (cancel keeps edit mode untouched). An empty diff is
    /// saved on the spot.
    fn start_edit_save(&mut self, and_quit: bool) {
        let Some(buf) = self.editbuf.as_mut() else {
            return;
        };
        let actions = buf.diff();
        if actions.is_empty() {
            // A pure reorder is dirty() but diffs empty: absorb it.
            buf.mark_saved();
            if and_quit {
                self.editbuf = None;
            }
            self.set_status("no changes".into());
            return;
        }
        let msg = self.edit_confirm_msg(&actions);
        self.modal = Some(Modal::Confirm {
            msg,
            commit: Commit::ApplyEdits { actions, and_quit },
        });
    }

    /// Human-readable one-liner for one diff action (confirm modal rows).
    fn edit_action_label(&self, a: &EditAction) -> String {
        let sess = |key: &SessionKey| -> String {
            self.meta_idx
                .get(key)
                .map(|i| dashboard::display_title(&self.state, &self.sessions[*i]))
                .unwrap_or_else(|| key.id.clone())
        };
        let fold = |id: &Option<String>| -> String {
            match id {
                Some(f) => format!(
                    "{}/",
                    self.state.folder(f).map(|f| f.name.as_str()).unwrap_or(f)
                ),
                None => "Inbox".into(),
            }
        };
        match a {
            EditAction::CreateFolder {
                parent: parent @ Some(_),
                name,
            } => format!("create folder {name}/ (in {})", fold(parent)),
            EditAction::CreateFolder { parent: None, name } => format!("create folder {name}/"),
            EditAction::RenameFolder { id, name } => {
                format!("rename folder {} → {name}/", fold(&Some(id.clone())))
            }
            EditAction::DeleteFolder { id } => format!(
                "delete folder {} (contents re-parent)",
                fold(&Some(id.clone()))
            ),
            EditAction::RenameSession { key, name } if name.is_empty() => {
                format!("rename session {} → (default title)", sess(key))
            }
            EditAction::RenameSession { key, name } => {
                format!("rename session {} → {name}", sess(key))
            }
            EditAction::HideSession { key } => format!("hide {}", sess(key)),
            EditAction::MoveSession { key, folder } => {
                format!("move {} → {}", sess(key), fold(folder))
            }
            EditAction::ForkInto { key, folder } => {
                format!("fork {} into {}", sess(key), fold(folder))
            }
            EditAction::IgnoredLine { text } => format!("ignored line: {}", text.trim()),
        }
    }

    fn edit_confirm_msg(&self, actions: &[EditAction]) -> String {
        // Confirm-list cap; the tail is summarized as "…and N more".
        const MAX_LINES: usize = 12;
        let mut lines = vec![format!("apply {} change(s)?", actions.len())];
        for a in actions.iter().take(MAX_LINES) {
            lines.push(self.edit_action_label(a));
        }
        if actions.len() > MAX_LINES {
            lines.push(format!("…and {} more", actions.len() - MAX_LINES));
        }
        lines.join("\n")
    }

    /// Confirmed apply: run the actions in diff order with the app's normal
    /// operations, keep going on individual failures (first error reaches
    /// the status line), persist once, and re-baseline the buffer.
    fn apply_edit_actions(&mut self, actions: Vec<EditAction>, and_quit: bool) {
        let mut first_err: Option<String> = None;
        let mut ignored: Vec<String> = Vec::new();
        for a in actions {
            let res: std::result::Result<(), String> = match a {
                EditAction::CreateFolder { parent, name } => self
                    .state
                    .create_folder_scoped(&name, parent.as_deref(), self.view_scope())
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}")),
                EditAction::RenameFolder { id, name } => self
                    .state
                    .rename_folder(&id, &name)
                    .map_err(|e| format!("{e:#}")),
                EditAction::DeleteFolder { id } => {
                    self.collapsed.remove(&id);
                    self.state.delete_folder(&id).map_err(|e| format!("{e:#}"))
                }
                EditAction::RenameSession { key, name } => {
                    let name = name.trim().to_string();
                    self.state.session_mut(&key).name_override =
                        if name.is_empty() { None } else { Some(name) };
                    Ok(())
                }
                EditAction::MoveSession { key, folder } => self
                    .state
                    .set_session_folder(&key, folder.as_deref())
                    .map_err(|e| format!("{e:#}")),
                EditAction::HideSession { key } => {
                    self.state.session_mut(&key).hidden = true;
                    Ok(())
                }
                EditAction::ForkInto { key, folder } => self.fork_session(&key, Some(folder)),
                EditAction::IgnoredLine { text } => {
                    ignored.push(text.trim().to_string());
                    Ok(())
                }
            };
            if let Err(e) = res
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        // Re-baseline even on partial failure: replaying the same diff on
        // the next :w would re-fork sessions. Failures were surfaced.
        if let Some(buf) = self.editbuf.as_mut() {
            buf.mark_saved();
        }
        if and_quit {
            self.editbuf = None;
        }
        // Fork spawns steal pane focus; an ongoing edit stays in the tree.
        if self.editbuf.is_some() {
            self.focus_tree();
        }
        self.persist();
        self.rebuild_rows();
        self.request_scan();
        if let Some(e) = first_err {
            self.set_status(format!("some edits failed: {e}"));
        } else if !ignored.is_empty() {
            self.set_status(format!(
                "ignored non-folder line(s): {}",
                ignored.join(", ")
            ));
        } else {
            self.set_status("changes applied".into());
        }
    }

    // ---------- modals ----------

    fn start_new_session(&mut self) {
        // A machine header under the cursor pre-selects that machine: the
        // location step is skipped and local CLI availability is moot.
        if let Some(name) = self.machine_under_cursor() {
            self.start_new_session_on_machine(name);
            return;
        }
        // Re-probe on every open: the user may have (un)installed an agent
        // since startup, and the picker's dimming must reflect reality.
        self.agents_ok = Self::probe_agents(&self.cfg);
        let (claude_ok, codex_ok) = self.agents_ok;
        self.agent_notice = (!claude_ok && !codex_ok).then(|| NO_AGENTS_MSG.to_string());
        if !claude_ok && !codex_ok {
            self.set_status(NO_AGENTS_MSG.into());
            return;
        }
        let folder = self.folder_context();
        let dir_hint = folder
            .as_ref()
            .and_then(|id| self.state.folder(id))
            .and_then(|f| f.default_dir.as_ref())
            .map(|p| p.display().to_string())
            .or_else(|| {
                self.session_under_cursor()
                    .and_then(|k| self.meta_idx.get(&k).copied())
                    .map(|i| self.sessions[i].cwd.display().to_string())
            })
            .or_else(|| {
                // Scoped to a repo: new sessions default there.
                self.scoped
                    .then(|| self.scope_root.as_ref().map(|p| p.display().to_string()))
                    .flatten()
            });
        self.modal = Some(Modal::PickAgent {
            folder,
            dir_hint,
            // Land the highlight on the first installed agent.
            idx: if claude_ok { 0 } else { 1 },
            claude_ok,
            codex_ok,
            remote: None,
        });
    }

    /// n/enter on a machine header: agent picker with the location
    /// pre-selected. Both agents are offered — the binaries live on the box,
    /// so local availability doesn't gate remote sessions.
    fn start_new_session_on_machine(&mut self, name: String) {
        if self.cfg.remote(&name).is_none() {
            self.set_status(format!("remote {name} no longer configured"));
            return;
        }
        self.modal = Some(Modal::PickAgent {
            folder: None,
            dir_hint: None,
            idx: 0,
            claude_ok: true,
            codex_ok: true,
            remote: Some(name),
        });
    }

    fn start_new_folder(&mut self) {
        let parent = self.folder_under_cursor();
        self.modal = Some(Modal::Input {
            title: match &parent {
                Some(id) => format!(
                    "new folder inside `{}`",
                    self.state
                        .folder(id)
                        .map(|f| f.name.as_str())
                        .unwrap_or("?")
                ),
                None => "new folder".into(),
            },
            edit: LineEdit::default(),
            kind: InputKind::NewFolder { parent },
        });
    }

    /// State-mutating actions must not run against a provisional
    /// "pending-…" key: they'd persist garbage entries in state.json and
    /// the edit would be lost when the real id resolves.
    fn guard_provisional(&mut self, key: &SessionKey) -> bool {
        if key.id.starts_with("pending-") {
            self.set_status("session still starting — try again in a moment".into());
            return true;
        }
        false
    }

    /// Shell panes are ephemeral (no state entry, gone on close): every
    /// state-mutating action refuses them or it would persist garbage
    /// "shell:…" records.
    fn refuse_shell(&mut self, key: &SessionKey) -> bool {
        if key.agent == AgentKind::Shell {
            self.set_status("shells are ephemeral — nothing to keep (w closes the pane)".into());
            return true;
        }
        false
    }

    fn start_move_session(&mut self) {
        let Some(key) = self.session_under_cursor() else {
            return;
        };
        if self.refuse_shell(&key) || self.guard_provisional(&key) {
            return;
        }
        let mut options: Vec<(Option<String>, String)> = vec![(None, "Inbox".into())];
        fn walk(
            state: &VagState,
            parent: Option<&str>,
            depth: usize,
            out: &mut Vec<(Option<String>, String)>,
        ) {
            for f in state.children_of(parent) {
                out.push((
                    Some(f.id.clone()),
                    format!("{}{}", "  ".repeat(depth), f.name),
                ));
                walk(state, Some(&f.id), depth + 1, out);
            }
        }
        walk(&self.state, None, 0, &mut options);
        let current = self.state.session(&key).and_then(|r| r.folder.clone());
        let idx = options
            .iter()
            .position(|(id, _)| *id == current)
            .unwrap_or(0);
        self.modal = Some(Modal::PickFolder { key, options, idx });
    }

    fn start_rename(&mut self) {
        match self.rows.get(self.cursor).cloned() {
            Some(Row::Folder { id, name, .. }) => {
                self.modal = Some(Modal::Input {
                    title: "rename folder".into(),
                    edit: LineEdit::with_text(&name),
                    kind: InputKind::RenameFolder { id },
                });
            }
            Some(Row::Session { key, meta_idx, .. }) => {
                if self.refuse_shell(&key) || self.guard_provisional(&key) {
                    return;
                }
                let current = self
                    .state
                    .session(&key)
                    .and_then(|r| r.name_override.clone())
                    .or_else(|| {
                        meta_idx.map(|i| dashboard::display_title(&self.state, &self.sessions[i]))
                    })
                    .unwrap_or_default();
                self.modal = Some(Modal::Input {
                    title: "rename session (empty = reset)".into(),
                    edit: LineEdit::with_text(&current),
                    kind: InputKind::RenameSession { key },
                });
            }
            // Renames would orphan the state entries that reference the
            // machine by name — not v1.
            Some(Row::Machine { .. }) => {
                self.set_status("edit ~/.config/vag/config.toml to rename machines".into());
            }
            _ => {}
        }
    }

    fn start_bind_dir(&mut self) {
        let Some(id) = self.folder_under_cursor() else {
            return;
        };
        let current = self
            .state
            .folder(&id)
            .and_then(|f| f.default_dir.as_ref())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.open_dir_picker(
            "folder default directory (empty = clear)".into(),
            &current,
            DirTarget::BindFolder { id },
        );
    }

    fn toggle_hidden(&mut self) {
        let Some(key) = self.session_under_cursor() else {
            return;
        };
        if self.refuse_shell(&key) || self.guard_provisional(&key) {
            return;
        }
        let r = self.state.session_mut(&key);
        r.hidden = !r.hidden;
        let hidden = r.hidden;
        self.persist();
        self.set_status(if hidden {
            "session hidden — press H to show hidden, d to unhide".into()
        } else {
            "session unhidden".into()
        });
        self.rebuild_rows();
    }

    fn toggle_scope(&mut self) {
        let Some(root) = &self.scope_root else {
            self.set_status("not inside a git repository — nothing to scope to".into());
            return;
        };
        self.scoped = !self.scoped;
        self.set_status(if self.scoped {
            format!(
                "scoped to {} (g shows everything)",
                root.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string())
            )
        } else {
            "showing all projects (g scopes to this repo)".into()
        });
        self.rebuild_rows();
    }

    fn toggle_show_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.set_status(if self.show_hidden {
            "showing hidden/archived sessions (dimmed) — d unhides, A unarchives".into()
        } else {
            "hidden/archived sessions concealed".into()
        });
        self.rebuild_rows();
    }

    fn start_archive(&mut self) {
        let Some(key) = self.session_under_cursor() else {
            return;
        };
        if self.guard_provisional(&key) {
            return;
        }
        if key.agent != AgentKind::Codex {
            self.set_status("archive is codex-native; use d to hide claude sessions".into());
            return;
        }
        // The codex CLI shell-out runs locally; it can't see a remote store.
        if self
            .state
            .session(&key)
            .and_then(|r| r.remote.as_ref())
            .is_some()
        {
            self.set_status("archive isn't supported on remote sessions yet".into());
            return;
        }
        if self.archive_in_flight.contains(&key) {
            self.set_status("archive already in progress for this session".into());
            return;
        }
        let archived = self
            .meta_idx
            .get(&key)
            .map(|i| self.sessions[*i].archived)
            .unwrap_or(false);
        self.modal = Some(Modal::Confirm {
            msg: format!(
                "{} this codex session via the codex CLI?",
                if archived { "Unarchive" } else { "Archive" }
            ),
            commit: Commit::ArchiveCodex {
                key,
                archived: !archived,
            },
        });
    }

    fn start_delete_folder(&mut self) {
        // x on a session row: delete it (for real where the backend can).
        if let Some(key) = self.session_under_cursor() {
            self.start_delete_session(key);
            return;
        }
        // x on a machine header: remove it from config.toml, not the tree.
        if let Some(name) = self.machine_under_cursor() {
            self.modal = Some(Modal::Confirm {
                msg: format!(
                    "remove machine `{name}` from config? (its sessions stay in your tree)"
                ),
                commit: Commit::RemoveMachine { name },
            });
            return;
        }
        let Some(id) = self.folder_under_cursor() else {
            return;
        };
        let name = self
            .state
            .folder(&id)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        self.modal = Some(Modal::Confirm {
            msg: format!("Delete folder `{name}`? (sessions move to its parent)"),
            commit: Commit::DeleteFolder { id },
        });
    }

    fn start_set_color(&mut self) {
        let Some(key) = self.session_under_cursor() else {
            return;
        };
        if key.agent == AgentKind::Shell {
            self.set_status("shells are ephemeral — no color to keep".into());
            return;
        }
        if self.guard_provisional(&key) {
            return;
        }
        let current = self.state.session(&key).and_then(|r| r.color.clone());
        let mut options: Vec<Option<String>> = vec![None];
        options.extend(
            dashboard::SESSION_PALETTE
                .iter()
                .map(|c| Some((*c).to_string())),
        );
        let idx = options.iter().position(|o| *o == current).unwrap_or(0);
        self.modal = Some(Modal::PickColor { key, options, idx });
    }

    fn start_delete_session(&mut self, key: SessionKey) {
        // Shell panes are ephemeral: x behaves like w (close, no ceremony).
        if key.agent == AgentKind::Shell {
            self.close_runtime(&key);
            return;
        }
        if self.guard_provisional(&key) {
            return;
        }
        let title = self
            .meta_idx
            .get(&key)
            .map(|i| dashboard::display_title(&self.state, &self.sessions[*i]))
            .unwrap_or_else(|| key.id.chars().take(8).collect());
        let running = self
            .runtimes
            .get(&key)
            .map(|r| r.is_running())
            .unwrap_or(false);
        let stop = if running {
            " Its process will be stopped."
        } else {
            ""
        };
        let remote = self.state.session(&key).and_then(|r| r.remote.clone());
        let msg = match (&remote, key.agent) {
            (Some(m), _) => {
                format!("Remove `{title}` from vag?{stop} The session itself stays on `{m}`.")
            }
            (None, AgentKind::Codex) => format!(
                "Delete `{title}` permanently?{stop} This runs `codex delete` and cannot be undone."
            ),
            _ => format!(
                "Remove `{title}` from the list?{stop} claude has no delete command — \
                 the transcript stays in ~/.claude (H shows removed sessions; d restores)."
            ),
        };
        self.modal = Some(Modal::Confirm {
            msg,
            commit: Commit::DeleteSession { key },
        });
    }

    fn commit_delete_session(&mut self, key: SessionKey) {
        if self.runtimes.contains_key(&key) {
            self.close_runtime(&key);
        }
        let remote = self.state.session(&key).and_then(|r| r.remote.clone());
        if remote.is_some() {
            // Remote sessions exist only in vag state; dropping the entry
            // removes them from the tree (the machine still has them).
            self.state.sessions.remove(&key.to_key_string());
            self.persist();
            self.request_scan();
            self.set_status("removed from vag (session kept on the machine)".into());
            self.rebuild_rows();
            return;
        }
        match key.agent {
            AgentKind::Codex => {
                // Real deletion through codex's own CLI, off the UI thread.
                if !self.archive_in_flight.insert(key.clone()) {
                    self.set_status("an operation is already running for this session".into());
                    return;
                }
                self.set_status("deleting…".into());
                let cfg = self.cfg.clone();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let result = actions::codex_delete(&cfg, &key.id).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppEvent::DeleteDone { key, result });
                });
            }
            _ => {
                // claude: no delete CLI, and vag never writes agent stores —
                // removing from the listing is the honest operation.
                self.state.session_mut(&key).hidden = true;
                self.persist();
                self.set_status(
                    "removed from the list (claude keeps the transcript; H reveals, d restores)"
                        .into(),
                );
                self.rebuild_rows();
            }
        }
    }

    fn on_delete_done(&mut self, key: SessionKey, result: Result<(), String>) {
        self.archive_in_flight.remove(&key);
        match result {
            Ok(()) => {
                self.state.sessions.remove(&key.to_key_string());
                self.persist();
                self.set_status("deleted".into());
                self.request_scan();
            }
            Err(e) => self.set_status(e),
        }
        self.rebuild_rows();
        self.dirty = true;
    }

    fn start_close_runtime(&mut self) {
        let Some(key) = self.session_under_cursor().or_else(|| self.active.clone()) else {
            return;
        };
        if !self.runtimes.contains_key(&key) {
            return;
        }
        let running = self
            .runtimes
            .get(&key)
            .map(|r| r.is_running())
            .unwrap_or(false);
        if running {
            self.modal = Some(Modal::Confirm {
                msg: "Stop this session's process? (transcript is saved by the agent)".into(),
                commit: Commit::CloseRuntime { key },
            });
        } else {
            self.close_runtime(&key);
        }
    }

    fn close_runtime(&mut self, key: &SessionKey) {
        if let Some(mut rt) = self.runtimes.remove(key) {
            rt.kill();
        }
        // Attach-only remote codex sessions exist only while their pane is
        // open: closing evaporates the state entry and its synthesized row.
        // (Remote claude entries persist — they're resumable later.)
        if actions::is_synthetic_remote_id(&key.id) {
            self.state.sessions.remove(&key.to_key_string());
            self.persist();
            if self.meta_idx.remove(key).is_some() {
                self.sessions.retain(|m| m.key != *key);
                self.meta_idx = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.key.clone(), i))
                    .collect();
            }
        }
        self.activity.remove(key);
        self.last_input_at.remove(key);
        self.exited.remove(key);
        self.open_order.retain(|k| k != key);
        self.pending.remove(key);
        // Ephemeral shell panes: the label was their only trace.
        self.provisional_labels.remove(key);
        if self.active.as_ref() == Some(key) {
            self.set_active(self.open_order.last().cloned());
            if self.active.is_none() {
                self.focus_tree();
            }
        }
        self.rebuild_rows();
    }

    // ---------- settings page ----------

    /// Build the settings page from live cfg and open it with the cursor on
    /// row `idx` (bumped off section headers).
    fn open_settings(&mut self, idx: usize) {
        let rows = self.settings_rows();
        let mut idx = idx.min(rows.len().saturating_sub(1));
        while matches!(rows.get(idx), Some(SettingRow::Section(_))) {
            idx += 1;
        }
        self.modal = Some(Modal::Settings { rows, idx });
    }

    fn settings_rows(&self) -> Vec<SettingRow> {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        let mut rows = vec![
            SettingRow::Section("appearance".into()),
            SettingRow::Value {
                id: SettingId::Theme,
                label: "theme".into(),
                value: self.cfg.ui.theme.clone(),
            },
            SettingRow::Value {
                id: SettingId::Icons,
                label: "icons".into(),
                value: icon_mode_name(self.cfg.ui.icons).into(),
            },
            SettingRow::Value {
                id: SettingId::Pane,
                label: "pane style".into(),
                value: pane_style_name(self.cfg.ui.pane).into(),
            },
            SettingRow::Value {
                id: SettingId::Tree,
                label: "tree while open".into(),
                value: tree_mode_name(self.cfg.ui.tree).into(),
            },
            SettingRow::Value {
                id: SettingId::SidebarWidth,
                label: "sidebar width".into(),
                value: self.cfg.ui.sidebar_width.to_string(),
            },
            SettingRow::Section("behavior".into()),
            SettingRow::Value {
                id: SettingId::EditDefault,
                label: "start in edit mode".into(),
                value: on_off(self.cfg.ui.edit_default),
            },
            SettingRow::Value {
                id: SettingId::RepoScope,
                label: "repo scope at launch".into(),
                value: on_off(self.cfg.behavior.repo_scope),
            },
            SettingRow::Value {
                id: SettingId::ShowHidden,
                label: "show hidden at launch".into(),
                value: on_off(self.cfg.behavior.show_hidden),
            },
            SettingRow::Section("keys".into()),
        ];
        for a in CtrlAction::ALL {
            rows.push(SettingRow::Key {
                target: BindTarget::Ctrl(a),
                label: a.title().into(),
                value: self.cfg.keys.get_ctrl(a).label(),
            });
        }
        for a in KeyAction::ALL {
            rows.push(SettingRow::Key {
                target: BindTarget::Action(a),
                label: a.title().into(),
                value: self.cfg.keys.get(a).to_string(),
            });
        }
        rows
    }

    /// Write one `[section] key = value` to config.toml; failures surface on
    /// the status line but never lose the in-memory change.
    fn persist_setting<V: Into<toml_edit::Value>>(&mut self, section: &str, key: &str, value: V) {
        if let Err(e) = crate::config::set_config_item(&Config::config_path(), section, key, value)
        {
            self.set_status(format!("config write failed: {e:#}"));
        }
    }

    /// Resolve a named theme with the user's `[theme]` overrides layered on.
    fn theme_for_name(&self, name: &str) -> Theme {
        let mut cfg = self.cfg.clone();
        cfg.ui.theme = name.to_string();
        Theme::from_config(&cfg)
    }

    fn apply_theme_live(&mut self, t: Theme) {
        self.theme = t;
        apply_theme_globals(&t);
    }

    /// Live preview while the theme picker is open: apply whatever the
    /// cursor rests on.
    fn preview_picked_theme(&mut self, modal: &Modal) {
        if let Modal::PickTheme { options, idx, .. } = modal
            && let Some(name) = options.get(*idx).cloned()
        {
            let t = self.theme_for_name(&name);
            self.apply_theme_live(t);
        }
    }

    /// Step a settings value through its variants, apply it live where it
    /// has a live effect, and persist it to config.toml.
    fn cycle_setting(&mut self, id: SettingId, dir: i8) {
        let step = |i: usize, n: usize| (i as i64 + dir as i64).rem_euclid(n as i64) as usize;
        match id {
            // The theme row opens the picker instead of cycling.
            SettingId::Theme => {}
            SettingId::Icons => {
                let order = [IconMode::Ascii, IconMode::Nerd, IconMode::Auto];
                let i = order
                    .iter()
                    .position(|m| *m == self.cfg.ui.icons)
                    .unwrap_or(0);
                self.cfg.ui.icons = order[step(i, order.len())];
                self.icons = Icons::for_mode(self.cfg.ui.icons);
                self.persist_setting("ui", "icons", icon_mode_name(self.cfg.ui.icons));
            }
            SettingId::Pane => {
                self.cfg.ui.pane = match self.cfg.ui.pane {
                    PaneStyle::Titlebar => PaneStyle::Border,
                    PaneStyle::Border => PaneStyle::Titlebar,
                };
                self.persist_setting("ui", "pane", pane_style_name(self.cfg.ui.pane));
            }
            SettingId::Tree => {
                self.cfg.ui.tree = match self.cfg.ui.tree {
                    TreeMode::Sidebar => TreeMode::Float,
                    TreeMode::Float => TreeMode::Sidebar,
                };
                // Same rule set_active applies: the float overlay exists
                // only while a session is open.
                self.tree_float = self.cfg.ui.tree == TreeMode::Float && self.active.is_some();
                self.persist_setting("ui", "tree", tree_mode_name(self.cfg.ui.tree));
            }
            SettingId::SidebarWidth => {
                let w = (self.cfg.ui.sidebar_width as i64 + 2 * dir as i64).clamp(20, 60) as u16;
                self.cfg.ui.sidebar_width = w;
                self.persist_setting("ui", "sidebar_width", w as i64);
            }
            SettingId::EditDefault => {
                self.cfg.ui.edit_default = !self.cfg.ui.edit_default;
                self.persist_setting("ui", "edit_default", self.cfg.ui.edit_default);
            }
            SettingId::RepoScope => {
                self.cfg.behavior.repo_scope = !self.cfg.behavior.repo_scope;
                self.scoped = self.scope_root.is_some() && self.cfg.behavior.repo_scope;
                self.rebuild_rows();
                self.persist_setting("behavior", "repo_scope", self.cfg.behavior.repo_scope);
            }
            SettingId::ShowHidden => {
                self.cfg.behavior.show_hidden = !self.cfg.behavior.show_hidden;
                self.show_hidden = self.cfg.behavior.show_hidden;
                self.rebuild_rows();
                self.persist_setting("behavior", "show_hidden", self.cfg.behavior.show_hidden);
            }
        }
    }

    /// Enter in the theme picker: make the hovered theme THE theme — live
    /// and in config.toml (until the next switch; a --theme/VAG_THEME per-run
    /// override still wins at launch).
    fn commit_set_theme(&mut self, name: String, from_idx: usize) {
        self.cfg.ui.theme = name.clone();
        let t = Theme::from_config(&self.cfg);
        self.apply_theme_live(t);
        self.persist_setting("ui", "theme", name);
        if from_idx != usize::MAX {
            self.open_settings(from_idx);
        }
    }

    /// A captured keypress: rebind live + persist, refusing chars another
    /// action holds (rebind that one first — never silent double-binds).
    fn commit_set_binding(&mut self, target: BindTarget, ch: char, from_idx: usize) {
        match target {
            BindTarget::Ctrl(a) => {
                // The capture modal already validated the chord shape.
                let Some(k) = DetachKey::parse(&format!("ctrl-{ch}")) else {
                    self.open_settings(from_idx);
                    return;
                };
                if let Some(taken) = self.cfg.keys.ctrl_collision(a, k) {
                    self.set_status(format!(
                        "ctrl-{ch} is already bound to {} — rebind that first",
                        taken.title()
                    ));
                    self.open_settings(from_idx);
                    return;
                }
                self.cfg.keys.set_ctrl(a, k);
                self.persist_setting("keys", a.name(), k.label());
            }
            BindTarget::Action(a) => {
                if let Some(taken) = self.cfg.keys.collision(a, ch) {
                    self.set_status(format!(
                        "`{ch}` is already bound to {} — rebind that first",
                        taken.title()
                    ));
                    self.open_settings(from_idx);
                    return;
                }
                self.cfg.keys.set(a, ch);
                self.persist_setting("keys", a.name(), ch.to_string());
            }
        }
        self.open_settings(from_idx);
    }

    fn apply_commit(&mut self, c: Commit, term: &mut Terminal<Backend>) -> Result<()> {
        match c {
            Commit::Quit => self.quit = true,
            Commit::SettingCycle { id, dir, idx } => {
                self.cycle_setting(id, dir);
                self.open_settings(idx);
            }
            Commit::OpenThemePicker { from_idx } => {
                let options: Vec<String> = Theme::ALL.iter().map(|(n, _)| n.to_string()).collect();
                let idx = options
                    .iter()
                    .position(|o| Theme::by_name(o) == Theme::by_name(&self.cfg.ui.theme))
                    .unwrap_or(0);
                self.modal = Some(Modal::PickTheme {
                    options,
                    idx,
                    original: self.cfg.ui.theme.clone(),
                    from_idx,
                });
            }
            Commit::SetTheme { name, from_idx } => self.commit_set_theme(name, from_idx),
            Commit::StartCapture { target, from_idx } => {
                self.modal = Some(Modal::CaptureKey { target, from_idx });
            }
            Commit::SetBinding {
                target,
                ch,
                from_idx,
            } => self.commit_set_binding(target, ch, from_idx),
            Commit::NewFolder { parent, name } => {
                match self
                    .state
                    .create_folder_scoped(&name, parent.as_deref(), self.view_scope())
                {
                    Ok(_) => {
                        self.persist();
                        self.rebuild_rows();
                    }
                    Err(e) => self.set_status(format!("{e:#}")),
                }
            }
            Commit::RenameFolder { id, name } => {
                if let Err(e) = self.state.rename_folder(&id, &name) {
                    self.set_status(format!("{e:#}"));
                } else {
                    self.persist();
                    self.rebuild_rows();
                }
            }
            Commit::DeleteFolder { id } => {
                if let Err(e) = self.state.delete_folder(&id) {
                    self.set_status(format!("{e:#}"));
                } else {
                    self.collapsed.remove(&id);
                    self.persist();
                    self.rebuild_rows();
                }
            }
            Commit::BindFolderDir { id, dir } => {
                let dir = dir.trim().to_string();
                let val = if dir.is_empty() {
                    None
                } else {
                    Some(expand_tilde(&dir))
                };
                if let Some(p) = &val
                    && !p.is_dir()
                {
                    self.set_status(format!("not a directory: {}", p.display()));
                    return Ok(());
                }
                if let Err(e) = self.state.set_folder_default_dir(&id, val) {
                    self.set_status(format!("{e:#}"));
                } else {
                    self.persist();
                    self.rebuild_rows();
                }
            }
            Commit::RenameSession { key, name } => {
                let name = name.trim().to_string();
                self.state.session_mut(&key).name_override =
                    if name.is_empty() { None } else { Some(name) };
                self.persist();
                self.rebuild_rows();
            }
            Commit::MoveSession { key, folder } => {
                if let Err(e) = self.state.set_session_folder(&key, folder.as_deref()) {
                    self.set_status(format!("{e:#}"));
                } else {
                    self.persist();
                    self.rebuild_rows();
                }
            }
            Commit::NewSessionAgent {
                agent,
                folder,
                dir_hint,
                remote,
            } => self.commit_new_session_agent(agent, folder, dir_hint, remote),
            Commit::NewSessionLocation {
                agent,
                folder,
                dir_hint,
                remote,
            } => self.commit_new_session_location(agent, folder, dir_hint, remote),
            Commit::NewSessionDir {
                agent,
                folder,
                dir,
                remote,
            } => self.commit_new_session_dir(agent, folder, dir, remote),
            Commit::NewSessionName {
                agent,
                folder,
                dir,
                name,
                remote,
            } => {
                let name = name.trim().to_string();
                let name = if name.is_empty() { None } else { Some(name) };
                self.launch_new(
                    agent,
                    folder,
                    &PathBuf::from(dir),
                    name.as_deref(),
                    remote.as_deref(),
                );
            }
            Commit::CloseRuntime { key } => self.close_runtime(&key),
            Commit::OpenAnyway { key } => self.open_session_unchecked(&key, term)?,
            Commit::StartAddMachine => self.start_add_machine(),
            Commit::AddMachineName { name } => self.open_add_machine_host(name),
            Commit::AddMachineHost { name, host } => {
                self.modal = Some(Modal::Input {
                    title: format!("add machine `{name}` — default directory (optional)"),
                    edit: LineEdit::default(),
                    kind: InputKind::AddMachineDir { name, host },
                });
            }
            Commit::AddMachine { name, host, dir } => self.commit_add_machine(name, host, dir),
            Commit::RemoveMachine { name } => self.commit_remove_machine(name),
            Commit::DeleteSession { key } => self.commit_delete_session(key),
            Commit::SetSessionColor { key, color } => {
                self.state.session_mut(&key).color = color;
                self.persist();
                self.rebuild_rows();
            }
            Commit::ApplyEdits { actions, and_quit } => self.apply_edit_actions(actions, and_quit),
            Commit::ArchiveCodex { key, archived } => {
                // The codex shell-out can block for seconds — run it on a
                // worker so the event loop (and all open panes) stay live.
                if !self.archive_in_flight.insert(key.clone()) {
                    self.set_status("archive already in progress for this session".into());
                    return Ok(());
                }
                self.set_status(if archived {
                    "archiving…".into()
                } else {
                    "unarchiving…".into()
                });
                let cfg = self.cfg.clone();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let result = actions::codex_set_archived(&cfg, &key.id, archived)
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppEvent::ArchiveDone {
                        key,
                        archived,
                        result,
                    });
                });
            }
        }
        let _ = term;
        Ok(())
    }

    /// PickAgent commit: a pre-selected machine (n on its header) skips the
    /// location step; with `[[remotes]]` configured the location picker is
    /// inserted; otherwise straight to the directory prompt (historical
    /// flow).
    fn commit_new_session_agent(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir_hint: Option<String>,
        remote: Option<String>,
    ) {
        if remote.is_some() {
            self.commit_new_session_location(agent, folder, dir_hint, remote);
            return;
        }
        if self.cfg.remotes.is_empty() {
            if let Err(e) = actions::check_agent_available(&self.cfg, agent) {
                self.set_status(e);
                return;
            }
            self.open_new_session_dir_input(agent, folder, dir_hint, None);
            return;
        }
        let mut options: Vec<LocationChoice> = vec![LocationChoice::Local];
        for r in &self.cfg.remotes {
            options.push(LocationChoice::Remote {
                name: r.name.clone(),
                host: r.host.clone(),
            });
        }
        options.push(LocationChoice::AddMachine);
        self.modal = Some(Modal::PickLocation {
            agent,
            folder,
            dir_hint,
            options,
            idx: 0,
        });
    }

    /// PickLocation commit: local keeps the historical prefill (and the
    /// local CLI availability check); a remote prefills its default_dir.
    fn commit_new_session_location(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir_hint: Option<String>,
        remote: Option<String>,
    ) {
        match remote {
            None => {
                if let Err(e) = actions::check_agent_available(&self.cfg, agent) {
                    self.set_status(e);
                    return;
                }
                self.open_new_session_dir_input(agent, folder, dir_hint, None);
            }
            Some(name) => {
                let Some(rc) = self.cfg.remote(&name) else {
                    self.set_status(format!("remote {name} no longer configured"));
                    return;
                };
                let hint = rc.default_dir.clone().unwrap_or_else(|| "~".into());
                self.open_new_session_dir_input(agent, folder, Some(hint), Some(name));
            }
        }
    }

    fn open_new_session_dir_input(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir_hint: Option<String>,
        remote: Option<String>,
    ) {
        let hint = dir_hint.unwrap_or_else(|| "~/".into());
        // Remote filesystems can't be scanned: the plain input stays.
        if let Some(r) = remote {
            self.modal = Some(Modal::Input {
                title: format!("new {} session on {r} — directory", agent.label()),
                edit: LineEdit::with_text(&hint),
                kind: InputKind::NewSessionDir {
                    agent,
                    folder,
                    remote: Some(r),
                },
            });
            return;
        }
        self.open_dir_picker(
            format!("new {} session — directory", agent.label()),
            &hint,
            DirTarget::NewSession { agent, folder },
        );
    }

    /// Open the fuzzy directory picker seeded with known-good directories
    /// (session cwds, folder defaults) and kick off the background walk of
    /// `$HOME` that streams the rest in.
    fn open_dir_picker(&mut self, title: String, prefill: &str, target: DirTarget) {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        // Candidates are tilde-abbreviated; an absolute prefill (folder
        // default, session cwd) must match them, so abbreviate it too.
        let prefill = if prefill.starts_with('/') {
            crate::dirscan::abbrev_home(std::path::Path::new(prefill), &home)
        } else {
            prefill.to_string()
        };
        let seeds = self.dir_picker_seeds(&home);
        self.modal = Some(Modal::PickDir(DirPick::new(
            title,
            &prefill,
            target,
            seeds.clone(),
        )));
        self.dir_scan_id += 1;
        let id = self.dir_scan_id;
        // Unit tests drive the modal directly; walking the test machine's
        // real $HOME would be wasted work (the receiver is never read).
        if cfg!(test) {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let skip: HashSet<String> = seeds.into_iter().collect();
            let mut emit = |dirs: Vec<String>| {
                let _ = tx.send(AppEvent::DirScan {
                    id,
                    dirs,
                    done: false,
                });
            };
            crate::dirscan::scan(&home, &home, &skip, &mut emit);
            let _ = tx.send(AppEvent::DirScan {
                id,
                dirs: vec![],
                done: true,
            });
        });
    }

    /// Instant candidates before the walk delivers: `~`, every local
    /// session's cwd and every folder default dir — the places the user
    /// demonstrably works in.
    fn dir_picker_seeds(&self, home: &std::path::Path) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = vec!["~".to_string()];
        seen.insert("~".to_string());
        let push = |p: &std::path::Path, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            // Remote session cwds are paths on another box ("~/work") —
            // only absolute (= local) paths belong in a local picker.
            if !p.is_absolute() {
                return;
            }
            let s = crate::dirscan::abbrev_home(p, home);
            if seen.insert(s.clone()) {
                out.push(s);
            }
        };
        for m in &self.sessions {
            push(&m.cwd, &mut out, &mut seen);
        }
        for f in &self.state.folders {
            if let Some(d) = &f.default_dir {
                push(d, &mut out, &mut seen);
            }
        }
        out
    }

    /// A walk batch arrived: feed it to the picker if it's still the one
    /// that asked (the modal may have closed or been reopened since).
    fn on_dir_scan(&mut self, id: u64, dirs: Vec<String>, done: bool) {
        if id != self.dir_scan_id {
            return;
        }
        if let Some(Modal::PickDir(pick)) = &mut self.modal {
            if !dirs.is_empty() {
                pick.push_candidates(dirs);
            }
            if done {
                pick.scanning = false;
            }
            self.dirty = true;
        }
    }

    /// Directory commit of the new-session flow. Remote paths are validated
    /// by the remote shell, not us — a local is_dir() check would be
    /// nonsense there.
    fn commit_new_session_dir(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir: String,
        remote: Option<String>,
    ) {
        if let Some(rname) = remote {
            match agent {
                AgentKind::Claude => {
                    self.modal = Some(Modal::Input {
                        title: "session name (optional)".into(),
                        edit: LineEdit::default(),
                        kind: InputKind::NewSessionName {
                            agent,
                            folder,
                            dir,
                            remote: Some(rname),
                        },
                    });
                }
                // Shell can't reach this agent-session flow; launch_new
                // surfaces the builder's refusal if it ever does.
                AgentKind::Codex | AgentKind::Shell => {
                    self.launch_new(
                        agent,
                        folder,
                        std::path::Path::new(&dir),
                        None,
                        Some(&rname),
                    );
                }
            }
            return;
        }
        let path = expand_tilde(dir.trim());
        if !path.is_dir() {
            self.set_status(format!("not a directory: {}", path.display()));
            // reopen the picker with what they typed
            self.open_new_session_dir_input(agent, folder, Some(dir), None);
            return;
        }
        match agent {
            AgentKind::Claude => {
                self.modal = Some(Modal::Input {
                    title: "session name (optional)".into(),
                    edit: LineEdit::default(),
                    kind: InputKind::NewSessionName {
                        agent,
                        folder,
                        dir: path.display().to_string(),
                        remote: None,
                    },
                });
            }
            // Shell can't reach this agent-session flow; launch_new surfaces
            // the builder's refusal if it ever does.
            AgentKind::Codex | AgentKind::Shell => {
                self.launch_new(agent, folder, &path, None, None);
            }
        }
    }

    fn launch_new(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir: &std::path::Path,
        name: Option<&str>,
        remote: Option<&str>,
    ) {
        if let Some(rname) = remote {
            self.launch_new_remote(agent, folder, dir, name, rname);
            return;
        }
        match actions::new_session_spec(&self.cfg, agent, dir, name) {
            Ok((spec, PendingId::Known(id))) => {
                let key = SessionKey::new(agent, id);
                if let Some(f) = &folder
                    && self.state.folder(f).is_some()
                {
                    let _ = self.state.set_session_folder(&key, Some(f));
                }
                self.state.session_mut(&key).last_opened = Some(Utc::now());
                self.persist();
                self.spawn_runtime(key, &spec, None);
            }
            Ok((spec, PendingId::Discover)) => {
                let prov = Self::provisional_key(agent);
                self.spawn_runtime(prov, &spec, Some(PendingCtx { folder }));
            }
            Err(e) => self.set_status(format!("{e:#}")),
        }
    }

    /// Remote launch: no local dir validation (the path lives on the box),
    /// no local agent availability check (so does the binary), and no
    /// id-discovery thread — both agents get a known id up front (codex a
    /// synthetic attach-only one).
    fn launch_new_remote(
        &mut self,
        agent: AgentKind,
        folder: Option<String>,
        dir: &std::path::Path,
        name: Option<&str>,
        rname: &str,
    ) {
        let Some(rc) = self.cfg.remote(rname).cloned() else {
            self.set_status(format!("remote {rname} no longer configured"));
            return;
        };
        let dir = dir.to_string_lossy();
        let dir = dir.trim();
        let dir = if dir.is_empty() { "~" } else { dir };
        let (spec, pending) = match actions::remote_new_session_spec(&rc, agent, dir, name) {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("{e:#}"));
                return;
            }
        };
        let PendingId::Known(id) = pending else {
            // remote_new_session_spec always pre-assigns; defensive only.
            self.set_status("remote spawn produced no session id".into());
            return;
        };
        let key = SessionKey::new(agent, id);
        record_remote_session(&mut self.state, &key, folder.as_deref(), rname, dir);
        self.persist();
        self.spawn_runtime(key, &spec, None);
        self.request_scan();
    }

    // ---------- machines & shells ----------

    /// `R`: step 1 of the add-machine flow (name → host → optional dir).
    fn start_add_machine(&mut self) {
        self.modal = Some(Modal::Input {
            title: "add machine — name".into(),
            edit: LineEdit::default(),
            kind: InputKind::AddMachineName,
        });
    }

    /// Step 2: the ssh host, with ~/.ssh/config aliases as suggestions.
    fn open_add_machine_host(&mut self, name: String) {
        let mut suggestions = crate::config::ssh_config_aliases();
        suggestions.truncate(8);
        self.modal = Some(Modal::Input {
            title: format!("add machine `{name}` — ssh host (user@host or alias)"),
            edit: LineEdit::default(),
            kind: InputKind::AddMachineHost { name, suggestions },
        });
    }

    /// Final add-machine commit: append the `[[remotes]]` entry to
    /// config.toml (format-preserving) and reload. Errors (duplicate name,
    /// malformed config) reopen the name input prefilled.
    fn commit_add_machine(&mut self, name: String, host: String, dir: String) {
        let dir = dir.trim().to_string();
        let rc = RemoteConfig {
            name: name.trim().to_string(),
            host: host.trim().to_string(),
            default_dir: (!dir.is_empty()).then_some(dir),
            claude_command: String::new(),
            codex_command: String::new(),
        };
        match crate::config::add_remote_to_file(&Config::config_path(), &rc) {
            Ok(()) => {
                self.reload_config();
                self.set_status(format!(
                    "added {} — n on its group creates a session there; first connect may \
                     prompt for credentials in the pane",
                    rc.name
                ));
                self.rebuild_rows();
            }
            Err(e) => {
                self.set_status(format!("{e:#}"));
                self.modal = Some(Modal::Input {
                    title: "add machine — name".into(),
                    edit: LineEdit::with_text(&name),
                    kind: InputKind::AddMachineName,
                });
            }
        }
    }

    /// Confirmed `x` on a machine header. Sessions stay in vag state — only
    /// the config entry goes (unfoldered ones fall back to the Inbox).
    fn commit_remove_machine(&mut self, name: String) {
        match crate::config::remove_remote_from_file(&Config::config_path(), &name) {
            Ok(true) => {
                self.collapsed.remove(&machine_collapse_key(&name));
                self.reload_config();
                self.set_status(format!(
                    "removed machine {name} from config — its sessions stay in your tree"
                ));
                self.rebuild_rows();
            }
            Ok(false) => self.set_status(format!("machine {name} not found in config")),
            Err(e) => self.set_status(format!("{e:#}")),
        }
    }

    /// Re-read config.toml after an add/remove edit so cfg.remotes (and the
    /// machine groups built from it) reflect the file.
    fn reload_config(&mut self) {
        match Config::load() {
            Ok(cfg) => self.cfg = cfg,
            Err(e) => self.set_status(format!("config reload failed: {e:#}")),
        }
    }

    /// `s`: ephemeral shell pane. On a machine header a plain `ssh <host>`;
    /// anywhere else the local `$SHELL` in the cursor's directory context.
    /// Exactly like synthetic codex remotes it lives on the provisional row
    /// path: no state entry, closes without a trace.
    fn start_shell(&mut self) {
        let (spec, label) = self.shell_spawn_plan();
        let key = SessionKey::new(
            AgentKind::Shell,
            format!("shell-{}", uuid::Uuid::new_v4().simple()),
        );
        self.provisional_labels.insert(key.clone(), label.clone());
        self.set_status(format!("{label} — ephemeral (w closes, nothing is saved)"));
        self.spawn_runtime(key.clone(), &spec, None);
        // A failed spawn (status already set by spawn_runtime) must not
        // leave the label behind — ephemeral means no traces, ever.
        if !self.runtimes.contains_key(&key) {
            self.provisional_labels.remove(&key);
        }
    }

    /// Pure spawn planning for `s` (unit-testable without spawning): the
    /// SpawnSpec plus the provisional-row label.
    fn shell_spawn_plan(&self) -> (actions::SpawnSpec, String) {
        if let Some(name) = self.machine_under_cursor()
            && let Some(rc) = self.cfg.remote(&name)
        {
            return (ssh_shell_spec(&rc.host), format!("shell @ {name}"));
        }
        let cwd = self.shell_context_cwd();
        let dirname = cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string());
        (local_shell_spec(cwd), format!("shell: {dirname}"))
    }

    /// Directory context for a local shell: session-under-cursor's cwd,
    /// then the cursor row's folder default_dir, then the repo scope root,
    /// else home. Non-existent candidates (e.g. a remote session's remote
    /// cwd) are skipped.
    fn shell_context_cwd(&self) -> PathBuf {
        let session = self
            .session_under_cursor()
            .and_then(|k| self.meta_idx.get(&k).copied())
            .map(|i| self.sessions[i].cwd.clone());
        let folder = self
            .folder_context()
            .and_then(|id| self.state.folder(&id).and_then(|f| f.default_dir.clone()));
        let scope = self.scoped.then(|| self.scope_root.clone()).flatten();
        [session, folder, scope]
            .into_iter()
            .flatten()
            .find(|p| p.is_dir())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(std::env::temp_dir))
    }

    // ---------- zoom ----------

    fn enter_zoom(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        let Some(active) = self.active.clone() else {
            return Ok(());
        };
        let Some(rt) = self.runtimes.get(&active) else {
            return Ok(());
        };
        self.zoomed = true;
        // Zoom takes the real screen; an open float is dismissed first.
        // (Inline focus_pane: `rt` still borrows self.runtimes below.)
        self.focus = Focus::Pane;
        self.tree_float = false;
        // The real terminal never saw the child's mode-set sequences (the
        // emulator consumed them), so replay them for the handoff.
        let mode = *rt.term().lock().mode();
        let mut out = std::io::stdout();
        // Our chrome modes off; the child now owns the real terminal. Keep
        // bracketed paste on when the child itself enabled it — otherwise
        // a zoomed multi-line paste arrives unframed and the first CR
        // submits a truncated prompt.
        if !mode.contains(TermMode::BRACKETED_PASTE) {
            execute!(out, ctevent::DisableBracketedPaste)?;
        }
        execute!(out, terminal::LeaveAlternateScreen, cursor::Show)?;
        if mode.contains(TermMode::ALT_SCREEN) {
            out.write_all(b"\x1b[?1049h")?;
        }
        out.write_all(&pane::serialize_screen(rt))?;
        out.write_all(&zoom_mode_replay(mode))?;
        out.flush()?;
        rt.set_zoom(true);
        // Resize last: the SIGWINCH-triggered child repaint self-heals any
        // output that raced the blit before set_zoom(true) started teeing.
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        rt.resize(PaneSize { rows, cols });
        let _ = term;
        Ok(())
    }

    fn exit_zoom(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        if !self.zoomed {
            return Ok(());
        }
        self.zoomed = false;
        if let Some(active) = self.active.clone()
            && let Some(rt) = self.runtimes.get(&active)
        {
            rt.set_zoom(false);
        }
        let mut out = std::io::stdout();
        // Reset modes the child may have left on the REAL terminal
        // (ccmanager's hard-won list + mouse + any child alt screen),
        // including everything zoom_mode_replay can enable.
        out.write_all(b"\x1b[<u\x1b[>4;0m\x1b[?1004l\x1b[?2004l\x1b[?7h\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1007l\x1b[?1049l\x1b[0m")?;
        out.flush()?;
        execute!(
            out,
            terminal::EnterAlternateScreen,
            ctevent::EnableBracketedPaste,
            cursor::Hide
        )?;
        term.clear()?;
        // restore pane-sized PTY (draw() will also correct it)
        if let Some(rect) = self.last_pane_rect
            && let Some(active) = &self.active
            && let Some(rt) = self.runtimes.get(active)
        {
            rt.resize(PaneSize {
                rows: rect.height,
                cols: rect.width,
            });
        }
        // In float mode this reopens the float: tree focus with no visible
        // tree would swallow input.
        self.focus_tree();
        self.dirty = true;
        Ok(())
    }

    // ---------- rows / badges ----------

    /// The repo root the view is currently filtered to (None = global).
    /// Folders created now belong to this scope.
    fn view_scope(&self) -> Option<PathBuf> {
        if self.scoped {
            self.scope_root.clone()
        } else {
            None
        }
    }

    fn rebuild_rows(&mut self) {
        let anchor = self.rows.get(self.cursor).map(row_anchor);
        let was_settings = !self.rows.is_empty() && self.settings_selected();
        // Any open runtime the scan doesn't know must stay visible: pending
        // codex/fork discoveries, but also fresh known-id claude sessions
        // whose transcript hasn't hit a rescan yet, and open sessions whose
        // files vanished mid-run.
        let mut provisional: Vec<SessionKey> = self
            .open_order
            .iter()
            .filter(|k| !self.meta_idx.contains_key(*k))
            .cloned()
            .collect();
        for k in self.pending.keys() {
            if !provisional.contains(k) {
                provisional.push(k.clone());
            }
        }
        let machines: Vec<(String, String)> = self
            .cfg
            .remotes
            .iter()
            .map(|r| (r.name.clone(), r.host.clone()))
            .collect();
        self.rows = dashboard::build_rows(
            &self.state,
            &self.sessions,
            &provisional,
            &machines,
            &self.collapsed,
            self.filter.as_deref(),
            self.show_hidden,
            // H reveals everything concealed: hidden AND archived rows.
            // (codex_show_automation is scan-noise filtering only; wiring
            // it here made archived sessions unreachable by default.)
            self.show_hidden,
            self.view_scope().as_deref(),
        );
        // Re-anchor the cursor to the row it was on: background rescans
        // reorder rows, and a bare index would silently select a different
        // session for the next action. The settings sentinel survives as
        // "still on settings", whatever the new row count.
        if was_settings {
            self.cursor = self.rows.len();
        } else if let Some(i) = anchor.and_then(|a| locate_row(&self.rows, &a)) {
            self.cursor = i;
        } else if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// Spinner animation frame; advances with the 100ms tick cadence that
    /// is already active whenever a runtime is open.
    fn spin_frame(&self) -> usize {
        (self.started.elapsed().as_millis() / 100) as usize
    }

    fn badges(&self) -> HashMap<SessionKey, BadgeInfo> {
        let mut out = HashMap::new();
        for (k, rt) in &self.runtimes {
            let info = if !rt.is_running() {
                BadgeInfo {
                    kind: Badge::Exited,
                    dur: None,
                }
            } else {
                match self.activity.get(k).map(|a| a.turn).unwrap_or(Turn::Idle) {
                    Turn::Working { .. } => BadgeInfo {
                        kind: Badge::Working,
                        // Active output time, not wall-clock: waiting on
                        // approvals or idle keepalives must not count.
                        dur: self.activity.get(k).map(|a| a.active_time()),
                    },
                    Turn::Done {
                        finished,
                        unread: true,
                    } => BadgeInfo {
                        kind: Badge::DoneUnread,
                        dur: Some(finished.elapsed()),
                    },
                    Turn::Done { .. } | Turn::Idle => BadgeInfo {
                        kind: Badge::Idle,
                        dur: None,
                    },
                }
            };
            out.insert(k.clone(), info);
        }
        for id in self.external_claude.keys() {
            let key = SessionKey::new(AgentKind::Claude, id.clone());
            out.entry(key).or_insert(BadgeInfo {
                kind: Badge::External,
                dur: self.ext_working.get(id).map(Instant::elapsed),
            });
        }
        out
    }

    fn set_status(&mut self, s: String) {
        self.status = Some((s, Instant::now()));
        self.dirty = true;
    }

    /// Save state, surfacing failures on the status line — a silently
    /// failing save would lose every folder/rename/move on exit.
    fn persist(&mut self) {
        if let Err(e) = self.state.save() {
            self.set_status(format!("FAILED to save state: {e:#}"));
        }
    }

    fn pane_size(&self) -> PaneSize {
        if let Some(r) = self.last_pane_rect {
            return PaneSize {
                rows: r.height.max(2),
                cols: r.width.max(2),
            };
        }
        let (cols, rows) = terminal::size().unwrap_or((120, 40));
        let side = match self.cfg.ui.tree {
            TreeMode::Sidebar => self.cfg.ui.sidebar_width,
            TreeMode::Float => 0,
        };
        let (row_chrome, col_chrome) = match self.cfg.ui.pane {
            PaneStyle::Border => (2, 2),
            PaneStyle::Titlebar => (1, 0),
        };
        PaneSize {
            rows: rows.saturating_sub(row_chrome).max(2),
            cols: cols.saturating_sub(side + col_chrome).max(2),
        }
    }

    /// The cells the child grid actually occupies inside the pane area,
    /// depending on the chrome style. MUST be the single source for both
    /// the pre-draw PTY resize and the painter, or the emulator grid and
    /// the on-screen rect drift apart.
    fn pane_inner(&self, main: Rect) -> Rect {
        match self.cfg.ui.pane {
            PaneStyle::Border => Rect {
                x: main.x + 1,
                y: main.y + 1,
                width: main.width.saturating_sub(2),
                height: main.height.saturating_sub(2),
            },
            // Borderless: one fixed title-bar line on top, tmux-style.
            PaneStyle::Titlebar => Rect {
                x: main.x,
                y: main.y + 1,
                width: main.width,
                height: main.height.saturating_sub(1),
            },
        }
    }

    // ---------- drawing ----------

    /// Sidebar/pane split of the whole screen while a session is active.
    /// Float mode has no sidebar: the pane owns the full width whether or
    /// not the float is open — the float is an overlay, not a re-layout,
    /// so opening it never resizes the child PTY.
    fn split_areas(&self, area: Rect) -> (Option<Rect>, Rect) {
        match self.cfg.ui.tree {
            TreeMode::Sidebar if !self.sidebar_hidden => {
                let [side, main] = Layout::horizontal([
                    Constraint::Length(self.cfg.ui.sidebar_width),
                    Constraint::Min(10),
                ])
                .areas(area);
                (Some(side), main)
            }
            // ctrl-e collapsed it: the pane takes the full width, same as
            // float mode — draw_split already skips draw_sidebar when side
            // is None, so no other draw-path change is needed.
            TreeMode::Sidebar => (None, area),
            TreeMode::Float => (None, area),
        }
    }

    fn draw(&mut self, term: &mut Terminal<Backend>) -> Result<()> {
        self.dirty = false;
        let badges = self.badges();
        // Pre-compute pane rect to resize the runtime before painting.
        let size_area = term
            .size()
            .map(|s| Rect::new(0, 0, s.width, s.height))
            .unwrap_or(Rect::new(0, 0, 120, 40));
        if let Some(active) = self.active.clone() {
            let (_, main) = self.split_areas(size_area);
            let inner = self.pane_inner(main);
            if Some(inner) != self.last_pane_rect {
                self.last_pane_rect = Some(inner);
            }
            if let Some(rt) = self.runtimes.get(&active) {
                rt.resize(PaneSize {
                    rows: inner.height.max(2),
                    cols: inner.width.max(2),
                });
            }
        }

        let now = Utc::now();
        term.draw(|f| {
            let area = f.area();
            // Solid-theme base coat: paint bg/fg under everything (widgets
            // with unset style bits inherit these cells). Transparent theme
            // paints nothing — the terminal shows through, as always.
            if self.theme.bg != Color::Reset {
                f.render_widget(
                    Block::new().style(Style::new().bg(self.theme.bg).fg(self.theme.fg)),
                    area,
                );
            }
            if let Some(active) = self.active.clone() {
                self.draw_split(f, area, &active, &badges, now);
            } else {
                self.draw_dashboard(f, area, &badges, now);
            }
            if let Some(modal) = &self.modal {
                modal.render(
                    f,
                    area,
                    &self.icons,
                    self.theme.bg,
                    self.theme.sel,
                    &self.cfg.keys,
                );
            }
        })?;
        Ok(())
    }

    fn draw_dashboard(
        &self,
        f: &mut Frame,
        area: Rect,
        badges: &HashMap<SessionKey, BadgeInfo>,
        now: chrono::DateTime<Utc>,
    ) {
        // The full dashboard IS the tree/browser chrome (no pane beside it):
        // paint it with sidebar_bg so it matches the sidebar's look and never
        // visually jumps when a session opens into split view.
        if self.theme.sidebar_bg != Color::Reset {
            f.render_widget(
                Block::new().style(Style::new().bg(self.theme.sidebar_bg)),
                area,
            );
        }
        let [header, head_rule, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(dashboard::rule_line(&self.theme, head_rule.width)),
            head_rule,
        );

        let visible_sessions = self
            .rows
            .iter()
            .filter(|r| r.session_key().is_some())
            .count();
        let mut head_spans = vec![
            Span::styled(
                " vag ",
                Style::new()
                    .fg(Color::Black)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {} sessions · {} folders · {} open",
                visible_sessions,
                self.state.folders.len(),
                self.runtimes.len()
            )),
        ];
        if let Some(root) = &self.scope_root {
            let label = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string());
            head_spans.push(if self.scoped {
                Span::styled(format!("  [repo: {label}]"), Style::new().fg(Color::Green))
            } else {
                Span::styled("  [all projects]", Style::new().fg(self.theme.dim))
            });
        }
        // The no-agents notice outlives scan warnings (which are replaced
        // every rescan), so it renders first in the warnings slot.
        if let Some(notice) = &self.agent_notice {
            head_spans.push(Span::styled(
                format!("  ⚠ {notice}"),
                Style::new().fg(Color::Yellow),
            ));
        }
        if !self.warnings.is_empty() {
            head_spans.push(Span::styled(
                format!("  ⚠ {}", self.warnings[0]),
                Style::new().fg(Color::Yellow),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(head_spans)), header);

        let ctx = RowCtx {
            state: &self.state,
            sessions: &self.sessions,
            badges,
            now,
            active: self.active.as_ref(),
            open_order: &self.open_order,
            spin_frame: self.spin_frame(),
            icons: &self.icons,
            provisional_labels: &self.provisional_labels,
            theme: self.theme,
        };
        // The ⚙ settings footer is pinned BELOW the scrolling list (its own
        // region — never a viewport slot), set off by a hairline.
        let [list, set_rule, set_row] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(body);
        f.render_widget(
            Paragraph::new(dashboard::rule_line(&self.theme, set_rule.width)),
            set_rule,
        );
        if let Some(buf) = &self.editbuf {
            dashboard::render_editbuf(f, list, buf, &self.theme);
        } else if self.rows.is_empty() {
            let msg = if self.filter.is_some() {
                "no matches — esc clears the filter"
            } else {
                "no sessions found — press n to start one"
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::new().fg(self.theme.dim),
                )))
                .centered(),
                list,
            );
        } else {
            dashboard::render_rows(f, list, &self.rows, self.cursor, &ctx, false, true);
        }
        f.render_widget(
            Paragraph::new(dashboard::settings_line(
                &self.icons,
                &self.theme,
                self.editbuf.is_none() && self.settings_selected(),
                set_row.width,
                self.cfg.keys.get(KeyAction::Settings),
            )),
            set_row,
        );

        let mut foot = String::new();
        if let Some(fil) = &self.filter {
            let editing = self.filter_edit.is_some();
            foot.push_str(&format!("/{fil}{} · ", if editing { "▌" } else { "" }));
        }
        let k = |a: KeyAction| self.cfg.keys.get(a);
        foot.push_str(&format!(
            "enter:open {}:new {}:folder {}:fork {}:move {}:rename {}:machine {}:shell {}:hide \
             {}:scope {}:zoom {}:help {}:quit",
            k(KeyAction::NewSession),
            k(KeyAction::NewFolder),
            k(KeyAction::Fork),
            k(KeyAction::MoveSession),
            k(KeyAction::Rename),
            k(KeyAction::AddMachine),
            k(KeyAction::Shell),
            k(KeyAction::Hide),
            k(KeyAction::Scope),
            k(KeyAction::Zoom),
            k(KeyAction::Help),
            k(KeyAction::Quit),
        ));
        let status_line = self
            .status
            .as_ref()
            .map(|(s, _)| {
                Line::from(Span::styled(
                    format!(" {s}"),
                    Style::new().fg(Color::Yellow),
                ))
            })
            .unwrap_or_else(|| Line::raw(""));
        let hints = if let Some(buf) = &self.editbuf {
            dashboard::editbuf_footer_line(buf)
        } else {
            Line::from(Span::styled(
                format!(" {foot}"),
                Style::new().fg(self.theme.dim),
            ))
        };
        let [l1, l2] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(footer);
        f.render_widget(Paragraph::new(status_line), l1);
        f.render_widget(Paragraph::new(hints), l2);
    }

    fn draw_split(
        &self,
        f: &mut Frame,
        area: Rect,
        active: &SessionKey,
        badges: &HashMap<SessionKey, BadgeInfo>,
        now: chrono::DateTime<Utc>,
    ) {
        let (side, main) = self.split_areas(area);
        if let Some(side) = side {
            self.draw_sidebar(f, side, active, badges, now);
        }

        // pane chrome: bordered box, or a tmux-style title bar
        let focused = self.focus == Focus::Pane;
        let inner = self.pane_inner(main);
        match self.cfg.ui.pane {
            PaneStyle::Border => {
                let accent =
                    dashboard::session_color(&self.state, active).unwrap_or(self.theme.accent);
                let border_style = if focused {
                    Style::new().fg(accent)
                } else {
                    Style::new().fg(self.theme.dim)
                };
                let block = Block::new()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(self.pane_title(active));
                f.render_widget(block, main);
            }
            PaneStyle::Titlebar => {
                let bar = Rect {
                    height: 1.min(main.height),
                    ..main
                };
                f.render_widget(self.pane_titlebar(active, bar.width, focused), bar);
            }
        }
        if let Some(rt) = self.runtimes.get(active) {
            pane::render(rt, inner, f.buffer_mut(), focused);
        } else {
            f.render_widget(
                Paragraph::new("no process — enter to start").centered(),
                inner,
            );
        }

        if self.tree_float {
            self.draw_tree_float(f, area, active, badges, now);
        }
    }

    fn draw_sidebar(
        &self,
        f: &mut Frame,
        side: Rect,
        active: &SessionKey,
        badges: &HashMap<SessionKey, BadgeInfo>,
        now: chrono::DateTime<Utc>,
    ) {
        let mut side_block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(self.theme.dim));
        if self.theme.sidebar_bg != Color::Reset {
            side_block = side_block.style(Style::new().bg(self.theme.sidebar_bg));
        }
        let side_inner = side_block.inner(side);
        f.render_widget(side_block, side);
        // Header, hairline, scrolling rows, hairline, PINNED ⚙ settings
        // line, footer hints — the rules mark where chrome ends and the
        // tree begins; the settings line sits outside the row viewport so
        // it's always visible however far the list scrolls.
        let [sb_head, sb_hrule, sb_body, sb_srule, sb_set, sb_foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(side_inner);
        f.render_widget(
            Paragraph::new(dashboard::rule_line(&self.theme, sb_hrule.width)),
            sb_hrule,
        );
        f.render_widget(
            Paragraph::new(dashboard::rule_line(&self.theme, sb_srule.width)),
            sb_srule,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    " {}",
                    truncate_middle(
                        &self.launch_dir,
                        side_inner.width.saturating_sub(2) as usize
                    )
                ),
                Style::new()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))),
            sb_head,
        );
        let ctx = RowCtx {
            state: &self.state,
            sessions: &self.sessions,
            badges,
            now,
            active: Some(active),
            open_order: &self.open_order,
            spin_frame: self.spin_frame(),
            icons: &self.icons,
            provisional_labels: &self.provisional_labels,
            theme: self.theme,
        };
        if let Some(buf) = &self.editbuf {
            dashboard::render_editbuf(f, sb_body, buf, &self.theme);
        } else {
            dashboard::render_rows(
                f,
                sb_body,
                &self.rows,
                self.cursor,
                &ctx,
                true,
                self.focus == Focus::Tree,
            );
        }
        f.render_widget(
            Paragraph::new(dashboard::settings_line(
                &self.icons,
                &self.theme,
                self.editbuf.is_none() && self.focus == Focus::Tree && self.settings_selected(),
                sb_set.width,
                self.cfg.keys.get(KeyAction::Settings),
            )),
            sb_set,
        );
        let foot_line = if let Some(buf) = &self.editbuf
            && self.focus == Focus::Tree
        {
            dashboard::editbuf_footer_line(buf)
        } else {
            let foot = if self.focus == Focus::Tree {
                format!(
                    " enter:show esc:dashboard {}:help",
                    self.cfg.keys.get(KeyAction::Help)
                )
            } else {
                " (pane focused)".to_string()
            };
            Line::from(Span::styled(foot, Style::new().fg(self.theme.dim)))
        };
        f.render_widget(Paragraph::new(foot_line), sb_foot);
        // Overlay status on the footer when present — but never over an
        // in-progress `:cmd` / insert-mode indicator the user is typing.
        let mid_edit = self
            .editbuf
            .as_ref()
            .is_some_and(|b| *b.mode() != EditMode::Normal);
        if let Some((s, _)) = &self.status
            && !mid_edit
        {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {s}"),
                    Style::new().fg(Color::Yellow),
                ))),
                sb_foot,
            );
        }
    }

    /// The oil.nvim-style floating tree: a centered overlay over the
    /// full-width pane, showing the same wide row list as the dashboard.
    fn draw_tree_float(
        &self,
        f: &mut Frame,
        area: Rect,
        active: &SessionKey,
        badges: &HashMap<SessionKey, BadgeInfo>,
        now: chrono::DateTime<Utc>,
    ) {
        let r = float_rect(area);
        f.render_widget(Clear, r);
        let mut block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.theme.accent))
            .title(" sessions ");
        if self.theme.sidebar_bg != Color::Reset {
            block = block.style(Style::new().bg(self.theme.sidebar_bg));
        }
        let inner = block.inner(r);
        f.render_widget(block, r);
        if inner.height == 0 {
            return;
        }
        let [body, set_row, foot] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let ctx = RowCtx {
            state: &self.state,
            sessions: &self.sessions,
            badges,
            now,
            active: Some(active),
            open_order: &self.open_order,
            spin_frame: self.spin_frame(),
            icons: &self.icons,
            provisional_labels: &self.provisional_labels,
            theme: self.theme,
        };
        if let Some(buf) = &self.editbuf {
            dashboard::render_editbuf(f, body, buf, &self.theme);
        } else {
            dashboard::render_rows(f, body, &self.rows, self.cursor, &ctx, false, true);
        }
        f.render_widget(
            Paragraph::new(dashboard::settings_line(
                &self.icons,
                &self.theme,
                self.editbuf.is_none() && self.settings_selected(),
                set_row.width,
                self.cfg.keys.get(KeyAction::Settings),
            )),
            set_row,
        );
        // footer: status wins the single line when set, hints otherwise; an
        // in-progress `:cmd` / insert-mode indicator outranks the status.
        let foot_line = if let Some(buf) = &self.editbuf
            && (self.status.is_none() || *buf.mode() != EditMode::Normal)
        {
            dashboard::editbuf_footer_line(buf)
        } else if let Some((s, _)) = &self.status {
            Line::from(Span::styled(
                format!(" {s}"),
                Style::new().fg(Color::Yellow),
            ))
        } else {
            let mut hints = String::from(" ");
            if let Some(fil) = &self.filter {
                let editing = self.filter_edit.is_some();
                hints.push_str(&format!("/{fil}{} · ", if editing { "▌" } else { "" }));
            }
            hints.push_str(&format!(
                "enter:open {}:dashboard esc:close {}:help",
                self.cfg.keys.get(KeyAction::BindDir),
                self.cfg.keys.get(KeyAction::Help)
            ));
            Line::from(Span::styled(hints, Style::new().fg(self.theme.dim)))
        };
        f.render_widget(Paragraph::new(foot_line), foot);
    }

    /// tmux-style full-width bar for PaneStyle::Titlebar: title on the
    /// left, detach hint pinned right, one background across the line.
    fn pane_titlebar(&self, key: &SessionKey, width: u16, focused: bool) -> Paragraph<'static> {
        let meta = self.meta_idx.get(key).map(|i| &self.sessions[*i]);
        let name = meta
            .map(|m| dashboard::display_title(&self.state, m))
            .or_else(|| self.provisional_labels.get(key).cloned())
            .or_else(|| self.runtimes.get(key).and_then(|rt| rt.title()))
            .unwrap_or_else(|| "starting…".into());
        let exited = if self.exited.contains(key) {
            format!(
                " [exited — {} closes]",
                self.cfg.keys.get(KeyAction::CloseRuntime)
            )
        } else {
            String::new()
        };
        let left = format!(" {} {}{}", self.icons.agent(key.agent), name, exited);

        // Identity cluster on the LEFT (project/@machine, branch — bold),
        // status cluster on the RIGHT (active working time, created) next
        // to the detach hint, tmux-style. Drop priority (lowest goes first
        // on narrow bars): branch(0) → created(1) → location(2) → turn(3).
        let mut parts: Vec<BarPart> = Vec::new();
        if let Some(rname) = self.state.session(key).and_then(|r| r.remote.clone()) {
            parts.push(BarPart::left(2, format!("@{rname}"), false));
        } else if let Some(m) = meta {
            parts.push(BarPart::left(2, m.project_label(), false));
        }
        if let Some(b) = meta.and_then(|m| m.git_branch.clone()) {
            parts.push(BarPart::left(0, format!("{} {b}", self.icons.branch), true));
        }
        match self.activity.get(key) {
            Some(a) if matches!(a.turn, Turn::Working { .. }) => {
                parts.push(BarPart::right(
                    3,
                    format!("working {}", dashboard::fmt_work_dur(a.active_time())),
                ));
            }
            Some(a) => {
                if let Turn::Done { finished, .. } = a.turn {
                    parts.push(BarPart::right(
                        3,
                        format!("done {}", dashboard::fmt_work_dur(finished.elapsed())),
                    ));
                }
            }
            None => {}
        }
        if let Some(c) = meta.and_then(|m| m.created) {
            let rel = dashboard::rel_time(Some(c), Utc::now());
            parts.push(BarPart::right(
                1,
                if self.icons.clock.is_empty() {
                    format!("created {rel} ago")
                } else {
                    format!("{} {rel}", self.icons.clock)
                },
            ));
        }

        let right_hint = format!("{}:tree ", self.cfg.keys.detach.label());
        let kept = fit_bar_parts(
            &parts,
            (width as usize)
                .saturating_sub(left.chars().count())
                .saturating_sub(right_hint.chars().count()),
        );
        let used: usize = kept
            .iter()
            .map(|&i| parts[i].text.chars().count() + 3)
            .sum();
        // Push the hint to the right edge (char-count ≈ width is fine for
        // the bar; overlong titles just crowd the hint out).
        let pad = (width as usize)
            .saturating_sub(left.chars().count())
            .saturating_sub(used)
            .saturating_sub(right_hint.chars().count());
        let accent = dashboard::session_color(&self.state, key);
        let style = if focused {
            Style::new()
                .fg(Color::Black)
                .bg(accent.unwrap_or(self.theme.accent))
                .add_modifier(Modifier::BOLD)
        } else {
            // Uncolored sessions keep the quiet gray; colored ones show
            // their accent as the bar text tint.
            Style::new()
                .fg(accent.unwrap_or(Color::Gray))
                .bg(self.theme.surface)
        };
        let hint_style = if focused {
            style.remove_modifier(Modifier::BOLD)
        } else {
            style.fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(left, style)];
        for &i in &kept {
            let p = &parts[i];
            if p.right_side {
                continue;
            }
            spans.push(Span::styled(" · ".to_string(), hint_style));
            let s = if p.bold {
                hint_style.add_modifier(Modifier::BOLD)
            } else {
                hint_style
            };
            spans.push(Span::styled(p.text.clone(), s));
        }
        spans.push(Span::styled(" ".repeat(pad), style));
        for &i in &kept {
            let p = &parts[i];
            if !p.right_side {
                continue;
            }
            spans.push(Span::styled(p.text.clone(), hint_style));
            spans.push(Span::styled(" · ".to_string(), hint_style));
        }
        spans.push(Span::styled(right_hint, hint_style));
        Paragraph::new(Line::from(spans)).style(style)
    }

    fn pane_title(&self, key: &SessionKey) -> String {
        let name = self
            .meta_idx
            .get(key)
            .map(|i| dashboard::display_title(&self.state, &self.sessions[*i]))
            .or_else(|| self.provisional_labels.get(key).cloned())
            .or_else(|| self.runtimes.get(key).and_then(|rt| rt.title()))
            .unwrap_or_else(|| "starting…".into());
        let exited = if self.exited.contains(key) {
            " [exited — w closes]"
        } else {
            ""
        };
        let detach = self.cfg.keys.detach.label();
        format!(
            " {} {}{} · {}:tree ",
            self.icons.agent(key.agent),
            name,
            exited,
            detach
        )
    }
}

/// Visible tree rows → edit-buffer lines: the "+ new session" row is
/// dropped, the Inbox header and provisional sessions (no scan entry yet)
/// are readonly, folders carry an oil-style trailing slash, and session
/// text is the current display title. Machine headers become readonly
/// LineId::Inbox lines ("<name>/ (machine)") so the folder-context
/// semantics of the lines below them stay None; shell panes are readonly
/// (labelled) provisional lines.
fn edit_lines_from_rows(
    rows: &[Row],
    state: &VagState,
    sessions: &[SessionMeta],
    provisional_labels: &HashMap<SessionKey, String>,
) -> Vec<EditLine> {
    rows.iter()
        .filter_map(|r| match r {
            // Rendering-only rows carry no editable identity.
            Row::NewSession | Row::Spacer | Row::Empty { .. } => None,
            Row::Inbox { .. } => Some(EditLine {
                id: LineId::Inbox,
                text: "Inbox".into(),
                depth: 0,
                readonly: true,
                copied: false,
            }),
            Row::Machine { name, .. } => Some(EditLine {
                id: LineId::Inbox,
                text: format!("{name}/ (machine)"),
                depth: 0,
                readonly: true,
                copied: false,
            }),
            Row::Folder {
                id, depth, name, ..
            } => Some(EditLine {
                id: LineId::Folder(id.clone()),
                text: format!("{name}/"),
                depth: *depth,
                readonly: false,
                copied: false,
            }),
            Row::Session {
                key,
                depth,
                meta_idx,
            } => {
                let (text, readonly) = match meta_idx {
                    // Shell panes are ephemeral: visible in the buffer (with
                    // their label) but never editable.
                    Some(i) if key.agent != AgentKind::Shell => {
                        (dashboard::display_title(state, &sessions[*i]), false)
                    }
                    _ => (
                        provisional_labels
                            .get(key)
                            .cloned()
                            .unwrap_or_else(|| "(starting…)".to_string()),
                        true,
                    ),
                };
                Some(EditLine {
                    id: LineId::Session(key.clone()),
                    text,
                    depth: *depth,
                    readonly,
                    copied: false,
                })
            }
        })
        .collect()
}

/// Centered rect for the floating tree, recomputed every draw as a pure
/// function of the screen area: 4/5 of the width capped at 96 columns,
/// 7/10 of the height, with sane minimums (30x8) that a tiny terminal
/// clamps back down to its own size.
fn float_rect(area: Rect) -> Rect {
    let w = ((u32::from(area.width) * 4 / 5).min(96) as u16)
        .max(30)
        .min(area.width);
    let h = ((u32::from(area.height) * 7 / 10) as u16)
        .max(8)
        .min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// State writes for a freshly launched remote session: folder binding (when
/// the folder still exists), last_opened, and the remote identity that makes
/// the entry gc-exempt and resumable (claude) / attach-only (codex).
fn record_remote_session(
    state: &mut VagState,
    key: &SessionKey,
    folder: Option<&str>,
    remote: &str,
    dir: &str,
) {
    if let Some(f) = folder
        && state.folder(f).is_some()
    {
        let _ = state.set_session_folder(key, Some(f));
    }
    let r = state.session_mut(key);
    r.last_opened = Some(Utc::now());
    r.remote = Some(remote.to_string());
    r.remote_cwd = Some(dir.to_string());
}

/// Synthesize a SessionMeta per remote state entry so remote sessions
/// render, filter and sort like scanned rows. Keys already in `existing`
/// are skipped (shouldn't happen — remote claude ids are fresh uuids and
/// codex ids are synthetic — but a collision must not produce twins).
fn synthesize_remote_metas(state: &VagState, existing: &[SessionMeta]) -> Vec<SessionMeta> {
    let have: HashSet<&SessionKey> = existing.iter().map(|m| &m.key).collect();
    state
        .sessions
        .iter()
        .filter_map(|(ks, r)| {
            let remote = r.remote.as_ref()?;
            let key = SessionKey::parse(ks)?;
            if have.contains(&key) {
                return None;
            }
            Some(SessionMeta {
                title: None,
                preview: Some(format!("{} @ {}", key.agent.label(), remote)),
                cwd: PathBuf::from(r.remote_cwd.as_deref().unwrap_or("~")),
                created: None,
                last_activity: r.last_opened,
                // Remote stores are unscannable; interaction time is the
                // best "last message sent" proxy we have.
                last_user_activity: r.last_opened,
                archived: false,
                source_path: PathBuf::new(),
                git_branch: None,
                key,
            })
        })
        .collect()
}

/// Env for shell panes — same pair actions.rs gives agent children.
fn shell_env() -> Vec<(String, String)> {
    vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ]
}

/// Local ephemeral shell: `$SHELL` (fallback /bin/sh) in `cwd`.
fn local_shell_spec(cwd: PathBuf) -> actions::SpawnSpec {
    let program = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    actions::SpawnSpec {
        program,
        args: vec![],
        cwd,
        env: shell_env(),
    }
}

/// Remote ephemeral shell: a plain `ssh -t <host>` login shell — the
/// no-agent escape hatch (ssh's own credential prompts render in the pane).
fn ssh_shell_spec(host: &str) -> actions::SpawnSpec {
    actions::SpawnSpec {
        program: "ssh".into(),
        args: vec!["-t".into(), host.to_string()],
        cwd: dirs::home_dir().unwrap_or_else(std::env::temp_dir),
        env: shell_env(),
    }
}

/// Walk up from `start` looking for a `.git` entry (dir, or file for
/// worktrees). No git subprocess needed.
fn detect_git_root(start: &std::path::Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// One optional titlebar segment: identity parts sit LEFT of the pad,
/// status parts RIGHT of it (before the detach hint). `prio`: lowest is
/// dropped first when the bar runs out of columns; display order is kept.
struct BarPart {
    prio: u8,
    right_side: bool,
    bold: bool,
    text: String,
}

impl BarPart {
    fn left(prio: u8, text: String, bold: bool) -> BarPart {
        BarPart {
            prio,
            right_side: false,
            bold,
            text,
        }
    }

    fn right(prio: u8, text: String) -> BarPart {
        BarPart {
            prio,
            right_side: true,
            bold: false,
            text,
        }
    }
}

/// Kept indices (original order) after dropping lowest-priority parts until
/// everything (each costing its text + a " · " separator) fits in `budget`.
fn fit_bar_parts(parts: &[BarPart], budget: usize) -> Vec<usize> {
    let mut keep: Vec<bool> = vec![true; parts.len()];
    loop {
        let used: usize = parts
            .iter()
            .enumerate()
            .filter(|(i, _)| keep[*i])
            .map(|(_, p)| p.text.chars().count() + 3)
            .sum();
        if used <= budget {
            return (0..parts.len()).filter(|&i| keep[i]).collect();
        }
        let Some(drop_idx) = (0..parts.len())
            .filter(|&i| keep[i])
            .min_by_key(|&i| parts[i].prio)
        else {
            return Vec::new();
        };
        keep[drop_idx] = false;
    }
}

/// `/Users/me/x/y` → `~/x/y` for display.
fn tilde_shorten(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if let Some(rest) = s.strip_prefix(&h) {
            if rest.is_empty() {
                return "~".into();
            }
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    s
}

/// Keep the tail (the informative part of a path) when truncating.
fn truncate_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    let tail: String = s
        .chars()
        .skip(n.saturating_sub(max.saturating_sub(1)))
        .collect();
    format!("…{tail}")
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETACH: u8 = 0x11; // default ctrl-q
    const TOGGLE_SIDEBAR: u8 = 0x05; // default ctrl-e
    const FOCUS_TREE: u8 = 0x08; // default ctrl-h
    const FOCUS_PANE_CTRL: char = 'l'; // default ctrl-l

    /// A real (never-drawn-to) terminal for exercising on_key/on_stdin in
    /// tests — none of the new ctrl-e/h/l code paths touch it, they only
    /// need a valid &mut Terminal<Backend> to satisfy the signature.
    fn test_terminal() -> Terminal<Backend> {
        Terminal::new(CrosstermBackend::new(std::io::stdout())).unwrap()
    }

    // ---------- PasteGate ----------

    #[test]
    fn paste_gate_protects_detach_inside_paste() {
        let mut g = PasteGate::default();
        let bytes = b"\x1b[200~ab\x11cd\x1b[201~";
        let flags: Vec<bool> = bytes.iter().map(|&b| g.feed(b)).collect();
        let pos = bytes.iter().position(|&b| b == DETACH).unwrap();
        assert!(flags[pos], "detach byte inside paste must be protected");
        assert!(!g.in_paste, "paste closed by end marker");
    }

    #[test]
    fn paste_gate_detach_outside_paste_not_protected() {
        let mut g = PasteGate::default();
        for &b in b"hello " {
            g.feed(b);
        }
        assert!(!g.feed(DETACH), "detach outside paste must still detach");
        // …including right after a complete paste
        for &b in b"\x1b[200~x\x1b[201~" {
            g.feed(b);
        }
        assert!(!g.feed(DETACH));
    }

    #[test]
    fn paste_gate_markers_split_across_chunks() {
        let mut g = PasteGate::default();
        // start marker split across two reads
        for &b in b"\x1b[2" {
            g.feed(b);
        }
        for &b in b"00~" {
            g.feed(b);
        }
        assert!(g.feed(DETACH), "inside paste after split start marker");
        // end marker split across two reads
        for &b in b"\x1b[20" {
            g.feed(b);
        }
        for &b in b"1~" {
            g.feed(b);
        }
        assert!(!g.feed(DETACH), "paste ended after split end marker");
    }

    #[test]
    fn paste_gate_aborted_marker_keeps_detach_live() {
        let mut g = PasteGate::default();
        for &b in b"\x1b[20X" {
            g.feed(b);
        }
        assert!(!g.feed(DETACH));
    }

    #[test]
    fn paste_gate_esc_restarts_marker_match() {
        let mut g = PasteGate::default();
        for &b in b"\x1b[2\x1b[200~" {
            g.feed(b);
        }
        assert!(g.feed(DETACH));
    }

    // ---------- zoom mode replay ----------

    #[test]
    fn zoom_mode_replay_emits_enabled_modes_only() {
        let mode = TermMode::BRACKETED_PASTE | TermMode::SGR_MOUSE | TermMode::ALTERNATE_SCROLL;
        let s = String::from_utf8(zoom_mode_replay(mode)).unwrap();
        assert!(s.contains("\x1b[?2004h"));
        assert!(s.contains("\x1b[?1006h"));
        assert!(s.contains("\x1b[?1007h"));
        assert!(!s.contains("\x1b[?1004h"));
        assert!(!s.contains("\x1b[?1000h"));
        assert!(!s.contains("\x1b[?1h"));
    }

    #[test]
    fn zoom_mode_replay_empty_without_modes() {
        assert!(zoom_mode_replay(TermMode::NONE).is_empty());
    }

    // ---------- cursor anchoring ----------

    fn skey(id: &str) -> SessionKey {
        SessionKey::new(AgentKind::Claude, id.to_string())
    }

    fn srow(id: &str) -> Row {
        Row::Session {
            key: skey(id),
            depth: 1,
            meta_idx: None,
        }
    }

    #[test]
    fn cursor_anchor_survives_reorder() {
        let inbox = Row::Inbox {
            count: 2,
            collapsed: false,
        };
        let old = [inbox.clone(), srow("a"), srow("b")];
        let anchor = row_anchor(&old[2]); // cursor on session b
        let new = [inbox, srow("b"), srow("a")];
        assert_eq!(locate_row(&new, &anchor), Some(1));
        assert_eq!(locate_row(&new, &RowAnchor::Inbox), Some(0));
        assert_eq!(locate_row(&new, &RowAnchor::Folder("f1".into())), None);
    }

    #[test]
    fn cursor_anchor_matches_folder_rows() {
        let folder = Row::Folder {
            id: "f1".into(),
            depth: 0,
            name: "work".into(),
            collapsed: false,
            session_count: 0,
            default_dir: None,
            scope_label: None,
        };
        let rows = vec![srow("a"), folder.clone()];
        assert_eq!(locate_row(&rows, &row_anchor(&folder)), Some(1));
    }

    // ---------- app-level behavior ----------

    /// Redirect any accidental state.save() to a throwaway dir so a broken
    /// guard can never write the user's real state.json from a test.
    fn isolate_data_dir() {
        use std::sync::OnceLock;
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"));
        // SAFETY: test-only; no other test in this crate reads
        // XDG_DATA_HOME concurrently.
        unsafe { std::env::set_var("XDG_DATA_HOME", dir.path()) };
    }

    #[test]
    fn delete_session_claude_hides_remote_drops_entry() {
        let (mut app, _rx) = test_app();
        // claude local: x = remove from listing (hidden), entry kept
        let claude = SessionKey::new(AgentKind::Claude, "aaaa-bbbb");
        app.commit_delete_session(claude.clone());
        assert!(
            app.state
                .session(&claude)
                .map(|r| r.hidden)
                .unwrap_or(false)
        );
        // remote: x = drop the vag state entry entirely
        let remote = SessionKey::new(AgentKind::Claude, "cccc-dddd");
        app.state.session_mut(&remote).remote = Some("gpu".into());
        app.commit_delete_session(remote.clone());
        assert!(app.state.session(&remote).is_none());
    }

    #[test]
    fn bar_parts_fit_by_dropping_lowest_priority_first() {
        let parts = vec![
            BarPart::left(2, "vibe-aggregator".into(), false),
            BarPart::left(0, "⎇ main".into(), true),
            BarPart::right(3, "working 4m12s".into()),
            BarPart::right(1, "created 3h ago".into()),
        ];
        let cost = |idx: &[usize]| -> usize {
            idx.iter().map(|&i| parts[i].text.chars().count() + 3).sum()
        };
        // Everything fits.
        let all = fit_bar_parts(&parts, 200);
        assert_eq!(all, vec![0, 1, 2, 3]);
        // Tight: branch (prio 0) dropped first, order preserved.
        let kept = fit_bar_parts(&parts, cost(&all) - 1);
        assert_eq!(kept, vec![0, 2, 3]);
        // Tighter: created (prio 1) goes next.
        let kept2 = fit_bar_parts(&parts, cost(&kept) - 1);
        assert_eq!(kept2, vec![0, 2]);
        // Tightest: only the turn survives; then nothing.
        assert_eq!(fit_bar_parts(&parts, 17), vec![2]);
        assert_eq!(fit_bar_parts(&parts, 3), Vec::<usize>::new());
        assert_eq!(fit_bar_parts(&[], 50), Vec::<usize>::new());
    }

    fn test_app() -> (App, Receiver<AppEvent>) {
        isolate_data_dir();
        let (tx, rx) = unbounded();
        let mut app = App::new(Config::default(), VagState::default(), tx);
        // Tests may themselves run inside tmux — never let an edge-nav key
        // actually move the developer's panes.
        app.tmux_nav = false;
        (app, rx)
    }

    fn spawn_cat(key: &SessionKey, events: &Sender<(SessionKey, RuntimeEvent)>) -> SessionRuntime {
        let spec = actions::SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            cwd: std::env::temp_dir(),
            env: vec![],
        };
        SessionRuntime::spawn(
            key.clone(),
            &spec,
            PaneSize { rows: 10, cols: 40 },
            events.clone(),
        )
        .expect("spawning /bin/cat test child")
    }

    #[test]
    fn id_resolution_collision_keeps_both_runtimes_alive() {
        let (mut app, _rx) = test_app();
        let real = SessionKey::new(AgentKind::Codex, "real-id".to_string());
        let prov = SessionKey::new(AgentKind::Codex, "pending-abc123".to_string());
        let (es, _er) = unbounded();
        app.runtimes.insert(real.clone(), spawn_cat(&real, &es));
        app.runtimes.insert(prov.clone(), spawn_cat(&prov, &es));
        app.open_order.push(real.clone());
        app.open_order.push(prov.clone());
        app.pending
            .insert(prov.clone(), PendingCtx { folder: None });

        app.on_id_resolved(prov.clone(), Some("real-id".to_string()));

        // Neither runtime was clobbered; the collided pane stays
        // provisional (and pending, so its row stays visible).
        assert!(app.runtimes.get(&real).is_some_and(|r| r.is_running()));
        assert!(app.runtimes.get(&prov).is_some_and(|r| r.is_running()));
        assert!(app.pending.contains_key(&prov));
        assert_eq!(app.open_order, vec![real.clone(), prov.clone()]);
        assert!(app.status.is_some(), "collision must be surfaced");
        assert!(app.state.sessions.is_empty(), "no state written for real");

        for (_, mut rt) in app.runtimes.drain() {
            rt.kill();
        }
    }

    #[test]
    fn discovery_timeout_keeps_provisional_row_visible() {
        let (mut app, _rx) = test_app();
        let prov = SessionKey::new(AgentKind::Codex, "pending-xyz789".to_string());
        app.pending
            .insert(prov.clone(), PendingCtx { folder: None });

        app.on_id_resolved(prov.clone(), None);

        assert!(app.pending.contains_key(&prov), "pending entry retained");
        assert!(
            app.rows.iter().any(|r| r.session_key() == Some(&prov)),
            "provisional row still rendered after timeout"
        );
    }

    #[test]
    fn state_actions_guarded_on_provisional_rows() {
        let (mut app, _rx) = test_app();
        let prov = SessionKey::new(AgentKind::Codex, "pending-xyz789".to_string());
        app.pending
            .insert(prov.clone(), PendingCtx { folder: None });
        app.rebuild_rows();
        app.cursor = locate_row(&app.rows, &RowAnchor::Session(prov.clone())).unwrap();

        app.start_move_session();
        assert!(app.modal.is_none(), "move must be blocked");
        app.start_rename();
        assert!(app.modal.is_none(), "rename must be blocked");
        app.start_archive();
        assert!(app.modal.is_none(), "archive must be blocked");
        // toggle_hidden (d) shares the same guard; exercise it directly so
        // the test never risks a real state.json write on regression.
        assert!(app.guard_provisional(&prov));
        assert!(!app.guard_provisional(&skey("resolved-id")));
        assert!(
            app.state.sessions.is_empty(),
            "no pending-… key may reach state.json"
        );
        assert!(app.status.is_some());
    }

    // ---------- floating tree (ui.tree = "float") ----------

    fn float_app() -> (App, Receiver<AppEvent>) {
        isolate_data_dir();
        let mut cfg = Config::default();
        cfg.ui.tree = TreeMode::Float;
        let (tx, rx) = unbounded();
        (App::new(cfg, VagState::default(), tx), rx)
    }

    #[test]
    fn float_rect_typical_caps_and_clamps() {
        // 120x40: 4/5 of the width hits the 96 cap exactly, 7/10 height.
        let r = float_rect(Rect::new(0, 0, 120, 40));
        assert_eq!((r.width, r.height), (96, 28));
        assert_eq!((r.x, r.y), (12, 6)); // centered
        // very wide screens cap at 96 columns
        let r = float_rect(Rect::new(0, 0, 300, 80));
        assert_eq!(r.width, 96);
        assert_eq!(r.height, 56);
        // narrow-but-usable screens keep the 30x8 minimum
        let r = float_rect(Rect::new(0, 0, 34, 10));
        assert_eq!((r.width, r.height), (30, 8));
        // tiny screens clamp to the area (never overflow it)
        let r = float_rect(Rect::new(0, 0, 10, 5));
        assert_eq!((r.width, r.height), (10, 5));
        assert_eq!((r.x, r.y), (0, 0));
        // offset areas stay inside themselves
        let area = Rect::new(3, 2, 100, 30);
        let r = float_rect(area);
        assert!(r.x >= area.x && r.right() <= area.right());
        assert!(r.y >= area.y && r.bottom() <= area.bottom());
    }

    #[test]
    fn split_areas_sidebar_vs_float() {
        let area = Rect::new(0, 0, 120, 40);
        let (mut app, _rx) = test_app();
        let (side, main) = app.split_areas(area);
        let side = side.expect("sidebar mode has a sidebar");
        assert_eq!(side.width, app.cfg.ui.sidebar_width);
        assert_eq!(main.x, side.width);
        assert_eq!(main.width, 120 - side.width);

        app.cfg.ui.tree = TreeMode::Float;
        let (side, main) = app.split_areas(area);
        assert!(side.is_none(), "float mode has no sidebar");
        assert_eq!(main, area, "pane owns the full screen in float mode");
    }

    #[test]
    fn focus_tree_opens_float_only_with_active_session() {
        let (mut app, _rx) = float_app();
        app.focus_tree();
        assert!(!app.tree_float, "no active session: dashboard, no float");
        app.active = Some(skey("s1"));
        app.focus_tree();
        assert!(app.tree_float, "detach with a session active opens float");
        assert_eq!(app.focus, Focus::Tree);
        app.focus_pane();
        assert!(!app.tree_float, "pane focus dismisses the float");
        assert_eq!(app.focus, Focus::Pane);
    }

    #[test]
    fn focus_tree_never_floats_in_sidebar_mode() {
        let (mut app, _rx) = test_app();
        app.active = Some(skey("s1"));
        app.focus_tree();
        assert!(!app.tree_float);
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn esc_precedence_filter_then_float_then_dashboard() {
        let (mut app, _rx) = float_app();
        app.active = Some(skey("s1"));
        app.focus_tree();
        app.filter = Some("x".into());
        app.on_esc_tree();
        assert!(app.filter.is_none(), "esc clears the filter first");
        assert!(app.tree_float, "float stays open under the filter");
        app.on_esc_tree();
        assert!(!app.tree_float, "second esc closes the float");
        assert_eq!(app.focus, Focus::Pane, "…returning focus to the pane");
        assert!(app.active.is_some(), "esc never leaves the session");

        // sidebar mode keeps its historical esc: back to the dashboard
        let (mut app, _rx) = test_app();
        app.active = Some(skey("s1"));
        app.focus_tree();
        app.on_esc_tree();
        assert!(app.active.is_none());
    }

    #[test]
    fn b_binds_on_folder_rows_and_returns_to_dashboard_elsewhere() {
        let (mut app, _rx) = float_app();
        // tests run inside a git repo: unscope so the empty folder shows
        app.scoped = false;
        app.state.create_folder("work", None).unwrap();
        app.active = Some(skey("s1"));
        app.focus_tree();
        app.rebuild_rows();
        let fid = app
            .rows
            .iter()
            .position(|r| r.folder_id().is_some())
            .expect("folder row present");
        app.cursor = fid;
        app.bind_dir_or_dashboard();
        assert!(app.modal.is_some(), "folder row keeps the bind-dir modal");
        assert!(app.active.is_some(), "…and never leaves the session");
        app.modal = None;

        // non-folder row: back to the full dashboard, float state reset
        app.cursor = app
            .rows
            .iter()
            .position(|r| r.folder_id().is_none())
            .expect("non-folder row present");
        app.bind_dir_or_dashboard();
        assert!(app.active.is_none(), "b goes back to the dashboard");
        assert!(!app.tree_float, "float state resets with the dashboard");
        assert!(app.modal.is_none());
    }

    #[test]
    fn float_resets_when_last_session_closes() {
        let (mut app, _rx) = float_app();
        let k = skey("s1");
        app.open_order.push(k.clone());
        app.active = Some(k.clone());
        app.focus_tree();
        assert!(app.tree_float);
        app.close_runtime(&k);
        assert!(app.active.is_none());
        assert!(!app.tree_float, "no session left: float state resets");
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn float_pane_size_uses_full_width() {
        let (mut app, _rx) = float_app();
        // last_pane_rect wins when known — full-width inner rect in float
        // mode, regardless of the float being open.
        app.last_pane_rect = Some(Rect::new(1, 1, 118, 38));
        let s = app.pane_size();
        assert_eq!((s.cols, s.rows), (118, 38));
    }

    // ---------- edit mode ----------

    fn meta_for(key: &SessionKey, title: &str) -> SessionMeta {
        SessionMeta {
            key: key.clone(),
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

    fn index_sessions(app: &mut App) {
        app.meta_idx = app
            .sessions
            .iter()
            .enumerate()
            .map(|(i, m)| (m.key.clone(), i))
            .collect();
    }

    #[test]
    fn edit_lines_built_from_rows() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        let k1 = skey("aaa");
        let k2 = skey("bbb");
        let prov = skey("pending-xyz");
        let shell = SessionKey::new(AgentKind::Shell, "shell-abc123");
        st.session_mut(&k2).name_override = Some("renamed".into());
        let sessions = vec![meta_for(&k1, "one"), meta_for(&k2, "two")];
        let mut labels = HashMap::new();
        labels.insert(shell.clone(), "shell @ gpu".to_string());
        let rows = vec![
            Row::NewSession,
            Row::Inbox {
                count: 2,
                collapsed: false,
            },
            Row::Session {
                key: prov.clone(),
                depth: 1,
                meta_idx: None,
            },
            Row::Session {
                key: shell.clone(),
                depth: 1,
                meta_idx: None,
            },
            Row::Session {
                key: k2.clone(),
                depth: 1,
                meta_idx: Some(1),
            },
            Row::Machine {
                name: "gpu".into(),
                host: "user@gpu.example".into(),
                count: 0,
                collapsed: false,
            },
            Row::Folder {
                id: fid.clone(),
                depth: 0,
                name: "work".into(),
                collapsed: false,
                session_count: 1,
                default_dir: None,
                scope_label: None,
            },
            Row::Session {
                key: k1.clone(),
                depth: 1,
                meta_idx: Some(0),
            },
        ];
        let lines = edit_lines_from_rows(&rows, &st, &sessions, &labels);
        assert_eq!(lines.len(), 7, "the + new session row has no line");
        assert_eq!(lines[0].id, LineId::Inbox);
        assert_eq!(lines[0].text, "Inbox");
        assert_eq!(lines[0].depth, 0);
        assert!(lines[0].readonly);
        // provisional session: readonly placeholder
        assert_eq!(lines[1].id, LineId::Session(prov));
        assert_eq!(lines[1].text, "(starting…)");
        assert!(lines[1].readonly);
        // shell pane: readonly, labelled
        assert_eq!(lines[2].id, LineId::Session(shell));
        assert_eq!(lines[2].text, "shell @ gpu");
        assert!(lines[2].readonly);
        // session text is the display title (name_override respected)
        assert_eq!(lines[3].id, LineId::Session(k2));
        assert_eq!(lines[3].text, "renamed");
        assert_eq!(lines[3].depth, 1);
        assert!(!lines[3].readonly);
        // machine header: readonly, Inbox identity (None-folder context)
        assert_eq!(lines[4].id, LineId::Inbox);
        assert_eq!(lines[4].text, "gpu/ (machine)");
        assert_eq!(lines[4].depth, 0);
        assert!(lines[4].readonly);
        // folder: oil-style trailing slash, identity = folder id
        assert_eq!(lines[5].id, LineId::Folder(fid));
        assert_eq!(lines[5].text, "work/");
        assert!(!lines[5].readonly);
        assert_eq!(lines[6].id, LineId::Session(k1));
        assert_eq!(lines[6].text, "one");
    }

    #[test]
    fn edit_action_labels_human_readable() {
        let (mut app, _rx) = test_app();
        let fid = app.state.create_folder("work", None).unwrap();
        let k = skey("aaa");
        app.sessions = vec![meta_for(&k, "one")];
        index_sessions(&mut app);
        let label = |a: &EditAction| app.edit_action_label(a);
        assert_eq!(
            label(&EditAction::CreateFolder {
                parent: None,
                name: "x".into()
            }),
            "create folder x/"
        );
        assert_eq!(
            label(&EditAction::CreateFolder {
                parent: Some(fid.clone()),
                name: "x".into()
            }),
            "create folder x/ (in work/)"
        );
        assert_eq!(
            label(&EditAction::RenameFolder {
                id: fid.clone(),
                name: "code".into()
            }),
            "rename folder work/ → code/"
        );
        assert_eq!(
            label(&EditAction::DeleteFolder { id: fid.clone() }),
            "delete folder work/ (contents re-parent)"
        );
        assert_eq!(
            label(&EditAction::RenameSession {
                key: k.clone(),
                name: "two".into()
            }),
            "rename session one → two"
        );
        assert_eq!(
            label(&EditAction::RenameSession {
                key: k.clone(),
                name: String::new()
            }),
            "rename session one → (default title)"
        );
        assert_eq!(
            label(&EditAction::HideSession { key: k.clone() }),
            "hide one"
        );
        assert_eq!(
            label(&EditAction::MoveSession {
                key: k.clone(),
                folder: None
            }),
            "move one → Inbox"
        );
        assert_eq!(
            label(&EditAction::ForkInto {
                key: k.clone(),
                folder: Some(fid.clone())
            }),
            "fork one into work/"
        );
        assert_eq!(
            label(&EditAction::IgnoredLine {
                text: " junk ".into()
            }),
            "ignored line: junk"
        );
        // a key with no scan entry falls back to its raw id
        assert_eq!(
            label(&EditAction::HideSession { key: skey("zzz") }),
            "hide zzz"
        );
    }

    #[test]
    fn edit_confirm_msg_caps_at_twelve_action_lines() {
        let (app, _rx) = test_app();
        let actions: Vec<EditAction> = (0..20)
            .map(|i| EditAction::HideSession {
                key: skey(&format!("s{i}")),
            })
            .collect();
        let msg = app.edit_confirm_msg(&actions);
        let lines: Vec<&str> = msg.split('\n').collect();
        assert_eq!(lines[0], "apply 20 change(s)?");
        assert_eq!(lines.len(), 14); // header + 12 actions + "…and N more"
        assert_eq!(lines[13], "…and 8 more");
        let msg = app.edit_confirm_msg(&actions[..3]);
        assert_eq!(msg.split('\n').count(), 4); // header + all 3, no tail
    }

    #[test]
    fn enter_edit_mode_snapshots_rows_and_lands_on_cursor_row() {
        let (mut app, _rx) = test_app();
        app.scoped = false; // tests run inside a repo; /tmp cwds must show
        let k1 = skey("aaa");
        let k2 = skey("bbb");
        app.sessions = vec![meta_for(&k1, "one"), meta_for(&k2, "two")];
        index_sessions(&mut app);
        app.rebuild_rows();
        app.cursor = app.rows.len() - 1;
        app.enter_edit_mode();
        let buf = app.editbuf.as_ref().unwrap();
        assert_eq!(buf.lines().len(), app.rows.len() - 2); // minus button + spacer
        assert_eq!(buf.cursor().0, buf.lines().len() - 1);
        // re-entry is a no-op while a buffer is live
        app.enter_edit_mode();
        assert!(app.editbuf.is_some());
    }

    #[test]
    fn edit_save_empty_diff_saves_in_place_and_wq_leaves() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let k = skey("aaa");
        app.sessions = vec![meta_for(&k, "one")];
        index_sessions(&mut app);
        app.rebuild_rows();
        app.enter_edit_mode();
        app.start_edit_save(false);
        assert!(
            app.editbuf.is_some(),
            ":w with no changes stays in edit mode"
        );
        assert!(app.modal.is_none(), "no confirm for an empty diff");
        app.start_edit_save(true);
        assert!(
            app.editbuf.is_none(),
            ":wq with no changes leaves edit mode"
        );
    }

    #[test]
    fn edit_save_dirty_diff_opens_apply_confirm() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let k = skey("aaa");
        app.sessions = vec![meta_for(&k, "one")];
        index_sessions(&mut app);
        app.rebuild_rows();
        app.enter_edit_mode();
        let buf = app.editbuf.as_mut().unwrap();
        buf.handle_key(&Key::Char('j')); // Inbox → session line
        buf.handle_key(&Key::Char('d'));
        buf.handle_key(&Key::Char('d')); // cut it: HideSession on save
        app.start_edit_save(false);
        match &app.modal {
            Some(Modal::Confirm {
                msg,
                commit: Commit::ApplyEdits { actions, and_quit },
            }) => {
                assert!(!and_quit);
                assert_eq!(actions.len(), 1);
                assert!(matches!(&actions[0], EditAction::HideSession { key } if *key == k));
                assert!(msg.starts_with("apply 1 change(s)?"));
                assert!(msg.contains("hide one"));
            }
            other => panic!("expected ApplyEdits confirm, got {other:?}"),
        }
        assert!(app.editbuf.is_some(), "cancel path keeps the buffer");
    }

    #[test]
    fn apply_edit_actions_runs_ops_and_reports_ignored() {
        let (mut app, _rx) = test_app();
        let f1 = app.state.create_folder("work", None).unwrap();
        let f2 = app.state.create_folder("gone", None).unwrap();
        let k1 = skey("aaa");
        let k2 = skey("bbb");
        app.apply_edit_actions(
            vec![
                EditAction::CreateFolder {
                    parent: Some(f1.clone()),
                    name: "sub".into(),
                },
                EditAction::RenameFolder {
                    id: f1.clone(),
                    name: "code".into(),
                },
                EditAction::RenameSession {
                    key: k1.clone(),
                    name: "titled".into(),
                },
                EditAction::MoveSession {
                    key: k1.clone(),
                    folder: Some(f1.clone()),
                },
                EditAction::HideSession { key: k2.clone() },
                EditAction::DeleteFolder { id: f2.clone() },
                EditAction::IgnoredLine {
                    text: "junk".into(),
                },
            ],
            false,
        );
        let sub = app.state.children_of(Some(&f1));
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "sub");
        assert_eq!(app.state.folder(&f1).unwrap().name, "code");
        let r = app.state.session(&k1).unwrap();
        assert_eq!(r.name_override.as_deref(), Some("titled"));
        assert_eq!(r.folder.as_deref(), Some(f1.as_str()));
        assert!(app.state.session(&k2).unwrap().hidden);
        assert!(app.state.folder(&f2).is_none());
        let (s, _) = app.status.clone().expect("ignored lines surface");
        assert!(s.contains("ignored") && s.contains("junk"), "{s}");
    }

    #[test]
    fn apply_edit_actions_continues_past_errors_and_surfaces_first() {
        let (mut app, _rx) = test_app();
        let k = skey("aaa");
        app.apply_edit_actions(
            vec![
                EditAction::RenameFolder {
                    id: "missing".into(),
                    name: "x".into(),
                }, // fails: unknown folder id
                EditAction::ForkInto {
                    key: skey("nometa"),
                    folder: None,
                }, // fails: not in scan
                EditAction::HideSession { key: k.clone() }, // must still apply
            ],
            true,
        );
        assert!(app.state.session(&k).unwrap().hidden);
        assert!(app.editbuf.is_none(), "and_quit leaves edit mode");
        let (s, _) = app.status.clone().expect("first error surfaces");
        assert!(s.contains("some edits failed"), "{s}");
    }

    #[test]
    fn apply_edit_actions_rebaselines_the_buffer() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let k = skey("aaa");
        app.sessions = vec![meta_for(&k, "one")];
        index_sessions(&mut app);
        app.rebuild_rows();
        app.enter_edit_mode();
        let buf = app.editbuf.as_mut().unwrap();
        buf.handle_key(&Key::Char('j'));
        buf.handle_key(&Key::Char('d'));
        buf.handle_key(&Key::Char('d'));
        let actions = buf.diff();
        assert_eq!(actions.len(), 1);
        app.apply_edit_actions(actions, false);
        let buf = app.editbuf.as_ref().unwrap();
        assert!(!buf.dirty(), "apply re-baselines the buffer");
        assert!(buf.diff().is_empty(), "…so a second :w is a no-op");
        assert!(app.state.session(&k).unwrap().hidden);
    }

    // ---------- config defaults / agent detection ----------

    fn app_with_cfg(cfg: Config) -> (App, Receiver<AppEvent>) {
        isolate_data_dir();
        let (tx, rx) = unbounded();
        (App::new(cfg, VagState::default(), tx), rx)
    }

    /// A config whose agent commands point at paths that can never exist,
    /// so availability is deterministic regardless of the host machine.
    fn cfg_missing_agents() -> Config {
        let mut cfg = Config::default();
        cfg.agents.claude.command = "/nonexistent/vag-test/claude".into();
        cfg.agents.codex.command = "/nonexistent/vag-test/codex".into();
        cfg
    }

    #[test]
    fn scoped_default_honors_repo_scope_config() {
        let mut cfg = Config::default();
        cfg.behavior.repo_scope = false;
        let (app, _rx) = app_with_cfg(cfg);
        assert!(
            app.scope_root.is_some(),
            "tests run inside the vag git repo"
        );
        assert!(!app.scoped, "repo_scope=false must not auto-scope");

        let (app, _rx) = app_with_cfg(Config::default()); // repo_scope=true
        assert!(app.scoped, "default keeps the historical auto-scope");
    }

    #[test]
    fn new_session_without_agents_sets_notice_and_skips_the_modal() {
        let (mut app, _rx) = app_with_cfg(cfg_missing_agents());
        assert_eq!(app.agents_ok, (false, false));
        assert!(
            app.agent_notice.as_deref() == Some(NO_AGENTS_MSG),
            "startup probe raises the persistent header notice"
        );
        app.start_new_session();
        assert!(app.modal.is_none(), "no PickAgent without any agent");
        let (s, _) = app.status.clone().expect("status set");
        assert_eq!(s, NO_AGENTS_MSG);
    }

    #[test]
    fn pick_agent_defaults_to_the_first_available_agent() {
        let mut cfg = cfg_missing_agents();
        // An executable stand-in (never spawned): codex "installed".
        cfg.agents.codex.command = "/bin/cat".into();
        let (mut app, _rx) = app_with_cfg(cfg);
        assert!(app.agent_notice.is_none(), "one agent is enough");
        app.start_new_session();
        match &app.modal {
            Some(Modal::PickAgent {
                idx,
                claude_ok,
                codex_ok,
                ..
            }) => {
                assert_eq!(*idx, 1, "highlight lands on the installed agent");
                assert!(!claude_ok);
                assert!(*codex_ok);
            }
            other => panic!("expected PickAgent, got {other:?}"),
        }
    }

    fn scan_with(sessions: Vec<SessionMeta>) -> ScanResult {
        ScanResult {
            sessions,
            warnings: vec![],
            failed_agents: vec![],
        }
    }

    #[test]
    fn edit_default_enters_edit_mode_once_after_the_first_good_scan() {
        let mut cfg = Config::default();
        cfg.ui.edit_default = true;
        let (mut app, _rx) = app_with_cfg(cfg);
        app.scoped = false; // the /tmp test session must stay visible
        let meta = meta_for(&skey("aaa"), "one");

        // A total-failure scan is not "the dashboard has rows": stay armed.
        app.on_scan_done(ScanResult {
            sessions: vec![],
            warnings: vec!["boom".into()],
            failed_agents: vec![AgentKind::Claude, AgentKind::Codex],
        });
        assert!(app.editbuf.is_none(), "failed scan must not trigger");
        assert!(app.edit_default_pending, "…and keeps the trigger armed");

        app.on_scan_done(scan_with(vec![meta.clone()]));
        assert!(
            app.editbuf.is_some(),
            "first successful scan enters edit mode"
        );
        assert!(!app.edit_default_pending);

        // One-shot: leaving edit mode stays left across later scans.
        app.editbuf = None;
        app.on_scan_done(scan_with(vec![meta]));
        assert!(app.editbuf.is_none());
    }

    #[test]
    fn edit_default_off_never_auto_enters() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        app.on_scan_done(scan_with(vec![meta_for(&skey("aaa"), "one")]));
        assert!(app.editbuf.is_none());
    }

    // ---------- remote sessions ----------

    fn cfg_with_remote() -> Config {
        let mut cfg = Config::default();
        cfg.remotes.push(crate::config::RemoteConfig {
            name: "gpu".into(),
            host: "user@gpu.example".into(),
            default_dir: Some("~/work".into()),
            claude_command: String::new(),
            codex_command: String::new(),
        });
        cfg
    }

    #[test]
    fn new_session_agent_commit_inserts_location_step_only_with_remotes() {
        // No remotes: straight to the directory prompt, exactly as before.
        let mut cfg = Config::default();
        cfg.agents.claude.command = "/bin/cat".into(); // "installed" stand-in
        let (mut app, _rx) = app_with_cfg(cfg);
        app.commit_new_session_agent(AgentKind::Claude, None, Some("/hint".into()), None);
        match &app.modal {
            Some(Modal::PickDir(pick)) => {
                assert_eq!(pick.edit.buf, "/hint");
                assert!(matches!(
                    pick.target,
                    DirTarget::NewSession {
                        agent: AgentKind::Claude,
                        ..
                    }
                ));
            }
            other => panic!("expected dir picker, got {other:?}"),
        }

        // Remotes configured: the location picker comes first, local on
        // top, "+ add a machine…" last.
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        app.commit_new_session_agent(
            AgentKind::Claude,
            Some("f1".into()),
            Some("/hint".into()),
            None,
        );
        match &app.modal {
            Some(Modal::PickLocation {
                options,
                idx,
                dir_hint,
                folder,
                ..
            }) => {
                assert_eq!(
                    options,
                    &vec![
                        LocationChoice::Local,
                        LocationChoice::Remote {
                            name: "gpu".into(),
                            host: "user@gpu.example".into(),
                        },
                        LocationChoice::AddMachine,
                    ]
                );
                assert_eq!(*idx, 0, "This machine preselected");
                assert_eq!(dir_hint.as_deref(), Some("/hint"));
                assert_eq!(folder.as_deref(), Some("f1"));
            }
            other => panic!("expected PickLocation, got {other:?}"),
        }

        // Pre-selected machine (n on its header): the location step is
        // skipped entirely — straight to the dir prompt on that box.
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        app.commit_new_session_agent(AgentKind::Claude, None, None, Some("gpu".into()));
        match &app.modal {
            Some(Modal::Input {
                edit,
                kind: InputKind::NewSessionDir { remote, .. },
                ..
            }) => {
                assert_eq!(edit.buf, "~/work", "machine default_dir prefilled");
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected dir input, got {other:?}"),
        }
    }

    #[test]
    fn location_commit_prefills_remote_default_dir_and_threads_remote() {
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        app.commit_new_session_location(
            AgentKind::Claude,
            None,
            Some("/local/hint".into()),
            Some("gpu".into()),
        );
        match &app.modal {
            Some(Modal::Input {
                edit,
                kind: InputKind::NewSessionDir { remote, .. },
                ..
            }) => {
                assert_eq!(edit.buf, "~/work", "remote default_dir wins");
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected dir input, got {other:?}"),
        }
        // Unknown remote name: refused with a status, no modal.
        app.modal = None;
        app.commit_new_session_location(AgentKind::Claude, None, None, Some("nope".into()));
        assert!(app.modal.is_none());
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("no longer configured"), "{s}");

        // Local pick keeps the historical prefill (and the local CLI check).
        let mut cfg = cfg_with_remote();
        cfg.agents.codex.command = "/bin/cat".into();
        let (mut app, _rx) = app_with_cfg(cfg);
        app.commit_new_session_location(AgentKind::Codex, None, Some("/local/hint".into()), None);
        match &app.modal {
            Some(Modal::PickDir(pick)) => {
                assert_eq!(pick.edit.buf, "/local/hint");
                assert!(matches!(
                    pick.target,
                    DirTarget::NewSession {
                        agent: AgentKind::Codex,
                        ..
                    }
                ));
            }
            other => panic!("expected dir picker, got {other:?}"),
        }
    }

    #[test]
    fn remote_dir_commit_skips_local_validation() {
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        // A path that certainly doesn't exist locally must be accepted.
        app.commit_new_session_dir(
            AgentKind::Claude,
            None,
            "~/definitely/not/here".into(),
            Some("gpu".into()),
        );
        match &app.modal {
            Some(Modal::Input {
                kind: InputKind::NewSessionName { dir, remote, .. },
                ..
            }) => {
                assert_eq!(dir, "~/definitely/not/here");
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected name input, got {other:?}"),
        }
        assert!(app.status.is_none(), "no not-a-directory refusal");
    }

    #[test]
    fn dir_picker_seeds_prefill_and_scan_batches() {
        let mut cfg = Config::default();
        cfg.agents.claude.command = "/bin/cat".into();
        let (mut app, _rx) = app_with_cfg(cfg);
        let key = SessionKey::new(AgentKind::Claude, "s1");
        app.sessions = vec![meta_for(&key, "one")]; // cwd /tmp/proj
        let home = dirs::home_dir().unwrap();
        let hint = home.join("proj").display().to_string();
        app.open_new_session_dir_input(AgentKind::Claude, None, Some(hint), None);
        let Some(Modal::PickDir(pick)) = &app.modal else {
            panic!("expected dir picker, got {:?}", app.modal);
        };
        assert_eq!(pick.edit.buf, "~/proj", "absolute prefill matches corpus");
        assert!(pick.candidates.contains(&"~".to_string()));
        assert!(
            pick.candidates.contains(&"/tmp/proj".to_string()),
            "session cwd seeded: {:?}",
            pick.candidates
        );
        assert!(pick.scanning);

        // A batch from a previous (stale) walk is dropped.
        let id = app.dir_scan_id;
        app.on_dir_scan(id - 1, vec!["~/stale".into()], true);
        let Some(Modal::PickDir(pick)) = &app.modal else {
            unreachable!()
        };
        assert!(pick.scanning, "stale done flag ignored");
        assert!(!pick.candidates.iter().any(|c| c == "~/stale"));

        // Current-walk batches land; done clears the scanning hint.
        app.on_dir_scan(id, vec!["~/fresh".into()], false);
        app.on_dir_scan(id, vec![], true);
        let Some(Modal::PickDir(pick)) = &app.modal else {
            unreachable!()
        };
        assert!(pick.candidates.iter().any(|c| c == "~/fresh"));
        assert!(!pick.scanning);
    }

    #[test]
    fn record_remote_session_writes_identity() {
        let mut st = VagState::default();
        let fid = st.create_folder("work", None).unwrap();
        let key = SessionKey::new(AgentKind::Claude, "abc-uuid");
        record_remote_session(&mut st, &key, Some(&fid), "gpu", "~/proj");
        let r = st.session(&key).unwrap();
        assert_eq!(r.folder.as_deref(), Some(fid.as_str()));
        assert_eq!(r.remote.as_deref(), Some("gpu"));
        assert_eq!(r.remote_cwd.as_deref(), Some("~/proj"));
        assert!(r.last_opened.is_some());

        // Dangling folder: binding skipped, identity still written.
        let k2 = SessionKey::new(AgentKind::Codex, "remote-xyz");
        record_remote_session(&mut st, &k2, Some("gone"), "gpu", "~");
        let r = st.session(&k2).unwrap();
        assert_eq!(r.folder, None);
        assert_eq!(r.remote.as_deref(), Some("gpu"));
        assert_eq!(r.remote_cwd.as_deref(), Some("~"));
    }

    #[test]
    fn synthesize_remote_metas_builds_rows_from_state() {
        let mut st = VagState::default();
        let rkey = SessionKey::new(AgentKind::Codex, "remote-abc");
        let opened = Utc::now();
        let r = st.session_mut(&rkey);
        r.remote = Some("gpu".into());
        r.remote_cwd = Some("~/work".into());
        r.last_opened = Some(opened);
        // Local entry: never synthesized.
        st.session_mut(&skey("local-1")).hidden = true;
        // Remote entry whose id the scan already returned: skipped.
        let dup = skey("dup-id");
        st.session_mut(&dup).remote = Some("gpu".into());
        let existing = vec![meta_for(&dup, "already scanned")];

        let synth = synthesize_remote_metas(&st, &existing);
        assert_eq!(synth.len(), 1);
        let m = &synth[0];
        assert_eq!(m.key, rkey);
        assert_eq!(m.title, None);
        assert_eq!(m.preview.as_deref(), Some("codex @ gpu"));
        assert_eq!(m.cwd, PathBuf::from("~/work"));
        assert_eq!(m.last_activity, Some(opened));
        assert!(!m.archived);
        assert_eq!(m.source_path, PathBuf::new());

        // remote_cwd unset falls back to "~".
        st.session_mut(&rkey).remote_cwd = None;
        let synth = synthesize_remote_metas(&st, &[]);
        let m = synth.iter().find(|m| m.key == rkey).unwrap();
        assert_eq!(m.cwd, PathBuf::from("~"));
    }

    #[test]
    fn scan_done_grafts_remote_sessions_into_the_tree() {
        let (mut app, _rx) = test_app();
        // Tests run inside the vag repo, so scoping is on by default:
        // a remote session must survive it even with a remote-only cwd.
        assert!(app.scoped, "precondition: repo scoping active");
        let rkey = SessionKey::new(AgentKind::Claude, "rrr-uuid");
        let r = app.state.session_mut(&rkey);
        r.remote = Some("gpu".into());
        r.remote_cwd = Some("~/work".into());

        app.on_scan_done(scan_with(vec![meta_for(&skey("aaa"), "local")]));
        assert!(app.meta_idx.contains_key(&rkey), "remote meta synthesized");
        assert!(
            app.rows.iter().any(|row| row.session_key() == Some(&rkey)),
            "remote row visible under repo scope"
        );
        // gc must not drop the remote entry despite absence from the scan.
        assert!(app.state.session(&rkey).is_some());
    }

    #[test]
    fn closing_synthetic_remote_runtime_evaporates_state() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let rkey = SessionKey::new(AgentKind::Codex, "remote-abc123");
        let r = app.state.session_mut(&rkey);
        r.remote = Some("gpu".into());
        r.remote_cwd = Some("~".into());
        app.on_scan_done(scan_with(vec![]));
        assert!(app.meta_idx.contains_key(&rkey));
        app.open_order.push(rkey.clone());

        app.close_runtime(&rkey);
        assert!(app.state.session(&rkey).is_none(), "attach-only entry gone");
        assert!(!app.meta_idx.contains_key(&rkey), "synthesized meta gone");
        assert!(app.rows.iter().all(|row| row.session_key() != Some(&rkey)));

        // Remote claude entries persist across close (that's the point).
        let ckey = SessionKey::new(AgentKind::Claude, "cccc-uuid");
        app.state.session_mut(&ckey).remote = Some("gpu".into());
        app.close_runtime(&ckey);
        assert!(app.state.session(&ckey).is_some());
    }

    #[test]
    fn open_remote_session_error_paths() {
        // Remote name no longer configured.
        let (mut app, _rx) = test_app();
        let k = SessionKey::new(AgentKind::Claude, "abc-uuid");
        app.state.session_mut(&k).remote = Some("gone-box".into());
        assert!(app.open_remote_session(&k), "remote keys are handled here");
        let (s, _) = app.status.clone().expect("status set");
        assert!(
            s.contains("gone-box") && s.contains("no longer configured"),
            "{s}"
        );
        assert!(app.runtimes.is_empty(), "nothing spawned");

        // Synthetic codex id: attach-only, resume refused by actions.
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        let k = SessionKey::new(AgentKind::Codex, "remote-abc");
        app.state.session_mut(&k).remote = Some("gpu".into());
        assert!(app.open_remote_session(&k));
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("re-attached"), "{s}");
        assert!(app.runtimes.is_empty());

        // Local sessions are not handled by the remote path.
        let (mut app, _rx) = test_app();
        assert!(!app.open_remote_session(&skey("local-id")));
        assert!(app.status.is_none());
    }

    #[test]
    fn fork_and_archive_refused_on_remote_sessions() {
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        let rkey = SessionKey::new(AgentKind::Codex, "remote-abc");
        let r = app.state.session_mut(&rkey);
        r.remote = Some("gpu".into());
        r.remote_cwd = Some("~".into());
        app.on_scan_done(scan_with(vec![]));

        let err = app.fork_session(&rkey, None).unwrap_err();
        assert!(
            err.contains("fork isn't supported on remote sessions"),
            "{err}"
        );

        app.cursor = locate_row(&app.rows, &RowAnchor::Session(rkey.clone())).unwrap();
        app.start_archive();
        assert!(app.modal.is_none(), "no archive confirm for remote rows");
        let (s, _) = app.status.clone().expect("status set");
        assert!(
            s.contains("archive isn't supported on remote sessions"),
            "{s}"
        );
    }

    // ---------- settings page ----------

    #[test]
    fn cursor_hops_over_spacer_rows() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let k1 = skey("aaa");
        app.sessions = vec![meta_for(&k1, "one")];
        index_sessions(&mut app);
        app.rebuild_rows();
        // rows: + new session, spacer, Inbox, session
        assert!(matches!(app.rows[1], Row::Spacer));
        app.cursor = 0;
        app.move_cursor(1);
        assert_eq!(app.cursor, 2, "j from the button skips the spacer");
        app.move_cursor(-1);
        assert_eq!(app.cursor, 0, "k back over it too");
    }

    #[test]
    fn settings_footer_is_a_cursor_sentinel_past_the_rows() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let k1 = skey("aaa");
        let k2 = skey("bbb");
        app.sessions = vec![meta_for(&k1, "one"), meta_for(&k2, "two")];
        index_sessions(&mut app);
        app.rebuild_rows();
        let n = app.rows.len();
        assert!(n > 0);
        // j from the last row lands on settings; further j stays there
        app.cursor = n - 1;
        app.move_cursor(1);
        assert!(app.settings_selected());
        app.move_cursor(1);
        assert_eq!(app.cursor, n, "sentinel clamps");
        // background rescans keep the cursor on settings
        app.rebuild_rows();
        assert!(app.settings_selected(), "sentinel survives rebuilds");
        // k steps back into the list
        app.move_cursor(-1);
        assert_eq!(app.cursor, app.rows.len() - 1);
        assert!(!app.settings_selected());
        // the settings button is chrome, not a row: no row-based action
        // (delete/rename/open) can resolve it
        app.cursor = app.rows.len();
        assert!(app.session_under_cursor().is_none());
        assert!(app.folder_under_cursor().is_none());
    }

    #[test]
    fn settings_page_opens_off_headers_cycles_live_and_persists() {
        let _config_lock = isolate_config_dir();
        let (mut app, _rx) = test_app();
        app.open_settings(0);
        let Some(Modal::Settings { rows, idx }) = &app.modal else {
            panic!("expected settings modal, got {:?}", app.modal);
        };
        // idx 0 is the "appearance" header → bumped to the theme row.
        assert!(matches!(
            rows[*idx],
            SettingRow::Value {
                id: SettingId::Theme,
                ..
            }
        ));
        // every key action has a row
        let key_rows = rows
            .iter()
            .filter(|r| matches!(r, SettingRow::Key { .. }))
            .count();
        assert_eq!(key_rows, KeyAction::ALL.len() + CtrlAction::ALL.len());

        // cycling icons applies live AND lands in config.toml
        app.cycle_setting(SettingId::Icons, 1);
        assert_eq!(app.icons, Icons::NERD);
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(text.contains("icons = \"nerd\""), "{text}");
        app.cycle_setting(SettingId::Icons, -1);
        assert_eq!(app.icons, Icons::ASCII);

        // sidebar width steps by 2 and clamps at 60
        app.cfg.ui.sidebar_width = 60;
        app.cycle_setting(SettingId::SidebarWidth, 1);
        assert_eq!(app.cfg.ui.sidebar_width, 60);
        app.cycle_setting(SettingId::SidebarWidth, -1);
        assert_eq!(app.cfg.ui.sidebar_width, 58);

        // show-hidden default also flips the live toggle
        app.cycle_setting(SettingId::ShowHidden, 1);
        assert!(app.show_hidden && app.cfg.behavior.show_hidden);
    }

    #[test]
    fn binding_capture_rejects_collisions_and_rebinds_live() {
        let _config_lock = isolate_config_dir();
        let (mut app, _rx) = test_app();
        // 'n' belongs to new_session: refused, binding unchanged
        app.commit_set_binding(BindTarget::Action(KeyAction::Fork), 'n', 3);
        assert_eq!(app.cfg.keys.get(KeyAction::Fork), 'F');
        let (s, _) = app.status.clone().expect("collision surfaces");
        assert!(s.contains("already bound"), "{s}");
        // a free char rebinds, routes, persists, reopens the page
        app.commit_set_binding(BindTarget::Action(KeyAction::Fork), 'f', 3);
        assert_eq!(app.cfg.keys.get(KeyAction::Fork), 'f');
        assert_eq!(app.cfg.keys.action_for('f'), Some(KeyAction::Fork));
        assert_eq!(app.cfg.keys.action_for('F'), None);
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(text.contains("fork = \"f\""), "{text}");
        assert!(matches!(app.modal, Some(Modal::Settings { .. })));
        // detach chord
        app.commit_set_binding(BindTarget::Ctrl(CtrlAction::Detach), 'a', 0);
        assert_eq!(app.cfg.keys.detach.label(), "ctrl-a");
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(text.contains("detach = \"ctrl-a\""), "{text}");
        // a ctrl-chord collision refuses and reports which action holds it
        app.commit_set_binding(BindTarget::Ctrl(CtrlAction::FocusTree), 'e', 0);
        let (s, _) = app.status.clone().expect("ctrl collision surfaces");
        assert!(s.contains("already bound"), "{s}");
        assert_eq!(
            app.cfg.keys.focus_tree.label(),
            "ctrl-h",
            "unchanged on refusal"
        );
    }

    #[test]
    fn ctrl_e_toggles_sidebar_visibility_while_pane_focused() {
        let (mut app, _rx) = test_app();
        let key = SessionKey::new(AgentKind::Codex, "sess".to_string());
        let (es, _er) = unbounded();
        app.runtimes.insert(key.clone(), spawn_cat(&key, &es));
        app.set_active(Some(key));
        app.focus_pane();
        let area = Rect::new(0, 0, 120, 40);
        assert!(
            app.split_areas(area).0.is_some(),
            "sidebar visible by default"
        );

        let mut term = test_terminal();
        app.dirty = false;
        app.on_stdin(vec![TOGGLE_SIDEBAR], &mut term).unwrap();
        assert!(app.sidebar_hidden);
        assert!(
            app.dirty,
            "the raw-byte path must request a repaint or the toggle is invisible"
        );
        assert!(
            app.split_areas(area).0.is_none(),
            "hidden: pane takes the full width"
        );
        assert_eq!(
            app.focus,
            Focus::Pane,
            "toggling the sidebar keeps pane focus"
        );

        app.on_stdin(vec![TOGGLE_SIDEBAR], &mut term).unwrap();
        assert!(!app.sidebar_hidden);
        assert!(app.split_areas(area).0.is_some(), "toggled back on");

        for (_, mut rt) in app.runtimes.drain() {
            rt.kill();
        }
    }

    #[test]
    fn ctrl_h_focuses_tree_from_pane() {
        let (mut app, _rx) = test_app();
        let key = SessionKey::new(AgentKind::Codex, "sess".to_string());
        let (es, _er) = unbounded();
        app.runtimes.insert(key.clone(), spawn_cat(&key, &es));
        app.set_active(Some(key));
        app.focus_pane();
        assert_eq!(app.focus, Focus::Pane);

        let mut term = test_terminal();
        app.dirty = false;
        app.on_stdin(vec![FOCUS_TREE], &mut term).unwrap();
        assert_eq!(app.focus, Focus::Tree);
        assert!(
            app.dirty,
            "the focus move must request a repaint — without it ctrl-h feels dead"
        );

        for (_, mut rt) in app.runtimes.drain() {
            rt.kill();
        }
    }

    #[test]
    fn ctrl_l_focuses_active_session_without_opening_cursor_row() {
        let (mut app, _rx) = test_app();
        app.scoped = false; // tests run inside a repo; /tmp cwds must show
        let active_key = SessionKey::new(AgentKind::Codex, "active".to_string());
        let other_key = SessionKey::new(AgentKind::Codex, "other".to_string());
        app.sessions = vec![
            meta_for(&active_key, "active"),
            meta_for(&other_key, "other"),
        ];
        index_sessions(&mut app);
        let (es, _er) = unbounded();
        app.runtimes
            .insert(active_key.clone(), spawn_cat(&active_key, &es));
        app.set_active(Some(active_key.clone()));
        app.focus_tree();
        app.rebuild_rows();
        // Park the cursor on a DIFFERENT session's row than the active one.
        let other_row = app
            .rows
            .iter()
            .position(|r| r.session_key() == Some(&other_key))
            .expect("other session row");
        app.cursor = other_row;

        let mut term = test_terminal();
        app.on_key(Key::Ctrl(FOCUS_PANE_CTRL), &mut term).unwrap();

        assert_eq!(app.focus, Focus::Pane, "ctrl-l focused the pane");
        assert_eq!(
            app.active,
            Some(active_key),
            "ctrl-l must NOT switch to the row under the cursor — that's enter's job"
        );
        assert!(app.modal.is_none(), "ctrl-l never opens a modal");

        for (_, mut rt) in app.runtimes.drain() {
            rt.kill();
        }
    }

    #[test]
    fn ctrl_l_is_a_noop_with_no_active_session() {
        let (mut app, _rx) = test_app();
        app.focus_tree();
        let mut term = test_terminal();
        app.on_key(Key::Ctrl(FOCUS_PANE_CTRL), &mut term).unwrap();
        assert_eq!(app.focus, Focus::Tree, "nothing to focus, stays put");
    }

    #[test]
    fn tree_focus_edge_keys_are_safe_noops_outside_tmux() {
        // Inside tmux the edge arms shell out to `tmux select-pane`; with
        // tmux_nav off (test_app forces it) they must be inert: no focus
        // change, no modal, no cursor movement, no panic.
        let (mut app, _rx) = test_app();
        app.focus_tree();
        let cursor = app.cursor;
        let mut term = test_terminal();
        app.on_key(Key::Ctrl('h'), &mut term).unwrap();
        app.on_key(Key::Ctrl(FOCUS_PANE_CTRL), &mut term).unwrap();
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(app.cursor, cursor);
        assert!(app.modal.is_none());
    }

    #[test]
    fn theme_picker_previews_live_reverts_on_esc_and_persists_on_enter() {
        let _config_lock = isolate_config_dir();
        let (mut app, _rx) = test_app();
        assert_eq!(app.theme, Theme::NIGHT);
        // hovering gruvbox in the picker repaints immediately
        let picker = Modal::PickTheme {
            options: vec!["gruvbox".into()],
            idx: 0,
            original: app.cfg.ui.theme.clone(),
            from_idx: usize::MAX,
        };
        app.preview_picked_theme(&picker);
        assert_eq!(app.theme, Theme::GRUVBOX);
        // esc puts the original back (the Cancel arm re-applies `original`)
        let t = app.theme_for_name("night");
        app.apply_theme_live(t);
        assert_eq!(app.theme, Theme::NIGHT);
        // enter applies AND persists
        app.commit_set_theme("mocha".into(), usize::MAX);
        assert_eq!(app.theme, Theme::MOCHA);
        assert_eq!(app.cfg.ui.theme, "mocha");
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(text.contains("theme = \"mocha\""), "{text}");
    }

    // ---------- machines & shells ----------

    /// Redirect config.toml to a throwaway dir so config-writing flows can
    /// never touch the user's real ~/.config — and hold the returned guard
    /// for the test's duration: the writes are read-modify-write on ONE
    /// shared file, so concurrent tests would silently drop each other's
    /// keys.
    fn isolate_config_dir() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        static LOCK: Mutex<()> = Mutex::new(());
        let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"));
        // SAFETY: test-only; no other test in this crate reads
        // XDG_CONFIG_HOME concurrently.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn machine_header_actions_dispatch() {
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        app.on_scan_done(scan_with(vec![]));
        let mi = locate_row(&app.rows, &RowAnchor::Machine("gpu".into())).expect("machine row");
        app.cursor = mi;

        // n: agent picker with the machine pre-selected, both agents
        // offered (their binaries live on the box).
        app.start_new_session();
        match &app.modal {
            Some(Modal::PickAgent {
                remote,
                claude_ok,
                codex_ok,
                folder,
                ..
            }) => {
                assert_eq!(remote.as_deref(), Some("gpu"));
                assert!(*claude_ok && *codex_ok, "local CLIs must not gate remotes");
                assert!(folder.is_none(), "machine members are unfoldered");
            }
            other => panic!("expected PickAgent, got {other:?}"),
        }
        app.modal = None;

        // r: renames stay config-file-only (they'd orphan state refs).
        app.start_rename();
        assert!(app.modal.is_none());
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("config.toml"), "{s}");

        // x: remove-from-config confirm, sessions promised to survive.
        app.start_delete_folder();
        match &app.modal {
            Some(Modal::Confirm {
                msg,
                commit: Commit::RemoveMachine { name },
            }) => {
                assert_eq!(name, "gpu");
                assert!(msg.contains("stay in your tree"), "{msg}");
            }
            other => panic!("expected RemoveMachine confirm, got {other:?}"),
        }
        app.modal = None;

        // space/h: collapse the group via the machine collapse key.
        app.cursor = mi;
        app.toggle_collapse();
        assert!(app.collapsed.contains(&machine_collapse_key("gpu")));
        assert!(matches!(
            app.rows[locate_row(&app.rows, &RowAnchor::Machine("gpu".into())).unwrap()],
            Row::Machine {
                collapsed: true,
                ..
            }
        ));

        // A stale machine row (config edited away mid-session) refuses n.
        app.cfg.remotes.clear();
        app.cursor = mi;
        app.start_new_session_on_machine("gpu".into());
        assert!(app.modal.is_none());
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("no longer configured"), "{s}");
    }

    #[test]
    fn add_and_remove_machine_commits_edit_the_config() {
        let _config_lock = isolate_config_dir();
        let (mut app, _rx) = test_app();
        app.commit_add_machine("boxy".into(), "user@boxy.example".into(), " ~/work ".into());
        assert!(app.modal.is_none());
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("added boxy"), "{s}");
        assert!(s.contains("credentials"), "{s}");
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(text.contains("name = \"boxy\""), "{text}");
        let rc = app
            .cfg
            .remote("boxy")
            .expect("cfg reloaded with the machine");
        assert_eq!(rc.host, "user@boxy.example");
        assert_eq!(rc.default_dir.as_deref(), Some("~/work"));
        assert!(
            app.rows.iter().any(|r| r.machine_name() == Some("boxy")),
            "group appears immediately"
        );

        // Duplicate name: error surfaces, name input reopens prefilled.
        app.commit_add_machine("boxy".into(), "other@host".into(), String::new());
        let (s, _) = app.status.clone().expect("status set");
        assert!(s.contains("already exists"), "{s}");
        match &app.modal {
            Some(Modal::Input {
                edit,
                kind: InputKind::AddMachineName,
                ..
            }) => assert_eq!(edit.buf, "boxy"),
            other => panic!("expected reopened name input, got {other:?}"),
        }
        app.modal = None;

        // Remove: config entry gone, collapse state cleaned, cfg reloaded.
        app.collapsed.insert(machine_collapse_key("boxy"));
        app.commit_remove_machine("boxy".into());
        assert!(app.cfg.remote("boxy").is_none());
        let text = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(!text.contains("boxy"), "{text}");
        assert!(!app.collapsed.contains(&machine_collapse_key("boxy")));
        let (s, _) = app.status.clone().unwrap();
        assert!(s.contains("stay in your tree"), "{s}");
        assert!(app.rows.iter().all(|r| r.machine_name() != Some("boxy")));

        // Removing an absent machine: a hint, not an error.
        app.commit_remove_machine("boxy".into());
        let (s, _) = app.status.clone().unwrap();
        assert!(s.contains("not found"), "{s}");
    }

    #[test]
    fn shell_spawn_plan_targets_machine_or_local_context() {
        let (mut app, _rx) = app_with_cfg(cfg_with_remote());
        app.scoped = false;
        let tmp = std::env::temp_dir();
        let mut m = meta_for(&skey("aaa"), "one");
        m.cwd = tmp.clone();
        app.sessions = vec![m];
        index_sessions(&mut app);
        let fid = app.state.create_folder("work", None).unwrap();
        app.state
            .set_folder_default_dir(&fid, Some(tmp.clone()))
            .unwrap();
        app.rebuild_rows();

        // Machine header: a plain `ssh -t <host>` (no remote command).
        app.cursor = locate_row(&app.rows, &RowAnchor::Machine("gpu".into())).unwrap();
        let (spec, label) = app.shell_spawn_plan();
        assert_eq!(spec.program, "ssh");
        assert_eq!(
            spec.args,
            vec!["-t".to_string(), "user@gpu.example".to_string()]
        );
        assert_eq!(label, "shell @ gpu");
        assert!(spec.env.contains(&("TERM".into(), "xterm-256color".into())));
        assert!(spec.env.contains(&("COLORTERM".into(), "truecolor".into())));

        // Session row: local $SHELL (fallback /bin/sh) in the session cwd.
        let want_shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        app.cursor = locate_row(&app.rows, &RowAnchor::Session(skey("aaa"))).unwrap();
        let (spec, label) = app.shell_spawn_plan();
        assert_eq!(spec.program, want_shell);
        assert!(spec.args.is_empty());
        assert_eq!(spec.cwd, tmp);
        assert_eq!(
            label,
            format!("shell: {}", tmp.file_name().unwrap().to_string_lossy())
        );

        // Folder row: its default_dir is the context.
        app.cursor = locate_row(&app.rows, &RowAnchor::Folder(fid)).unwrap();
        let (spec, _) = app.shell_spawn_plan();
        assert_eq!(spec.cwd, tmp);

        // No context at all (the "+ new session" row, unscoped): home.
        app.cursor = 0;
        let (spec, _) = app.shell_spawn_plan();
        assert_eq!(spec.cwd, dirs::home_dir().unwrap());
    }

    #[test]
    fn shell_rows_refuse_state_mutations_and_close_removes_every_trace() {
        let (mut app, _rx) = test_app();
        app.scoped = false;
        let sk = SessionKey::new(AgentKind::Shell, "shell-abc123");
        app.open_order.push(sk.clone());
        app.provisional_labels
            .insert(sk.clone(), "shell: proj".into());
        app.rebuild_rows();
        app.cursor = locate_row(&app.rows, &RowAnchor::Session(sk.clone())).expect("shell row");

        app.start_move_session();
        assert!(app.modal.is_none(), "move refused");
        app.start_rename();
        assert!(app.modal.is_none(), "rename refused");
        app.toggle_hidden();
        let err = app.fork_session(&sk, None).unwrap_err();
        assert!(err.contains("ephemeral"), "{err}");
        let (s, _) = app.status.clone().expect("refusals surface");
        assert!(s.contains("ephemeral"), "{s}");
        assert!(
            app.state.sessions.is_empty(),
            "no shell:… key may reach state.json"
        );

        app.close_runtime(&sk);
        assert!(
            app.provisional_labels.is_empty(),
            "label evaporates with the pane"
        );
        assert!(app.rows.iter().all(|r| r.session_key() != Some(&sk)));
    }
}
