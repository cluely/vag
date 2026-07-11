//! Per-session diff view: the second "tab" of an open session pane.
//!
//! The displayed content is ALWAYS the live `git diff <base>..worktree`
//! (computed off-thread by `gitdiff`), scoped by default to the files this
//! runtime's transcript says the agent edited (`touched`). Recorded
//! transcript patches are never rendered as content — measured on real
//! stores, ~1/3 of recorded hunks no longer match the file on disk by
//! review time.
//!
//! Layout mirrors a GitHub PR: a collapsible file tree on the left (single
//! -child directory chains compressed to one row), the concatenated unified
//! diff on the right. Selecting a file jumps the body; the body also
//! free-scrolls end to end.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::gitdiff::{DiffSnapshot, FileDiff, FileStatus, LineKind};
use crate::ui::ansi;
use crate::ui::dashboard;
use crate::ui::icons::Icons;
use crate::ui::theme::Theme;

/// Semantic diff colors are deliberately fixed, not themed — the same
/// precedent as the badge colors and the heatmap greens.
const ADDED: Color = Color::Green;
const REMOVED: Color = Color::Red;
const RENAMED: Color = Color::Cyan;
const MODIFIED: Color = Color::Yellow;

/// Chrome rows above the panels: tab/summary line + hairline rule.
const HEADER_ROWS: u16 = 2;

/// The files/diff focus chip on each panel's first row: an unmistakable
/// "you are here" (the tree's cursor bar alone was too subtle), with the
/// switch key spelled on the unfocused side.
fn panel_chip(label: &str, focused: bool, th: &Theme) -> Line<'static> {
    if focused {
        Line::from(Span::styled(
            format!(" {label} "),
            Style::new()
                .fg(Color::Black)
                .bg(th.accent)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled(
                format!(" {label} "),
                Style::new().fg(th.dim).bg(th.surface),
            ),
            Span::styled(" tab⇢".to_string(), Style::new().fg(th.dim)),
        ])
    }
}

fn status_color(s: FileStatus) -> Color {
    match s {
        FileStatus::Added | FileStatus::Untracked => ADDED,
        FileStatus::Deleted => REMOVED,
        FileStatus::Renamed => RENAMED,
        FileStatus::Modified => MODIFIED,
    }
}

/// Pre-rendered delta output for one snapshot: ANSI-parsed lines per file,
/// in `DiffSnapshot.files` order. Produced on the diff worker thread.
pub struct DeltaOutput {
    /// The pane width delta wrapped for — a resize invalidates the render.
    pub width: u16,
    pub per_file: Vec<Vec<Line<'static>>>,
}

/// One `delta --version` probe per process. A missing binary is the normal
/// case on machines without delta — the builtin renderer takes over.
pub fn delta_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("delta")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Render each file's raw diff section through delta, capturing its ANSI
/// output. Runs on the diff worker thread — one short-lived process per
/// file. Any failure returns None and the builtin renderer takes over;
/// delta must only ever improve the view, never break it.
///
/// vag pins --paging=never/--width/--file-style=omit (vag draws its own
/// file headers as jump anchors); everything else — themes, side-by-side,
/// line-numbers — comes from the user's own delta/git config, plus
/// `[diff] delta_args` which are appended last and therefore win.
pub fn render_delta(files: &[FileDiff], width: u16, extra: &[String]) -> Option<DeltaOutput> {
    if !delta_available() {
        return None;
    }
    // Concurrent waves: each delta run costs ~30-80ms mostly in startup +
    // syntax-asset load, so a whole-repo scope rendered sequentially takes
    // seconds — the "loading" the scope toggle was blamed for. The wave
    // bound keeps fd/process usage tame on 100-file diffs.
    const WAVE: usize = 8;
    let mut per_file: Vec<Vec<Line<'static>>> = Vec::with_capacity(files.len());
    for wave in files.chunks(WAVE) {
        let mut running: Vec<Option<std::process::Child>> = Vec::with_capacity(wave.len());
        let mut writers = Vec::new();
        for f in wave {
            if f.raw.is_empty() {
                running.push(None);
                continue;
            }
            let mut child = Command::new("delta")
                .arg("--paging=never")
                .arg(format!("--width={}", width.max(20)))
                .arg("--file-style=omit")
                .args(extra)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            // Feed stdin from a helper thread: writing the whole section
            // before reading can deadlock once delta fills its stdout pipe.
            let mut stdin = child.stdin.take()?;
            let payload = f.raw.clone().into_bytes();
            writers.push(std::thread::spawn(move || {
                let _ = stdin.write_all(&payload);
            }));
            running.push(Some(child));
        }
        for child in running {
            match child {
                None => per_file.push(Vec::new()),
                Some(child) => {
                    let out = child.wait_with_output().ok()?;
                    if !out.status.success() {
                        return None;
                    }
                    per_file.push(ansi::lines(&String::from_utf8_lossy(&out.stdout)));
                }
            }
        }
        for w in writers {
            let _ = w.join();
        }
    }
    Some(DeltaOutput { width, per_file })
}

/// Git context captured at the session-spawn boundary (App::spawn_runtime).
/// `base` is the persisted first-open sha when one exists, else HEAD at
/// spawn; validity (rebase, gc) is re-checked by the worker at diff time.
pub struct RepoCtx {
    pub root: Option<PathBuf>,
    pub base: Option<String>,
    /// The transcript scanner's attribution boundary: records older than
    /// this are fork/resume history, not this runtime's work.
    pub spawned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFocus {
    Tree,
    Body,
}

enum TreeNode {
    Dir {
        /// Display label — compressed chains render as "a/b/c".
        label: String,
        /// Full path prefix, the stable key for the collapsed set.
        key: String,
        adds: u32,
        dels: u32,
        collapsed: bool,
    },
    File {
        file_idx: usize,
    },
}

struct TreeRow {
    depth: usize,
    node: TreeNode,
}

/// One renderable line of the concatenated diff body. Indices point into
/// the snapshot so the window render never clones file contents.
enum BodyLine {
    FileHeader(usize),
    HunkHeader(usize, usize),
    Diff(usize, usize, usize),
    /// One pre-rendered delta line: (file index, row within that file).
    Delta(usize, usize),
    Note(&'static str),
    Blank,
}

pub struct DiffView {
    /// The diff tab is showing (false = the ordinary agent PTY tab).
    pub shown: bool,
    pub focus: DiffFocus,
    /// false = scope to transcript-touched files; true = whole repo.
    pub scope_all: bool,
    pub snapshot: Option<DiffSnapshot>,
    pub error: Option<String>,
    /// A compute request is in flight (spinner in the header).
    pub pending: bool,
    /// Monotonic request id; stale worker replies are dropped.
    pub generation: u64,
    /// A refresh trigger fired while a request was in flight — run once
    /// more on completion so the view converges on the latest state.
    pub want_again: bool,
    pub req_at: Instant,
    pub last_done: Option<Instant>,
    /// True while the session has no transcript-derived file list yet.
    pub touched_empty: bool,
    /// The pane was resized since delta rendered — the worker must re-run
    /// so wrapping/side-by-side match the new width. Set during the draw
    /// prelude, consumed by the 1s poll.
    pub needs_width_refresh: bool,

    delta: Option<DeltaOutput>,
    rows: Vec<TreeRow>,
    pub cursor: usize,
    collapsed: HashSet<String>,
    body: Vec<BodyLine>,
    file_starts: Vec<usize>,
    pub scroll: usize,
    /// Width of one line-number gutter column, from the snapshot's largest
    /// line number (so gutters never jitter while scrolling).
    gutter: usize,
    /// Panel geometry observed at the last clamp — key handlers page by it.
    body_view_h: u16,
    tree_view_h: u16,
}

impl DiffView {
    pub fn new(scope_all: bool) -> Self {
        DiffView {
            shown: false,
            focus: DiffFocus::Tree,
            scope_all,
            snapshot: None,
            error: None,
            pending: false,
            generation: 0,
            want_again: false,
            req_at: Instant::now(),
            last_done: None,
            touched_empty: true,
            needs_width_refresh: false,
            delta: None,
            rows: Vec::new(),
            cursor: 0,
            collapsed: HashSet::new(),
            body: Vec::new(),
            file_starts: Vec::new(),
            scroll: 0,
            gutter: 3,
            body_view_h: 20,
            tree_view_h: 20,
        }
    }

    pub fn apply_snapshot(&mut self, snap: DiffSnapshot, delta: Option<DeltaOutput>) {
        // Keep the reader's place across live refreshes: the selected file
        // survives by path, the body offset by clamping.
        let selected_path = self.selected_file().map(|i| {
            self.snapshot.as_ref().map(|s| s.files[i].path.clone())
        });
        self.snapshot = Some(snap);
        self.delta = delta;
        self.needs_width_refresh = false;
        self.error = None;
        self.rebuild();
        if let Some(Some(path)) = selected_path
            && let Some(pos) = self.rows.iter().position(|r| {
                matches!(&r.node, TreeNode::File { file_idx }
                    if self.snapshot.as_ref().is_some_and(|s| s.files[*file_idx].path == path))
            })
        {
            self.cursor = pos;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Rebuild tree rows + body index from the current snapshot, honoring
    /// the collapsed set. Cheap (index-only), called on every collapse too.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.body.clear();
        self.file_starts.clear();
        let Some(snap) = &self.snapshot else { return };

        // --- file tree ---
        #[derive(Default)]
        struct DirNode {
            dirs: BTreeMap<String, DirNode>,
            files: Vec<(String, usize)>,
            adds: u32,
            dels: u32,
        }
        let mut root = DirNode::default();
        for (i, f) in snap.files.iter().enumerate() {
            let mut parts: Vec<&str> = f.path.split('/').filter(|p| !p.is_empty()).collect();
            let name = parts.pop().unwrap_or("").to_string();
            let mut node = &mut root;
            node.adds += f.adds;
            node.dels += f.dels;
            for p in parts {
                node = node.dirs.entry(p.to_string()).or_default();
                node.adds += f.adds;
                node.dels += f.dels;
            }
            node.files.push((name, i));
        }
        fn flatten(
            node: &DirNode,
            prefix: &str,
            depth: usize,
            collapsed: &HashSet<String>,
            out: &mut Vec<TreeRow>,
        ) {
            for (name, child) in &node.dirs {
                // GitHub-style chain compression: a dir whose only content
                // is one subdir collapses into a single "a/b/c" row.
                let mut label = name.clone();
                let mut full = format!("{prefix}{name}");
                let mut cur = child;
                while cur.files.is_empty() && cur.dirs.len() == 1 {
                    let (n2, c2) = cur.dirs.iter().next().unwrap();
                    label.push('/');
                    label.push_str(n2);
                    full.push('/');
                    full.push_str(n2);
                    cur = c2;
                }
                let is_collapsed = collapsed.contains(&full);
                out.push(TreeRow {
                    depth,
                    node: TreeNode::Dir {
                        label,
                        key: full.clone(),
                        adds: cur.adds,
                        dels: cur.dels,
                        collapsed: is_collapsed,
                    },
                });
                if !is_collapsed {
                    flatten(cur, &format!("{full}/"), depth + 1, collapsed, out);
                }
            }
            let mut files = node.files.clone();
            files.sort();
            for (_, fi) in files {
                out.push(TreeRow {
                    depth,
                    node: TreeNode::File { file_idx: fi },
                });
            }
        }
        flatten(&root, "", 0, &self.collapsed, &mut self.rows);

        // --- body index ---
        // Delta path: its pre-rendered lines ARE the body (they carry their
        // own hunk headers/numbers); vag keeps only its file-header anchors.
        let delta_rows: Option<Vec<usize>> = self
            .delta
            .as_ref()
            .filter(|d| d.per_file.len() == snap.files.len())
            .map(|d| d.per_file.iter().map(Vec::len).collect());
        let mut max_no = 1_u32;
        for (i, f) in snap.files.iter().enumerate() {
            self.file_starts.push(self.body.len());
            self.body.push(BodyLine::FileHeader(i));
            if let Some(rows) = &delta_rows {
                if rows[i] == 0 {
                    self.body.push(BodyLine::Note(if f.binary {
                        "binary file"
                    } else {
                        "no content changes"
                    }));
                }
                for j in 0..rows[i] {
                    self.body.push(BodyLine::Delta(i, j));
                }
                self.body.push(BodyLine::Blank);
                continue;
            }
            if f.binary {
                self.body.push(BodyLine::Note("binary file"));
            } else if f.hunks.is_empty() {
                self.body.push(BodyLine::Note("no content changes"));
            }
            for (h, hunk) in f.hunks.iter().enumerate() {
                self.body.push(BodyLine::HunkHeader(i, h));
                for l in 0..hunk.lines.len() {
                    let dl = &hunk.lines[l];
                    max_no = max_no
                        .max(dl.old_no.unwrap_or(0))
                        .max(dl.new_no.unwrap_or(0));
                    self.body.push(BodyLine::Diff(i, h, l));
                }
            }
            self.body.push(BodyLine::Blank);
        }
        self.gutter = (max_no.max(1).ilog10() as usize + 1).max(3);
    }

    /// Width of the delta render currently shown, if any.
    pub fn delta_width(&self) -> Option<u16> {
        self.delta.as_ref().map(|d| d.width)
    }

    /// The file whose section the body is currently scrolled into — the
    /// tree marks it with the sidebar's attached-session rail so the
    /// reader always knows where they are in a long diff.
    pub fn current_file(&self) -> Option<usize> {
        if self.file_starts.is_empty() {
            return None;
        }
        Some(
            self.file_starts
                .partition_point(|&start| start <= self.scroll)
                .saturating_sub(1),
        )
    }

    /// The width an external renderer should wrap for, given the pane
    /// content rect (body panel width; full width when the narrow layout
    /// currently shows only the tree).
    pub fn body_width(&self, area: Rect) -> u16 {
        let (_, body) = self.panels(area);
        if body.width > 0 { body.width } else { area.width }
    }

    pub fn selected_file(&self) -> Option<usize> {
        match self.rows.get(self.cursor)?.node {
            TreeNode::File { file_idx } => Some(file_idx),
            TreeNode::Dir { .. } => None,
        }
    }

    pub fn move_cursor(&mut self, d: i64) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let c = (self.cursor as i64 + d).clamp(0, len as i64 - 1);
        self.cursor = c as usize;
    }

    /// Enter on the selection: a dir toggles, a file jumps the body to its
    /// header and moves focus there (the "open the file" gesture).
    pub fn activate_selected(&mut self) {
        match &self.rows.get(self.cursor).map(|r| &r.node) {
            Some(TreeNode::Dir { .. }) => self.toggle_collapse(),
            Some(TreeNode::File { file_idx }) => {
                if let Some(&start) = self.file_starts.get(*file_idx) {
                    self.scroll = start;
                    self.focus = DiffFocus::Body;
                }
            }
            None => {}
        }
    }

    pub fn toggle_collapse(&mut self) {
        let Some(TreeRow {
            node: TreeNode::Dir { key, .. },
            ..
        }) = self.rows.get(self.cursor)
        else {
            return;
        };
        let key = key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        let keep = self.keyed_cursor();
        self.rebuild();
        self.restore_cursor(keep);
    }

    /// The selection's stable identity across rebuilds.
    fn keyed_cursor(&self) -> Option<String> {
        match &self.rows.get(self.cursor)?.node {
            TreeNode::Dir { key, .. } => Some(key.clone()),
            TreeNode::File { file_idx } => self
                .snapshot
                .as_ref()
                .map(|s| s.files[*file_idx].path.clone()),
        }
    }

    fn restore_cursor(&mut self, keep: Option<String>) {
        if let Some(keep) = keep
            && let Some(pos) = self.rows.iter().position(|r| match &r.node {
                TreeNode::Dir { key, .. } => *key == keep,
                TreeNode::File { file_idx } => self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.files[*file_idx].path == keep),
            })
        {
            self.cursor = pos;
            return;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    pub fn scroll_by(&mut self, d: i64) {
        // Saturating: `G` parks scroll at a beyond-end sentinel until the
        // next draw clamps it; an `as i64` round-trip would wrap it to -1
        // and a fast G-then-j would jump to the top instead of the bottom.
        self.scroll = self.scroll.saturating_add_signed(d as isize);
    }

    pub fn page(&mut self, dir: i64) {
        match self.focus {
            DiffFocus::Tree => self.move_cursor(dir * (self.tree_view_h.max(2) as i64 - 1)),
            DiffFocus::Body => self.scroll_by(dir * (self.body_view_h.max(2) as i64 - 1)),
        }
    }

    /// Vim's ctrl-d/ctrl-u: half the focused panel's viewport.
    pub fn half_page(&mut self, dir: i64) {
        match self.focus {
            DiffFocus::Tree => {
                self.move_cursor(dir * (self.tree_view_h as i64 / 2).max(1));
            }
            DiffFocus::Body => {
                self.scroll_by(dir * (self.body_view_h as i64 / 2).max(1));
            }
        }
    }

    pub fn jump_top(&mut self) {
        match self.focus {
            DiffFocus::Tree => self.cursor = 0,
            DiffFocus::Body => self.scroll = 0,
        }
    }

    pub fn jump_bottom(&mut self) {
        match self.focus {
            DiffFocus::Tree => self.cursor = self.rows.len().saturating_sub(1),
            // Finite beyond-end sentinel, clamped at the next draw.
            DiffFocus::Body => self.scroll = self.body.len(),
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            DiffFocus::Tree => DiffFocus::Body,
            DiffFocus::Body => DiffFocus::Tree,
        };
    }

    /// Panel geometry for `area` (the pane content rect): tree rect (None
    /// when too narrow for the split) and body rect, below the header rows.
    fn panels(&self, area: Rect) -> (Option<Rect>, Rect) {
        let panel = Rect {
            y: area.y + HEADER_ROWS.min(area.height),
            height: area.height.saturating_sub(HEADER_ROWS),
            ..area
        };
        let tree_w: u16 = if area.width >= 100 {
            36
        } else if area.width >= 72 {
            30
        } else if area.width >= 56 {
            24
        } else {
            0
        };
        if tree_w == 0 {
            // Too narrow to split: the focused panel takes the full width.
            return match self.focus {
                DiffFocus::Tree => (Some(panel), Rect { width: 0, ..panel }),
                DiffFocus::Body => (None, panel),
            };
        }
        let tree = Rect {
            width: tree_w.min(panel.width),
            ..panel
        };
        let body = Rect {
            x: panel.x + tree.width,
            width: panel.width.saturating_sub(tree.width),
            ..panel
        };
        (Some(tree), body)
    }

    /// Called from App::draw's &mut prelude: record panel heights and clamp
    /// scroll/cursor against them (render itself is &self).
    pub fn clamp_viewport(&mut self, area: Rect) {
        let (tree, body) = self.panels(area);
        // Each panel spends its first row on the files/diff focus chip.
        self.tree_view_h = tree.map(|t| t.height.saturating_sub(1)).unwrap_or(0);
        self.body_view_h = body.height.saturating_sub(1);
        // A delta render is width-bound: flag a re-run when the panel no
        // longer matches (consumed by the 1s poll, so resize storms don't
        // spawn a delta per frame).
        if let Some(w) = self.delta_width()
            && body.width > 0
            && w != body.width
        {
            self.needs_width_refresh = true;
        }
        let max_scroll = self
            .body
            .len()
            .saturating_sub(self.body_view_h as usize);
        self.scroll = self.scroll.min(max_scroll);
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Paint the diff view into the pane content rect.
pub fn render(
    f: &mut Frame,
    area: Rect,
    dv: &DiffView,
    repo: Option<&RepoCtx>,
    focused: bool,
    th: &Theme,
    icons: &Icons,
) {
    if area.height < 3 || area.width < 10 {
        return;
    }
    render_header(f, area, dv, repo, th);
    let (tree, body) = dv.panels(area);
    if let Some(tree) = tree
        && tree.width > 0
    {
        render_tree(f, tree, dv, focused, th, icons, body.width > 0);
    }
    if body.width > 0 {
        render_body(f, body, dv, focused, th);
    }
}

fn render_header(f: &mut Frame, area: Rect, dv: &DiffView, repo: Option<&RepoCtx>, th: &Theme) {
    let bar = Rect {
        height: 1,
        ..area
    };
    let dim = Style::new().fg(th.dim);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(" agent ", dim),
        Span::styled("│", dim),
        Span::styled(
            " diff ",
            Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", dim),
    ];
    // Prefer the base the worker ACTUALLY diffed against (post re-anchor)
    // over the recorded one, so the header never contradicts the content.
    let base = dv
        .snapshot
        .as_ref()
        .and_then(|s| s.base.as_deref())
        .or_else(|| repo.and_then(|r| r.base.as_deref()));
    if let Some(base) = base {
        spans.push(Span::styled(
            format!("⎇ {}..worktree", &base[..base.len().min(8)]),
            dim,
        ));
    }
    if let Some(snap) = &dv.snapshot {
        spans.push(Span::styled(
            format!("  {} files ", snap.files.len()),
            dim,
        ));
        spans.push(Span::styled(format!("+{}", snap.total_adds), Style::new().fg(ADDED)));
        spans.push(Span::styled(" ", dim));
        spans.push(Span::styled(format!("−{}", snap.total_dels), Style::new().fg(REMOVED)));
        if snap.truncated {
            spans.push(Span::styled("  (truncated)", Style::new().fg(MODIFIED)));
        }
        if let Some(note) = &snap.base_note {
            spans.push(Span::styled(format!("  ⚠ {note}"), Style::new().fg(MODIFIED)));
        }
    }
    spans.push(Span::styled(
        if dv.scope_all {
            "  [all files]"
        } else {
            "  [agent files]"
        },
        Style::new().fg(th.info),
    ));
    if dv.pending {
        spans.push(Span::styled("  refreshing…", dim));
    }
    // Right-aligned key hints, dropped entirely when the bar is crowded.
    let hints = "a:scope  r:refresh  B:rebase  tab:panel  esc:agent ";
    let used: usize = spans.iter().map(Span::width).sum();
    let width = bar.width as usize;
    if used + hints.len() + 2 <= width {
        spans.push(Span::styled(
            " ".repeat(width - used - hints.len()),
            dim,
        ));
        spans.push(Span::styled(hints.to_string(), dim));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
    let rule = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    f.render_widget(Paragraph::new(dashboard::rule_line(th, rule.width)), rule);
}

fn render_tree(
    f: &mut Frame,
    area: Rect,
    dv: &DiffView,
    focused: bool,
    th: &Theme,
    icons: &Icons,
    with_border: bool,
) {
    let block = Block::new()
        .borders(if with_border {
            Borders::RIGHT
        } else {
            Borders::NONE
        })
        .border_style(Style::new().fg(th.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height < 2 {
        return;
    }
    let tree_focused = focused && dv.focus == DiffFocus::Tree;
    f.render_widget(
        Paragraph::new(panel_chip("files", tree_focused, th)),
        Rect { height: 1, ..inner },
    );
    let inner = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    let visible = inner.height as usize;
    // Same rough-centering the session tree uses.
    let top = dv
        .cursor
        .saturating_sub(visible / 2)
        .min(dv.rows.len().saturating_sub(visible));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible);
    let Some(snap) = &dv.snapshot else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if dv.pending { " computing…" } else { " no diff" },
                Style::new().fg(th.dim),
            ))),
            inner,
        );
        return;
    };
    let current = dv.current_file();
    for (i, row) in dv.rows.iter().enumerate().skip(top).take(visible) {
        let selected = i == dv.cursor;
        let indent = "  ".repeat(row.depth);
        // The file the body is scrolled into gets the sidebar's attached
        // rail (replacing the lead padding, so nothing shifts).
        let is_current = matches!(&row.node, TreeNode::File { file_idx } if Some(*file_idx) == current);
        let mut spans: Vec<Span<'static>> = if is_current {
            vec![
                Span::styled("▌", Style::new().fg(th.accent)),
                Span::raw(indent.clone()),
            ]
        } else {
            vec![Span::raw(format!(" {indent}"))]
        };
        match &row.node {
            TreeNode::Dir {
                label,
                adds,
                dels,
                collapsed,
                ..
            } => {
                let arrow = if *collapsed {
                    icons.folder_collapsed
                } else {
                    icons.folder_expanded
                };
                spans.push(Span::styled(
                    format!("{arrow} "),
                    Style::new().fg(th.dim),
                ));
                spans.push(Span::styled(
                    label.clone(),
                    Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                ));
                if *collapsed && (*adds > 0 || *dels > 0) {
                    spans.push(Span::styled(
                        format!(" +{adds} −{dels}"),
                        Style::new().fg(th.dim),
                    ));
                }
            }
            TreeNode::File { file_idx } => {
                let fd = &snap.files[*file_idx];
                let name = fd.path.rsplit('/').next().unwrap_or(&fd.path);
                spans.push(Span::styled(
                    format!("{} ", fd.status.glyph()),
                    Style::new().fg(status_color(fd.status)),
                ));
                let icon = icons.file_icon(name);
                if !icon.is_empty() {
                    spans.push(Span::styled(
                        format!("{icon} "),
                        Style::new().fg(th.info),
                    ));
                }
                let name_style = if is_current {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                spans.push(Span::styled(name.to_string(), name_style));
                if fd.adds > 0 {
                    spans.push(Span::styled(
                        format!(" +{}", fd.adds),
                        Style::new().fg(ADDED),
                    ));
                }
                if fd.dels > 0 {
                    spans.push(Span::styled(
                        format!(" −{}", fd.dels),
                        Style::new().fg(REMOVED),
                    ));
                }
            }
        }
        let mut line = Line::from(spans);
        if selected {
            // House rule: selection is a solid bg bar padded to full width,
            // never Modifier::REVERSED. Pad by DISPLAY width (CJK/emoji
            // paths occupy more cells than chars).
            let bg = if tree_focused { th.sel } else { th.surface };
            let used = line.width();
            line.spans.push(Span::raw(
                " ".repeat((inner.width as usize).saturating_sub(used)),
            ));
            line = line.style(Style::new().bg(bg));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_body(f: &mut Frame, area: Rect, dv: &DiffView, focused: bool, th: &Theme) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    f.render_widget(
        Paragraph::new(panel_chip("diff", focused && dv.focus == DiffFocus::Body, th)),
        Rect { height: 1, ..area },
    );
    let area = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    if let Some(err) = &dv.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {err}"),
                Style::new().fg(REMOVED),
            ))),
            area,
        );
        return;
    }
    let Some(snap) = &dv.snapshot else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if dv.pending {
                    " computing diff…"
                } else {
                    " no diff yet"
                },
                Style::new().fg(th.dim),
            ))),
            area,
        );
        return;
    };
    if snap.files.is_empty() {
        let msg = if dv.scope_all {
            " working tree matches the base commit — nothing to show".to_string()
        } else if dv.touched_empty {
            " the agent hasn't edited any files this run — `a` shows the whole repo".to_string()
        } else {
            " no remaining changes in agent-touched files — `a` shows the whole repo".to_string()
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(th.dim)))),
            area,
        );
        return;
    }
    let gw = dv.gutter;
    let dim = Style::new().fg(th.dim);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    for bl in dv.body.iter().skip(dv.scroll).take(area.height as usize) {
        lines.push(match bl {
            BodyLine::FileHeader(i) => {
                let fd = &snap.files[*i];
                let rename = fd
                    .old_path
                    .as_ref()
                    .map(|o| format!("{o} → "))
                    .unwrap_or_default();
                let mut spans = vec![
                    Span::styled(
                        format!(" {} ", fd.status.glyph()),
                        Style::new()
                            .fg(status_color(fd.status))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{rename}{}", fd.path),
                        Style::new().fg(th.fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  +{}", fd.adds), Style::new().fg(ADDED)),
                    Span::styled(format!(" −{}", fd.dels), Style::new().fg(REMOVED)),
                ];
                let used: usize = spans.iter().map(Span::width).sum();
                spans.push(Span::raw(
                    " ".repeat((area.width as usize).saturating_sub(used)),
                ));
                Line::from(spans).style(Style::new().bg(th.surface))
            }
            BodyLine::HunkHeader(i, h) => Line::from(Span::styled(
                format!(" {}", snap.files[*i].hunks[*h].header),
                Style::new().fg(th.accent),
            )),
            BodyLine::Diff(i, h, l) => {
                let dl = &snap.files[*i].hunks[*h].lines[*l];
                let (sign, style) = match dl.kind {
                    LineKind::Add => ("+", Style::new().fg(ADDED)),
                    LineKind::Remove => ("-", Style::new().fg(REMOVED)),
                    LineKind::Context => (" ", Style::new().fg(th.fg)),
                    LineKind::Meta => ("", dim),
                };
                let old = dl
                    .old_no
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let new = dl
                    .new_no
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                Line::from(vec![
                    Span::styled(format!(" {old:>gw$} {new:>gw$} "), dim),
                    Span::styled(format!("{sign}{}", dl.text), style),
                ])
            }
            BodyLine::Delta(i, j) => dv
                .delta
                .as_ref()
                .and_then(|d| d.per_file.get(*i).and_then(|f| f.get(*j)))
                .cloned()
                .unwrap_or_default(),
            BodyLine::Note(note) => Line::from(Span::styled(
                format!("   ({note})"),
                dim.add_modifier(Modifier::ITALIC),
            )),
            BodyLine::Blank => Line::default(),
        });
    }
    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitdiff::{DiffLine, FileDiff, Hunk};

    fn file(path: &str, status: FileStatus, adds: u32, dels: u32) -> FileDiff {
        let lines = (0..adds)
            .map(|n| DiffLine {
                kind: LineKind::Add,
                old_no: None,
                new_no: Some(n + 1),
                text: format!("line {n}"),
            })
            .chain((0..dels).map(|n| DiffLine {
                kind: LineKind::Remove,
                old_no: Some(n + 1),
                new_no: None,
                text: format!("gone {n}"),
            }))
            .collect::<Vec<_>>();
        FileDiff {
            path: path.into(),
            old_path: None,
            status,
            binary: false,
            adds,
            dels,
            hunks: if lines.is_empty() {
                vec![]
            } else {
                vec![Hunk {
                    header: "@@ -1 +1 @@".into(),
                    lines,
                }]
            },
            raw: String::new(),
        }
    }

    fn snap(files: Vec<FileDiff>) -> DiffSnapshot {
        let total_adds = files.iter().map(|f| f.adds).sum();
        let total_dels = files.iter().map(|f| f.dels).sum();
        DiffSnapshot {
            base: Some("abc".into()),
            base_note: None,
            files,
            total_adds,
            total_dels,
            truncated: false,
            fingerprint: 1,
        }
    }

    fn labels(dv: &DiffView) -> Vec<(usize, String)> {
        dv.rows
            .iter()
            .map(|r| match &r.node {
                TreeNode::Dir { label, .. } => (r.depth, format!("{label}/")),
                TreeNode::File { file_idx } => {
                    let p = &dv.snapshot.as_ref().unwrap().files[*file_idx].path;
                    (r.depth, p.rsplit('/').next().unwrap().to_string())
                }
            })
            .collect()
    }

    #[test]
    fn tree_nests_sorts_and_compresses_chains() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(snap(vec![
            file("src/ui/pane.rs", FileStatus::Modified, 1, 0),
            file("src/ui/app.rs", FileStatus::Modified, 2, 1),
            file("src/gitdiff.rs", FileStatus::Added, 5, 0),
            file("README.md", FileStatus::Deleted, 0, 3),
            file("deep/a/b/leaf.txt", FileStatus::Untracked, 1, 0),
        ]), None);
        assert_eq!(
            labels(&dv),
            vec![
                (0, "deep/a/b/".to_string()), // chain compressed
                (1, "leaf.txt".to_string()),
                (0, "src/".to_string()),
                (1, "ui/".to_string()),
                (2, "app.rs".to_string()), // files sorted
                (2, "pane.rs".to_string()),
                (1, "gitdiff.rs".to_string()),
                (0, "README.md".to_string()),
            ]
        );
    }

    #[test]
    fn collapse_hides_subtree_and_survives_refresh() {
        let mut dv = DiffView::new(false);
        let files = vec![
            file("src/a.rs", FileStatus::Modified, 1, 0),
            file("src/b.rs", FileStatus::Modified, 1, 0),
            file("top.txt", FileStatus::Modified, 1, 0),
        ];
        dv.apply_snapshot(snap(files.clone()), None);
        assert_eq!(dv.rows.len(), 4); // src/, a, b, top.txt
        dv.cursor = 0;
        dv.toggle_collapse();
        assert_eq!(dv.rows.len(), 2); // src/ (collapsed), top.txt
        // A refresh keeps the collapse state.
        dv.apply_snapshot(snap(files), None);
        assert_eq!(dv.rows.len(), 2);
        dv.toggle_collapse();
        assert_eq!(dv.rows.len(), 4);
    }

    #[test]
    fn activate_file_jumps_body_to_its_header() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(snap(vec![
            file("a.txt", FileStatus::Modified, 2, 1),
            file("b.txt", FileStatus::Modified, 1, 0),
        ]), None);
        // rows: a.txt, b.txt (no dirs)
        dv.cursor = 1;
        dv.activate_selected();
        assert_eq!(dv.focus, DiffFocus::Body);
        // body: header(a) hunk 3 lines blank header(b)...
        assert_eq!(dv.scroll, dv.file_starts[1]);
        assert!(matches!(dv.body[dv.scroll], BodyLine::FileHeader(1)));
    }

    #[test]
    fn selection_survives_snapshot_refresh_by_path() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(snap(vec![
            file("a.txt", FileStatus::Modified, 1, 0),
            file("c.txt", FileStatus::Modified, 1, 0),
        ]), None);
        dv.cursor = 1; // c.txt
        // A new file appears before c.txt; the selection must follow it.
        dv.apply_snapshot(snap(vec![
            file("a.txt", FileStatus::Modified, 1, 0),
            file("b.txt", FileStatus::Added, 1, 0),
            file("c.txt", FileStatus::Modified, 1, 0),
        ]), None);
        assert_eq!(dv.selected_file(), Some(2));
    }

    #[test]
    fn viewport_clamp_bounds_scroll_and_page() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(snap(vec![file("a.txt", FileStatus::Modified, 50, 0)]), None);
        dv.focus = DiffFocus::Body;
        dv.jump_bottom();
        assert_eq!(dv.scroll, dv.body.len());
        // G followed immediately by j must not wrap the sentinel to 0.
        dv.scroll_by(1);
        assert!(dv.scroll > dv.body.len() / 2, "no wrap-to-top after G,j");
        dv.clamp_viewport(Rect::new(0, 0, 120, 22));
        // body = file header + hunk header + 50 lines + blank = 53;
        // viewport = 22 minus 2 chrome rows minus the panel focus chip
        assert_eq!(dv.scroll, 53 - 19);
        dv.page(-1);
        dv.clamp_viewport(Rect::new(0, 0, 120, 22));
        assert_eq!(dv.scroll, 53 - 19 - 18);
        // vim half-page: ctrl-u up by half the viewport, ctrl-d back down.
        dv.half_page(-1);
        assert_eq!(dv.scroll, 53 - 19 - 18 - 9);
        dv.half_page(1);
        assert_eq!(dv.scroll, 53 - 19 - 18);
        dv.jump_top();
        assert_eq!(dv.scroll, 0);
        dv.scroll_by(-5);
        assert_eq!(dv.scroll, 0);
    }

    #[test]
    fn current_file_follows_body_scroll() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(
            snap(vec![
                file("a.txt", FileStatus::Modified, 3, 0),
                file("b.txt", FileStatus::Modified, 2, 0),
            ]),
            None,
        );
        // body: hdr(a)=0 hunk 1-3 blank=5 hdr(b)=6 hunk 7-8 blank
        assert_eq!(dv.current_file(), Some(0));
        dv.scroll = dv.file_starts[1] - 1;
        assert_eq!(dv.current_file(), Some(0));
        dv.scroll = dv.file_starts[1];
        assert_eq!(dv.current_file(), Some(1));
        dv.scroll = dv.body.len() - 1;
        assert_eq!(dv.current_file(), Some(1));
        assert_eq!(DiffView::new(false).current_file(), None);
    }

    #[test]
    fn delta_lines_replace_builtin_body_and_keep_file_anchors() {
        let mut dv = DiffView::new(false);
        let files = vec![
            file("a.txt", FileStatus::Modified, 2, 1),
            file("b.txt", FileStatus::Modified, 1, 0),
        ];
        let delta = DeltaOutput {
            width: 100,
            per_file: vec![
                vec![Line::raw("δ1"), Line::raw("δ2")],
                vec![Line::raw("δ3")],
            ],
        };
        dv.apply_snapshot(snap(files), Some(delta));
        assert_eq!(dv.delta_width(), Some(100));
        // body: header(a) δ1 δ2 blank header(b) δ3 blank
        assert_eq!(dv.body.len(), 7);
        assert!(matches!(dv.body[0], BodyLine::FileHeader(0)));
        assert!(matches!(dv.body[1], BodyLine::Delta(0, 0)));
        assert!(matches!(dv.body[4], BodyLine::FileHeader(1)));
        assert_eq!(dv.file_starts, vec![0, 4]);
        // A resize away from the rendered width flags a re-render once.
        dv.clamp_viewport(Rect::new(0, 0, 90, 20));
        assert!(dv.needs_width_refresh);
    }

    /// Real end-to-end delta run — skipped on machines without delta, the
    /// same graceful degradation the feature itself has.
    #[test]
    fn render_delta_produces_lines_for_raw_sections() {
        if !delta_available() {
            eprintln!("delta not installed — skipping");
            return;
        }
        let mut f = file("x.rs", FileStatus::Modified, 1, 1);
        f.raw = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,1 @@\n-fn old() {}\n+fn new_fn() {}\n".into();
        let out = render_delta(&[f], 100, &[]).expect("delta run succeeds");
        assert_eq!(out.per_file.len(), 1);
        let text: String = out.per_file[0]
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("new_fn"), "delta output carries the content");
    }

    #[test]
    fn narrow_panels_follow_focus() {
        let mut dv = DiffView::new(false);
        dv.apply_snapshot(snap(vec![file("a.txt", FileStatus::Modified, 1, 0)]), None);
        let narrow = Rect::new(0, 0, 50, 20);
        dv.focus = DiffFocus::Tree;
        let (tree, body) = dv.panels(narrow);
        assert!(tree.is_some() && body.width == 0);
        dv.focus = DiffFocus::Body;
        let (tree, body) = dv.panels(narrow);
        assert!(tree.is_none() && body.width > 0);
        let wide = Rect::new(0, 0, 120, 20);
        let (tree, body) = dv.panels(wide);
        assert_eq!(tree.unwrap().width, 36);
        assert!(body.width > 0);
    }
}
