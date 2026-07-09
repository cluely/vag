//! oil.nvim-style editable tree buffer: the session/folder tree rendered as
//! a modal text buffer. Rename by editing a line, `dd` deletes (cut),
//! `yy` yanks (copy), `p`/`P` paste — a pasted *copy* of a session means
//! FORK it into the paste location's folder, a pasted *cut* means MOVE.
//! `o`/`O` open a new line; typing a name ending in `/` creates a folder.
//! `:w` produces a diff of actions (the app confirms then applies),
//! `:q` leaves edit mode (refused when dirty; `:q!` forces), `:wq` both.
//! Enter in Normal mode on a session opens it (only when not dirty).
//!
//! CRUCIAL DESIGN FACT (why this is simpler than real oil.nvim): every
//! keystroke is mediated by this state machine, so each line permanently
//! carries its IDENTITY as metadata. dd/p moves the identity with the line;
//! yy/p duplicates it with `copied: true`. The `:w` diff therefore compares
//! identities, never parses text — no hidden-id columns needed.
//!
//! SEMANTICS:
//! - A line's *folder context* is the nearest folder line above it with a
//!   smaller depth (or Inbox/top level → None). Pasting `p` with the cursor
//!   ON a folder line inserts the pasted line as its first child; on any
//!   other line, as a sibling below. Depths are recomputed on insert; the
//!   user never edits indentation.
//! - Readonly lines (Inbox header, provisional "(starting…)" sessions) can
//!   be moved past but not edited, deleted, or duplicated; mutating keys on
//!   them are no-ops with a message event.
//! - Folder lines display as `name/` (trailing slash, oil-style); editing
//!   the text renames. Deleting a folder line deletes the folder (children
//!   and member sessions re-parent — the app's confirm dialog spells that
//!   out). Folder lines cannot be duplicated (yy allowed, but pasting a
//!   copied folder is refused with a message).
//! - New lines (`o`/`O`, then Insert text): ending with `/` → CreateFolder
//!   in that position's folder context. Any other non-empty new-line text
//!   is reported in the diff as an ignored line (the app warns; sessions
//!   can't be typed into existence).
//! - Session rename: text edits on a session line → RenameSession with the
//!   new text (empty → reset override). Text is the DISPLAY title; the
//!   original text is remembered so unchanged lines produce no action.
//!
//! VIM SUBSET (Normal): h j k l arrows 0 $ gg G, i a I A → Insert, x
//! (delete char), dd (cut line), yy (yank line), p P (paste), o O (open
//! line + Insert), u (undo), ctrl-r (redo), : (Cmdline), enter (open
//! session). Insert: printable chars, backspace, esc. Cmdline: w, q, q!,
//! wq (+ esc cancels). Counts (e.g. `3j`) supported for j/k only.
//! Everything else is a silent no-op.

use crate::types::SessionKey;
use crate::ui::input::Key;

/// Stable identity a buffer line carries through every edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineId {
    Session(SessionKey),
    Folder(String),
    /// The Inbox pseudo-folder header (readonly).
    Inbox,
    /// A line the user created in this editing session (candidate folder).
    New,
}

#[derive(Debug, Clone)]
pub struct EditLine {
    pub id: LineId,
    pub text: String,
    pub depth: usize,
    pub readonly: bool,
    /// True when this line is a yank-paste duplicate of another line.
    pub copied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Command line content after `:` (rendered as `:{0}`).
    Cmdline(String),
}

/// What the app must do in response to a key the buffer handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditEvent {
    None,
    /// Show a transient message (refused ops, hints).
    Message(String),
    /// `:w` — compute `diff()` and run the confirm/apply flow. The buffer
    /// stays in edit mode until the app calls `mark_saved()` (after a
    /// successful apply) or discards it.
    Save,
    /// Leave edit mode (`:q` when clean, `:q!` always).
    Quit,
    /// `:wq` — save then leave.
    SaveQuit,
    /// Enter on a session line while clean.
    OpenSession(SessionKey),
}

/// One entry of the `:w` diff, in apply order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditAction {
    /// `parent` must be a pre-existing folder id (or None = top level).
    /// A new folder nested under another NEW folder is refused at diff time
    /// (surfaced as IgnoredLine) — keep v1 simple.
    CreateFolder {
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
    RenameSession {
        key: SessionKey,
        /// New display title (empty = clear the override).
        name: String,
    },
    /// Session line vanished from the buffer (dd without re-paste).
    HideSession {
        key: SessionKey,
    },
    MoveSession {
        key: SessionKey,
        folder: Option<String>,
    },
    /// A yank-pasted duplicate of a session → fork it into this folder.
    ForkInto {
        key: SessionKey,
        folder: Option<String>,
    },
    /// A typed line that isn't a folder (no trailing '/') — surfaced so the
    /// app can warn instead of silently dropping it.
    IgnoredLine {
        text: String,
    },
}

const MSG_UNSAVED: &str = "unsaved changes — :w first (or :q!)";
/// Upper bound for accumulated Normal-mode counts.
const COUNT_MAX: usize = 999;

fn readonly_msg() -> EditEvent {
    EditEvent::Message("readonly line".to_string())
}

/// Byte offset of the char at `char_col` (`text.len()` when past the end).
fn byte_at(text: &str, char_col: usize) -> usize {
    text.char_indices()
        .nth(char_col)
        .map_or(text.len(), |(i, _)| i)
}

/// Equality for dirty-tracking. `readonly` is skipped: it can never change.
fn lines_eq(a: &[EditLine], b: &[EditLine]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.id == y.id && x.text == y.text && x.depth == y.depth && x.copied == y.copied
        })
}

/// Canonical folder name from line text: trimmed, one trailing '/' dropped.
/// The slash is optional on save — deleting it alone is not a rename.
fn folder_name(text: &str) -> String {
    let t = text.trim();
    t.strip_suffix('/').unwrap_or(t).trim_end().to_string()
}

/// Folder context per line: the nearest line above with a smaller depth
/// whose id is a Folder (→ Some) or the Inbox (→ None). Smaller-depth
/// session/New lines are skipped, so members of a `dd`-ed folder (their
/// depth left dangling) attach to the next folder above, or the top level.
fn contexts(lines: &[EditLine]) -> Vec<Option<String>> {
    (0..lines.len())
        .map(|i| {
            lines[..i]
                .iter()
                .rev()
                .find_map(|l| {
                    if l.depth >= lines[i].depth {
                        return None;
                    }
                    match &l.id {
                        LineId::Folder(f) => Some(Some(f.clone())),
                        LineId::Inbox => Some(None),
                        _ => None, // dangling depth: keep walking upward
                    }
                })
                .flatten()
        })
        .collect()
}

#[derive(Clone, Copy)]
enum InsertAt {
    Cursor,
    AfterCursor,
    LineStart,
    LineEnd,
}

/// Full-state undo/redo entry (lines are few and small; cloning is cheap).
#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<EditLine>,
    row: usize,
    col: usize,
}

#[derive(Debug)]
pub struct EditBuf {
    lines: Vec<EditLine>,
    /// What diff()/dirty() compare against (new() / last mark_saved()).
    baseline: Vec<EditLine>,
    row: usize,
    /// Cursor column as a CHAR index (converted to bytes in cursor()).
    /// Normal mode keeps it ON a char; Insert may sit one past the end.
    col: usize,
    mode: Mode,
    /// Operator prefix awaiting its second key: 'd', 'y' or 'g'.
    pending: Option<char>,
    /// Accumulated Normal-mode count (applies to j/k only); 0 = none.
    count: usize,
    /// Line register: (line, was_yank). dd stores a cut, yy a copy.
    register: Option<(EditLine, bool)>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl EditBuf {
    /// Build from the CURRENT visible tree. `lines` must be in display
    /// order with correct depths (the app builds them from its Row list;
    /// the "+ new session" row is excluded).
    pub fn new(lines: Vec<EditLine>) -> EditBuf {
        EditBuf {
            baseline: lines.clone(),
            lines,
            row: 0,
            col: 0,
            mode: Mode::Normal,
            pending: None,
            count: 0,
            register: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Feed one parsed key. Never panics; unknown keys are no-ops.
    pub fn handle_key(&mut self, key: &Key) -> EditEvent {
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Insert => self.key_insert(key),
            Mode::Cmdline(cmd) => self.key_cmdline(cmd, key),
        }
    }

    /// Current lines for rendering (text, depth, readonly, id kind).
    pub fn lines(&self) -> &[EditLine] {
        &self.lines
    }

    /// (row, byte-offset-in-text) cursor for rendering.
    pub fn cursor(&self) -> (usize, usize) {
        let byte = self
            .lines
            .get(self.row)
            .map_or(0, |l| byte_at(&l.text, self.col));
        (self.row, byte)
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Any un-saved difference vs the original (or last mark_saved) state?
    pub fn dirty(&self) -> bool {
        !lines_eq(&self.lines, &self.baseline)
    }

    /// Planned actions in safe apply order: creates → renames → moves →
    /// forks → hides → folder deletes, with IgnoredLine entries last.
    /// Must be stable/idempotent (no actions for unchanged lines).
    pub fn diff(&self) -> Vec<EditAction> {
        let cur_ctx = contexts(&self.lines);
        let base_ctx = contexts(&self.baseline);

        let mut creates: Vec<(usize, EditAction)> = Vec::new();
        let mut renames: Vec<(usize, EditAction)> = Vec::new();
        let mut moves: Vec<(usize, EditAction)> = Vec::new();
        let mut forks: Vec<(usize, EditAction)> = Vec::new();
        let mut hides: Vec<EditAction> = Vec::new();
        let mut deletes: Vec<EditAction> = Vec::new();
        let mut ignored: Vec<(usize, EditAction)> = Vec::new();

        self.diff_sessions(
            &cur_ctx,
            &base_ctx,
            &mut renames,
            &mut moves,
            &mut forks,
            &mut hides,
        );
        self.diff_folders(
            &cur_ctx,
            &mut creates,
            &mut renames,
            &mut deletes,
            &mut ignored,
        );
        self.diff_new_lines(&cur_ctx, &base_ctx, &mut creates, &mut ignored);

        creates.sort_by_key(|(i, _)| *i);
        renames.sort_by_key(|(i, _)| *i);
        moves.sort_by_key(|(i, _)| *i);
        forks.sort_by_key(|(i, _)| *i);
        ignored.sort_by_key(|(i, _)| *i);

        let mut out = Vec::new();
        out.extend(creates.into_iter().map(|(_, a)| a));
        out.extend(renames.into_iter().map(|(_, a)| a));
        out.extend(moves.into_iter().map(|(_, a)| a));
        out.extend(forks.into_iter().map(|(_, a)| a));
        out.extend(hides);
        out.extend(deletes);
        out.extend(ignored.into_iter().map(|(_, a)| a));
        out
    }

    /// The app applied the diff: current buffer state becomes the new
    /// baseline (dirty() → false, copied flags cleared).
    pub fn mark_saved(&mut self) {
        for l in &mut self.lines {
            l.copied = false;
        }
        self.baseline = self.lines.clone();
    }

    // ---- diff internals -------------------------------------------------

    /// Per session key: greedily pair each current occurrence with an
    /// unconsumed baseline occurrence in the same folder context. A paired
    /// occurrence is already where it was at save time (no action). The
    /// "real" occurrence is the first paired one — or, when none sits in an
    /// original context, the first occurrence, which then MOVES; every
    /// other unpaired occurrence is a yank-paste duplicate → ForkInto.
    /// Keys only in the current buffer (a line cut before mark_saved() and
    /// pasted back after it) have no baseline occurrence: first is a Move.
    fn diff_sessions(
        &self,
        cur_ctx: &[Option<String>],
        base_ctx: &[Option<String>],
        renames: &mut Vec<(usize, EditAction)>,
        moves: &mut Vec<(usize, EditAction)>,
        forks: &mut Vec<(usize, EditAction)>,
        hides: &mut Vec<EditAction>,
    ) {
        let mut seen: Vec<&SessionKey> = Vec::new();
        let keys = self
            .baseline
            .iter()
            .chain(self.lines.iter())
            .filter_map(|l| match &l.id {
                LineId::Session(k) => Some(k),
                _ => None,
            });
        for key in keys {
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let bocc: Vec<usize> = self
                .baseline
                .iter()
                .enumerate()
                .filter(|(_, l)| matches!(&l.id, LineId::Session(k) if k == key))
                .map(|(i, _)| i)
                .collect();
            let cocc: Vec<usize> = self
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| matches!(&l.id, LineId::Session(k) if k == key))
                .map(|(i, _)| i)
                .collect();
            if cocc.is_empty() {
                hides.push(EditAction::HideSession { key: key.clone() });
                continue;
            }
            let mut bfree = bocc.clone();
            let mut paired = vec![false; cocc.len()];
            for (ci, &li) in cocc.iter().enumerate() {
                if let Some(p) = bfree.iter().position(|&bi| base_ctx[bi] == cur_ctx[li]) {
                    bfree.swap_remove(p);
                    paired[ci] = true;
                }
            }
            let real = paired.iter().position(|&p| p).unwrap_or(0);
            for (ci, &li) in cocc.iter().enumerate() {
                if ci == real {
                    if !paired[ci] {
                        moves.push((
                            li,
                            EditAction::MoveSession {
                                key: key.clone(),
                                folder: cur_ctx[li].clone(),
                            },
                        ));
                    }
                    if let Some(&bi) = bocc.first() {
                        let name = self.lines[li].text.trim();
                        if name != self.baseline[bi].text.trim() {
                            renames.push((
                                li,
                                EditAction::RenameSession {
                                    key: key.clone(),
                                    name: name.to_string(),
                                },
                            ));
                        }
                    }
                } else if !paired[ci] {
                    forks.push((
                        li,
                        EditAction::ForkInto {
                            key: key.clone(),
                            folder: cur_ctx[li].clone(),
                        },
                    ));
                }
            }
        }
    }

    fn diff_folders(
        &self,
        cur_ctx: &[Option<String>],
        creates: &mut Vec<(usize, EditAction)>,
        renames: &mut Vec<(usize, EditAction)>,
        deletes: &mut Vec<EditAction>,
        ignored: &mut Vec<(usize, EditAction)>,
    ) {
        let mut seen: Vec<&str> = Vec::new();
        let ids = self
            .baseline
            .iter()
            .chain(self.lines.iter())
            .filter_map(|l| match &l.id {
                LineId::Folder(f) => Some(f.as_str()),
                _ => None,
            });
        for id in ids {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let bpos = self
                .baseline
                .iter()
                .position(|l| matches!(&l.id, LineId::Folder(f) if f == id));
            let cpos = self
                .lines
                .iter()
                .position(|l| matches!(&l.id, LineId::Folder(f) if f == id));
            match (bpos, cpos) {
                (Some(_), None) => deletes.push(EditAction::DeleteFolder { id: id.to_string() }),
                (Some(bi), Some(ci)) => {
                    let name = folder_name(&self.lines[ci].text);
                    // Renaming a folder to nothing is dropped: it keeps its
                    // old name (only CreateFolder surfaces empty names).
                    if !name.is_empty() && name != folder_name(&self.baseline[bi].text) {
                        renames.push((
                            ci,
                            EditAction::RenameFolder {
                                id: id.to_string(),
                                name,
                            },
                        ));
                    }
                }
                (None, Some(ci)) => {
                    // A folder cut before mark_saved() and pasted after it:
                    // the delete was applied, so pasting recreates it.
                    let name = folder_name(&self.lines[ci].text);
                    if name.is_empty() {
                        ignored.push((
                            ci,
                            EditAction::IgnoredLine {
                                text: self.lines[ci].text.clone(),
                            },
                        ));
                    } else {
                        creates.push((
                            ci,
                            EditAction::CreateFolder {
                                parent: cur_ctx[ci].clone(),
                                name,
                            },
                        ));
                    }
                }
                (None, None) => {}
            }
        }
    }

    /// New lines all share LineId::New, so lines carried across a
    /// mark_saved() are matched by (text, context) — a matched line already
    /// produced its action at the last save and must not repeat it.
    fn diff_new_lines(
        &self,
        cur_ctx: &[Option<String>],
        base_ctx: &[Option<String>],
        creates: &mut Vec<(usize, EditAction)>,
        ignored: &mut Vec<(usize, EditAction)>,
    ) {
        let mut bnew: Vec<(usize, &EditLine)> = self
            .baseline
            .iter()
            .enumerate()
            .filter(|(_, l)| l.id == LineId::New)
            .collect();
        for (ci, line) in self.lines.iter().enumerate() {
            if line.id != LineId::New {
                continue;
            }
            if let Some(p) = bnew
                .iter()
                .position(|(bi, bl)| bl.text == line.text && base_ctx[*bi] == cur_ctx[ci])
            {
                bnew.swap_remove(p);
                continue;
            }
            let t = line.text.trim();
            if t.is_empty() {
                continue; // untyped o/O leftover: silently dropped
            }
            match t.strip_suffix('/') {
                Some(name) if !name.trim_end().is_empty() => creates.push((
                    ci,
                    EditAction::CreateFolder {
                        parent: cur_ctx[ci].clone(),
                        name: name.trim_end().to_string(),
                    },
                )),
                _ => ignored.push((
                    ci,
                    EditAction::IgnoredLine {
                        text: line.text.clone(),
                    },
                )),
            }
        }
    }

    // ---- normal mode ----------------------------------------------------

    fn key_normal(&mut self, key: &Key) -> EditEvent {
        if let Some(op) = self.pending.take() {
            self.count = 0;
            return match (op, key) {
                ('d', Key::Char('d')) => self.delete_line(),
                ('y', Key::Char('y')) => self.yank_line(),
                ('g', Key::Char('g')) => {
                    self.move_to_row(0);
                    EditEvent::None
                }
                // Unsupported operator+motion: both keys are swallowed.
                _ => EditEvent::None,
            };
        }
        if let Key::Char(c) = key
            && c.is_ascii_digit()
            && (*c != '0' || self.count > 0)
        {
            let d = *c as usize - '0' as usize;
            self.count = (self.count * 10 + d).min(COUNT_MAX);
            return EditEvent::None;
        }
        let n = self.count.max(1);
        self.count = 0; // counts apply to j/k only and die with any command
        match key {
            Key::Char('h') | Key::Left => {
                self.col = self.col.saturating_sub(1);
                EditEvent::None
            }
            Key::Char('l') | Key::Right => {
                self.col = self.col.saturating_add(1);
                self.clamp_col_normal();
                EditEvent::None
            }
            Key::Char('j') | Key::Down => {
                self.move_to_row(self.row.saturating_add(n));
                EditEvent::None
            }
            Key::Char('k') | Key::Up => {
                self.move_to_row(self.row.saturating_sub(n));
                EditEvent::None
            }
            Key::Char('0') | Key::Home => {
                self.col = 0;
                EditEvent::None
            }
            Key::Char('$') | Key::End => {
                self.col = self.max_col_normal();
                EditEvent::None
            }
            Key::Char('G') => {
                self.move_to_row(usize::MAX);
                EditEvent::None
            }
            Key::Char(c @ ('g' | 'd' | 'y')) => {
                self.pending = Some(*c);
                EditEvent::None
            }
            Key::Char('x') => self.delete_char(),
            Key::Char('p') => self.paste(false),
            Key::Char('P') => self.paste(true),
            Key::Char('o') => self.open_line(false),
            Key::Char('O') => self.open_line(true),
            Key::Char('i') => self.enter_insert(InsertAt::Cursor),
            Key::Char('a') => self.enter_insert(InsertAt::AfterCursor),
            Key::Char('I') => self.enter_insert(InsertAt::LineStart),
            Key::Char('A') => self.enter_insert(InsertAt::LineEnd),
            Key::Char('u') => self.undo_op(),
            Key::Ctrl('r') => self.redo_op(),
            Key::Char(':') => {
                self.mode = Mode::Cmdline(String::new());
                EditEvent::None
            }
            Key::Enter => self.open_session(),
            _ => EditEvent::None,
        }
    }

    fn max_col_normal(&self) -> usize {
        self.lines
            .get(self.row)
            .map_or(0, |l| l.text.chars().count().saturating_sub(1))
    }

    fn clamp_col_normal(&mut self) {
        self.col = self.col.min(self.max_col_normal());
    }

    fn move_to_row(&mut self, target: usize) {
        self.row = target.min(self.lines.len().saturating_sub(1));
        self.clamp_col_normal();
    }

    fn open_session(&mut self) -> EditEvent {
        match self.lines.get(self.row).map(|l| l.id.clone()) {
            Some(LineId::Session(k)) => {
                if self.dirty() {
                    EditEvent::Message(MSG_UNSAVED.to_string())
                } else {
                    EditEvent::OpenSession(k)
                }
            }
            _ => EditEvent::None,
        }
    }

    fn delete_char(&mut self) -> EditEvent {
        let Some(line) = self.lines.get(self.row) else {
            return EditEvent::None;
        };
        if line.readonly {
            return readonly_msg();
        }
        if line.text.is_empty() {
            return EditEvent::None;
        }
        self.push_undo();
        let text = &mut self.lines[self.row].text;
        let col = self.col.min(text.chars().count() - 1);
        let b = byte_at(text, col);
        text.remove(b);
        self.col = col;
        // Deleting the last char pulls the cursor left, vim-style.
        self.clamp_col_normal();
        EditEvent::None
    }

    fn delete_line(&mut self) -> EditEvent {
        let Some(line) = self.lines.get(self.row) else {
            return EditEvent::None;
        };
        if line.readonly {
            return readonly_msg();
        }
        self.push_undo();
        let line = self.lines.remove(self.row);
        self.register = Some((line, false));
        if self.lines.is_empty() {
            self.row = 0;
            self.col = 0;
        } else {
            self.move_to_row(self.row);
        }
        EditEvent::None
    }

    fn yank_line(&mut self) -> EditEvent {
        let Some(line) = self.lines.get(self.row) else {
            return EditEvent::None;
        };
        if line.readonly {
            return readonly_msg();
        }
        self.register = Some((line.clone(), true));
        EditEvent::None
    }

    fn paste(&mut self, above: bool) -> EditEvent {
        let Some((mut line, was_yank)) = self.register.clone() else {
            return EditEvent::None;
        };
        if let LineId::Folder(_) = line.id {
            if was_yank {
                return EditEvent::Message("can't paste a copied folder".to_string());
            }
            // A cut folder pasted twice would duplicate its identity, and
            // folders (unlike sessions) have no fork semantics.
            if self.lines.iter().any(|l| l.id == line.id) {
                return EditEvent::Message("folder is already in the buffer".to_string());
            }
        }
        self.push_undo();
        let (idx, depth) = self.insert_slot(above);
        line.depth = depth;
        if was_yank {
            // The diff decides copy-vs-move by identity counts; the flag is
            // informational for rendering.
            line.copied = true;
        }
        self.lines.insert(idx, line);
        self.row = idx;
        self.col = 0;
        EditEvent::None
    }

    fn open_line(&mut self, above: bool) -> EditEvent {
        self.push_undo();
        let (idx, depth) = self.insert_slot(above);
        self.lines.insert(
            idx,
            EditLine {
                id: LineId::New,
                text: String::new(),
                depth,
                readonly: false,
                copied: false,
            },
        );
        self.row = idx;
        self.col = 0;
        self.mode = Mode::Insert;
        EditEvent::None
    }

    /// Where a pasted/opened line lands: `p`/`o` on a folder line (or the
    /// Inbox header) insert as its first child — the only way to put
    /// something inside an empty folder; on any other line as a sibling
    /// below. `P`/`O` insert as a sibling above. Depth comes from the slot,
    /// never from the inserted line ("depths are recomputed on insert").
    fn insert_slot(&self, above: bool) -> (usize, usize) {
        let Some(cur) = self.lines.get(self.row) else {
            return (0, 0);
        };
        if above {
            (self.row, cur.depth)
        } else if matches!(cur.id, LineId::Folder(_) | LineId::Inbox) {
            (self.row + 1, cur.depth + 1)
        } else {
            (self.row + 1, cur.depth)
        }
    }

    fn enter_insert(&mut self, at: InsertAt) -> EditEvent {
        let Some(line) = self.lines.get(self.row) else {
            return EditEvent::None;
        };
        if line.readonly {
            return readonly_msg();
        }
        let n = line.text.chars().count();
        self.push_undo();
        self.col = match at {
            InsertAt::Cursor => self.col.min(n),
            InsertAt::AfterCursor => self.col.saturating_add(1).min(n),
            InsertAt::LineStart => 0,
            InsertAt::LineEnd => n,
        };
        self.mode = Mode::Insert;
        EditEvent::None
    }

    // ---- insert mode ----------------------------------------------------

    fn key_insert(&mut self, key: &Key) -> EditEvent {
        match key {
            Key::Esc => {
                // One insert session = one undo unit (the snapshot was
                // pushed on entry); a no-change session leaves no entry.
                if let Some(top) = self.undo.last()
                    && lines_eq(&top.lines, &self.lines)
                {
                    self.undo.pop();
                }
                self.mode = Mode::Normal;
                self.col = self.col.saturating_sub(1);
                self.clamp_col_normal();
                EditEvent::None
            }
            Key::Char(c) if !c.is_control() => {
                if let Some(line) = self.lines.get_mut(self.row) {
                    let b = byte_at(&line.text, self.col);
                    line.text.insert(b, *c);
                    self.col += 1;
                }
                EditEvent::None
            }
            // Ctrl('h') = the 0x08 byte: vim's insert-mode C-h is backspace.
            Key::Backspace | Key::Ctrl('h') => {
                if self.col > 0
                    && let Some(line) = self.lines.get_mut(self.row)
                {
                    self.col -= 1;
                    let b = byte_at(&line.text, self.col);
                    line.text.remove(b);
                }
                EditEvent::None
            }
            _ => EditEvent::None,
        }
    }

    // ---- cmdline mode ---------------------------------------------------

    fn key_cmdline(&mut self, mut cmd: String, key: &Key) -> EditEvent {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                EditEvent::None
            }
            Key::Backspace | Key::Ctrl('h') => {
                if cmd.pop().is_none() {
                    self.mode = Mode::Normal; // backspace over the ':'
                } else {
                    self.mode = Mode::Cmdline(cmd);
                }
                EditEvent::None
            }
            Key::Char(c) if !c.is_control() => {
                cmd.push(*c);
                self.mode = Mode::Cmdline(cmd);
                EditEvent::None
            }
            Key::Enter => {
                self.mode = Mode::Normal;
                self.run_command(cmd.trim())
            }
            _ => EditEvent::None,
        }
    }

    fn run_command(&mut self, cmd: &str) -> EditEvent {
        match cmd {
            "" => EditEvent::None,
            "w" => EditEvent::Save,
            "q" => {
                if self.dirty() {
                    EditEvent::Message(MSG_UNSAVED.to_string())
                } else {
                    EditEvent::Quit
                }
            }
            "q!" => EditEvent::Quit,
            "wq" => EditEvent::SaveQuit,
            other => EditEvent::Message(format!("not an editor command: {other}")),
        }
    }

    // ---- undo/redo ------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.row = s.row;
        self.col = s.col;
    }

    fn push_undo(&mut self) {
        self.undo.push(self.snapshot());
        self.redo.clear();
    }

    fn undo_op(&mut self) -> EditEvent {
        if let Some(s) = self.undo.pop() {
            let now = self.snapshot();
            self.redo.push(now);
            self.restore(s);
        }
        EditEvent::None
    }

    fn redo_op(&mut self) -> EditEvent {
        if let Some(s) = self.redo.pop() {
            let now = self.snapshot();
            self.undo.push(now);
            self.restore(s);
        }
        EditEvent::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentKind;

    fn skey(id: &str) -> SessionKey {
        SessionKey::new(AgentKind::Claude, id)
    }

    fn sess(id: &str, text: &str, depth: usize) -> EditLine {
        EditLine {
            id: LineId::Session(skey(id)),
            text: text.to_string(),
            depth,
            readonly: false,
            copied: false,
        }
    }

    fn folder(id: &str, text: &str, depth: usize) -> EditLine {
        EditLine {
            id: LineId::Folder(id.to_string()),
            text: text.to_string(),
            depth,
            readonly: false,
            copied: false,
        }
    }

    fn inbox() -> EditLine {
        EditLine {
            id: LineId::Inbox,
            text: "Inbox".to_string(),
            depth: 0,
            readonly: true,
            copied: false,
        }
    }

    /// Feed each char as Key::Char; returns the LAST event.
    fn press(b: &mut EditBuf, keys: &str) -> EditEvent {
        let mut last = EditEvent::None;
        for c in keys.chars() {
            last = b.handle_key(&Key::Char(c));
        }
        last
    }

    fn key(b: &mut EditBuf, k: Key) -> EditEvent {
        b.handle_key(&k)
    }

    fn esc(b: &mut EditBuf) -> EditEvent {
        b.handle_key(&Key::Esc)
    }

    fn enter(b: &mut EditBuf) -> EditEvent {
        b.handle_key(&Key::Enter)
    }

    fn texts(b: &EditBuf) -> Vec<&str> {
        b.lines().iter().map(|l| l.text.as_str()).collect()
    }

    /// Inbox with one member, folder "proj/" with two members, one
    /// top-level session.
    fn fixture() -> EditBuf {
        EditBuf::new(vec![
            inbox(),                  // 0
            sess("a", "alpha", 1),    // 1 (inbox member: context None)
            folder("f1", "proj/", 0), // 2
            sess("b", "beta", 1),     // 3
            sess("c", "gamma", 1),    // 4
            sess("d", "delta", 0),    // 5
        ])
    }

    fn twelve() -> EditBuf {
        EditBuf::new(
            (0..12)
                .map(|i| sess(&format!("s{i}"), &format!("line{i}"), 0))
                .collect(),
        )
    }

    // ---- movement ------------------------------------------------------

    #[test]
    fn movement_hjkl_arrows_clamp() {
        let mut b = EditBuf::new(vec![sess("a", "abcdef", 0), sess("b", "xy", 0)]);
        assert_eq!(b.cursor(), (0, 0));
        press(&mut b, "ll");
        assert_eq!(b.cursor(), (0, 2));
        press(&mut b, "h");
        assert_eq!(b.cursor(), (0, 1));
        press(&mut b, "hh"); // clamped at column 0
        assert_eq!(b.cursor(), (0, 0));
        press(&mut b, "$");
        assert_eq!(b.cursor(), (0, 5));
        press(&mut b, "j"); // shorter line clamps the column
        assert_eq!(b.cursor(), (1, 1));
        press(&mut b, "j"); // clamped at the last row
        assert_eq!(b.cursor(), (1, 1));
        key(&mut b, Key::Up);
        assert_eq!(b.cursor(), (0, 1));
        key(&mut b, Key::Right);
        assert_eq!(b.cursor(), (0, 2));
        key(&mut b, Key::Left);
        assert_eq!(b.cursor(), (0, 1));
        key(&mut b, Key::Down);
        assert_eq!(b.cursor(), (1, 1));
        press(&mut b, "k0");
        assert_eq!(b.cursor(), (0, 0));
    }

    #[test]
    fn counts_for_j_k_with_reset_and_bound() {
        let mut b = twelve();
        press(&mut b, "3j");
        assert_eq!(b.cursor().0, 3);
        press(&mut b, "2k");
        assert_eq!(b.cursor().0, 1);
        press(&mut b, "10j"); // '0' after a digit extends the count
        assert_eq!(b.cursor().0, 11);
        press(&mut b, "999999999999k"); // capped at 999, clamped at the top
        assert_eq!(b.cursor().0, 0);
        press(&mut b, "3l"); // counts apply to j/k only: l moves one column
        assert_eq!(b.cursor(), (0, 1));
        press(&mut b, "j"); // and l consumed the pending count
        assert_eq!(b.cursor().0, 1);
    }

    #[test]
    fn gg_and_g_jump() {
        let mut b = twelve();
        press(&mut b, "G");
        assert_eq!(b.cursor().0, 11);
        press(&mut b, "gg");
        assert_eq!(b.cursor().0, 0);
        press(&mut b, "gj"); // 'g' + anything else: both swallowed
        assert_eq!(b.cursor().0, 0);
        press(&mut b, "dG"); // same for 'd' + non-'d'
        assert_eq!(b.cursor().0, 0);
        assert_eq!(b.lines().len(), 12);
    }

    #[test]
    fn multibyte_cursor_and_dollar() {
        let mut b = EditBuf::new(vec![sess("a", "é你x", 0)]);
        assert_eq!(b.cursor(), (0, 0));
        press(&mut b, "l");
        assert_eq!(b.cursor(), (0, 2)); // é is 2 bytes
        press(&mut b, "l");
        assert_eq!(b.cursor(), (0, 5)); // 你 is 3 bytes
        press(&mut b, "l"); // clamped ON the last char
        assert_eq!(b.cursor(), (0, 5));
        press(&mut b, "0");
        assert_eq!(b.cursor(), (0, 0));
        press(&mut b, "$");
        assert_eq!(b.cursor(), (0, 5));
    }

    // ---- insert mode ---------------------------------------------------

    #[test]
    fn insert_i_a_upper_i_upper_a() {
        let mut b = EditBuf::new(vec![sess("s", "beta", 0)]);
        press(&mut b, "llix"); // insert before 't'
        esc(&mut b);
        assert_eq!(texts(&b), ["bexta"]);
        assert_eq!(b.cursor(), (0, 2)); // esc lands ON the typed char

        let mut b = EditBuf::new(vec![sess("s", "beta", 0)]);
        press(&mut b, "llax"); // append after 't'
        esc(&mut b);
        assert_eq!(texts(&b), ["betxa"]);
        assert_eq!(b.cursor(), (0, 3));

        let mut b = EditBuf::new(vec![sess("s", "beta", 0)]);
        press(&mut b, "llIx");
        esc(&mut b);
        assert_eq!(texts(&b), ["xbeta"]);
        assert_eq!(b.cursor(), (0, 0));

        let mut b = EditBuf::new(vec![sess("s", "beta", 0)]);
        press(&mut b, "llAx");
        esc(&mut b);
        assert_eq!(texts(&b), ["betax"]);
        assert_eq!(b.cursor(), (0, 4));
    }

    #[test]
    fn insert_multibyte_and_backspace_boundaries() {
        let mut b = EditBuf::new(vec![sess("s", "b", 0)]);
        press(&mut b, "Aé你");
        assert_eq!(texts(&b), ["bé你"]);
        key(&mut b, Key::Backspace); // removes 你 at its boundary
        assert_eq!(texts(&b), ["bé"]);
        key(&mut b, Key::Backspace);
        key(&mut b, Key::Backspace);
        assert_eq!(texts(&b), [""]);
        key(&mut b, Key::Backspace); // column 0: no-op
        assert_eq!(texts(&b), [""]);
        esc(&mut b);
        assert_eq!(b.cursor(), (0, 0));
        assert_eq!(b.mode(), &Mode::Normal);
    }

    #[test]
    fn backspace_mid_line_multibyte() {
        let mut b = EditBuf::new(vec![sess("s", "é你", 0)]);
        press(&mut b, "li"); // insert point just before 你
        key(&mut b, Key::Backspace);
        assert_eq!(texts(&b), ["你"]);
        press(&mut b, "à");
        assert_eq!(texts(&b), ["à你"]);
    }

    // ---- x -------------------------------------------------------------

    #[test]
    fn x_deletes_char_last_char_moves_left() {
        let mut b = EditBuf::new(vec![sess("s", "beta", 0)]);
        press(&mut b, "$x");
        assert_eq!(texts(&b), ["bet"]);
        assert_eq!(b.cursor(), (0, 2)); // pulled left onto the new last char
        press(&mut b, "0x");
        assert_eq!(texts(&b), ["et"]);
        press(&mut b, "xx");
        assert_eq!(texts(&b), [""]);
        press(&mut b, "x"); // empty line: no-op
        assert_eq!(texts(&b), [""]);
        assert_eq!(b.cursor(), (0, 0));
    }

    #[test]
    fn x_multibyte() {
        let mut b = EditBuf::new(vec![sess("s", "é你x", 0)]);
        press(&mut b, "x");
        assert_eq!(texts(&b), ["你x"]);
        press(&mut b, "x");
        assert_eq!(texts(&b), ["x"]);
    }

    // ---- dd / p / P ------------------------------------------------------

    #[test]
    fn dd_p_same_folder_is_reorder_no_action() {
        let mut b = fixture();
        press(&mut b, "jjjdd"); // cut beta; cursor lands on gamma
        press(&mut b, "p"); // back below gamma: same folder, new order
        assert_eq!(
            texts(&b),
            ["Inbox", "alpha", "proj/", "gamma", "beta", "delta"]
        );
        assert!(b.dirty());
        assert_eq!(b.diff(), vec![]);
    }

    #[test]
    fn dd_p_into_other_context_is_move() {
        let mut b = fixture();
        press(&mut b, "jjjdd"); // cut beta
        press(&mut b, "kk"); // onto alpha (inbox member)
        press(&mut b, "p"); // sibling below alpha: context None
        assert_eq!(b.lines()[2].text, "beta");
        assert_eq!(b.lines()[2].depth, 1);
        assert_eq!(
            b.diff(),
            vec![EditAction::MoveSession {
                key: skey("b"),
                folder: None
            }]
        );
    }

    #[test]
    fn p_on_folder_line_inserts_first_child() {
        let mut b = fixture();
        press(&mut b, "Gdd"); // cut delta
        press(&mut b, "ggjj"); // onto the proj/ line
        press(&mut b, "p");
        assert_eq!(
            texts(&b),
            ["Inbox", "alpha", "proj/", "delta", "beta", "gamma"]
        );
        assert_eq!(b.lines()[3].depth, 1);
        assert_eq!(
            b.diff(),
            vec![EditAction::MoveSession {
                key: skey("d"),
                folder: Some("f1".into())
            }]
        );
    }

    #[test]
    fn p_on_inbox_inserts_child_with_no_context() {
        let mut b = fixture();
        press(&mut b, "jjjdd");
        press(&mut b, "gg");
        press(&mut b, "p"); // on the Inbox header: first child
        assert_eq!(texts(&b)[1], "beta");
        assert_eq!(b.lines()[1].depth, 1);
        assert_eq!(
            b.diff(),
            vec![EditAction::MoveSession {
                key: skey("b"),
                folder: None
            }]
        );
    }

    #[test]
    fn upper_p_pastes_above_as_sibling() {
        let mut b = fixture();
        press(&mut b, "jjjdd"); // cut beta; cursor on gamma
        press(&mut b, "P"); // back above gamma: original layout restored
        assert!(!b.dirty());
        assert_eq!(b.diff(), vec![]);
        assert_eq!(b.cursor().0, 3);
    }

    #[test]
    fn dd_without_paste_hides_session() {
        let mut b = fixture();
        press(&mut b, "jjjdd");
        assert_eq!(b.diff(), vec![EditAction::HideSession { key: skey("b") }]);
    }

    #[test]
    fn paste_with_empty_register_is_noop() {
        let mut b = fixture();
        assert_eq!(press(&mut b, "p"), EditEvent::None);
        assert_eq!(press(&mut b, "P"), EditEvent::None);
        assert!(!b.dirty());
    }

    #[test]
    fn cut_pasted_twice_is_move_plus_fork() {
        let mut b = fixture();
        press(&mut b, "jjjdd");
        press(&mut b, "G"); // delta, top level
        press(&mut b, "pp");
        assert_eq!(
            b.diff(),
            vec![
                EditAction::MoveSession {
                    key: skey("b"),
                    folder: None
                },
                EditAction::ForkInto {
                    key: skey("b"),
                    folder: None
                },
            ]
        );
    }

    // ---- folders -------------------------------------------------------

    #[test]
    fn dd_folder_deletes_it_and_members_reparent_to_none() {
        let mut b = fixture();
        press(&mut b, "jjdd"); // cut the proj/ line only
        assert_eq!(
            b.diff(),
            vec![
                EditAction::MoveSession {
                    key: skey("b"),
                    folder: None
                },
                EditAction::MoveSession {
                    key: skey("c"),
                    folder: None
                },
                EditAction::DeleteFolder { id: "f1".into() },
            ]
        );
    }

    #[test]
    fn dd_folder_members_attach_to_folder_above() {
        let mut b = EditBuf::new(vec![
            folder("f1", "one/", 0),
            sess("a", "aaa", 1),
            folder("f2", "two/", 0),
            sess("x", "xxx", 1),
        ]);
        press(&mut b, "jjdd"); // cut two/: xxx now dangles under one/
        assert_eq!(
            b.diff(),
            vec![
                EditAction::MoveSession {
                    key: skey("x"),
                    folder: Some("f1".into())
                },
                EditAction::DeleteFolder { id: "f2".into() },
            ]
        );
    }

    #[test]
    fn pasting_copied_folder_refused() {
        let mut b = fixture();
        press(&mut b, "jjyy");
        let ev = press(&mut b, "p");
        assert!(matches!(ev, EditEvent::Message(_)));
        assert_eq!(b.lines().len(), 6);
        assert!(!b.dirty());
    }

    #[test]
    fn pasting_cut_folder_twice_refused() {
        let mut b = fixture();
        press(&mut b, "jjdd");
        assert_eq!(press(&mut b, "p"), EditEvent::None);
        let ev = press(&mut b, "p");
        assert!(matches!(ev, EditEvent::Message(_)));
    }

    #[test]
    fn folder_rename_and_optional_slash() {
        let mut b = fixture();
        press(&mut b, "jj$x"); // delete the trailing slash: still "proj"
        assert!(b.dirty());
        assert_eq!(b.diff(), vec![]); // same name — the slash is optional
        press(&mut b, "A");
        for _ in 0..4 {
            key(&mut b, Key::Backspace);
        }
        press(&mut b, "work");
        esc(&mut b);
        assert_eq!(texts(&b)[2], "work");
        assert_eq!(
            b.diff(),
            vec![EditAction::RenameFolder {
                id: "f1".into(),
                name: "work".into()
            }]
        );
    }

    #[test]
    fn folder_renamed_to_empty_keeps_old_name() {
        let mut b = fixture();
        press(&mut b, "jjxxxxx"); // delete all of "proj/"
        assert_eq!(texts(&b)[2], "");
        assert_eq!(b.diff(), vec![]);
    }

    // ---- yy / fork -------------------------------------------------------

    #[test]
    fn yy_p_forks_once_no_move() {
        let mut b = fixture();
        press(&mut b, "jjjyyp");
        assert_eq!(texts(&b)[4], "beta");
        assert!(b.lines()[4].copied);
        assert!(!b.lines()[3].copied);
        assert_eq!(
            b.diff(),
            vec![EditAction::ForkInto {
                key: skey("b"),
                folder: Some("f1".into())
            }]
        );
    }

    #[test]
    fn yy_p_into_other_folder_forks_there() {
        let mut b = fixture();
        press(&mut b, "jjjyy");
        press(&mut b, "G"); // delta, top level
        press(&mut b, "p");
        assert_eq!(
            b.diff(),
            vec![EditAction::ForkInto {
                key: skey("b"),
                folder: None
            }]
        );
    }

    // ---- o / O / new lines -----------------------------------------------

    #[test]
    fn o_typed_folder_creates_with_parent() {
        let mut b = fixture();
        press(&mut b, "jjjo"); // open below beta, inside proj/
        press(&mut b, "sub/");
        esc(&mut b);
        assert_eq!(b.lines()[4].depth, 1);
        assert_eq!(
            b.diff(),
            vec![EditAction::CreateFolder {
                parent: Some("f1".into()),
                name: "sub".into()
            }]
        );
    }

    #[test]
    fn o_on_folder_line_creates_inside_it() {
        let mut b = fixture();
        press(&mut b, "jjo"); // o ON the proj/ line: first child
        press(&mut b, "docs/");
        esc(&mut b);
        assert_eq!(b.lines()[3].text, "docs/");
        assert_eq!(b.lines()[3].depth, 1);
        assert_eq!(
            b.diff(),
            vec![EditAction::CreateFolder {
                parent: Some("f1".into()),
                name: "docs".into()
            }]
        );
    }

    #[test]
    fn upper_o_creates_top_level_folder() {
        let mut b = fixture();
        press(&mut b, "GO"); // open above delta at depth 0
        press(&mut b, "misc/");
        esc(&mut b);
        assert_eq!(
            b.diff(),
            vec![EditAction::CreateFolder {
                parent: None,
                name: "misc".into()
            }]
        );
    }

    #[test]
    fn o_junk_reports_ignored_line() {
        let mut b = fixture();
        press(&mut b, "Go");
        press(&mut b, "junk");
        esc(&mut b);
        assert_eq!(
            b.diff(),
            vec![EditAction::IgnoredLine {
                text: "junk".into()
            }]
        );
    }

    #[test]
    fn o_bare_slash_is_ignored_line() {
        let mut b = fixture();
        press(&mut b, "Go/");
        esc(&mut b);
        assert_eq!(b.diff(), vec![EditAction::IgnoredLine { text: "/".into() }]);
    }

    #[test]
    fn empty_new_line_and_deleted_new_line_produce_nothing() {
        let mut b = fixture();
        press(&mut b, "Go");
        esc(&mut b); // empty New line stays in the buffer
        assert!(b.dirty());
        assert_eq!(b.diff(), vec![]);
        press(&mut b, "o");
        press(&mut b, "tmp/");
        esc(&mut b);
        press(&mut b, "dd"); // delete the line just typed → nothing
        assert_eq!(b.diff(), vec![]);
    }

    // ---- readonly --------------------------------------------------------

    #[test]
    fn readonly_lines_immune_to_edits() {
        let mut b = fixture(); // row 0 = Inbox (readonly)
        for keys in ["i", "a", "I", "A", "x", "dd", "yy"] {
            let ev = press(&mut b, keys);
            assert!(matches!(ev, EditEvent::Message(_)), "{keys}");
        }
        assert_eq!(b.mode(), &Mode::Normal);
        assert!(!b.dirty());
        assert_eq!(press(&mut b, "p"), EditEvent::None); // nothing yanked
    }

    #[test]
    fn provisional_session_readonly_but_openable() {
        let mut b = EditBuf::new(vec![EditLine {
            id: LineId::Session(skey("prov")),
            text: "(starting…)".into(),
            depth: 0,
            readonly: true,
            copied: false,
        }]);
        assert!(matches!(press(&mut b, "dd"), EditEvent::Message(_)));
        assert!(matches!(press(&mut b, "i"), EditEvent::Message(_)));
        // Opening is not a mutation: the app decides what Enter means here.
        assert_eq!(enter(&mut b), EditEvent::OpenSession(skey("prov")));
    }

    // ---- renames ---------------------------------------------------------

    #[test]
    fn rename_session_via_edit() {
        let mut b = fixture();
        press(&mut b, "jjjA!");
        esc(&mut b);
        assert_eq!(
            b.diff(),
            vec![EditAction::RenameSession {
                key: skey("b"),
                name: "beta!".into()
            }]
        );
    }

    #[test]
    fn rename_session_to_empty_clears_override() {
        let mut b = fixture();
        press(&mut b, "jjjxxxx"); // delete all four chars of "beta"
        assert_eq!(texts(&b)[3], "");
        assert_eq!(
            b.diff(),
            vec![EditAction::RenameSession {
                key: skey("b"),
                name: "".into()
            }]
        );
    }

    // ---- undo / redo -----------------------------------------------------

    #[test]
    fn undo_redo_roundtrip() {
        let mut b = fixture();
        press(&mut b, "jjjdd");
        assert!(b.dirty());
        press(&mut b, "u");
        assert!(!b.dirty());
        assert_eq!(texts(&b)[3], "beta");
        assert_eq!(b.cursor().0, 3); // cursor restored with the snapshot
        key(&mut b, Key::Ctrl('r'));
        assert!(b.dirty());
        assert_eq!(b.lines().len(), 5);
        press(&mut b, "u");
        assert!(!b.dirty());
    }

    #[test]
    fn insert_session_is_one_undo_unit() {
        let mut b = fixture();
        press(&mut b, "jjjAxyz");
        esc(&mut b);
        assert_eq!(texts(&b)[3], "betaxyz");
        press(&mut b, "u");
        assert_eq!(texts(&b)[3], "beta");
        assert!(!b.dirty());
    }

    #[test]
    fn unchanged_insert_leaves_no_undo_entry() {
        let mut b = fixture();
        press(&mut b, "jjjdd"); // real mutation first
        press(&mut b, "i"); // cursor is on gamma now
        esc(&mut b); // typed nothing
        press(&mut b, "u"); // undoes the dd, not the empty insert
        assert!(!b.dirty());
        assert_eq!(texts(&b)[3], "beta");
    }

    #[test]
    fn undo_redo_empty_stacks_noop() {
        let mut b = fixture();
        assert_eq!(press(&mut b, "u"), EditEvent::None);
        assert_eq!(key(&mut b, Key::Ctrl('r')), EditEvent::None);
        assert!(!b.dirty());
    }

    // ---- cmdline ---------------------------------------------------------

    #[test]
    fn q_refused_when_dirty_forced_with_bang() {
        let mut b = fixture();
        press(&mut b, "jjjA!");
        esc(&mut b);
        press(&mut b, ":q");
        assert!(matches!(enter(&mut b), EditEvent::Message(_)));
        assert_eq!(b.mode(), &Mode::Normal);
        press(&mut b, ":q!");
        assert_eq!(enter(&mut b), EditEvent::Quit);
    }

    #[test]
    fn w_wq_and_clean_q() {
        let mut b = fixture();
        press(&mut b, ":w");
        assert_eq!(enter(&mut b), EditEvent::Save);
        press(&mut b, ":wq");
        assert_eq!(enter(&mut b), EditEvent::SaveQuit);
        press(&mut b, ":q"); // clean: allowed
        assert_eq!(enter(&mut b), EditEvent::Quit);
    }

    #[test]
    fn cmdline_esc_and_backspace_cancel() {
        let mut b = fixture();
        press(&mut b, ":wq");
        assert_eq!(b.mode(), &Mode::Cmdline("wq".into()));
        esc(&mut b);
        assert_eq!(b.mode(), &Mode::Normal);
        press(&mut b, ":q");
        key(&mut b, Key::Backspace);
        assert_eq!(b.mode(), &Mode::Cmdline(String::new()));
        key(&mut b, Key::Backspace); // backspace over the ':' cancels
        assert_eq!(b.mode(), &Mode::Normal);
        press(&mut b, "j"); // and normal keys work again
        assert_eq!(b.cursor().0, 1);
    }

    #[test]
    fn unknown_command_reports_message() {
        let mut b = fixture();
        press(&mut b, ":zz");
        let ev = enter(&mut b);
        assert!(matches!(ev, EditEvent::Message(m) if m.contains("zz")));
        assert_eq!(b.mode(), &Mode::Normal);
    }

    // ---- enter -----------------------------------------------------------

    #[test]
    fn enter_opens_session_only_when_clean() {
        let mut b = fixture();
        press(&mut b, "jjj");
        assert_eq!(enter(&mut b), EditEvent::OpenSession(skey("b")));
        press(&mut b, "A!");
        esc(&mut b);
        assert_eq!(enter(&mut b), EditEvent::Message(MSG_UNSAVED.into()));
        press(&mut b, "u"); // clean again
        assert_eq!(enter(&mut b), EditEvent::OpenSession(skey("b")));
    }

    #[test]
    fn enter_on_non_session_lines_is_noop() {
        let mut b = fixture();
        assert_eq!(enter(&mut b), EditEvent::None); // Inbox header
        press(&mut b, "jj");
        assert_eq!(enter(&mut b), EditEvent::None); // folder line
    }

    // ---- save / baseline -------------------------------------------------

    #[test]
    fn mark_saved_resets_baseline_and_copied_flags() {
        let mut b = fixture();
        press(&mut b, "jjjyyp");
        assert!(b.lines()[4].copied);
        assert!(b.dirty());
        assert_eq!(b.diff().len(), 1);
        b.mark_saved();
        assert!(!b.dirty());
        assert!(b.lines().iter().all(|l| !l.copied));
        assert_eq!(b.diff(), vec![]); // idempotent after save
        press(&mut b, "A!");
        esc(&mut b);
        assert!(b.dirty());
    }

    #[test]
    fn cut_line_pasted_after_save_is_a_move() {
        let mut b = fixture();
        press(&mut b, "jjjdd");
        assert_eq!(b.diff(), vec![EditAction::HideSession { key: skey("b") }]);
        b.mark_saved();
        assert!(!b.dirty());
        press(&mut b, "p"); // the register survives the save
        assert_eq!(
            b.diff(),
            vec![EditAction::MoveSession {
                key: skey("b"),
                folder: Some("f1".into())
            }]
        );
    }

    // ---- empty buffer ----------------------------------------------------

    #[test]
    fn buffer_can_empty_out_and_o_still_works() {
        let mut b = EditBuf::new(vec![sess("z", "zzz", 0)]);
        press(&mut b, "dd");
        assert!(b.lines().is_empty());
        assert_eq!(b.cursor(), (0, 0));
        // Movement and edits on the empty buffer: harmless no-ops.
        assert_eq!(press(&mut b, "jkxl$0Gi"), EditEvent::None);
        press(&mut b, "gg");
        assert_eq!(b.mode(), &Mode::Normal);
        press(&mut b, "o");
        assert_eq!(b.mode(), &Mode::Insert);
        press(&mut b, "new/");
        esc(&mut b);
        assert_eq!(
            b.diff(),
            vec![
                EditAction::CreateFolder {
                    parent: None,
                    name: "new".into()
                },
                EditAction::HideSession { key: skey("z") },
            ]
        );
    }

    #[test]
    fn paste_into_empty_buffer() {
        let mut b = EditBuf::new(vec![sess("z", "zzz", 0)]);
        press(&mut b, "ddp");
        assert_eq!(texts(&b), ["zzz"]);
        assert_eq!(b.lines()[0].depth, 0);
        assert!(!b.dirty());
    }

    // ---- diff ordering ---------------------------------------------------

    #[test]
    fn diff_orders_actions_by_category() {
        let mut b = EditBuf::new(vec![
            folder("f1", "one/", 0),
            sess("a", "aaa", 1),
            folder("f2", "two/", 0),
            sess("x", "xxx", 1),
            sess("d", "ddd", 0),
        ]);
        press(&mut b, "o"); // create inside f1 (cursor on its line)
        press(&mut b, "n/");
        esc(&mut b);
        press(&mut b, "G"); // ddd
        press(&mut b, "A!");
        esc(&mut b); // rename d → "ddd!"
        press(&mut b, "kdd"); // hide x
        press(&mut b, "kdd"); // delete folder f2
        press(&mut b, "kdd"); // cut a…
        press(&mut b, "Gp"); // …paste at top level → move
        press(&mut b, "o");
        press(&mut b, "junk");
        esc(&mut b);
        assert_eq!(
            b.diff(),
            vec![
                EditAction::CreateFolder {
                    parent: Some("f1".into()),
                    name: "n".into()
                },
                EditAction::RenameSession {
                    key: skey("d"),
                    name: "ddd!".into()
                },
                EditAction::MoveSession {
                    key: skey("a"),
                    folder: None
                },
                EditAction::HideSession { key: skey("x") },
                EditAction::DeleteFolder { id: "f2".into() },
                EditAction::IgnoredLine {
                    text: "junk".into()
                },
            ]
        );
    }
}
