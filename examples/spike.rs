//! M0 fidelity spike: prove the PTY → alacritty_terminal → ratatui pane
//! embed path end to end against the REAL claude/codex CLIs.
//!
//!   cargo run --example spike -- [--headless SECS] [--log FILE]
//!                                [--cols N --rows N] -- <command> [args...]
//!
//! Interactive (default): raw mode + alt screen, 30-col dummy sidebar +
//! pane running <command>. Stdin bytes are forwarded raw except ctrl-q
//! (0x11) which exits cleanly. SIGWINCH relayouts and resizes the PTY.
//!
//! --headless SECS: no terminal takeover; spawn at --cols/--rows (default
//! 100x30), pump for SECS seconds, then print the final grid as plain text
//! between ---SCREEN--- / ---END--- markers plus a stats line. --log FILE
//! appends every raw PTY chunk (binary) for startup-sequence analysis.
//!
//! Exit code 0 in both modes unless spawn/setup failed.
//!
//! NOTE: the crate is a binary (no lib target), so the modules under test
//! are pulled in via #[path]. `actions` is stubbed to just SpawnSpec — the
//! runtime only needs that one type, and this keeps the spike decoupled
//! from the real argv builders.

#![allow(dead_code)]

#[path = "../src/types.rs"]
mod types;

/// Mirror of `src/actions.rs::SpawnSpec` (the only piece of `actions` the
/// runtime depends on).
mod actions {
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    pub struct SpawnSpec {
        pub program: String,
        pub args: Vec<String>,
        pub cwd: PathBuf,
        /// Extra/override environment (name, value) pairs; parent env inherited.
        pub env: Vec<(String, String)>,
    }
}

#[path = "../src/runtime/mod.rs"]
mod runtime;

#[path = "../src/ui/pane.rs"]
mod pane;

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, unbounded};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};

use actions::SpawnSpec;
use runtime::{PaneSize, RuntimeEvent, SessionRuntime};
use types::{AgentKind, SessionKey};

const SIDEBAR_WIDTH: u16 = 30;
const CTRL_Q: u8 = 0x11;

struct Opts {
    headless: Option<u64>,
    log: Option<PathBuf>,
    cols: u16,
    rows: u16,
    cmd: Vec<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: spike [--headless SECS] [--log FILE] [--cols N --rows N] -- <command> [args...]"
    );
    exit(2);
}

fn parse_args() -> Opts {
    let mut opts = Opts {
        headless: None,
        log: None,
        cols: 100,
        rows: 30,
        cmd: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--headless" => match args.next().and_then(|v| v.parse().ok()) {
                Some(secs) => opts.headless = Some(secs),
                None => usage(),
            },
            "--log" => match args.next() {
                Some(f) => opts.log = Some(PathBuf::from(f)),
                None => usage(),
            },
            "--cols" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => opts.cols = n,
                None => usage(),
            },
            "--rows" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => opts.rows = n,
                None => usage(),
            },
            "--" => {
                opts.cmd = args.collect();
                break;
            }
            _ => usage(),
        }
    }
    if opts.cmd.is_empty() {
        usage();
    }
    opts
}

fn main() {
    let opts = parse_args();
    let spec = SpawnSpec {
        program: opts.cmd[0].clone(),
        args: opts.cmd[1..].to_vec(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
        env: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
    };
    let result = match opts.headless {
        Some(secs) => run_headless(&spec, &opts, secs),
        None => run_interactive(&spec, &opts),
    };
    if let Err(e) = result {
        eprintln!("spike: {e:#}");
        exit(1);
    }
}

fn spawn(spec: &SpawnSpec, size: PaneSize, opts: &Opts) -> Result<(SessionRuntime, EventRx)> {
    let (tx, rx) = unbounded();
    let key = SessionKey::new(AgentKind::Claude, "spike");
    let rt = SessionRuntime::spawn(key, spec, size, tx).context("spawn failed")?;
    if let Some(path) = &opts.log {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        rt.set_raw_log(Some(file));
    }
    Ok((rt, rx))
}

type EventRx = Receiver<(SessionKey, RuntimeEvent)>;

// ---------------- headless ----------------

fn run_headless(spec: &SpawnSpec, opts: &Opts, secs: u64) -> Result<()> {
    let size = PaneSize {
        rows: opts.rows,
        cols: opts.cols,
    };
    let (mut rt, rx) = spawn(spec, size, opts)?;

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut wakeups: u64 = 0;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(100))) {
            Ok((_, RuntimeEvent::Wakeup)) => {
                wakeups += 1;
                rt.ack_wakeup();
            }
            Ok((_, RuntimeEvent::Exited(status))) => {
                eprintln!("child exited early (status {status:?})");
                break;
            }
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("---SCREEN---");
    for line in screen_text(&rt) {
        println!("{line}");
    }
    println!("---END---");
    println!(
        "bytes_received={} wakeups={} running={} title={:?}",
        rt.bytes_received(),
        wakeups,
        rt.is_running(),
        rt.title().unwrap_or_default()
    );
    rt.kill();
    Ok(())
}

/// Plain-text visible grid, trailing spaces trimmed.
fn screen_text(rt: &SessionRuntime) -> Vec<String> {
    let term = rt.term().lock();
    let mut lines = Vec::with_capacity(term.screen_lines());
    for line in 0..term.screen_lines() {
        let row = &term.grid()[Line(line as i32)];
        let mut s = String::with_capacity(term.columns());
        for col in 0..term.columns() {
            let cell = &row[Column(col)];
            if !cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                s.push(cell.c);
            }
        }
        lines.push(s.trim_end_matches(' ').to_string());
    }
    lines
}

// ---------------- interactive ----------------

/// Restores the host terminal even on early return / panic unwind.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn split(area: Rect) -> (Rect, Rect) {
    let sw = SIDEBAR_WIDTH.min(area.width);
    let sidebar = Rect::new(area.x, area.y, sw, area.height);
    let pane = Rect::new(
        area.x + sw,
        area.y,
        area.width.saturating_sub(sw),
        area.height,
    );
    (sidebar, pane)
}

fn run_interactive(spec: &SpawnSpec, opts: &Opts) -> Result<()> {
    let (width, height) = crossterm::terminal::size().context("not a terminal?")?;
    let (_, pane_area) = split(Rect::new(0, 0, width, height));
    if pane_area.width == 0 || pane_area.height == 0 {
        bail!("terminal too small ({width}x{height})");
    }

    let (mut rt, rx) = spawn(
        spec,
        PaneSize {
            rows: pane_area.height,
            cols: pane_area.width,
        },
        opts,
    )?;

    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let _guard = TermGuard;
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;

    let winch = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone())
        .context("registering SIGWINCH")?;

    // Raw stdin pump. The thread stays blocked in read() at shutdown and
    // dies with the process — fine for a spike.
    let (stdin_tx, stdin_rx) = unbounded::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut exit_reason = "detached (ctrl-q)";

    'outer: loop {
        if winch.swap(false, Ordering::Relaxed) {
            let _ = terminal.autoresize();
        }
        // Track terminal size every pass; resize is a no-op when unchanged.
        if let Ok((w, h)) = crossterm::terminal::size() {
            let (_, pane_area) = split(Rect::new(0, 0, w, h));
            rt.resize(PaneSize {
                rows: pane_area.height,
                cols: pane_area.width,
            });
        }

        terminal.draw(|f| {
            let (sidebar, pane_area) = split(f.area());
            let title = rt.title().unwrap_or_default();
            let text = format!(
                "vag fidelity spike\n\n\
                 ctrl-q  quit\n\n\
                 All other keys go to the\n\
                 embedded session on the\n\
                 right. Resize the window\n\
                 to test SIGWINCH.\n\n\
                 running: {}\n\
                 bytes: {}\n\
                 title: {}",
                rt.is_running(),
                rt.bytes_received(),
                title,
            );
            f.render_widget(
                Paragraph::new(text).block(Block::new().borders(Borders::RIGHT)),
                sidebar,
            );
            pane::render(&rt, pane_area, f.buffer_mut(), true);
        })?;

        // Wait for something to happen (or a tick to poll SIGWINCH).
        crossbeam_channel::select! {
            recv(rx) -> ev => match ev {
                Ok((_, RuntimeEvent::Wakeup)) => rt.ack_wakeup(),
                Ok((_, RuntimeEvent::Exited(_))) => {
                    exit_reason = "child exited";
                    break 'outer;
                }
                Ok(_) => {}
                Err(_) => break 'outer,
            },
            recv(stdin_rx) -> bytes => match bytes {
                Ok(bytes) => {
                    if bytes.contains(&CTRL_Q) {
                        break 'outer;
                    }
                    rt.write_input(&bytes);
                }
                Err(_) => break 'outer,
            },
            default(Duration::from_millis(50)) => {}
        }
    }

    drop(terminal);
    drop(_guard);
    rt.kill();
    eprintln!("spike: {exit_reason}");
    Ok(())
}
