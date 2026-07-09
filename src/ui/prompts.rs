//! Modal state for vag's chrome: text inputs, pickers, confirms, help.
//! Pure state machines — `handle_key` returns an `Outcome` the app executes;
//! rendering is a centered overlay box.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::{CtrlAction, DetachKey, KeyAction, KeysConfig};
use crate::types::{AgentKind, SessionKey};
use crate::ui::editbuf::EditAction;
use crate::ui::icons::Icons;
use crate::ui::input::Key;

/// What a modal wants the app to do when it completes.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Keep showing the modal.
    Pending,
    /// Keep showing the modal AND surface a status-line message (e.g. a
    /// refused selection).
    Msg(String),
    /// Close the modal, do nothing.
    Cancel,
    Commit(Commit),
}

#[derive(Debug, Clone)]
pub enum Commit {
    NewFolder {
        parent: Option<String>,
        name: String,
    },
    RenameFolder {
        id: String,
        name: String,
    },
    DeleteFolder {
        id: String,
    },
    BindFolderDir {
        id: String,
        dir: String,
    },
    RenameSession {
        key: SessionKey,
        name: String,
    },
    MoveSession {
        key: SessionKey,
        folder: Option<String>,
    },
    NewSessionAgent {
        agent: AgentKind,
        folder: Option<String>,
        dir_hint: Option<String>,
        /// Pre-selected machine (n/enter on a machine header): the app
        /// skips the location step and goes straight to the dir prompt.
        remote: Option<String>,
    },
    /// Location step (only shown when `[[remotes]]` are configured):
    /// `remote` None = this machine, Some(name) = that remote.
    NewSessionLocation {
        agent: AgentKind,
        folder: Option<String>,
        dir_hint: Option<String>,
        remote: Option<String>,
    },
    NewSessionDir {
        agent: AgentKind,
        folder: Option<String>,
        dir: String,
        remote: Option<String>,
    },
    NewSessionName {
        agent: AgentKind,
        folder: Option<String>,
        dir: String,
        name: String,
        remote: Option<String>,
    },
    CloseRuntime {
        key: SessionKey,
    },
    /// x on a session row: codex → real `codex delete`; claude → hide (no
    /// delete CLI exists); remote → drop the vag state entry.
    DeleteSession {
        key: SessionKey,
    },
    /// c on a session row: accent color for its tree row + titlebar
    /// (None = back to default styling).
    SetSessionColor {
        key: SessionKey,
        color: Option<String>,
    },
    /// Resume a session even though it appears to be running in another
    /// terminal (bypasses open_session's external double-attach guard).
    OpenAnyway {
        key: SessionKey,
    },
    ArchiveCodex {
        key: SessionKey,
        archived: bool,
    },
    /// Confirmed `:w` from edit mode: apply the diffed actions in order.
    /// `and_quit` = the save came from `:wq`, so edit mode ends after.
    ApplyEdits {
        actions: Vec<EditAction>,
        and_quit: bool,
    },
    /// Open the add-machine flow (R, or the location picker's final row).
    StartAddMachine,
    /// Add-machine step 1 → the app opens the host input.
    AddMachineName {
        name: String,
    },
    /// Add-machine step 2 → the app opens the optional-dir input.
    AddMachineHost {
        name: String,
        host: String,
    },
    /// Add-machine step 3: write the `[[remotes]]` entry (empty dir = none).
    AddMachine {
        name: String,
        host: String,
        dir: String,
    },
    /// Confirmed `x` on a machine header: drop it from config.toml (its
    /// sessions stay in vag state).
    RemoveMachine {
        name: String,
    },
    /// Settings page: step a value row through its variants (dir ±1). The
    /// app mutates cfg, applies it live, persists to config.toml and
    /// rebuilds the page at `idx`.
    SettingCycle {
        id: SettingId,
        dir: i8,
        idx: usize,
    },
    /// Settings theme row / future direct key: open the live-preview theme
    /// picker. `from_idx` = settings row to return to (usize::MAX = the
    /// picker wasn't opened from the settings page).
    OpenThemePicker {
        from_idx: usize,
    },
    /// Enter in the theme picker: apply + persist. Esc instead reverts to
    /// the theme active when the picker opened.
    SetTheme {
        name: String,
        from_idx: usize,
    },
    /// Enter on a settings key row: open the capture overlay.
    StartCapture {
        target: BindTarget,
        from_idx: usize,
    },
    /// A captured keypress: rebind (app validates collisions, persists).
    /// For BindTarget::Ctrl(_) `ch` is the ctrl letter.
    SetBinding {
        target: BindTarget,
        ch: char,
        from_idx: usize,
    },
    Quit,
}

/// What a settings key row rebinds: a tree command or the detach chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindTarget {
    Action(KeyAction),
    Ctrl(CtrlAction),
}

impl BindTarget {
    pub fn title(&self) -> &'static str {
        match self {
            BindTarget::Action(a) => a.title(),
            BindTarget::Ctrl(a) => a.title(),
        }
    }
}

/// A cycling value on the settings page. The app owns the variants; the
/// modal only reports "step this one".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingId {
    Theme,
    Icons,
    Pane,
    Tree,
    SidebarWidth,
    EditDefault,
    RepoScope,
    ShowHidden,
}

/// One row of the settings page, prebuilt by the app (label + rendered
/// current value) so rendering needs no Config access.
#[derive(Debug, Clone)]
pub enum SettingRow {
    /// Non-selectable header.
    Section(String),
    Value {
        id: SettingId,
        label: String,
        value: String,
    },
    Key {
        target: BindTarget,
        label: String,
        value: String,
    },
}

/// Next selectable row from `idx` in `dir` (−1/+1), skipping headers;
/// stays put at the edges.
fn settings_step(rows: &[SettingRow], idx: usize, dir: i64) -> usize {
    let mut i = idx as i64;
    loop {
        i += dir;
        if i < 0 || i >= rows.len() as i64 {
            return idx;
        }
        if !matches!(rows[i as usize], SettingRow::Section(_)) {
            return i as usize;
        }
    }
}

impl Commit {
    /// Rewrite a captured session key `from` → `to` (provisional id
    /// resolution while a modal referencing it is open).
    fn rekey_session(&mut self, from: &SessionKey, to: &SessionKey) {
        let key = match self {
            Commit::RenameSession { key, .. }
            | Commit::MoveSession { key, .. }
            | Commit::CloseRuntime { key }
            | Commit::DeleteSession { key }
            | Commit::SetSessionColor { key, .. }
            | Commit::OpenAnyway { key }
            | Commit::ArchiveCodex { key, .. } => key,
            Commit::ApplyEdits { actions, .. } => {
                for a in actions.iter_mut() {
                    let key = match a {
                        EditAction::RenameSession { key, .. }
                        | EditAction::HideSession { key }
                        | EditAction::MoveSession { key, .. }
                        | EditAction::ForkInto { key, .. } => key,
                        _ => continue,
                    };
                    if key == from {
                        *key = to.clone();
                    }
                }
                return;
            }
            _ => return,
        };
        if key == from {
            *key = to.clone();
        }
    }
}

/// A single-line editor.
#[derive(Debug, Clone, Default)]
pub struct LineEdit {
    pub buf: String,
    pub cursor: usize, // byte offset, always on a char boundary
}

impl LineEdit {
    pub fn with_text(text: &str) -> Self {
        LineEdit {
            buf: text.to_string(),
            cursor: text.len(),
        }
    }

    /// Returns true when the key was consumed as an edit.
    pub fn handle(&mut self, key: &Key) -> bool {
        match key {
            Key::Char(c) => {
                self.buf.insert(self.cursor, *c);
                self.cursor += c.len_utf8();
                true
            }
            Key::Paste(s) => {
                let clean: String = s.chars().filter(|c| !c.is_control() || *c == ' ').collect();
                self.buf.insert_str(self.cursor, &clean);
                self.cursor += clean.len();
                true
            }
            // Ctrl('h') is the 0x08 byte (readline/vim treat it as
            // backspace; legacy terminals send it for the Backspace key).
            Key::Backspace | Key::Ctrl('h') => {
                if self.cursor > 0 {
                    let prev = prev_boundary(&self.buf, self.cursor);
                    self.buf.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
                true
            }
            Key::Delete => {
                if self.cursor < self.buf.len() {
                    let next = next_boundary(&self.buf, self.cursor);
                    self.buf.replace_range(self.cursor..next, "");
                }
                true
            }
            Key::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_boundary(&self.buf, self.cursor);
                }
                true
            }
            Key::Right => {
                if self.cursor < self.buf.len() {
                    self.cursor = next_boundary(&self.buf, self.cursor);
                }
                true
            }
            Key::Home => {
                self.cursor = 0;
                true
            }
            Key::End => {
                self.cursor = self.buf.len();
                true
            }
            Key::Ctrl('u') => {
                self.buf.replace_range(..self.cursor, "");
                self.cursor = 0;
                true
            }
            Key::Ctrl('w') => {
                let mut i = self.cursor;
                while i > 0 && self.buf[..i].ends_with(' ') {
                    i = prev_boundary(&self.buf, i);
                }
                while i > 0 && !self.buf[..i].ends_with(' ') {
                    i = prev_boundary(&self.buf, i);
                }
                self.buf.replace_range(i..self.cursor, "");
                self.cursor = i;
                true
            }
            _ => false,
        }
    }
}

/// What the directory picker's accepted path is for.
#[derive(Debug, Clone)]
pub enum DirTarget {
    /// Local new-session flow (remote sessions keep the plain input — we
    /// can't scan a remote filesystem).
    NewSession {
        agent: AgentKind,
        folder: Option<String>,
    },
    /// `b` on a folder: bind its default directory (empty = clear).
    BindFolder { id: String },
}

/// Fuzzy directory picker: a line editor over a live-streamed candidate
/// list. The input row is the "selection" by default so enter-on-a-typed
/// path behaves exactly like the plain input it replaces; ↓/tab move into
/// the match list.
#[derive(Debug, Clone)]
pub struct DirPick {
    pub title: String,
    pub edit: LineEdit,
    pub target: DirTarget,
    /// Tilde-abbreviated directories: seeds (session cwds, folder defaults)
    /// first, then background-scan batches in BFS order.
    pub candidates: Vec<String>,
    /// Indices into `candidates`, best match first.
    pub filtered: Vec<usize>,
    /// None = the input row itself; Some(i) = filtered[i].
    pub sel: Option<usize>,
    /// Background walk still running (rendered as a hint).
    pub scanning: bool,
}

/// Rows of the match list shown below the input.
const DIR_PICK_ROWS: usize = 8;

impl DirPick {
    pub fn new(title: String, prefill: &str, target: DirTarget, candidates: Vec<String>) -> Self {
        let mut p = DirPick {
            title,
            edit: LineEdit::with_text(prefill),
            target,
            candidates,
            filtered: Vec::new(),
            sel: None,
            scanning: true,
        };
        p.refilter();
        p
    }

    /// Re-rank `filtered` against the current query. An empty query keeps
    /// corpus order (seeds first, then shallow-first BFS).
    fn refilter(&mut self) {
        let query = self.edit.buf.trim();
        if query.is_empty() {
            self.filtered = (0..self.candidates.len()).collect();
            return;
        }
        use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
        use nucleo_matcher::{Config, Matcher, Utf32Str};
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                pattern
                    .score(Utf32Str::new(c, &mut buf), &mut matcher)
                    .map(|s| (s, i))
            })
            .collect();
        // Ties break toward earlier candidates: seeds, then shallower dirs.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
    }

    /// A background-scan batch arrived. Keeps the highlighted candidate
    /// highlighted even when re-ranking shuffles its position.
    pub fn push_candidates(&mut self, dirs: Vec<String>) {
        let keep = self.sel.and_then(|i| self.filtered.get(i)).copied();
        self.candidates.extend(dirs);
        self.refilter();
        self.sel = keep.and_then(|c| self.filtered.iter().position(|&i| i == c));
    }

    fn selected_text(&self) -> Option<&str> {
        self.sel
            .and_then(|i| self.filtered.get(i))
            .map(|&i| self.candidates[i].as_str())
    }

    fn commit(&self) -> Outcome {
        let dir = self
            .selected_text()
            .map(str::to_string)
            .unwrap_or_else(|| self.edit.buf.trim().to_string());
        match &self.target {
            DirTarget::NewSession { agent, folder } => {
                if dir.is_empty() {
                    Outcome::Pending
                } else {
                    Outcome::Commit(Commit::NewSessionDir {
                        agent: *agent,
                        folder: folder.clone(),
                        dir,
                        remote: None,
                    })
                }
            }
            // Empty is meaningful here: it clears the binding.
            DirTarget::BindFolder { id } => Outcome::Commit(Commit::BindFolderDir {
                id: id.clone(),
                dir,
            }),
        }
    }

    pub fn handle_key(&mut self, key: &Key) -> Outcome {
        match key {
            Key::Esc => Outcome::Cancel,
            Key::Enter => self.commit(),
            Key::Down | Key::Ctrl('n') => {
                self.sel = match self.sel {
                    None if !self.filtered.is_empty() => Some(0),
                    Some(i) if i + 1 < self.filtered.len() => Some(i + 1),
                    s => s,
                };
                Outcome::Pending
            }
            Key::Up | Key::Ctrl('p') => {
                self.sel = match self.sel {
                    Some(0) | None => None,
                    Some(i) => Some(i - 1),
                };
                Outcome::Pending
            }
            Key::Tab => {
                // Complete the input with the highlighted (or best) match
                // and hand the cursor back to the editor.
                let text = self
                    .selected_text()
                    .or_else(|| self.filtered.first().map(|&i| self.candidates[i].as_str()))
                    .map(str::to_string);
                if let Some(t) = text {
                    self.edit = LineEdit::with_text(&t);
                    self.sel = None;
                    self.refilter();
                }
                Outcome::Pending
            }
            k => {
                if self.edit.handle(k) {
                    self.sel = None;
                    self.refilter();
                }
                Outcome::Pending
            }
        }
    }
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i - 1;
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn next_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Every modal vag can show.
#[derive(Debug, Clone)]
pub enum Modal {
    Help,
    Confirm {
        msg: String,
        commit: Commit,
    },
    Input {
        title: String,
        edit: LineEdit,
        kind: InputKind,
    },
    /// Local directory choice with fuzzy autocomplete (new session / bind
    /// folder dir). Candidates stream in from a background walk.
    PickDir(DirPick),
    PickAgent {
        folder: Option<String>,
        dir_hint: Option<String>,
        idx: usize,
        /// Availability probed by the app when the modal opens; missing
        /// agents render dimmed and refuse selection. Both true when a
        /// machine is pre-selected (the binaries live on the box).
        claude_ok: bool,
        codex_ok: bool,
        /// Pre-selected machine, threaded into the commit.
        remote: Option<String>,
    },
    PickFolder {
        key: SessionKey,
        options: Vec<(Option<String>, String)>,
        idx: usize,
    },
    /// Accent-color picker for a session (None = default styling).
    PickColor {
        key: SessionKey,
        /// None first ("default"), then the palette names.
        options: Vec<Option<String>>,
        idx: usize,
    },
    /// New-session location step, shown only when `[[remotes]]` exist.
    PickLocation {
        agent: AgentKind,
        folder: Option<String>,
        /// Carried through from PickAgent so the local branch keeps its
        /// directory prefill.
        dir_hint: Option<String>,
        /// "This machine" first, then each remote, then "+ add a machine…".
        options: Vec<LocationChoice>,
        idx: usize,
    },
    /// The settings page (⚙ row / opened by the app). Rows are prebuilt by
    /// the app; every change round-trips through a Commit so the app can
    /// apply live + persist + rebuild the page.
    Settings {
        rows: Vec<SettingRow>,
        idx: usize,
    },
    /// Theme picker with LIVE preview: the app re-applies the hovered theme
    /// after every keypress while this is open; Esc reverts to `original`.
    PickTheme {
        options: Vec<String>,
        idx: usize,
        /// ui.theme when the picker opened (Esc restores it).
        original: String,
        /// Settings row to return to on close; usize::MAX = standalone.
        from_idx: usize,
    },
    /// "Press a key" overlay for rebinding.
    CaptureKey {
        target: BindTarget,
        from_idx: usize,
    },
}

/// One row of the new-session location picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationChoice {
    Local,
    Remote {
        name: String,
        /// Display-only ssh host so render needs no Config access.
        host: String,
    },
    /// Final row: cancels the new-session flow into the add-machine flow.
    AddMachine,
}

#[derive(Debug, Clone)]
pub enum InputKind {
    NewFolder {
        parent: Option<String>,
    },
    RenameFolder {
        id: String,
    },
    RenameSession {
        key: SessionKey,
    },
    NewSessionDir {
        agent: AgentKind,
        folder: Option<String>,
        /// `[[remotes]]` name when the session will live on an ssh remote.
        remote: Option<String>,
    },
    NewSessionName {
        agent: AgentKind,
        folder: Option<String>,
        dir: String,
        remote: Option<String>,
    },
    /// Add-machine step 1: the machine's display name.
    AddMachineName,
    /// Add-machine step 2: the ssh host. `suggestions` = up to ~8
    /// `~/.ssh/config` aliases, rendered as a dim hint line.
    AddMachineHost {
        name: String,
        suggestions: Vec<String>,
    },
    /// Add-machine step 3: optional default directory on the box.
    AddMachineDir {
        name: String,
        host: String,
    },
}

impl Modal {
    /// Rewrite any session key the modal captured from `from` to `to`.
    /// Called when a provisional "pending-…" id resolves while the modal is
    /// open, so committing it acts on the re-keyed live runtime.
    pub fn rekey_session(&mut self, from: &SessionKey, to: &SessionKey) {
        match self {
            Modal::Confirm { commit, .. } => commit.rekey_session(from, to),
            Modal::PickFolder { key, .. } if key == from => *key = to.clone(),
            Modal::PickColor { key, .. } if key == from => *key = to.clone(),
            Modal::Input {
                kind: InputKind::RenameSession { key },
                ..
            } if key == from => *key = to.clone(),
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: &Key) -> Outcome {
        match self {
            Modal::Help => match key {
                Key::Esc | Key::Char('q') | Key::Char('?') | Key::Enter => Outcome::Cancel,
                _ => Outcome::Pending,
            },
            Modal::Confirm { commit, .. } => match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => Outcome::Commit(commit.clone()),
                Key::Esc | Key::Char('n') | Key::Char('N') => Outcome::Cancel,
                _ => Outcome::Pending,
            },
            Modal::Input { edit, kind, .. } => match key {
                Key::Esc => Outcome::Cancel,
                Key::Enter => {
                    let text = edit.buf.trim().to_string();
                    match kind {
                        InputKind::NewFolder { parent } => {
                            if text.is_empty() {
                                Outcome::Cancel
                            } else {
                                Outcome::Commit(Commit::NewFolder {
                                    parent: parent.clone(),
                                    name: text,
                                })
                            }
                        }
                        InputKind::RenameFolder { id } => {
                            if text.is_empty() {
                                Outcome::Cancel
                            } else {
                                Outcome::Commit(Commit::RenameFolder {
                                    id: id.clone(),
                                    name: text,
                                })
                            }
                        }
                        InputKind::RenameSession { key } => {
                            Outcome::Commit(Commit::RenameSession {
                                key: key.clone(),
                                name: text,
                            })
                        }
                        InputKind::NewSessionDir {
                            agent,
                            folder,
                            remote,
                        } => {
                            if text.is_empty() {
                                Outcome::Pending
                            } else {
                                Outcome::Commit(Commit::NewSessionDir {
                                    agent: *agent,
                                    folder: folder.clone(),
                                    dir: text,
                                    remote: remote.clone(),
                                })
                            }
                        }
                        InputKind::NewSessionName {
                            agent,
                            folder,
                            dir,
                            remote,
                        } => Outcome::Commit(Commit::NewSessionName {
                            agent: *agent,
                            folder: folder.clone(),
                            dir: dir.clone(),
                            name: text,
                            remote: remote.clone(),
                        }),
                        InputKind::AddMachineName => {
                            if text.is_empty() {
                                Outcome::Cancel
                            } else {
                                Outcome::Commit(Commit::AddMachineName { name: text })
                            }
                        }
                        InputKind::AddMachineHost { name, .. } => {
                            if text.is_empty() {
                                Outcome::Pending
                            } else {
                                Outcome::Commit(Commit::AddMachineHost {
                                    name: name.clone(),
                                    host: text,
                                })
                            }
                        }
                        InputKind::AddMachineDir { name, host } => {
                            Outcome::Commit(Commit::AddMachine {
                                name: name.clone(),
                                host: host.clone(),
                                dir: text,
                            })
                        }
                    }
                }
                k => {
                    let _ = edit.handle(k);
                    Outcome::Pending
                }
            },
            Modal::PickDir(p) => p.handle_key(key),
            Modal::PickAgent {
                folder,
                dir_hint,
                idx,
                claude_ok,
                codex_ok,
                remote,
            } => {
                // Selecting an uninstalled agent is refused here so the app
                // never has to re-validate the commit path.
                let pick = |agent: AgentKind| -> Outcome {
                    let ok = match agent {
                        AgentKind::Claude => *claude_ok,
                        AgentKind::Codex => *codex_ok,
                        // Not offered by this picker (shell panes have their
                        // own flow) — refuse defensively.
                        AgentKind::Shell => false,
                    };
                    if ok {
                        Outcome::Commit(Commit::NewSessionAgent {
                            agent,
                            folder: folder.clone(),
                            dir_hint: dir_hint.clone(),
                            remote: remote.clone(),
                        })
                    } else {
                        Outcome::Msg(format!(
                            "{} is not installed — run `vag doctor`",
                            agent.label()
                        ))
                    }
                };
                match key {
                    Key::Esc => Outcome::Cancel,
                    Key::Char('c') => pick(AgentKind::Claude),
                    Key::Char('x') => pick(AgentKind::Codex),
                    Key::Up | Key::Down | Key::Char('j') | Key::Char('k') | Key::Tab => {
                        *idx = 1 - *idx;
                        Outcome::Pending
                    }
                    Key::Enter => pick(if *idx == 0 {
                        AgentKind::Claude
                    } else {
                        AgentKind::Codex
                    }),
                    _ => Outcome::Pending,
                }
            }
            Modal::PickColor {
                key: skey,
                options,
                idx,
            } => match key {
                Key::Esc => Outcome::Cancel,
                Key::Up | Key::Char('k') => {
                    *idx = idx.saturating_sub(1);
                    Outcome::Pending
                }
                Key::Down | Key::Char('j') => {
                    if *idx + 1 < options.len() {
                        *idx += 1;
                    }
                    Outcome::Pending
                }
                Key::Enter => Outcome::Commit(Commit::SetSessionColor {
                    key: skey.clone(),
                    color: options.get(*idx).cloned().flatten(),
                }),
                _ => Outcome::Pending,
            },
            Modal::PickFolder {
                key: skey,
                options,
                idx,
            } => match key {
                Key::Esc => Outcome::Cancel,
                Key::Up | Key::Char('k') => {
                    *idx = idx.saturating_sub(1);
                    Outcome::Pending
                }
                Key::Down | Key::Char('j') => {
                    if *idx + 1 < options.len() {
                        *idx += 1;
                    }
                    Outcome::Pending
                }
                Key::Enter => {
                    let folder = options.get(*idx).and_then(|(id, _)| id.clone());
                    Outcome::Commit(Commit::MoveSession {
                        key: skey.clone(),
                        folder,
                    })
                }
                _ => Outcome::Pending,
            },
            Modal::PickLocation {
                agent,
                folder,
                dir_hint,
                options,
                idx,
                ..
            } => match key {
                Key::Esc => Outcome::Cancel,
                Key::Up | Key::Char('k') => {
                    *idx = idx.saturating_sub(1);
                    Outcome::Pending
                }
                Key::Down | Key::Char('j') => {
                    if *idx + 1 < options.len() {
                        *idx += 1;
                    }
                    Outcome::Pending
                }
                Key::Enter => match options.get(*idx) {
                    Some(LocationChoice::AddMachine) => Outcome::Commit(Commit::StartAddMachine),
                    Some(choice) => Outcome::Commit(Commit::NewSessionLocation {
                        agent: *agent,
                        folder: folder.clone(),
                        dir_hint: dir_hint.clone(),
                        remote: match choice {
                            LocationChoice::Remote { name, .. } => Some(name.clone()),
                            _ => None,
                        },
                    }),
                    None => Outcome::Pending,
                },
                _ => Outcome::Pending,
            },
            Modal::Settings { rows, idx } => match key {
                Key::Esc | Key::Char('q') => Outcome::Cancel,
                Key::Down | Key::Char('j') => {
                    *idx = settings_step(rows, *idx, 1);
                    Outcome::Pending
                }
                Key::Up | Key::Char('k') => {
                    *idx = settings_step(rows, *idx, -1);
                    Outcome::Pending
                }
                Key::Left | Key::Char('h') | Key::Right | Key::Char('l') | Key::Enter => {
                    let dir: i8 = if matches!(key, Key::Left | Key::Char('h')) {
                        -1
                    } else {
                        1
                    };
                    match rows.get(*idx) {
                        // The theme gets the live-preview picker, not a
                        // blind cycle.
                        Some(SettingRow::Value {
                            id: SettingId::Theme,
                            ..
                        }) => Outcome::Commit(Commit::OpenThemePicker { from_idx: *idx }),
                        Some(SettingRow::Value { id, .. }) => {
                            Outcome::Commit(Commit::SettingCycle {
                                id: *id,
                                dir,
                                idx: *idx,
                            })
                        }
                        Some(SettingRow::Key { target, .. }) if matches!(key, Key::Enter) => {
                            Outcome::Commit(Commit::StartCapture {
                                target: *target,
                                from_idx: *idx,
                            })
                        }
                        _ => Outcome::Pending,
                    }
                }
                _ => Outcome::Pending,
            },
            Modal::PickTheme {
                options,
                idx,
                from_idx,
                ..
            } => match key {
                Key::Esc | Key::Char('q') => Outcome::Cancel,
                Key::Up | Key::Char('k') => {
                    *idx = idx.saturating_sub(1);
                    Outcome::Pending
                }
                Key::Down | Key::Char('j') => {
                    if *idx + 1 < options.len() {
                        *idx += 1;
                    }
                    Outcome::Pending
                }
                Key::Enter => Outcome::Commit(Commit::SetTheme {
                    name: options[*idx].clone(),
                    from_idx: *from_idx,
                }),
                _ => Outcome::Pending,
            },
            Modal::CaptureKey { target, from_idx } => match (*target, key) {
                (_, Key::Esc) => Outcome::Cancel,
                (BindTarget::Ctrl(_), Key::Ctrl(c)) => {
                    match DetachKey::parse(&format!("ctrl-{c}")) {
                        Some(k) => Outcome::Commit(Commit::SetBinding {
                            target: *target,
                            ch: (b'a' + k.byte() - 1) as char,
                            from_idx: *from_idx,
                        }),
                        None => Outcome::Msg(format!(
                            "ctrl-{c} can't be bound (tab/enter aliases) — try another"
                        )),
                    }
                }
                (BindTarget::Ctrl(_), _) => {
                    Outcome::Msg("this is a ctrl chord — press ctrl-<letter>".into())
                }
                (BindTarget::Action(_), Key::Char(c)) => {
                    if KeysConfig::is_reserved(*c) {
                        Outcome::Msg(format!("`{c}` is reserved for navigation — try another"))
                    } else {
                        Outcome::Commit(Commit::SetBinding {
                            target: *target,
                            ch: *c,
                            from_idx: *from_idx,
                        })
                    }
                }
                (BindTarget::Action(_), _) => {
                    Outcome::Msg("bindings are single characters — press one (esc cancels)".into())
                }
            },
        }
    }

    /// `backdrop`: solid themes paint modal interiors with this color after
    /// clearing (otherwise the terminal's own background bleeds through the
    /// box); `Color::Reset` = classic transparent behavior. `sel` is the
    /// picker cursor-row background (same bar as the tree — REVERSED would
    /// flip colored labels into background patches). `keys` labels the help
    /// overlay with the user's actual bindings.
    pub fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        icons: &Icons,
        backdrop: Color,
        sel: Color,
        keys: &KeysConfig,
    ) {
        match self {
            Modal::Help => render_help(f, area, backdrop, keys),
            Modal::Confirm { msg, .. } => {
                let w = 60u16;
                let inner_w = w.saturating_sub(2).max(1) as usize;
                if msg.contains('\n') {
                    // Multi-line (e.g. the edit-mode apply list): one row per
                    // line, left-aligned and truncated instead of wrapped so
                    // the box height stays exact.
                    let parts: Vec<&str> = msg.split('\n').collect();
                    let r = centered(area, w, (parts.len() as u16).saturating_add(4));
                    clear_with_backdrop(f, r, backdrop);
                    let mut lines = vec![Line::raw("")];
                    for p in &parts {
                        lines.push(Line::raw(format!(
                            "  {}",
                            truncate_chars(p, inner_w.saturating_sub(2))
                        )));
                    }
                    lines.push(
                        Line::from(Span::styled("y / enter: yes    n / esc: no", dim())).centered(),
                    );
                    f.render_widget(Paragraph::new(lines).block(boxed(" confirm ")), r);
                    return;
                }
                // Long single-line messages wrap; size the box for the rows.
                let msg_rows = msg.chars().count().max(1).div_ceil(inner_w) as u16;
                let r = centered(area, w, 4 + msg_rows);
                clear_with_backdrop(f, r, backdrop);
                let p = Paragraph::new(vec![
                    Line::raw(""),
                    Line::from(Span::raw(msg.clone())).centered(),
                    Line::from(Span::styled("y / enter: yes    n / esc: no", dim())).centered(),
                ])
                .wrap(Wrap { trim: true })
                .block(boxed(" confirm "));
                f.render_widget(p, r);
            }
            Modal::Input { title, edit, kind } => {
                // Suggestion line for the add-machine host step: connectable
                // aliases already in ~/.ssh/config.
                let hint = match kind {
                    InputKind::AddMachineHost { suggestions, .. } if !suggestions.is_empty() => {
                        Some(format!("from ~/.ssh/config: {}", suggestions.join(", ")))
                    }
                    _ => None,
                };
                let r = centered(area, 64, if hint.is_some() { 5 } else { 4 });
                clear_with_backdrop(f, r, backdrop);
                let before = &edit.buf[..edit.cursor];
                let cursor_char = edit.buf[edit.cursor..].chars().next();
                let after = cursor_char
                    .map(|c| &edit.buf[edit.cursor + c.len_utf8()..])
                    .unwrap_or("");
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::raw(before.to_string()),
                    Span::styled(
                        cursor_char.map(String::from).unwrap_or_else(|| " ".into()),
                        Style::new().add_modifier(Modifier::REVERSED),
                    ),
                    Span::raw(after.to_string()),
                ]);
                let mut lines = vec![Line::raw(""), line];
                if let Some(h) = hint {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", truncate_chars(&h, 58)),
                        dim(),
                    )));
                }
                let p = Paragraph::new(lines).block(boxed(format!(" {title} ")));
                f.render_widget(p, r);
            }
            Modal::PickDir(pick) => {
                let w = 64u16.min(area.width.saturating_sub(2)).max(30);
                let inner_w = w.saturating_sub(2) as usize;
                let rows = pick.filtered.len().min(DIR_PICK_ROWS);
                let r = centered(area, w, rows as u16 + 5);
                clear_with_backdrop(f, r, backdrop);
                // Input row, cursor rendered exactly like Modal::Input.
                let edit = &pick.edit;
                let before = &edit.buf[..edit.cursor];
                let cursor_char = edit.buf[edit.cursor..].chars().next();
                let after = cursor_char
                    .map(|c| &edit.buf[edit.cursor + c.len_utf8()..])
                    .unwrap_or("");
                let mut lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::raw(before.to_string()),
                        Span::styled(
                            cursor_char.map(String::from).unwrap_or_else(|| " ".into()),
                            Style::new().add_modifier(Modifier::REVERSED),
                        ),
                        Span::raw(after.to_string()),
                    ]),
                ];
                // Match list, scrolled so the highlight stays visible.
                let top = pick.sel.unwrap_or(0).saturating_sub(rows.saturating_sub(1));
                for (i, &ci) in pick.filtered.iter().enumerate().skip(top).take(rows) {
                    let label = truncate_keep_tail(&pick.candidates[ci], inner_w.saturating_sub(4));
                    let style = if pick.sel == Some(i) {
                        Style::new().bg(sel)
                    } else {
                        Style::new()
                    };
                    let mut text = format!("  {label}");
                    if pick.sel == Some(i) {
                        // Fill the row so the highlight bar spans the box.
                        let used = text.chars().count();
                        if inner_w > used {
                            text.push_str(&" ".repeat(inner_w - used));
                        }
                    }
                    lines.push(Line::from(Span::styled(text, style)));
                }
                let mut hint = format!(
                    "  {} dirs · tab complete · ↑↓ pick · enter accept",
                    pick.filtered.len()
                );
                if pick.scanning {
                    hint.push_str(" · scanning…");
                }
                lines.push(Line::from(Span::styled(
                    truncate_chars(&hint, inner_w),
                    dim(),
                )));
                let p = Paragraph::new(lines).block(boxed(format!(" {} ", pick.title)));
                f.render_widget(p, r);
            }
            Modal::PickAgent {
                idx,
                claude_ok,
                codex_ok,
                ..
            } => {
                let r = centered(area, 44, 6);
                clear_with_backdrop(f, r, backdrop);
                let mk = |icon: &str, label: &str, hot: &str, on: bool, ok: bool| {
                    let mut style = if on {
                        Style::new().bg(sel)
                    } else {
                        Style::new()
                    };
                    let suffix = if ok {
                        ""
                    } else {
                        style = style.fg(Color::DarkGray);
                        " (not installed)"
                    };
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("[{hot}] {icon} {label}{suffix}"), style),
                    ])
                };
                let p = Paragraph::new(vec![
                    Line::raw(""),
                    mk(icons.claude, "Claude Code", "c", *idx == 0, *claude_ok),
                    mk(icons.codex, "Codex", "x", *idx == 1, *codex_ok),
                    Line::from(Span::styled("  enter: select   esc: cancel", dim())),
                ])
                .block(boxed(" new session — agent "));
                f.render_widget(p, r);
            }
            Modal::PickColor { options, idx, .. } => {
                let h = (options.len() as u16 + 3)
                    .min(area.height.saturating_sub(2))
                    .max(4);
                let r = centered(area, 34, h);
                clear_with_backdrop(f, r, backdrop);
                let visible = (h - 3) as usize;
                let top = idx.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::raw("")];
                for (i, opt) in options.iter().enumerate().skip(top).take(visible) {
                    let (label, mut style) = match opt {
                        None => ("default".to_string(), Style::new()),
                        Some(name) => (
                            format!("● {name}"),
                            crate::ui::dashboard::parse_session_color(name)
                                .map(|c| Style::new().fg(c))
                                .unwrap_or_default(),
                        ),
                    };
                    if i == *idx {
                        style = style.bg(sel);
                    }
                    lines.push(Line::from(Span::styled(format!("  {label}"), style)));
                }
                let p = Paragraph::new(lines).block(boxed(" session color "));
                f.render_widget(p, r);
            }
            Modal::PickFolder { options, idx, .. } => {
                let h = (options.len() as u16 + 3)
                    .min(area.height.saturating_sub(2))
                    .max(4);
                let r = centered(area, 50, h);
                clear_with_backdrop(f, r, backdrop);
                let visible = (h - 3) as usize;
                let top = idx.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::raw("")];
                for (i, (_, label)) in options.iter().enumerate().skip(top).take(visible) {
                    let style = if i == *idx {
                        Style::new().bg(sel)
                    } else {
                        Style::new()
                    };
                    lines.push(Line::from(Span::styled(format!("  {label}"), style)));
                }
                let p = Paragraph::new(lines).block(boxed(" move to folder "));
                f.render_widget(p, r);
            }
            Modal::PickLocation { options, idx, .. } => {
                let h = (options.len() as u16 + 3)
                    .min(area.height.saturating_sub(2))
                    .max(4);
                let r = centered(area, 54, h);
                clear_with_backdrop(f, r, backdrop);
                let visible = (h - 3) as usize;
                let top = idx.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::raw("")];
                for (i, opt) in options.iter().enumerate().skip(top).take(visible) {
                    let base = if i == *idx {
                        Style::new().bg(sel)
                    } else {
                        Style::new()
                    };
                    let mut spans = vec![Span::raw("  ")];
                    match opt {
                        LocationChoice::Local => {
                            spans.push(Span::styled(format!("{} This machine", icons.local), base));
                        }
                        LocationChoice::Remote { name, host } => {
                            spans.push(Span::styled(format!("{} {name}", icons.remote), base));
                            spans.push(Span::styled(
                                format!("  ({host})"),
                                base.fg(Color::DarkGray),
                            ));
                        }
                        LocationChoice::AddMachine => {
                            spans.push(Span::styled(
                                "+ add a machine…".to_string(),
                                base.fg(Color::Cyan),
                            ));
                        }
                    }
                    lines.push(Line::from(spans));
                }
                let p = Paragraph::new(lines).block(boxed(" new session — where "));
                f.render_widget(p, r);
            }
            Modal::Settings { rows, idx } => {
                let w = 62u16.min(area.width.saturating_sub(2)).max(30);
                let inner_w = w.saturating_sub(2) as usize;
                let h = (rows.len() as u16 + 4)
                    .min(area.height.saturating_sub(2))
                    .max(6);
                let r = centered(area, w, h);
                clear_with_backdrop(f, r, backdrop);
                let visible = (h - 4) as usize;
                let top = idx.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::raw("")];
                for (i, row) in rows.iter().enumerate().skip(top).take(visible) {
                    let mut line = match row {
                        SettingRow::Section(name) => Line::from(Span::styled(
                            format!("  {name}"),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        SettingRow::Value { label, value, .. } => Line::from(vec![
                            Span::raw(format!("    {label:<26}")),
                            Span::styled(
                                format!("‹ {value} ›"),
                                Style::new().add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        SettingRow::Key { label, value, .. } => Line::from(vec![
                            Span::raw(format!("    {label:<26}")),
                            Span::styled(value.clone(), Style::new().fg(Color::Cyan)),
                        ]),
                    };
                    if i == *idx {
                        let used: usize =
                            line.spans.iter().map(|s| s.content.chars().count()).sum();
                        if inner_w > used {
                            line.push_span(Span::raw(" ".repeat(inner_w - used)));
                        }
                        line = line.style(Style::new().bg(sel));
                    }
                    lines.push(line);
                }
                lines.push(Line::from(Span::styled(
                    "  ↑↓ move · enter/h/l change · enter on a key: rebind · esc close",
                    dim(),
                )));
                let p = Paragraph::new(lines).block(boxed(" settings "));
                f.render_widget(p, r);
            }
            Modal::PickTheme { options, idx, .. } => {
                let h = (options.len() as u16 + 4)
                    .min(area.height.saturating_sub(2))
                    .max(5);
                let r = centered(area, 36, h);
                clear_with_backdrop(f, r, backdrop);
                let visible = (h - 4) as usize;
                let top = idx.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::raw("")];
                for (i, name) in options.iter().enumerate().skip(top).take(visible) {
                    let style = if i == *idx {
                        Style::new().bg(sel).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    };
                    lines.push(Line::from(Span::styled(format!("  {name}"), style)));
                }
                lines.push(Line::from(Span::styled(
                    "  ↑↓ preview · enter apply · esc revert",
                    dim(),
                )));
                let p = Paragraph::new(lines).block(boxed(" theme "));
                f.render_widget(p, r);
            }
            Modal::CaptureKey { target, .. } => {
                let r = centered(area, 48, 4);
                clear_with_backdrop(f, r, backdrop);
                let want = match target {
                    BindTarget::Ctrl(_) => "press ctrl-<letter>",
                    BindTarget::Action(_) => "press a key",
                };
                let p = Paragraph::new(vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw(format!("  {want}")),
                        Span::styled("   (esc cancels)", dim()),
                    ]),
                ])
                .block(boxed(format!(" rebind — {} ", target.title())));
                f.render_widget(p, r);
            }
        }
    }
}

fn render_help(f: &mut Frame, area: Rect, backdrop: Color, keys: &KeysConfig) {
    // Commands grouped by what they act on, labelled with the CONFIGURED
    // bindings (settings page / [keys] can remap them). Two columns keep
    // the box short enough to never clip; the README carries the nuance.
    type Section = (&'static str, Vec<(String, &'static str)>);
    let k = |a: KeyAction| keys.get(a).to_string();
    let kc = |a: CtrlAction| keys.get_ctrl(a).label();

    let navigate: Section = (
        "navigate",
        vec![
            ("j/k ↑↓".into(), "move"),
            ("h / l".into(), "collapse / focus pane"),
            ("tab".into(), "tree ⇄ pane focus"),
            (
                format!(
                    "{} / {}",
                    kc(CtrlAction::FocusTree),
                    kc(CtrlAction::FocusPane)
                ),
                "pane ⇄ tree (keeps active session)",
            ),
            (kc(CtrlAction::ToggleSidebar), "toggle sidebar"),
            ("space".into(), "collapse folder"),
            ("/".into(), "filter (esc clears)"),
            ("1..9".into(), "jump to open session"),
            ("pgup/dn".into(), "scroll pane"),
            (format!("{} / end", k(KeyAction::Settings)), "settings"),
            (k(KeyAction::Quit), "quit"),
        ],
    );
    let folders: Section = (
        "folders",
        vec![
            (k(KeyAction::NewFolder), "new folder"),
            (k(KeyAction::Rename), "rename"),
            (k(KeyAction::BindDir), "bind default dir"),
            (k(KeyAction::Scope), "repo scope on/off"),
            (k(KeyAction::Delete), "delete folder"),
        ],
    );
    let edit: Section = (
        "edit mode",
        vec![(k(KeyAction::EditMode), "edit tree (vim; :w :q)")],
    );

    let sessions: Section = (
        "sessions",
        vec![
            ("enter".into(), "open"),
            (k(KeyAction::NewSession), "new session"),
            (k(KeyAction::Fork), "fork"),
            (k(KeyAction::Rename), "rename"),
            (k(KeyAction::MoveSession), "move to folder"),
            (k(KeyAction::Color), "set color"),
            (k(KeyAction::Hide), "hide / unhide"),
            (k(KeyAction::ShowHidden), "show hidden/archived"),
            (k(KeyAction::Archive), "archive (codex)"),
            (k(KeyAction::Delete), "delete"),
            (k(KeyAction::CloseRuntime), "close process"),
            (k(KeyAction::Zoom), "zoom full-screen"),
        ],
    );
    let machines: Section = (
        "machines & shells",
        vec![
            (k(KeyAction::AddMachine), "add machine (ssh)"),
            (k(KeyAction::Shell), "shell (here / on machine)"),
            (
                format!("{}/enter", k(KeyAction::NewSession)),
                "machine: new session",
            ),
            (k(KeyAction::Delete), "machine: remove"),
        ],
    );

    let render_section = |out: &mut Vec<Line<'static>>, (title, rows): Section| {
        out.push(Line::from(Span::styled(
            format!("  {title}"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        for (key_label, v) in rows {
            out.push(Line::from(vec![
                Span::styled(format!("  {key_label:<9}"), Style::new().fg(Color::Cyan)),
                Span::raw(v.to_string()),
            ]));
        }
    };

    let mut left: Vec<Line> = Vec::new();
    render_section(&mut left, navigate);
    left.push(Line::raw(""));
    render_section(&mut left, folders);
    left.push(Line::raw(""));
    render_section(&mut left, edit);

    let mut right: Vec<Line> = Vec::new();
    render_section(&mut right, sessions);
    right.push(Line::raw(""));
    render_section(&mut right, machines);

    let footer = vec![
        Line::from(Span::styled(
            "  first ssh connect asks for credentials inside the pane",
            dim(),
        )),
        Line::from(Span::styled(
            "  in a pane every key goes to the agent except the detach key",
            dim(),
        )),
    ];

    let body_h = left.len().max(right.len());
    let total = 1 + body_h + 1 + footer.len(); // top pad + body + gap + footer
    let r = centered(area, 78, (total as u16 + 2).min(area.height));
    clear_with_backdrop(f, r, backdrop);
    let block = boxed(" help ");
    let inner = block.inner(r);
    f.render_widget(block, r);

    let [_, body, _, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(body_h as u16),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let [lcol, rcol] =
        Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).areas(body);
    f.render_widget(Paragraph::new(left), lcol);
    f.render_widget(Paragraph::new(right), rcol);
    f.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), foot);
}

/// Char-count truncation with an ellipsis (display width is approximated by
/// char count — fine for modal box contents).
fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Truncation that keeps the END of the string — for paths, the tail is the
/// part that distinguishes candidates.
fn truncate_keep_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn clear_with_backdrop(f: &mut Frame, r: Rect, backdrop: Color) {
    f.render_widget(Clear, r);
    if backdrop != Color::Reset {
        f.render_widget(Block::new().style(Style::new().bg(backdrop)), r);
    }
}

fn boxed(title: impl Into<String>) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .title(title.into())
        .border_style(Style::new().fg(Color::Cyan))
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> SessionKey {
        SessionKey::new(AgentKind::Codex, id.to_string())
    }

    #[test]
    fn confirm_commits_open_anyway_on_y() {
        let k = key("abc");
        let mut m = Modal::Confirm {
            msg: "open?".into(),
            commit: Commit::OpenAnyway { key: k.clone() },
        };
        match m.handle_key(&Key::Char('y')) {
            Outcome::Commit(Commit::OpenAnyway { key: got }) => assert_eq!(got, k),
            other => panic!("expected OpenAnyway commit, got {other:?}"),
        }
    }

    #[test]
    fn rekey_session_rewrites_modal_payloads() {
        let from = key("pending-1");
        let to = key("real-id");

        let mut m = Modal::Confirm {
            msg: String::new(),
            commit: Commit::CloseRuntime { key: from.clone() },
        };
        m.rekey_session(&from, &to);
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::CloseRuntime { key: got }) => assert_eq!(got, to),
            other => panic!("expected CloseRuntime commit, got {other:?}"),
        }

        let mut m = Modal::PickFolder {
            key: from.clone(),
            options: vec![(None, "Inbox".into())],
            idx: 0,
        };
        m.rekey_session(&from, &to);
        match m {
            Modal::PickFolder { key: got, .. } => assert_eq!(got, to),
            _ => unreachable!(),
        }

        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::default(),
            kind: InputKind::RenameSession { key: from.clone() },
        };
        m.rekey_session(&from, &to);
        match m {
            Modal::Input {
                kind: InputKind::RenameSession { key: got },
                ..
            } => assert_eq!(got, to),
            _ => unreachable!(),
        }
    }

    #[test]
    fn rekey_session_rewrites_apply_edits_actions() {
        let from = key("pending-1");
        let to = key("real-id");
        let other = key("unrelated");
        let mut m = Modal::Confirm {
            msg: String::new(),
            commit: Commit::ApplyEdits {
                actions: vec![
                    EditAction::HideSession { key: from.clone() },
                    EditAction::MoveSession {
                        key: other.clone(),
                        folder: None,
                    },
                    EditAction::ForkInto {
                        key: from.clone(),
                        folder: Some("f1".into()),
                    },
                    EditAction::DeleteFolder { id: "f2".into() },
                ],
                and_quit: true,
            },
        };
        m.rekey_session(&from, &to);
        match m {
            Modal::Confirm {
                commit: Commit::ApplyEdits { actions, and_quit },
                ..
            } => {
                assert!(and_quit);
                assert_eq!(actions[0], EditAction::HideSession { key: to.clone() });
                assert_eq!(
                    actions[1],
                    EditAction::MoveSession {
                        key: other,
                        folder: None
                    }
                );
                assert_eq!(
                    actions[2],
                    EditAction::ForkInto {
                        key: to,
                        folder: Some("f1".into())
                    }
                );
                assert_eq!(actions[3], EditAction::DeleteFolder { id: "f2".into() });
            }
            _ => unreachable!(),
        }
    }

    fn pick_agent(idx: usize, claude_ok: bool, codex_ok: bool) -> Modal {
        Modal::PickAgent {
            folder: None,
            dir_hint: None,
            idx,
            claude_ok,
            codex_ok,
            remote: None,
        }
    }

    #[test]
    fn pick_agent_commits_available_agents() {
        let mut m = pick_agent(0, true, true);
        match m.handle_key(&Key::Char('c')) {
            Outcome::Commit(Commit::NewSessionAgent { agent, .. }) => {
                assert_eq!(agent, AgentKind::Claude);
            }
            other => panic!("expected Claude commit, got {other:?}"),
        }
        match m.handle_key(&Key::Char('x')) {
            Outcome::Commit(Commit::NewSessionAgent { agent, .. }) => {
                assert_eq!(agent, AgentKind::Codex);
            }
            other => panic!("expected Codex commit, got {other:?}"),
        }
        // enter picks the highlighted row
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionAgent { agent, .. }) => {
                assert_eq!(agent, AgentKind::Claude);
            }
            other => panic!("expected Claude commit, got {other:?}"),
        }
    }

    #[test]
    fn pick_agent_refuses_unavailable_agents() {
        let mut m = pick_agent(1, false, true);
        // hotkey on the missing agent: modal stays, message surfaces
        match m.handle_key(&Key::Char('c')) {
            Outcome::Msg(s) => assert!(s.contains("claude") && s.contains("vag doctor"), "{s}"),
            other => panic!("expected refusal Msg, got {other:?}"),
        }
        // enter on the missing agent's row is refused too
        m.handle_key(&Key::Char('j')); // toggle highlight onto claude
        match m.handle_key(&Key::Enter) {
            Outcome::Msg(s) => assert!(s.contains("not installed"), "{s}"),
            other => panic!("expected refusal Msg, got {other:?}"),
        }
        // the available agent still commits
        match m.handle_key(&Key::Char('x')) {
            Outcome::Commit(Commit::NewSessionAgent { agent, .. }) => {
                assert_eq!(agent, AgentKind::Codex);
            }
            other => panic!("expected Codex commit, got {other:?}"),
        }
        // …and the mirrored case refuses codex
        let mut m = pick_agent(0, true, false);
        match m.handle_key(&Key::Char('x')) {
            Outcome::Msg(s) => assert!(s.contains("codex"), "{s}"),
            other => panic!("expected refusal Msg, got {other:?}"),
        }
    }

    #[test]
    fn truncate_chars_ellipsis_and_bounds() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("exactly-10", 10), "exactly-10");
        assert_eq!(truncate_chars("longer-than-max", 8), "longer-…");
        assert_eq!(truncate_chars("é你é你é你", 4), "é你é…");
        assert_eq!(truncate_chars("anything", 0), "");
    }

    fn pick_location(idx: usize) -> Modal {
        Modal::PickLocation {
            agent: AgentKind::Claude,
            folder: Some("f1".into()),
            dir_hint: Some("/repo".into()),
            options: vec![
                LocationChoice::Local,
                LocationChoice::Remote {
                    name: "gpu".into(),
                    host: "user@gpu.example".into(),
                },
                LocationChoice::Remote {
                    name: "web".into(),
                    host: "root@web".into(),
                },
                LocationChoice::AddMachine,
            ],
            idx,
        }
    }

    #[test]
    fn pick_location_navigates_with_jk_and_clamps() {
        let mut m = pick_location(0);
        assert!(matches!(m.handle_key(&Key::Char('k')), Outcome::Pending));
        let Modal::PickLocation { idx, .. } = &m else {
            unreachable!()
        };
        assert_eq!(*idx, 0, "k clamps at the top");
        m.handle_key(&Key::Char('j'));
        m.handle_key(&Key::Down);
        m.handle_key(&Key::Char('j'));
        m.handle_key(&Key::Char('j')); // past the end: clamped
        let Modal::PickLocation { idx, .. } = &m else {
            unreachable!()
        };
        assert_eq!(*idx, 3, "j/down clamp at the last option");
        m.handle_key(&Key::Up);
        m.handle_key(&Key::Up);
        let Modal::PickLocation { idx, .. } = &m else {
            unreachable!()
        };
        assert_eq!(*idx, 1);
        assert!(matches!(m.handle_key(&Key::Esc), Outcome::Cancel));
    }

    #[test]
    fn pick_location_commits_local_and_remote() {
        // First row (None) = this machine; folder + dir_hint carried through.
        let mut m = pick_location(0);
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionLocation {
                agent,
                folder,
                dir_hint,
                remote,
            }) => {
                assert_eq!(agent, AgentKind::Claude);
                assert_eq!(folder.as_deref(), Some("f1"));
                assert_eq!(dir_hint.as_deref(), Some("/repo"));
                assert_eq!(remote, None);
            }
            other => panic!("expected local NewSessionLocation, got {other:?}"),
        }
        let mut m = pick_location(1);
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionLocation { remote, .. }) => {
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected remote NewSessionLocation, got {other:?}"),
        }
    }

    #[test]
    fn pick_location_add_machine_row_starts_the_add_flow() {
        let mut m = pick_location(3);
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::StartAddMachine) => {}
            other => panic!("expected StartAddMachine, got {other:?}"),
        }
    }

    #[test]
    fn pick_agent_threads_a_preselected_machine() {
        let mut m = Modal::PickAgent {
            folder: None,
            dir_hint: None,
            idx: 0,
            // Machine pre-selected: local availability is irrelevant, both ok.
            claude_ok: true,
            codex_ok: true,
            remote: Some("gpu".into()),
        };
        match m.handle_key(&Key::Char('x')) {
            Outcome::Commit(Commit::NewSessionAgent { agent, remote, .. }) => {
                assert_eq!(agent, AgentKind::Codex);
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected NewSessionAgent commit, got {other:?}"),
        }
    }

    #[test]
    fn add_machine_inputs_step_through_name_host_dir() {
        // Step 1: empty name cancels; a real one commits.
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::default(),
            kind: InputKind::AddMachineName,
        };
        assert!(matches!(m.handle_key(&Key::Enter), Outcome::Cancel));
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::with_text("  gpu  "),
            kind: InputKind::AddMachineName,
        };
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::AddMachineName { name }) => assert_eq!(name, "gpu"),
            other => panic!("expected AddMachineName, got {other:?}"),
        }
        // Step 2: host must not be empty (stays pending), then commits.
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::default(),
            kind: InputKind::AddMachineHost {
                name: "gpu".into(),
                suggestions: vec!["a".into(), "b".into()],
            },
        };
        assert!(matches!(m.handle_key(&Key::Enter), Outcome::Pending));
        for c in "user@10.0.0.5".chars() {
            m.handle_key(&Key::Char(c));
        }
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::AddMachineHost { name, host }) => {
                assert_eq!(name, "gpu");
                assert_eq!(host, "user@10.0.0.5");
            }
            other => panic!("expected AddMachineHost, got {other:?}"),
        }
        // Step 3: the dir is optional — empty commits too.
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::default(),
            kind: InputKind::AddMachineDir {
                name: "gpu".into(),
                host: "user@10.0.0.5".into(),
            },
        };
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::AddMachine { name, host, dir }) => {
                assert_eq!(name, "gpu");
                assert_eq!(host, "user@10.0.0.5");
                assert_eq!(dir, "");
            }
            other => panic!("expected AddMachine, got {other:?}"),
        }
    }

    #[test]
    fn new_session_inputs_thread_remote() {
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::with_text("~/work"),
            kind: InputKind::NewSessionDir {
                agent: AgentKind::Codex,
                folder: None,
                remote: Some("gpu".into()),
            },
        };
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionDir { dir, remote, .. }) => {
                assert_eq!(dir, "~/work");
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected NewSessionDir commit, got {other:?}"),
        }
        let mut m = Modal::Input {
            title: String::new(),
            edit: LineEdit::with_text("my run"),
            kind: InputKind::NewSessionName {
                agent: AgentKind::Claude,
                folder: None,
                dir: "~/work".into(),
                remote: Some("gpu".into()),
            },
        };
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionName {
                dir, name, remote, ..
            }) => {
                assert_eq!(dir, "~/work");
                assert_eq!(name, "my run");
                assert_eq!(remote.as_deref(), Some("gpu"));
            }
            other => panic!("expected NewSessionName commit, got {other:?}"),
        }
    }

    fn settings_fixture() -> Modal {
        Modal::Settings {
            rows: vec![
                SettingRow::Section("appearance".into()),
                SettingRow::Value {
                    id: SettingId::Theme,
                    label: "theme".into(),
                    value: "night".into(),
                },
                SettingRow::Value {
                    id: SettingId::Icons,
                    label: "icons".into(),
                    value: "ascii".into(),
                },
                SettingRow::Section("keys".into()),
                SettingRow::Key {
                    target: BindTarget::Ctrl(CtrlAction::Detach),
                    label: "detach".into(),
                    value: "ctrl-q".into(),
                },
            ],
            idx: 1,
        }
    }

    #[test]
    fn settings_page_skips_headers_and_routes_commits() {
        let mut m = settings_fixture();
        // j: 1 → 2; j again skips the "keys" header → 4; j at the end stays
        m.handle_key(&Key::Char('j'));
        m.handle_key(&Key::Char('j'));
        let Modal::Settings { idx, .. } = &m else {
            unreachable!()
        };
        assert_eq!(*idx, 4, "section headers are not selectable");
        m.handle_key(&Key::Char('j'));
        let Modal::Settings { idx, .. } = &m else {
            unreachable!()
        };
        assert_eq!(*idx, 4);
        // k twice: back over the header to icons
        m.handle_key(&Key::Char('k'));
        match m.handle_key(&Key::Char('l')) {
            Outcome::Commit(Commit::SettingCycle {
                id: SettingId::Icons,
                dir: 1,
                idx: 2,
            }) => {}
            other => panic!("expected icon cycle, got {other:?}"),
        }
        match m.handle_key(&Key::Char('h')) {
            Outcome::Commit(Commit::SettingCycle { dir: -1, .. }) => {}
            other => panic!("expected reverse cycle, got {other:?}"),
        }
        // enter on the theme row opens the live-preview picker
        m.handle_key(&Key::Char('k'));
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::OpenThemePicker { from_idx: 1 }) => {}
            other => panic!("expected theme picker, got {other:?}"),
        }
        // enter on a key row starts capture
        let mut m = settings_fixture();
        let Modal::Settings { idx, .. } = &mut m else {
            unreachable!()
        };
        *idx = 4;
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::StartCapture {
                target: BindTarget::Ctrl(CtrlAction::Detach),
                from_idx: 4,
            }) => {}
            other => panic!("expected capture, got {other:?}"),
        }
        assert!(matches!(m.handle_key(&Key::Esc), Outcome::Cancel));
    }

    #[test]
    fn theme_picker_moves_and_commits_hovered_name() {
        let mut m = Modal::PickTheme {
            options: vec!["night".into(), "gruvbox".into()],
            idx: 0,
            original: "night".into(),
            from_idx: 1,
        };
        assert!(matches!(m.handle_key(&Key::Char('j')), Outcome::Pending));
        match m.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::SetTheme { name, from_idx: 1 }) => {
                assert_eq!(name, "gruvbox")
            }
            other => panic!("expected SetTheme, got {other:?}"),
        }
        assert!(matches!(m.handle_key(&Key::Esc), Outcome::Cancel));
    }

    #[test]
    fn capture_key_validates_reserved_and_chord_shape() {
        let mut m = Modal::CaptureKey {
            target: BindTarget::Action(KeyAction::Fork),
            from_idx: 2,
        };
        // navigation chars are refused with a reason, not bound
        assert!(matches!(m.handle_key(&Key::Char('j')), Outcome::Msg(_)));
        assert!(matches!(m.handle_key(&Key::Char('1')), Outcome::Msg(_)));
        assert!(matches!(m.handle_key(&Key::Enter), Outcome::Msg(_)));
        match m.handle_key(&Key::Char('f')) {
            Outcome::Commit(Commit::SetBinding {
                target: BindTarget::Action(KeyAction::Fork),
                ch: 'f',
                from_idx: 2,
            }) => {}
            other => panic!("expected SetBinding, got {other:?}"),
        }
        // detach only accepts a valid ctrl chord
        let mut d = Modal::CaptureKey {
            target: BindTarget::Ctrl(CtrlAction::Detach),
            from_idx: 0,
        };
        assert!(matches!(d.handle_key(&Key::Char('a')), Outcome::Msg(_)));
        assert!(matches!(d.handle_key(&Key::Ctrl('i')), Outcome::Msg(_)));
        match d.handle_key(&Key::Ctrl('a')) {
            Outcome::Commit(Commit::SetBinding {
                target: BindTarget::Ctrl(CtrlAction::Detach),
                ch: 'a',
                ..
            }) => {}
            other => panic!("expected detach SetBinding, got {other:?}"),
        }
        assert!(matches!(d.handle_key(&Key::Esc), Outcome::Cancel));
    }

    fn dir_pick(prefill: &str, candidates: &[&str]) -> DirPick {
        DirPick::new(
            "dir".into(),
            prefill,
            DirTarget::NewSession {
                agent: AgentKind::Claude,
                folder: None,
            },
            candidates.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn typed(p: &mut DirPick, s: &str) {
        for c in s.chars() {
            p.handle_key(&Key::Char(c));
        }
    }

    #[test]
    fn dir_pick_enter_on_input_row_commits_typed_text() {
        // The input row is selected by default: enter must behave exactly
        // like the plain input it replaced, even with matches listed.
        let mut p = dir_pick("~/work", &["~/work", "~/work/api"]);
        match p.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionDir { dir, remote, .. }) => {
                assert_eq!(dir, "~/work");
                assert!(remote.is_none());
            }
            other => panic!("expected NewSessionDir, got {other:?}"),
        }
    }

    #[test]
    fn dir_pick_filters_and_commits_highlighted_candidate() {
        let mut p = dir_pick("", &["~/notes", "~/work/api", "~/work/web"]);
        typed(&mut p, "wrk");
        assert_eq!(p.filtered.len(), 2, "fuzzy query drops non-matches");
        p.handle_key(&Key::Down);
        p.handle_key(&Key::Down);
        match p.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionDir { dir, .. }) => {
                assert!(dir.starts_with("~/work/"), "{dir}");
            }
            other => panic!("expected NewSessionDir, got {other:?}"),
        }
    }

    #[test]
    fn dir_pick_up_returns_to_input_row_and_typing_resets_selection() {
        let mut p = dir_pick("", &["~/a", "~/b"]);
        p.handle_key(&Key::Down);
        assert_eq!(p.sel, Some(0));
        p.handle_key(&Key::Up);
        assert_eq!(p.sel, None, "up from the first match re-selects input");
        p.handle_key(&Key::Down);
        p.handle_key(&Key::Char('a'));
        assert_eq!(p.sel, None, "typing hands the selection back to input");
    }

    #[test]
    fn dir_pick_tab_completes_best_match_into_editor() {
        let mut p = dir_pick("", &["~/notes", "~/work/api"]);
        typed(&mut p, "api");
        p.handle_key(&Key::Tab);
        assert_eq!(p.edit.buf, "~/work/api");
        assert_eq!(p.sel, None);
        // enter now commits the completed text
        match p.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::NewSessionDir { dir, .. }) => {
                assert_eq!(dir, "~/work/api")
            }
            other => panic!("expected NewSessionDir, got {other:?}"),
        }
    }

    #[test]
    fn dir_pick_push_candidates_reranks_and_keeps_highlight() {
        let mut p = dir_pick("", &["~/work"]);
        typed(&mut p, "work");
        p.handle_key(&Key::Down);
        assert_eq!(p.selected_text(), Some("~/work"));
        p.push_candidates(vec!["~/work2".into(), "~/other".into()]);
        assert_eq!(
            p.selected_text(),
            Some("~/work"),
            "highlight survives a batch arriving mid-selection"
        );
        assert_eq!(p.filtered.len(), 2, "new batch joins the ranking");
    }

    #[test]
    fn dir_pick_bind_folder_allows_empty_commit_to_clear() {
        let mut p = DirPick::new(
            "bind".into(),
            "",
            DirTarget::BindFolder { id: "f1".into() },
            vec!["~/x".into()],
        );
        match p.handle_key(&Key::Enter) {
            Outcome::Commit(Commit::BindFolderDir { id, dir }) => {
                assert_eq!(id, "f1");
                assert_eq!(dir, "", "empty clears the binding");
            }
            other => panic!("expected BindFolderDir, got {other:?}"),
        }
        // …while the new-session flow refuses an empty path.
        let mut p = dir_pick("", &[]);
        assert!(matches!(p.handle_key(&Key::Enter), Outcome::Pending));
    }

    #[test]
    fn truncate_keep_tail_keeps_path_tails() {
        assert_eq!(truncate_keep_tail("short", 10), "short");
        assert_eq!(truncate_keep_tail("~/a/very/deep/path", 10), "…deep/path");
    }

    #[test]
    fn line_edit_treats_ctrl_h_as_backspace() {
        // 0x08 parses as Ctrl('h'); text inputs must erase with it, both
        // for readline muscle memory and legacy backspace-sends-^H setups.
        let mut e = LineEdit::with_text("ab");
        assert!(e.handle(&Key::Ctrl('h')));
        assert_eq!(e.buf, "a");
        assert!(e.handle(&Key::Backspace));
        assert_eq!(e.buf, "");
    }

    #[test]
    fn rekey_session_leaves_other_keys_alone() {
        let from = key("pending-1");
        let to = key("real-id");
        let other = key("unrelated");
        let mut m = Modal::Confirm {
            msg: String::new(),
            commit: Commit::CloseRuntime { key: other.clone() },
        };
        m.rekey_session(&from, &to);
        match m {
            Modal::Confirm {
                commit: Commit::CloseRuntime { key: got },
                ..
            } => assert_eq!(got, other),
            _ => unreachable!(),
        }
    }
}
