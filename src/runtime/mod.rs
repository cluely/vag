//! The terminal-embedding core: one `SessionRuntime` per opened session —
//! a PTY child (the real `claude`/`codex` TUI) pumped into a headless
//! alacritty_terminal emulator, rendered into a ratatui pane by
//! `crate::ui::pane`.
//!
//! ARCHITECTURE (fixed):
//! - PTY via portable-pty (`native_pty_system()`), child spawned from a
//!   `crate::actions::SpawnSpec` (cwd + env applied; parent env inherited).
//! - A reader thread drains the PTY master and feeds bytes through
//!   `alacritty_terminal::vte::ansi::Processor` into
//!   `Term<EventProxy>` behind `alacritty_terminal::sync::FairMutex`.
//!   After each chunk it stamps `last_output` (Instant) and sends
//!   `RuntimeEvent::Wakeup` (coalescing: don't flood the channel — skip the
//!   send if one is already pending via an AtomicBool the UI clears).
//! - A dedicated writer thread owns the blocking PTY writer, fed by an
//!   unbounded channel. ALL writes (user input from the UI thread, query
//!   responses from the reader thread's EventProxy) are channel sends and
//!   never block. This is load-bearing: a blocking write on the UI thread
//!   sharing a mutex with the reader's query-response path deadlocks
//!   three ways (UI blocked in write_all on a full PTY input queue, child
//!   blocked writing unread stdout, reader blocked on the writer mutex).
//! - EventProxy (implements `alacritty_terminal::event::EventListener`)
//!   handles:
//!     * Event::PtyWrite(s) → send to the writer thread (query responses:
//!       DA1, DSR/CPR, DECRQM incl. sync-output ?2026, OSC 10/11 colors…)
//!       — EXCEPT kitty-keyboard-protocol reports (`ESC [ ? <flags> u`):
//!       DROP those so children treat kitty as unsupported and fall back to
//!       legacy key encoding, matching what the host terminal actually
//!       sends us. (Half-advertised kitty produces CSI-u garbage — codex
//!       precedent.)
//!     * Event::ColorRequest(idx, fmt) → write fmt(palette color) back.
//!     * Event::Title(s) → RuntimeEvent::Title.
//!     * Event::ChildExit / EOF → RuntimeEvent::Exited (also delivered when
//!       the reader thread sees EOF and reaps the child).
//!     * Event::Bell → RuntimeEvent::Bell.
//! - Synchronized updates: alacritty's vte handles mode 2026 internally
//!   (buffers until commit) — the pane just renders current Term state on
//!   Wakeup; no extra gating needed beyond wakeup coalescing.
//!
//! ZOOM (full-screen handoff): `set_zoom(true)` makes the reader thread tee
//! raw PTY bytes to real stdout (in addition to feeding the emulator, which
//! keeps state consistent for un-zoom) AND suppresses EventProxy PtyWrite
//! responses (in zoom the child converses with the REAL terminal, which
//! answers queries itself; double answers = composer garbage). The UI layer
//! is responsible for screen setup/teardown around zoom and for re-blitting
//! the current grid via `crate::ui::pane::serialize_screen` on entry.
//! `set_zoom` synchronizes with the reader (see its docs): each chunk's
//! routing (tee decision + reply suppression) sees one consistent flag
//! value, and `set_zoom(false)` returns only after any in-flight teed chunk
//! has fully landed — so the UI can then safely restore its chrome.
//!
//! INPUT is raw bytes end-to-end: the UI forwards stdin bytes verbatim via
//! `write_input`; there is NO key re-encoding anywhere. Paste: the host
//! enables bracketed paste; if the child's Term mode lacks BRACKETED_PASTE,
//! `write_input_filtered` strips the ESC[200~/201~ markers (helper for the
//! UI which can't know the mode).
//!
//! Dropping a SessionRuntime (or calling `kill`) must SIGHUP/kill the child
//! and join threads promptly (no zombie claude processes after vag quits).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::actions::SpawnSpec;
use crate::types::SessionKey;

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// New output was processed; re-render if this session is visible.
    Wakeup,
    /// Child exited (status best-effort).
    Exited(Option<i32>),
    /// OSC title change from the child.
    Title,
    Bell,
}

/// Size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u16,
    pub cols: u16,
}

impl PaneSize {
    /// Zero-sized PTYs/grids misbehave; clamp to at least 1x1.
    fn clamped(self) -> PaneSize {
        PaneSize {
            rows: self.rows.max(1),
            cols: self.cols.max(1),
        }
    }

    fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// `Dimensions` adapter for sizing/resizing the Term (viewport only; the
/// scrollback total is managed by the grid itself).
struct GridSize {
    rows: usize,
    cols: usize,
}

impl From<PaneSize> for GridSize {
    fn from(s: PaneSize) -> GridSize {
        GridSize {
            rows: s.rows as usize,
            cols: s.cols as usize,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Lock a std Mutex ignoring poisoning (a panicked writer never leaves these
/// small values in an invalid state).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// State shared between the runtime handle, the reader thread and the
/// EventProxy embedded in the Term.
struct Shared {
    key: SessionKey,
    events: Sender<(SessionKey, RuntimeEvent)>,
    /// Feeds the dedicated [`writer_thread`], which owns the blocking PTY
    /// writer. `None` after `kill()` closed the channel. A channel — not a
    /// mutexed writer — so neither the UI thread (user input) nor the
    /// reader thread (EventProxy replies, emitted while the Term is
    /// locked) can ever block on a full PTY input queue.
    writer_tx: Mutex<Option<Sender<Vec<u8>>>>,
    zoomed: AtomicBool,
    /// Held by the reader across each chunk's zoomed-check + tee + advance,
    /// and by `set_zoom` around the flag flip: a zoom transition can't
    /// interleave with an in-flight chunk's routing. Lock order is
    /// zoom_io → stdout/Term — never acquire it while holding either.
    zoom_io: Mutex<()>,
    wakeup_pending: AtomicBool,
    running: AtomicBool,
    exited_sent: AtomicBool,
    /// True once ANY path (reader's reap_child, kill()'s wait loops) has
    /// wait()ed the child. After a reap the pid may be recycled, so kill()
    /// must never signal it again. std's Child caches the exit status, so
    /// post-reap try_wait calls return Ok(Some) without a second waitpid.
    reaped: AtomicBool,
    last_output: Mutex<Instant>,
    title: Mutex<Option<String>>,
    size: Mutex<PaneSize>,
    bytes_received: AtomicU64,
    /// Optional raw byte-log (spike/debugging); written by the reader thread.
    raw_log: Mutex<Option<std::fs::File>>,
}

impl Shared {
    /// Queue bytes for the writer thread. Never blocks; silently drops
    /// after kill() closed the channel (child is gone anyway).
    fn write_pty(&self, bytes: &[u8]) {
        if let Some(tx) = lock(&self.writer_tx).as_ref() {
            let _ = tx.send(bytes.to_vec());
        }
    }

    /// Coalesced wakeup: only one Wakeup is in flight until the UI acks.
    fn send_wakeup(&self) {
        if !self.wakeup_pending.swap(true, Ordering::Relaxed) {
            let _ = self.events.send((self.key.clone(), RuntimeEvent::Wakeup));
        }
    }

    fn send_exited(&self, status: Option<i32>) {
        if !self.exited_sent.swap(true, Ordering::Relaxed) {
            let _ = self
                .events
                .send((self.key.clone(), RuntimeEvent::Exited(status)));
        }
    }
}

/// Kitty keyboard protocol report: `ESC [ ? <digits> u`. Dropped so children
/// treat kitty as unsupported (we never re-encode keys, so advertising it
/// would leak CSI-u sequences the host terminal never sends us).
fn is_kitty_keyboard_report(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 5
        && b.starts_with(b"\x1b[?")
        && b[b.len() - 1] == b'u'
        && b[3..b.len() - 1].iter().all(u8::is_ascii_digit)
}

/// Strip bracketed-paste markers (used when the child hasn't enabled
/// bracketed paste). Limitation: a marker split across two write_input calls
/// is not detected — in practice the host terminal delivers each marker
/// within a single read.
fn strip_paste_markers(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b
            && bytes.len() - i >= 6
            && (&bytes[i..i + 6] == b"\x1b[200~" || &bytes[i..i + 6] == b"\x1b[201~")
        {
            i += 6;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Fallback theme answered to OSC 10/11 queries (claude/codex use these to
/// detect dark vs light). A dark theme: light-gray foreground on near-black.
const DEFAULT_FG: Rgb = Rgb {
    r: 0xe6,
    g: 0xe6,
    b: 0xe6,
};
const DEFAULT_BG: Rgb = Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x1e,
};

/// vag's active theme: the emulator's OSC 10/11 answers must match the
/// colors the pane actually paints — agents then pick palettes that sit
/// correctly on the themed background. Set at startup and again on every
/// in-app theme switch; None = the fixed dark fallback (transparent theme).
/// Agents only query at startup, so ones already running keep their palette
/// until restarted — the pane's default cells still recolor instantly.
static THEME_COLORS: std::sync::RwLock<Option<(Rgb, Rgb)>> = std::sync::RwLock::new(None);

/// An (r, g, b) triple as plain bytes, for callers that shouldn't know Rgb.
pub type RgbBytes = (u8, u8, u8);

/// (fg, bg) the pane theme uses; None reverts to the fixed dark palette.
pub fn set_theme_colors(colors: Option<(RgbBytes, RgbBytes)>) {
    *THEME_COLORS.write().unwrap() = colors.map(|(fg, bg)| {
        (
            Rgb {
                r: fg.0,
                g: fg.1,
                b: fg.2,
            },
            Rgb {
                r: bg.0,
                g: bg.1,
                b: bg.2,
            },
        )
    });
}

fn theme_fg() -> Rgb {
    THEME_COLORS
        .read()
        .unwrap()
        .map(|(f, _)| f)
        .unwrap_or(DEFAULT_FG)
}

fn theme_bg() -> Rgb {
    THEME_COLORS
        .read()
        .unwrap()
        .map(|(_, b)| b)
        .unwrap_or(DEFAULT_BG)
}

/// Standard xterm 16-color palette.
#[rustfmt::skip]
const ANSI16: [Rgb; 16] = [
    Rgb { r: 0x00, g: 0x00, b: 0x00 },
    Rgb { r: 0xcd, g: 0x00, b: 0x00 },
    Rgb { r: 0x00, g: 0xcd, b: 0x00 },
    Rgb { r: 0xcd, g: 0xcd, b: 0x00 },
    Rgb { r: 0x00, g: 0x00, b: 0xee },
    Rgb { r: 0xcd, g: 0x00, b: 0xcd },
    Rgb { r: 0x00, g: 0xcd, b: 0xcd },
    Rgb { r: 0xe5, g: 0xe5, b: 0xe5 },
    Rgb { r: 0x7f, g: 0x7f, b: 0x7f },
    Rgb { r: 0xff, g: 0x00, b: 0x00 },
    Rgb { r: 0x00, g: 0xff, b: 0x00 },
    Rgb { r: 0xff, g: 0xff, b: 0x00 },
    Rgb { r: 0x5c, g: 0x5c, b: 0xff },
    Rgb { r: 0xff, g: 0x00, b: 0xff },
    Rgb { r: 0x00, g: 0xff, b: 0xff },
    Rgb { r: 0xff, g: 0xff, b: 0xff },
];

fn dim(rgb: Rgb) -> Rgb {
    Rgb {
        r: (rgb.r as u16 * 2 / 3) as u8,
        g: (rgb.g as u16 * 2 / 3) as u8,
        b: (rgb.b as u16 * 2 / 3) as u8,
    }
}

/// Palette answered to ColorRequest (OSC 4 queries and, via NamedColor
/// indices 256/257, OSC 10/11 fg/bg queries). Index layout follows
/// `vte::ansi::NamedColor` (Foreground = 256, Background = 257, …).
fn default_palette_color(index: usize) -> Rgb {
    match index {
        0..=15 => ANSI16[index],
        16..=231 => {
            let v = index - 16;
            let comp = |x: usize| if x == 0 { 0u8 } else { (55 + 40 * x) as u8 };
            Rgb {
                r: comp(v / 36),
                g: comp((v / 6) % 6),
                b: comp(v % 6),
            }
        }
        232..=255 => {
            let g = (8 + 10 * (index - 232)) as u8;
            Rgb { r: g, g, b: g }
        }
        256 => theme_fg(),                     // NamedColor::Foreground
        257 => theme_bg(),                     // NamedColor::Background
        258 => theme_fg(),                     // NamedColor::Cursor
        259..=266 => dim(ANSI16[index - 259]), // NamedColor::Dim*
        267 => theme_fg(),                     // NamedColor::BrightForeground
        268 => dim(theme_fg()),                // NamedColor::DimForeground
        _ => theme_fg(),
    }
}

/// The Term's event listener. Public only because it appears in the type of
/// [`SessionRuntime::term`]; construction is crate-internal.
pub struct EventProxy(Arc<Shared>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let s = &*self.0;
        match event {
            Event::PtyWrite(text) => {
                // In zoom the REAL terminal answers the child's queries;
                // double answers turn into composer garbage.
                if s.zoomed.load(Ordering::Relaxed) || is_kitty_keyboard_report(&text) {
                    return;
                }
                s.write_pty(text.as_bytes());
            }
            Event::ColorRequest(index, fmt) => {
                if s.zoomed.load(Ordering::Relaxed) {
                    return;
                }
                s.write_pty(fmt(default_palette_color(index)).as_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                if s.zoomed.load(Ordering::Relaxed) {
                    return;
                }
                let size = *lock(&s.size);
                // Pixel sizes unknown (headless): report zeros, the honest
                // "unsupported" answer (same as tmux).
                let ws = WindowSize {
                    num_lines: size.rows,
                    num_cols: size.cols,
                    cell_width: 0,
                    cell_height: 0,
                };
                s.write_pty(fmt(ws).as_bytes());
            }
            Event::Title(title) => {
                *lock(&s.title) = Some(title.clone());
                let _ = s.events.send((s.key.clone(), RuntimeEvent::Title));
            }
            Event::ResetTitle => {
                *lock(&s.title) = None;
            }
            Event::Bell => {
                let _ = s.events.send((s.key.clone(), RuntimeEvent::Bell));
            }
            Event::Wakeup => s.send_wakeup(),
            // Clipboard, mouse-cursor, blink, exit: not applicable headless.
            _ => {}
        }
    }
}

pub struct SessionRuntime {
    shared: Arc<Shared>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    child_pid: Option<u32>,
    #[allow(dead_code)] // used by examples/spike.rs via cwd()
    cwd: PathBuf,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl SessionRuntime {
    /// Spawn the child on a fresh PTY of `size`. Events are tagged with
    /// `key` and sent on `events` from background threads.
    pub fn spawn(
        key: SessionKey,
        spec: &SpawnSpec,
        size: PaneSize,
        events: Sender<(SessionKey, RuntimeEvent)>,
    ) -> Result<SessionRuntime> {
        let size = size.clamped();
        let pty = native_pty_system()
            .openpty(size.pty_size())
            .context("opening pty")?;

        let mut cmd = CommandBuilder::new(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        cmd.cwd(&spec.cwd);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pty
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning `{}`", spec.program))?;
        // Close our copy of the slave so EOF propagates when the child dies.
        drop(pty.slave);

        let child_pid = child.process_id();
        let writer = pty.master.take_writer().context("taking pty writer")?;
        let reader = pty
            .master
            .try_clone_reader()
            .context("cloning pty reader")?;

        let (writer_tx, writer_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let shared = Arc::new(Shared {
            key: key.clone(),
            events,
            writer_tx: Mutex::new(Some(writer_tx)),
            zoomed: AtomicBool::new(false),
            zoom_io: Mutex::new(()),
            wakeup_pending: AtomicBool::new(false),
            running: AtomicBool::new(true),
            exited_sent: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            last_output: Mutex::new(Instant::now()),
            title: Mutex::new(None),
            size: Mutex::new(size),
            bytes_received: AtomicU64::new(0),
            raw_log: Mutex::new(None),
        });

        let term_config = TermConfig {
            scrolling_history: 10_000,
            // Do NOT enable kitty keyboard: we forward host bytes verbatim
            // and the host terminal may not speak kitty. With this off the
            // Term ignores kitty queries entirely (correct xterm behavior).
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &GridSize::from(size),
            EventProxy(shared.clone()),
        )));

        let child = Arc::new(Mutex::new(child));

        let thread_shared = shared.clone();
        let thread_term = term.clone();
        let thread_child = child.clone();
        let handle = std::thread::Builder::new()
            .name(format!("vag-pty-{}", &key.id[..key.id.len().min(12)]))
            .spawn(move || {
                reader_thread(thread_shared, thread_term, thread_child, child_pid, reader)
            })
            .context("spawning pty reader thread")?;

        let writer_handle = std::thread::Builder::new()
            .name(format!("vag-ptyw-{}", &key.id[..key.id.len().min(12)]))
            .spawn(move || writer_thread(writer_rx, writer))
            .context("spawning pty writer thread")?;

        Ok(SessionRuntime {
            shared,
            term,
            master: Mutex::new(pty.master),
            child,
            child_pid,
            cwd: spec.cwd.clone(),
            reader: Some(handle),
            writer: Some(writer_handle),
        })
    }

    #[allow(dead_code)] // used by examples/spike.rs
    pub fn key(&self) -> &SessionKey {
        &self.shared.key
    }

    /// The cwd this runtime was spawned with (for display).
    #[allow(dead_code)] // used by examples/spike.rs
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Resize PTY + emulator (no-op if size unchanged).
    pub fn resize(&self, size: PaneSize) {
        let size = size.clamped();
        {
            let mut cur = lock(&self.shared.size);
            if *cur == size {
                return;
            }
            *cur = size;
            // Drop the size lock before touching the Term: the EventProxy
            // locks size while the Term is held on the reader thread.
        }
        self.term.lock().resize(GridSize::from(size));
        // PTY last: SIGWINCH fires once the grid is ready (alacritty order).
        let _ = lock(&self.master).resize(size.pty_size());
    }

    /// Forward raw input bytes to the child verbatim.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            // Typing snaps the view back to the live screen (alacritty
            // semantics).
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
        }
        // Channel send: never blocks, even if the child isn't draining its
        // input queue (the writer thread absorbs the backpressure).
        self.shared.write_pty(bytes);
    }

    /// Like write_input, but strips bracketed-paste markers when the child
    /// hasn't enabled bracketed paste.
    pub fn write_input_filtered(&self, bytes: &[u8]) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            self.write_input(bytes);
        } else {
            self.write_input(&strip_paste_markers(bytes));
        }
    }

    /// Toggle zoom (raw tee to real stdout + query-reply suppression).
    ///
    /// Synchronizes with the reader thread via `zoom_io`: the reader holds
    /// that mutex across each chunk's tee + advance, so this returns only
    /// after any in-flight chunk finished routing under the OLD flag and
    /// every later chunk sees the new one. Consequences the UI relies on:
    /// - after `set_zoom(false)` returns, no more child bytes can reach the
    ///   real terminal — restoring the chrome (reset string, alt screen,
    ///   clear) cannot be corrupted by a straggling tee;
    /// - on `set_zoom(true)`, a chunk processed just before the flip went
    ///   only to the emulator and is missing from the real screen; the
    ///   caller's post-zoom PTY resize (SIGWINCH → full child repaint)
    ///   self-heals that, so entry needs no stronger coupling.
    ///
    /// Lock order: never call this while holding the Term lock (the reader
    /// acquires zoom_io BEFORE the Term lock).
    pub fn set_zoom(&self, on: bool) {
        let _io = lock(&self.shared.zoom_io);
        self.shared.zoomed.store(on, Ordering::Relaxed);
    }

    /// Scroll the display (positive = up/back in history). Any input write
    /// snaps back to bottom; output while scrolled keeps the view pinned
    /// (alacritty semantics).
    pub fn scroll_display(&self, delta: i32) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
        self.shared.send_wakeup();
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    /// Instant of the most recent child output (activity badges).
    pub fn last_output(&self) -> Instant {
        *lock(&self.shared.last_output)
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Latest OSC title, if any.
    pub fn title(&self) -> Option<String> {
        lock(&self.shared.title).clone()
    }

    /// Total raw PTY bytes received (diagnostics/spike).
    #[allow(dead_code)] // used by examples/spike.rs
    pub fn bytes_received(&self) -> u64 {
        self.shared.bytes_received.load(Ordering::Relaxed)
    }

    /// Tee every raw PTY chunk into `file` (append; spike/debugging).
    #[allow(dead_code)] // used by examples/spike.rs
    pub fn set_raw_log(&self, file: Option<std::fs::File>) {
        *lock(&self.shared.raw_log) = file;
    }

    /// Clear the coalescing flag. The UI must call this when it handles a
    /// `RuntimeEvent::Wakeup` (before reading Term state) so further output
    /// produces a new Wakeup.
    pub fn ack_wakeup(&self) {
        self.shared.wakeup_pending.store(false, Ordering::Relaxed);
    }

    /// Terminate the child (SIGHUP, then a ~1s grace period for the CLI's
    /// shutdown path, then SIGKILL) and join threads.
    pub fn kill(&mut self) {
        let Some(handle) = self.reader.take() else {
            return; // already killed
        };

        // Close the writer channel: no new writes are accepted and the
        // writer thread exits once it drains (or its blocked write fails
        // with EPIPE/EIO after the child dies).
        lock(&self.shared.writer_tx).take();

        let mut status = None;
        let mut reaped = self.shared.reaped.load(Ordering::Relaxed);

        if !reaped {
            // Polite: SIGHUP — lets the CLI shut down gracefully.
            // - Raw libc::kill, NOT portable-pty's ChildKiller::kill(): its
            //   unix impl escalates to SIGKILL by itself after ~200ms,
            //   which would defeat the grace period below.
            // - Never signal an already-reaped child: its pid may have
            //   been recycled and SIGHUP would hit an unrelated process.
            //   ONE guard is held across check + signal; the reap paths
            //   contend on this same mutex, so after Ok(None) the child is
            //   at worst an un-reaped zombie, whose pid cannot be recycled.
            let mut child = lock(&self.child);
            match child.try_wait() {
                Ok(Some(st)) => {
                    // Exited on its own (fresh or cached): nothing to signal.
                    status = Some(st.exit_code() as i32);
                    reaped = true;
                    self.shared.reaped.store(true, Ordering::Relaxed);
                }
                Ok(None) => {
                    #[cfg(unix)]
                    if let Some(pid) = self.child_pid {
                        unsafe {
                            libc::kill(pid as i32, libc::SIGHUP);
                        }
                    } else {
                        // Defensive fallback; process_id() is Some on unix.
                        let _ = child.kill();
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill();
                    }
                }
                // std children return the cached status after a reap, so
                // Err is a genuine OS failure: treat the child as gone.
                Err(_) => reaped = true,
            }
        }

        if !reaped {
            let deadline = Instant::now() + Duration::from_millis(1000);
            while Instant::now() < deadline {
                match lock(&self.child).try_wait() {
                    Ok(Some(st)) => {
                        status = Some(st.exit_code() as i32);
                        reaped = true;
                        self.shared.reaped.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(_) => {
                        reaped = true;
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }

        if !reaped {
            force_kill(&self.child, self.child_pid);
            for _ in 0..100 {
                match lock(&self.child).try_wait() {
                    Ok(Some(st)) => {
                        status = Some(st.exit_code() as i32);
                        self.shared.reaped.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }

        self.shared.running.store(false, Ordering::Relaxed);
        self.shared.send_exited(status);
        // The dead process group released the slave fds → the reader sees
        // EOF and exits, and a writer blocked mid-write fails. (Pathological
        // orphans that re-set their own pgid could keep the slave open; not
        // handled in v1.)
        let _ = handle.join();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }

    /// The live Term handle for the pane painter / screen serializer.
    /// Lock briefly; the reader thread contends on this for every chunk.
    pub fn term(&self) -> &Arc<FairMutex<Term<EventProxy>>> {
        &self.term
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.kill();
    }
}

/// SIGKILL the child and its process group (portable-pty children are
/// session leaders, so pgid == pid and grandchildren die too).
fn force_kill(child: &Mutex<Box<dyn Child + Send + Sync>>, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        return;
    }
    // Non-unix / no pid: best effort via portable-pty.
    let _ = lock(child).kill();
}

/// Owns the blocking PTY writer. Every producer (UI-thread input, reader-
/// thread EventProxy replies) is a non-blocking channel send; only this
/// thread can park on a full PTY input queue. Exits when the channel closes
/// (kill()) or a write fails (EPIPE/EIO once the child is gone) — a write
/// blocked on a live-but-stuck child unblocks with an error when kill()
/// tears the child down and the slave side closes.
fn writer_thread(rx: Receiver<Vec<u8>>, mut writer: Box<dyn Write + Send>) {
    for bytes in rx {
        if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
            break;
        }
    }
}

fn reader_thread(
    shared: Arc<Shared>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    child_pid: Option<u32>,
    mut reader: Box<dyn std::io::Read + Send>,
) {
    let mut processor: Processor = Processor::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                shared.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                if let Some(f) = lock(&shared.raw_log).as_mut() {
                    let _ = f.write_all(chunk);
                }
                {
                    // zoom_io held across zoomed-check + tee + advance (but
                    // never across the blocking read): set_zoom contends on
                    // it, so a zoom transition can't interleave with this
                    // chunk's routing — the tee decision and the EventProxy
                    // reply-suppression checks (which fire inside advance)
                    // observe one consistent flag value per chunk.
                    let _io = lock(&shared.zoom_io);
                    if shared.zoomed.load(Ordering::Relaxed) {
                        // Zoom: raw passthrough to the real terminal. The
                        // emulator still consumes the same bytes below so
                        // the grid stays correct for un-zoom.
                        let stdout = std::io::stdout();
                        let mut out = stdout.lock();
                        let _ = out.write_all(chunk);
                        let _ = out.flush();
                    }
                    // lock_unfair like alacritty's event loop: fair lock()
                    // holders (the renderer) keep priority over us.
                    let mut term = term.lock_unfair();
                    processor.advance(&mut *term, chunk);
                    // vte buffers a synchronized update (mode 2026) until
                    // ESU or a timeout — but only re-checks on new bytes.
                    // Flush an expired one so a child that wedges mid-sync
                    // can't freeze the pane forever.
                    if processor
                        .sync_timeout()
                        .sync_timeout()
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        processor.stop_sync(&mut *term);
                    }
                }
                *lock(&shared.last_output) = Instant::now();
                shared.send_wakeup();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO = slave side closed (Linux); treat like EOF.
            Err(_) => break,
        }
    }

    // EOF: the child is gone (or abandoned its terminal) — reap it.
    let status = reap_child(&child, child_pid, &shared.reaped);
    shared.running.store(false, Ordering::Relaxed);
    shared.send_exited(status);
    shared.send_wakeup(); // repaint whatever the child left on screen
}

/// Wait for the child to exit; escalate to SIGKILL if it survives its
/// terminal by more than ~2s. Returns the exit code if we reaped it.
/// Marks `reaped` so kill() never signals a possibly-recycled pid (a
/// post-reap try_wait returns the cached status — no double waitpid).
fn reap_child(
    child: &Mutex<Box<dyn Child + Send + Sync>>,
    pid: Option<u32>,
    reaped: &AtomicBool,
) -> Option<i32> {
    for _ in 0..40 {
        match lock(child).try_wait() {
            Ok(Some(status)) => {
                reaped.store(true, Ordering::Relaxed);
                return Some(status.exit_code() as i32);
            }
            // Genuine OS failure (post-reap calls return the cached
            // status, not Err): treat the child as gone.
            Err(_) => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    force_kill(child, pid);
    for _ in 0..40 {
        match lock(child).try_wait() {
            Ok(Some(status)) => {
                reaped.store(true, Ordering::Relaxed);
                return Some(status.exit_code() as i32);
            }
            Err(_) => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentKind;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    use crossbeam_channel::{Receiver, unbounded};

    fn test_key(id: &str) -> SessionKey {
        SessionKey::new(AgentKind::Claude, id)
    }

    fn spec(program: &str, args: &[&str]) -> SpawnSpec {
        SpawnSpec {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: std::env::temp_dir(),
            env: vec![
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string()),
            ],
        }
    }

    fn grid_text(rt: &SessionRuntime) -> String {
        let term = rt.term().lock();
        let mut out = String::new();
        for line in 0..term.screen_lines() {
            let row = &term.grid()[Line(line as i32)];
            for col in 0..term.columns() {
                let cell = &row[Column(col)];
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    out.push(cell.c);
                }
            }
            out.push('\n');
        }
        out
    }

    /// Pump events until `pred(grid)` or timeout; returns the last grid.
    fn pump_until(
        rt: &SessionRuntime,
        rx: &Receiver<(SessionKey, RuntimeEvent)>,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> (String, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let text = grid_text(rt);
            if pred(&text) {
                return (text, true);
            }
            let now = Instant::now();
            if now >= deadline {
                return (text, false);
            }
            match rx.recv_timeout(deadline - now) {
                Ok((_, RuntimeEvent::Wakeup)) => rt.ack_wakeup(),
                Ok(_) => {}
                Err(_) => return (grid_text(rt), pred(&grid_text(rt))),
            }
        }
    }

    #[test]
    fn spawn_echo_renders_output() {
        let (tx, rx) = unbounded();
        let rt = SessionRuntime::spawn(
            test_key("echo-test"),
            &spec("/bin/echo", &["hello"]),
            PaneSize { rows: 5, cols: 40 },
            tx,
        )
        .expect("pty spawn");

        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("hello"));
        assert!(ok, "grid never showed child output; grid:\n{text}");

        // echo exits on its own → Exited arrives after the reader reaps it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((_, RuntimeEvent::Exited(_))) => {
                    exited = true;
                    break;
                }
                Ok((_, RuntimeEvent::Wakeup)) => rt.ack_wakeup(),
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        assert!(exited, "no Exited event");
        assert!(!rt.is_running());
    }

    #[test]
    fn kill_terminates_and_reaps() {
        let (tx, rx) = unbounded();
        let mut rt = SessionRuntime::spawn(
            test_key("kill-test"),
            &spec("/bin/sh", &["-c", "printf READY; sleep 30"]),
            PaneSize { rows: 4, cols: 30 },
            tx,
        )
        .expect("pty spawn");

        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("READY"));
        assert!(ok, "child never started; grid:\n{text}");
        assert!(rt.is_running());
        let pid = rt.child_pid().expect("child pid");

        rt.kill();
        assert!(!rt.is_running());

        // Reaped, not a zombie: the pid must be fully gone (kill(pid, 0)
        // succeeds for zombies, ESRCH only after waitpid).
        #[cfg(unix)]
        {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            assert!(!alive, "child pid {pid} still exists (zombie or running)");
        }

        // Exited must have been delivered exactly once.
        let mut exited_count = 0;
        while let Ok((_, ev)) = rx.try_recv() {
            if matches!(ev, RuntimeEvent::Exited(_)) {
                exited_count += 1;
            }
        }
        assert_eq!(exited_count, 1);

        // Second kill (and Drop) are no-ops.
        rt.kill();
    }

    #[test]
    fn resize_is_applied_and_noop_when_unchanged() {
        let (tx, rx) = unbounded();
        let rt = SessionRuntime::spawn(
            test_key("resize-test"),
            &spec("/bin/sh", &["-c", "printf go; sleep 30"]),
            PaneSize { rows: 6, cols: 20 },
            tx,
        )
        .expect("pty spawn");
        let (_, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("go"));
        assert!(ok);

        rt.resize(PaneSize { rows: 10, cols: 33 });
        {
            let term = rt.term().lock();
            assert_eq!(term.screen_lines(), 10);
            assert_eq!(term.columns(), 33);
        }
        // Same size again: must be a no-op (and not deadlock).
        rt.resize(PaneSize { rows: 10, cols: 33 });
    }

    #[test]
    fn input_reaches_child_via_writer_thread() {
        let (tx, rx) = unbounded();
        let rt = SessionRuntime::spawn(
            test_key("input-test"),
            &spec(
                "/bin/sh",
                &["-c", "printf READY; read line; printf 'got:%s.' \"$line\""],
            ),
            PaneSize { rows: 5, cols: 40 },
            tx,
        )
        .expect("pty spawn");
        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("READY"));
        assert!(ok, "child never started; grid:\n{text}");

        // write_input is now a channel send to the writer thread; the bytes
        // must still arrive at the child (and its response render).
        rt.write_input(b"hello\n");
        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| {
            t.contains("got:hello.")
        });
        assert!(ok, "input never reached the child; grid:\n{text}");
    }

    #[test]
    fn kill_after_natural_exit_skips_signaling() {
        let (tx, rx) = unbounded();
        let mut rt = SessionRuntime::spawn(
            test_key("natural-exit"),
            &spec("/bin/echo", &["bye"]),
            PaneSize { rows: 4, cols: 30 },
            tx,
        )
        .expect("pty spawn");

        // Wait for the reader thread to reap the self-exited child.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline && !exited {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((_, RuntimeEvent::Exited(_))) => exited = true,
                Ok((_, RuntimeEvent::Wakeup)) => rt.ack_wakeup(),
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        assert!(exited, "no Exited event");
        assert!(
            rt.shared.reaped.load(Ordering::Relaxed),
            "reader must mark the child reaped"
        );

        // kill() must skip signaling (the pid may be recycled) and must not
        // re-enter the grace wait: prompt return, no second Exited.
        let start = Instant::now();
        rt.kill();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "kill() must not wait on an already-reaped child"
        );
        let mut extra_exited = 0;
        while let Ok((_, ev)) = rx.try_recv() {
            if matches!(ev, RuntimeEvent::Exited(_)) {
                extra_exited += 1;
            }
        }
        assert_eq!(
            extra_exited, 0,
            "kill() after natural exit must not re-report"
        );
    }

    #[test]
    fn sighup_grace_lets_child_finish_cleanup_without_sigkill() {
        let (tx, rx) = unbounded();
        let mut rt = SessionRuntime::spawn(
            test_key("grace-test"),
            &spec(
                "/bin/sh",
                &[
                    "-c",
                    // HUP trap needs ~400ms of cleanup, then exits 42. The
                    // trap also kills the background sleep so the slave fd
                    // closes and the reader sees EOF.
                    "trap 'kill $spid 2>/dev/null; sleep 0.4; exit 42' HUP; \
                     printf READY; sleep 30 & spid=$!; wait $spid",
                ],
            ),
            PaneSize { rows: 4, cols: 30 },
            tx,
        )
        .expect("pty spawn");
        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("READY"));
        assert!(ok, "child never started; grid:\n{text}");

        rt.kill();

        // Exit code 42 proves the child ran its full SIGHUP cleanup inside
        // the 1s grace. portable-pty's ChildKiller (the old polite phase)
        // SIGKILLs internally after ~200ms, which would yield a non-42
        // status here.
        let mut status = None;
        while let Ok((_, ev)) = rx.try_recv() {
            if let RuntimeEvent::Exited(st) = ev {
                status = Some(st);
            }
        }
        assert_eq!(
            status,
            Some(Some(42)),
            "child must survive the grace period and exit via its HUP trap"
        );
    }

    #[test]
    fn zoom_toggle_synchronizes_without_wedging_reader() {
        let (tx, rx) = unbounded();
        let rt = SessionRuntime::spawn(
            test_key("zoom-toggle"),
            &spec(
                "/bin/sh",
                &[
                    "-c",
                    "printf READY; read line; printf 'after:%s.' \"$line\"",
                ],
            ),
            PaneSize { rows: 5, cols: 40 },
            tx,
        )
        .expect("pty spawn");
        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| t.contains("READY"));
        assert!(ok, "child never started; grid:\n{text}");

        // set_zoom contends on zoom_io with the reader's per-chunk critical
        // section: both toggles must return (no deadlock) and the reader
        // must keep consuming output afterwards. The child is quiet while
        // zoomed, so nothing is teed into the test harness stdout.
        rt.set_zoom(true);
        rt.set_zoom(false);
        rt.write_input(b"zz\n");
        let (text, ok) = pump_until(&rt, &rx, Duration::from_secs(10), |t| {
            t.contains("after:zz.")
        });
        assert!(ok, "reader wedged after zoom toggle; grid:\n{text}");
    }

    // ---- pure tests (no PTY) ----

    struct SinkWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for SinkWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_thread_drains_then_exits_on_close() {
        let (tx, rx) = unbounded::<Vec<u8>>();
        let written = Arc::new(Mutex::new(Vec::new()));
        let handle = std::thread::spawn({
            let written = written.clone();
            move || writer_thread(rx, Box::new(SinkWriter(written)))
        });
        tx.send(b"abc".to_vec()).unwrap();
        tx.send(b"def".to_vec()).unwrap();
        drop(tx); // what kill() does: close the channel
        handle.join().unwrap();
        assert_eq!(&*lock(&written), b"abcdef");
    }

    struct FailWriter;
    impl Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_thread_exits_on_write_error_with_sender_alive() {
        let (tx, rx) = unbounded::<Vec<u8>>();
        let handle = std::thread::spawn(move || writer_thread(rx, Box::new(FailWriter)));
        tx.send(b"x".to_vec()).unwrap();
        // EPIPE/EIO after child death must end the thread even though the
        // channel is still open — otherwise kill()'s join would hang.
        handle.join().unwrap();
        drop(tx);
    }

    /// Second element receives what write_pty queued for the writer thread.
    #[allow(clippy::type_complexity)]
    fn test_proxy() -> (
        EventProxy,
        Receiver<Vec<u8>>,
        Receiver<(SessionKey, RuntimeEvent)>,
    ) {
        let (wtx, wrx) = unbounded();
        let (tx, rx) = unbounded();
        let shared = Arc::new(Shared {
            key: test_key("proxy"),
            events: tx,
            writer_tx: Mutex::new(Some(wtx)),
            zoomed: AtomicBool::new(false),
            zoom_io: Mutex::new(()),
            wakeup_pending: AtomicBool::new(false),
            running: AtomicBool::new(true),
            exited_sent: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            last_output: Mutex::new(Instant::now()),
            title: Mutex::new(None),
            size: Mutex::new(PaneSize { rows: 24, cols: 80 }),
            bytes_received: AtomicU64::new(0),
            raw_log: Mutex::new(None),
        });
        (EventProxy(shared), wrx, rx)
    }

    #[test]
    fn kitty_reports_are_dropped_other_replies_pass() {
        assert!(is_kitty_keyboard_report("\x1b[?1u"));
        assert!(is_kitty_keyboard_report("\x1b[?0u"));
        assert!(is_kitty_keyboard_report("\x1b[?31u"));
        assert!(!is_kitty_keyboard_report("\x1b[?6c")); // DA1
        assert!(!is_kitty_keyboard_report("\x1b[0n")); // DSR
        assert!(!is_kitty_keyboard_report("\x1b[?2026;2$y")); // DECRQM
        assert!(!is_kitty_keyboard_report("\x1b[?u")); // no digits
        assert!(!is_kitty_keyboard_report("plain"));

        let (proxy, wrx, _rx) = test_proxy();
        proxy.send_event(Event::PtyWrite("\x1b[?1u".into()));
        assert!(wrx.try_recv().is_err(), "kitty report must be dropped");
        proxy.send_event(Event::PtyWrite("\x1b[?6c".into()));
        assert_eq!(wrx.try_recv().unwrap(), b"\x1b[?6c");
    }

    #[test]
    fn zoom_suppresses_pty_write() {
        let (proxy, wrx, _rx) = test_proxy();
        proxy.0.zoomed.store(true, Ordering::Relaxed);
        proxy.send_event(Event::PtyWrite("\x1b[?6c".into()));
        proxy.send_event(Event::ColorRequest(257, Arc::new(|_| "x".into())));
        assert!(wrx.try_recv().is_err());
    }

    #[test]
    fn color_request_answers_theme() {
        use alacritty_terminal::vte::ansi::NamedColor;
        assert_eq!(NamedColor::Foreground as usize, 256);
        assert_eq!(NamedColor::Background as usize, 257);

        let (proxy, wrx, _rx) = test_proxy();
        proxy.send_event(Event::ColorRequest(
            NamedColor::Background as usize,
            Arc::new(|c| format!("{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
        ));
        assert_eq!(String::from_utf8_lossy(&wrx.try_recv().unwrap()), "1e1e1e");
    }

    #[test]
    fn palette_formulas() {
        assert_eq!(default_palette_color(1), ANSI16[1]);
        // Cube: 16 = (0,0,0), 231 = (255,255,255)-ish (ee,ee,ee → 255? no: 55+40*5=255)
        assert_eq!(default_palette_color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            default_palette_color(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        // Grayscale ends at 238.
        assert_eq!(
            default_palette_color(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[test]
    fn title_and_bell_events() {
        let (proxy, _written, rx) = test_proxy();
        proxy.send_event(Event::Title("hi".into()));
        proxy.send_event(Event::Bell);
        assert!(matches!(rx.try_recv(), Ok((_, RuntimeEvent::Title))));
        assert!(matches!(rx.try_recv(), Ok((_, RuntimeEvent::Bell))));
        assert_eq!(*lock(&proxy.0.title), Some("hi".to_string()));
        proxy.send_event(Event::ResetTitle);
        assert_eq!(*lock(&proxy.0.title), None);
    }

    #[test]
    fn wakeups_coalesce_until_acked() {
        let (proxy, _written, rx) = test_proxy();
        proxy.0.send_wakeup();
        proxy.0.send_wakeup();
        proxy.0.send_wakeup();
        assert!(matches!(rx.try_recv(), Ok((_, RuntimeEvent::Wakeup))));
        assert!(rx.try_recv().is_err(), "coalesced wakeups must not stack");
        proxy.0.wakeup_pending.store(false, Ordering::Relaxed); // ack
        proxy.0.send_wakeup();
        assert!(matches!(rx.try_recv(), Ok((_, RuntimeEvent::Wakeup))));
    }

    #[test]
    fn paste_marker_stripping() {
        assert_eq!(
            strip_paste_markers(b"\x1b[200~hello\x1b[201~"),
            b"hello".to_vec()
        );
        assert_eq!(strip_paste_markers(b"plain"), b"plain".to_vec());
        // Other escapes untouched.
        assert_eq!(
            strip_paste_markers(b"\x1b[31mred\x1b[200~x\x1b[201~"),
            b"\x1b[31mredx".to_vec()
        );
        // Truncated marker at end survives (documented limitation).
        assert_eq!(strip_paste_markers(b"\x1b[200"), b"\x1b[200".to_vec());
    }
}
