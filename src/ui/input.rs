//! Minimal input-byte parser for vag's OWN chrome (dashboard/sidebar/
//! prompts). Session panes get raw bytes verbatim — this parser is never in
//! that path.
//!
//! CONTRACT:
//! - `Parser::feed(&mut self, bytes) -> Vec<Key>`: incremental, keeps state
//!   across reads (escape sequences can split across read() chunks).
//! - Recognize: printable UTF-8 chars, Enter (CR), Tab, Backspace (0x7f;
//!   0x08 is Ctrl('h') — text editors alias it to backspace, the tree uses
//!   it for pane navigation), Esc, Ctrl+letter bytes, arrows (CSI A/B/C/D,
//!   SS3 variants),
//!   Home/End (CSI H/F, 1~/4~, 7~/8~), PageUp/Down (5~/6~), Delete (3~),
//!   Shift+Tab (CSI Z). Everything else (unknown CSI/OSC/SS3) is consumed
//!   and dropped silently — never leaks as garbage chars.
//! - Bracketed paste (host enables it): ESC[200~ ... ESC[201~ becomes
//!   `Key::Paste(String)` (lossy UTF-8).
//! - Lone-ESC disambiguation: an ESC at the END of a chunk is held with a
//!   timestamp; the caller ticks `flush_pending_esc()`, which releases it as
//!   Key::Esc only once ESC_QUIET (~50ms) of quiet has elapsed. ESC followed
//!   by a printable char in the SAME chunk = Alt+char (`Key::Alt(char)`); a
//!   printable arriving in a LATER chunk is a separate keystroke and yields
//!   Key::Esc + the char. A held ESC still joins genuine escape-sequence
//!   continuations (`[`, `O`, string intros, ESC) from a later chunk, so
//!   sequences split across reads stay intact.

use std::time::{Duration, Instant};

/// A parsed SGR mouse report (`ESC [ < btn ; col ; row M|m`). vag only ever
/// enables SGR encoding (DECSET 1006) on the host, so this is the sole wire
/// format the gate has to understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Raw SGR button code (low 2 bits = button, 4/8/16 = modifiers,
    /// 32 = motion, 64 = wheel).
    pub btn: u16,
    /// 1-based host-terminal cell coordinates.
    pub col: u16,
    pub row: u16,
    /// true for press/wheel (`M`), false for release (`m`).
    pub press: bool,
}

impl MouseEvent {
    /// +1 = wheel up (back in history), -1 = wheel down, regardless of
    /// modifier bits. Wheel left/right (btn&3 == 2/3) is not a scroll.
    pub fn wheel(&self) -> Option<i32> {
        match (self.btn & 64 != 0, self.btn & 3) {
            (true, 0) => Some(1),
            (true, 1) => Some(-1),
            _ => None,
        }
    }

    /// Motion flag: drag (with MOUSE_DRAG) or plain movement (MOUSE_MOTION).
    pub fn is_motion(&self) -> bool {
        self.btn & 64 == 0 && self.btn & 32 != 0
    }

    /// Left button press (not motion, not wheel): the "click" chrome acts on.
    pub fn is_left_press(&self) -> bool {
        self.press && self.btn & (64 | 32) == 0 && self.btn & 3 == 0
    }

    /// Re-encode as an SGR report with translated (1-based) coordinates,
    /// for forwarding into a child that enabled SGR mouse mode itself.
    pub fn encode_sgr(&self, col: u16, row: u16) -> Vec<u8> {
        let fin = if self.press { 'M' } else { 'm' };
        format!("\x1b[<{};{};{}{}", self.btn, col, row, fin).into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Alt(char),
    Ctrl(char), // 'a'..='z'
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Paste(String),
}

const ESC: u8 = 0x1b;
const PASTE_END: &[u8] = b"\x1b[201~";
/// Quiet period before a held trailing ESC is released as a lone Key::Esc.
/// Callers tick `flush_pending_esc()` (every ~100ms), so the effective
/// release latency is ESC_QUIET..ESC_QUIET+tick.
const ESC_QUIET: Duration = Duration::from_millis(50);
/// Longest CSI parameter run we keep; longer sequences are still consumed
/// (and dropped) but their params are ignored.
const CSI_MAX: usize = 32;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    /// Saw ESC; next byte decides (CSI/SS3/OSC/Alt+char/lone Esc).
    Esc,
    Csi,
    Ss3,
    /// OSC or DCS/SOS/PM/APC string: consumed until BEL or ST (ESC \).
    Str {
        esc: bool,
    },
    /// Collecting a UTF-8 multibyte char.
    Utf8 {
        need: u8,
    },
    /// Inside bracketed paste, scanning for ESC[201~.
    Paste,
}

#[derive(Debug, Default)]
pub struct Parser {
    state: State,
    utf8: Vec<u8>,
    csi: Vec<u8>,
    paste: Vec<u8>,
    /// Bytes of PASTE_END matched so far at the tail of the paste stream.
    paste_end_matched: usize,
    /// When the parser last entered State::Esc (quiet-period timing).
    esc_since: Option<Instant>,
    /// True when the pending ESC was left over from a previous feed() chunk:
    /// a printable then means two keystrokes (Esc + char), never Alt+char.
    esc_cross_chunk: bool,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        for &b in bytes {
            self.step(b, &mut out);
        }
        // An ESC still pending when the chunk ends came from its own read;
        // a printable in a later chunk must not join it into Alt+char.
        self.esc_cross_chunk = self.state == State::Esc;
        out
    }

    /// Release a held trailing ESC as Key::Esc, but only once it has been
    /// pending for at least ESC_QUIET. Callers tick this periodically;
    /// before the quiet period elapses it returns None.
    pub fn flush_pending_esc(&mut self) -> Option<Key> {
        if self.state == State::Esc && self.esc_since.is_none_or(|t| t.elapsed() >= ESC_QUIET) {
            self.reset_esc();
            Some(Key::Esc)
        } else {
            None
        }
    }

    /// Enter State::Esc, stamping the hold timestamp for quiet-period
    /// timing. The ESC is same-chunk until feed() returns with it pending.
    fn enter_esc(&mut self) {
        self.state = State::Esc;
        self.esc_since = Some(Instant::now());
        self.esc_cross_chunk = false;
    }

    fn reset_esc(&mut self) {
        self.state = State::Ground;
        self.esc_since = None;
        self.esc_cross_chunk = false;
    }

    fn step(&mut self, b: u8, out: &mut Vec<Key>) {
        match self.state {
            State::Ground => self.step_ground(b, out),
            State::Esc => self.step_esc(b, out),
            State::Csi => self.step_csi(b, out),
            State::Ss3 => self.step_ss3(b, out),
            State::Str { esc } => self.step_str(b, esc),
            State::Utf8 { need } => self.step_utf8(b, need, out),
            State::Paste => self.step_paste(b, out),
        }
    }

    fn step_ground(&mut self, b: u8, out: &mut Vec<Key>) {
        match b {
            ESC => self.enter_esc(),
            b'\r' | b'\n' => out.push(Key::Enter),
            b'\t' => out.push(Key::Tab),
            0x7f => out.push(Key::Backspace),
            // C0 control letters (ctrl-a .. ctrl-z). 0x08 lands here as
            // Ctrl('h') on purpose: the tree needs to tell ctrl-h (pane
            // navigation) apart from Backspace; every text-editing consumer
            // (LineEdit, editbuf) aliases Ctrl('h') back to backspace, so
            // legacy backspace-sends-^H terminals keep working.
            0x01..=0x1a => out.push(Key::Ctrl((b'a' + b - 1) as char)),
            // NUL and the other C0 controls: dropped.
            0x00 | 0x1c..=0x1f => {}
            0x20..=0x7e => out.push(Key::Char(b as char)),
            // UTF-8 lead byte.
            0xc2..=0xdf => self.start_utf8(b, 2),
            0xe0..=0xef => self.start_utf8(b, 3),
            0xf0..=0xf4 => self.start_utf8(b, 4),
            // Stray continuation / invalid lead byte: dropped.
            _ => {}
        }
    }

    fn start_utf8(&mut self, b: u8, need: u8) {
        self.utf8.clear();
        self.utf8.push(b);
        self.state = State::Utf8 { need };
    }

    fn step_utf8(&mut self, b: u8, need: u8, out: &mut Vec<Key>) {
        if (0x80..=0xbf).contains(&b) {
            self.utf8.push(b);
            if self.utf8.len() == need as usize {
                if let Ok(s) = std::str::from_utf8(&self.utf8)
                    && let Some(c) = s.chars().next()
                {
                    out.push(Key::Char(c));
                }
                self.state = State::Ground;
            }
        } else {
            // Malformed sequence: drop it and reprocess this byte fresh.
            self.state = State::Ground;
            self.step(b, out);
        }
    }

    fn step_esc(&mut self, b: u8, out: &mut Vec<Key>) {
        // An ESC held over from a previous chunk only joins genuine escape-
        // sequence continuations; any other byte is a separate keystroke, so
        // release the Esc and reprocess the byte in Ground (never Alt+char).
        if self.esc_cross_chunk
            && !matches!(b, b'[' | b'O' | b']' | b'P' | b'X' | b'^' | b'_' | ESC)
        {
            out.push(Key::Esc);
            self.reset_esc();
            self.step(b, out);
            return;
        }
        match b {
            b'[' => {
                self.csi.clear();
                self.state = State::Csi;
            }
            b'O' => self.state = State::Ss3,
            // OSC and DCS/SOS/PM/APC strings: consume until terminator.
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::Str { esc: false },
            // ESC ESC: emit one Esc; the second starts a NEW pending escape,
            // so its quiet-period timestamp is refreshed.
            ESC => {
                out.push(Key::Esc);
                self.enter_esc();
            }
            // Alt+printable (same-chunk only, per the cross-chunk gate above).
            0x20..=0x7e => {
                out.push(Key::Alt(b as char));
                self.state = State::Ground;
            }
            // Anything else: the ESC stands alone; reprocess the byte.
            _ => {
                out.push(Key::Esc);
                self.state = State::Ground;
                self.step(b, out);
            }
        }
    }

    fn step_csi(&mut self, b: u8, out: &mut Vec<Key>) {
        match b {
            // Parameter and intermediate bytes (excess params fall through
            // to `_` below: consumed but ignored).
            0x20..=0x3f if self.csi.len() < CSI_MAX => self.csi.push(b),
            // Final byte.
            0x40..=0x7e => {
                self.state = State::Ground;
                self.finish_csi(b, out);
            }
            // ESC aborts the malformed sequence and starts a fresh pending
            // escape (with a fresh quiet-period timestamp).
            ESC => self.enter_esc(),
            // Other C0 bytes inside CSI: ignored.
            _ => {}
        }
    }

    fn finish_csi(&mut self, final_byte: u8, out: &mut Vec<Key>) {
        // Private-parameter sequences (e.g. kitty `CSI ? .. u`) are ignored.
        if self
            .csi
            .first()
            .is_some_and(|&c| matches!(c, b'?' | b'<' | b'=' | b'>'))
        {
            return;
        }
        match final_byte {
            b'A' => out.push(Key::Up),
            b'B' => out.push(Key::Down),
            b'C' => out.push(Key::Right),
            b'D' => out.push(Key::Left),
            b'H' => out.push(Key::Home),
            b'F' => out.push(Key::End),
            b'Z' => out.push(Key::BackTab),
            b'~' => {
                let first: u32 = self
                    .csi
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .fold(0u32, |acc, &c| {
                        acc.saturating_mul(10).saturating_add((c - b'0') as u32)
                    });
                match first {
                    1 | 7 => out.push(Key::Home),
                    4 | 8 => out.push(Key::End),
                    3 => out.push(Key::Delete),
                    5 => out.push(Key::PageUp),
                    6 => out.push(Key::PageDown),
                    200 => {
                        self.paste.clear();
                        self.paste_end_matched = 0;
                        self.state = State::Paste;
                    }
                    // 2 (insert), function keys, stray 201: dropped.
                    _ => {}
                }
            }
            // Unknown finals (SGR replies, mouse, etc.): dropped.
            _ => {}
        }
    }

    fn step_ss3(&mut self, b: u8, out: &mut Vec<Key>) {
        // SS3 has exactly one final byte, but tolerate an aborting ESC
        // (fresh pending escape, fresh quiet-period timestamp).
        if b == ESC {
            self.enter_esc();
            return;
        }
        self.state = State::Ground;
        match b {
            b'A' => out.push(Key::Up),
            b'B' => out.push(Key::Down),
            b'C' => out.push(Key::Right),
            b'D' => out.push(Key::Left),
            b'H' => out.push(Key::Home),
            b'F' => out.push(Key::End),
            // F1-F4 and anything else: dropped.
            _ => {}
        }
    }

    fn step_str(&mut self, b: u8, esc: bool) {
        match (esc, b) {
            // BEL terminator (common for OSC).
            (_, 0x07) => self.state = State::Ground,
            (true, b'\\') => self.state = State::Ground, // ST
            (_, ESC) => self.state = State::Str { esc: true },
            _ => self.state = State::Str { esc: false },
        }
    }

    fn step_paste(&mut self, b: u8, out: &mut Vec<Key>) {
        if b == PASTE_END[self.paste_end_matched] {
            self.paste_end_matched += 1;
            if self.paste_end_matched == PASTE_END.len() {
                let text = String::from_utf8_lossy(&self.paste).into_owned();
                out.push(Key::Paste(text));
                self.paste.clear();
                self.paste_end_matched = 0;
                self.state = State::Ground;
            }
        } else {
            // A partial terminator match turned out to be paste content.
            if self.paste_end_matched > 0 {
                let matched = self.paste_end_matched;
                self.paste_end_matched = 0;
                self.paste.extend_from_slice(&PASTE_END[..matched]);
                // Reprocess: this byte may itself start a new match.
                self.step_paste(b, out);
            } else {
                self.paste.push(b);
            }
        }
    }
}

/// Longest sane SGR mouse report: `ESC [ <` + 3 params + final ≈ 20 bytes.
/// Anything that grows past this is not a mouse report — flush it verbatim.
const MOUSE_MAX: usize = 24;
const PASTE_START: &[u8] = b"\x1b[200~";

/// Extracts SGR mouse reports (`ESC [ < b ; x ; y M|m`) from the raw stdin
/// stream, passing every other byte through VERBATIM. Runs BEFORE both input
/// paths (chrome parser and raw pane forwarding), so mouse bytes never leak
/// into a child as garbage and never reach the chrome parser at all.
///
/// Byte-perfect passthrough rules:
/// - Only an unambiguous mouse prefix (`ESC [ <` …) is ever held across
///   chunk boundaries. A chunk ending in a bare ESC or `ESC [` is flushed
///   as-is: those are (or start) real keystrokes that must not be delayed,
///   and mouse reports arrive atomically from the terminal driver, so a
///   report torn exactly there is vanishingly rare.
/// - Host bracketed paste suspends extraction: pasted content that happens
///   to contain literal mouse-report bytes stays content. (A paste-start
///   marker split across reads evades this detection — accepted, the
///   double-rarity makes it noise.)
#[derive(Debug, Default)]
pub struct MouseGate {
    /// Held candidate bytes: a strict prefix of `ESC [ <…` (cross-chunk) or
    /// an in-chunk `ESC…` prefix still being classified.
    partial: Vec<u8>,
    /// Inside host bracketed paste (passthrough until PASTE_END).
    in_paste: bool,
    /// Bytes of PASTE_END matched at the tail of the passthrough stream.
    paste_end_matched: usize,
}

impl MouseGate {
    /// Split `bytes` into extracted mouse events and the passthrough rest.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<MouseEvent>, Vec<u8>) {
        let mut events = Vec::new();
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.step(b, &mut events, &mut out);
        }
        // Chunk boundary: keep only an unambiguous mouse prefix pending.
        if !self.partial.is_empty() && !self.partial.starts_with(b"\x1b[<") {
            out.extend_from_slice(&self.partial);
            self.partial.clear();
        }
        (events, out)
    }

    /// Drop any held state (pane switch / zoom transitions).
    pub fn reset(&mut self) {
        self.partial.clear();
        self.in_paste = false;
        self.paste_end_matched = 0;
    }

    fn step(&mut self, b: u8, events: &mut Vec<MouseEvent>, out: &mut Vec<u8>) {
        if self.in_paste {
            out.push(b);
            self.paste_end_matched = if b == PASTE_END[self.paste_end_matched] {
                self.paste_end_matched + 1
            } else if b == PASTE_END[0] {
                1
            } else {
                0
            };
            if self.paste_end_matched == PASTE_END.len() {
                self.in_paste = false;
                self.paste_end_matched = 0;
            }
            return;
        }
        if self.partial.is_empty() {
            if b == ESC {
                self.partial.push(b);
            } else {
                out.push(b);
            }
            return;
        }
        // Growing a candidate. Valid continuations: "[", then "<", then
        // params/final. Paste-start detection rides along: PASTE_START also
        // begins ESC [ — match it before giving up on non-mouse sequences.
        self.partial.push(b);
        let p = &self.partial[..];
        let still_mouse = match p.len() {
            2 => b == b'[',
            3 => b == b'<' || PASTE_START.starts_with(p),
            _ if p.starts_with(b"\x1b[<") => {
                if matches!(b, b'M' | b'm') {
                    let ev = parse_sgr_body(&p[3..p.len() - 1], b == b'M');
                    match ev {
                        Some(ev) => events.push(ev),
                        // Malformed params: pass the whole thing through.
                        None => out.extend_from_slice(p),
                    }
                    self.partial.clear();
                    return;
                }
                (b.is_ascii_digit() || b == b';') && p.len() < MOUSE_MAX
            }
            _ if PASTE_START.starts_with(p) => {
                if p == PASTE_START {
                    out.extend_from_slice(p);
                    self.partial.clear();
                    self.in_paste = true;
                    return;
                }
                true
            }
            _ => false,
        };
        if !still_mouse {
            // ESC aborting the candidate starts a NEW candidate; everything
            // else flushes verbatim (order preserved: candidate then byte).
            let flushed: Vec<u8> = self.partial.drain(..).collect();
            if b == ESC {
                out.extend_from_slice(&flushed[..flushed.len() - 1]);
                self.partial.push(ESC);
            } else {
                out.extend_from_slice(&flushed);
            }
        }
    }
}

/// Parse `b;x;y` from an SGR report body (final byte already stripped).
fn parse_sgr_body(body: &[u8], press: bool) -> Option<MouseEvent> {
    let mut it = body.split(|&c| c == b';');
    let num = |it: &mut dyn Iterator<Item = &[u8]>| -> Option<u16> {
        let part = it.next()?;
        if part.is_empty() || part.len() > 5 {
            return None;
        }
        let mut v: u32 = 0;
        for &c in part {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as u32;
        }
        u16::try_from(v).ok()
    };
    let btn = num(&mut it)?;
    let col = num(&mut it)?;
    let row = num(&mut it)?;
    if it.next().is_some() {
        return None;
    }
    Some(MouseEvent {
        btn,
        col,
        row,
        press,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_one(bytes: &[u8]) -> Vec<Key> {
        Parser::new().feed(bytes)
    }

    /// Sleep past ESC_QUIET so a held ESC becomes flushable.
    fn wait_quiet() {
        std::thread::sleep(ESC_QUIET + Duration::from_millis(10));
    }

    #[test]
    fn printable_ascii_and_basics() {
        assert_eq!(
            feed_one(b"ab Z"),
            vec![
                Key::Char('a'),
                Key::Char('b'),
                Key::Char(' '),
                Key::Char('Z')
            ]
        );
        assert_eq!(feed_one(b"\r"), vec![Key::Enter]);
        assert_eq!(feed_one(b"\n"), vec![Key::Enter]);
        assert_eq!(feed_one(b"\t"), vec![Key::Tab]);
        assert_eq!(feed_one(b"\x7f"), vec![Key::Backspace]);
        // 0x08 is Ctrl('h'), NOT Backspace: the tree distinguishes them for
        // pane navigation; text editors alias Ctrl('h') to backspace.
        assert_eq!(feed_one(b"\x08"), vec![Key::Ctrl('h')]);
    }

    #[test]
    fn ctrl_letters() {
        assert_eq!(feed_one(b"\x01"), vec![Key::Ctrl('a')]);
        assert_eq!(feed_one(b"\x03"), vec![Key::Ctrl('c')]);
        assert_eq!(feed_one(b"\x11"), vec![Key::Ctrl('q')]);
        assert_eq!(feed_one(b"\x1a"), vec![Key::Ctrl('z')]);
    }

    #[test]
    fn arrows_csi_and_ss3() {
        assert_eq!(feed_one(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(feed_one(b"\x1b[B"), vec![Key::Down]);
        assert_eq!(feed_one(b"\x1b[C"), vec![Key::Right]);
        assert_eq!(feed_one(b"\x1b[D"), vec![Key::Left]);
        assert_eq!(feed_one(b"\x1bOA"), vec![Key::Up]);
        assert_eq!(feed_one(b"\x1bOD"), vec![Key::Left]);
    }

    #[test]
    fn arrows_with_modifier_params() {
        // ctrl+right etc: modifiers ignored, base key kept.
        assert_eq!(feed_one(b"\x1b[1;5C"), vec![Key::Right]);
        assert_eq!(feed_one(b"\x1b[1;2A"), vec![Key::Up]);
    }

    #[test]
    fn home_end_variants() {
        for seq in [&b"\x1b[H"[..], b"\x1b[1~", b"\x1b[7~", b"\x1bOH"] {
            assert_eq!(feed_one(seq), vec![Key::Home], "{seq:?}");
        }
        for seq in [&b"\x1b[F"[..], b"\x1b[4~", b"\x1b[8~", b"\x1bOF"] {
            assert_eq!(feed_one(seq), vec![Key::End], "{seq:?}");
        }
    }

    #[test]
    fn page_delete_backtab() {
        assert_eq!(feed_one(b"\x1b[5~"), vec![Key::PageUp]);
        assert_eq!(feed_one(b"\x1b[6~"), vec![Key::PageDown]);
        assert_eq!(feed_one(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(feed_one(b"\x1b[Z"), vec![Key::BackTab]);
        // Delete with modifier params.
        assert_eq!(feed_one(b"\x1b[3;5~"), vec![Key::Delete]);
    }

    #[test]
    fn unknown_sequences_dropped_silently() {
        assert_eq!(feed_one(b"\x1b[38;5;10m"), vec![]);
        assert_eq!(feed_one(b"\x1b[?25l"), vec![]);
        assert_eq!(feed_one(b"\x1b[?1u"), vec![]); // kitty report
        assert_eq!(feed_one(b"\x1b[2~"), vec![]); // insert: unmapped
        assert_eq!(feed_one(b"\x1b[15~"), vec![]); // F5
        assert_eq!(feed_one(b"\x1bOP"), vec![]); // F1
        assert_eq!(feed_one(b"\x1b]0;title\x07"), vec![]); // OSC + BEL
        assert_eq!(feed_one(b"\x1b]0;title\x1b\\"), vec![]); // OSC + ST
        assert_eq!(feed_one(b"\x1bP1$r0m\x1b\\"), vec![]); // DCS + ST
        // Parser recovers to Ground afterwards.
        let mut p = Parser::new();
        p.feed(b"\x1b[999X");
        assert_eq!(p.feed(b"a"), vec![Key::Char('a')]);
    }

    #[test]
    fn utf8_multibyte() {
        assert_eq!(feed_one("é".as_bytes()), vec![Key::Char('é')]);
        assert_eq!(
            feed_one("你好".as_bytes()),
            vec![Key::Char('你'), Key::Char('好')]
        );
        assert_eq!(feed_one("🎉".as_bytes()), vec![Key::Char('🎉')]);
    }

    #[test]
    fn utf8_split_across_feeds() {
        let mut p = Parser::new();
        let bytes = "你".as_bytes(); // 3 bytes
        assert_eq!(p.feed(&bytes[..1]), vec![]);
        assert_eq!(p.feed(&bytes[1..2]), vec![]);
        assert_eq!(p.feed(&bytes[2..]), vec![Key::Char('你')]);
    }

    #[test]
    fn invalid_utf8_dropped_and_recovers() {
        // Lead byte followed by ASCII: sequence dropped, ASCII kept.
        assert_eq!(feed_one(b"\xe4a"), vec![Key::Char('a')]);
        // Stray continuation bytes: dropped.
        assert_eq!(feed_one(b"\x80\xbfx"), vec![Key::Char('x')]);
    }

    #[test]
    fn alt_char() {
        assert_eq!(feed_one(b"\x1ba"), vec![Key::Alt('a')]);
        assert_eq!(feed_one(b"\x1bx\x1bZ"), vec![Key::Alt('x'), Key::Alt('Z')]);
    }

    #[test]
    fn lone_esc_held_then_flushed_after_quiet_period() {
        let mut p = Parser::new();
        assert_eq!(p.feed(b"a\x1b"), vec![Key::Char('a')]);
        // Not released before the quiet period elapses.
        assert_eq!(p.flush_pending_esc(), None);
        wait_quiet();
        assert_eq!(p.flush_pending_esc(), Some(Key::Esc));
        assert_eq!(p.flush_pending_esc(), None);
        assert_eq!(p.feed(b"b"), vec![Key::Char('b')]);
    }

    #[test]
    fn held_esc_joins_next_chunk_sequences_only() {
        // ESC at end of one chunk + "[A" in the next = one Up key.
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b"), vec![]);
        assert_eq!(p.feed(b"[A"), vec![Key::Up]);
        // A printable in a LATER chunk is a separate keystroke, not Alt+char.
        assert_eq!(p.feed(b"\x1b"), vec![]);
        assert_eq!(p.feed(b"q"), vec![Key::Esc, Key::Char('q')]);
    }

    #[test]
    fn split_arrow_joins_even_after_quiet_period() {
        // Ticks somehow didn't flush: a late "[A" must still join, never
        // decay into Esc + Char('[') + Char('A') (archive-modal hazard).
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b"), vec![]);
        wait_quiet();
        assert_eq!(p.feed(b"[A"), vec![Key::Up]);
    }

    #[test]
    fn esc_then_char_after_quiet_period_is_two_keys() {
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b"), vec![]);
        wait_quiet();
        assert_eq!(p.feed(b"j"), vec![Key::Esc, Key::Char('j')]);
    }

    #[test]
    fn esc_then_char_cross_chunk_within_window_is_two_keys() {
        // Two fast keystrokes (< quiet period apart, separate reads) must
        // never fuse into Alt+char regardless of tick phase.
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b"), vec![]);
        assert_eq!(p.flush_pending_esc(), None); // tick inside the window
        assert_eq!(p.feed(b"j"), vec![Key::Esc, Key::Char('j')]);
    }

    #[test]
    fn esc_then_char_same_feed_is_alt() {
        assert_eq!(feed_one(b"\x1bj"), vec![Key::Alt('j')]);
    }

    #[test]
    fn esc_esc() {
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b\x1b"), vec![Key::Esc]);
        // The second ESC starts a fresh quiet period.
        assert_eq!(p.flush_pending_esc(), None);
        wait_quiet();
        assert_eq!(p.flush_pending_esc(), Some(Key::Esc));
    }

    #[test]
    fn cross_chunk_esc_esc_refreshes_quiet_period() {
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b"), vec![]);
        wait_quiet();
        // Second ESC in a later chunk: first Esc released, second held anew.
        assert_eq!(p.feed(b"\x1b"), vec![Key::Esc]);
        assert_eq!(p.flush_pending_esc(), None);
        wait_quiet();
        assert_eq!(p.flush_pending_esc(), Some(Key::Esc));
    }

    #[test]
    fn esc_aborting_csi_gets_fresh_quiet_period() {
        let mut p = Parser::new();
        // ESC aborts the malformed CSI and is held as a new pending escape.
        assert_eq!(p.feed(b"\x1b[\x1b"), vec![]);
        assert_eq!(p.flush_pending_esc(), None);
        wait_quiet();
        assert_eq!(p.flush_pending_esc(), Some(Key::Esc));
    }

    #[test]
    fn esc_before_control_byte() {
        // ESC then CR: Esc stands alone, CR processed normally.
        assert_eq!(feed_one(b"\x1b\r"), vec![Key::Esc, Key::Enter]);
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(
            feed_one(b"\x1b[200~hello world\x1b[201~"),
            vec![Key::Paste("hello world".to_string())]
        );
    }

    #[test]
    fn bracketed_paste_split_across_feeds() {
        let mut p = Parser::new();
        assert_eq!(p.feed(b"\x1b[200~hel"), vec![]);
        assert_eq!(p.feed(b"lo"), vec![]);
        // Terminator itself split mid-sequence.
        assert_eq!(p.feed(b"\x1b[20"), vec![]);
        assert_eq!(
            p.feed(b"1~x"),
            vec![Key::Paste("hello".to_string()), Key::Char('x')]
        );
    }

    #[test]
    fn paste_containing_escapes() {
        // ESC inside paste content that is not the terminator stays content.
        assert_eq!(
            feed_one(b"\x1b[200~a\x1bb\x1b[Ac\x1b[201~"),
            vec![Key::Paste("a\u{1b}b\u{1b}[Ac".to_string())]
        );
    }

    #[test]
    fn paste_with_partial_terminator_content() {
        // "\x1b[20" inside content, then real terminator.
        assert_eq!(
            feed_one(b"\x1b[200~x\x1b[20y\x1b[201~"),
            vec![Key::Paste("x\u{1b}[20y".to_string())]
        );
        // Partial match immediately followed by a fresh full terminator.
        assert_eq!(
            feed_one(b"\x1b[200~x\x1b[201\x1b[201~"),
            vec![Key::Paste("x\u{1b}[201".to_string())]
        );
    }

    #[test]
    fn paste_multibyte_lossy() {
        assert_eq!(
            feed_one("\x1b[200~héllo 你\x1b[201~".as_bytes()),
            vec![Key::Paste("héllo 你".to_string())]
        );
        // Invalid UTF-8 in paste → lossy replacement, never a panic.
        let keys = feed_one(b"\x1b[200~a\xffb\x1b[201~");
        assert_eq!(keys.len(), 1);
        assert!(matches!(&keys[0], Key::Paste(s) if s.starts_with('a') && s.ends_with('b')));
    }

    #[test]
    fn stray_paste_end_dropped() {
        assert_eq!(feed_one(b"\x1b[201~"), vec![]);
    }

    #[test]
    fn mixed_stream() {
        assert_eq!(
            feed_one(b"j\x1b[Bk\rq"),
            vec![
                Key::Char('j'),
                Key::Down,
                Key::Char('k'),
                Key::Enter,
                Key::Char('q')
            ]
        );
    }

    #[test]
    fn overlong_csi_still_consumed() {
        let mut seq = b"\x1b[".to_vec();
        seq.extend(std::iter::repeat_n(b'1', 100));
        seq.push(b'm');
        seq.push(b'z');
        assert_eq!(feed_one(&seq), vec![Key::Char('z')]);
    }

    // ---------- MouseGate ----------

    fn gate_one(bytes: &[u8]) -> (Vec<MouseEvent>, Vec<u8>) {
        MouseGate::default().feed(bytes)
    }

    #[test]
    fn mouse_gate_extracts_report_and_passes_rest_verbatim() {
        let (evs, rest) = gate_one(b"ab\x1b[<64;10;5Mcd");
        assert_eq!(
            evs,
            vec![MouseEvent {
                btn: 64,
                col: 10,
                row: 5,
                press: true
            }]
        );
        assert_eq!(rest, b"abcd");
    }

    #[test]
    fn mouse_gate_release_report() {
        let (evs, rest) = gate_one(b"\x1b[<0;3;4m");
        assert_eq!(
            evs,
            vec![MouseEvent {
                btn: 0,
                col: 3,
                row: 4,
                press: false
            }]
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn mouse_gate_report_split_across_chunks() {
        let mut g = MouseGate::default();
        let (evs, rest) = g.feed(b"x\x1b[<6");
        assert!(evs.is_empty());
        assert_eq!(rest, b"x", "unambiguous mouse prefix is held");
        let (evs, rest) = g.feed(b"5;7;8My");
        assert_eq!(
            evs,
            vec![MouseEvent {
                btn: 65,
                col: 7,
                row: 8,
                press: true
            }]
        );
        assert_eq!(rest, b"y");
    }

    #[test]
    fn mouse_gate_keyboard_sequences_pass_untouched() {
        for seq in [
            &b"\x1b[A"[..],
            b"\x1b[5~",
            b"\x1b[1;5C",
            b"\x1bOA",
            b"\x1bq",
            b"\x1b\x1b",
            b"plain text",
        ] {
            let (evs, rest) = gate_one(seq);
            assert!(evs.is_empty(), "{seq:?}");
            assert_eq!(rest, seq, "byte-perfect passthrough for {seq:?}");
        }
    }

    #[test]
    fn mouse_gate_lone_esc_not_delayed() {
        // A chunk ending in bare ESC (or ESC [) flushes immediately — the
        // chrome parser owns lone-ESC disambiguation, not the gate.
        let mut g = MouseGate::default();
        let (_, rest) = g.feed(b"\x1b");
        assert_eq!(rest, b"\x1b");
        let (_, rest) = g.feed(b"\x1b[");
        assert_eq!(rest, b"\x1b[");
    }

    #[test]
    fn mouse_gate_paste_content_is_immune() {
        let seq = b"\x1b[200~a\x1b[<64;1;1Mb\x1b[201~\x1b[<64;1;1M";
        let (evs, rest) = gate_one(seq);
        // Only the report AFTER the paste closes is extracted.
        assert_eq!(evs.len(), 1);
        assert_eq!(rest, b"\x1b[200~a\x1b[<64;1;1Mb\x1b[201~");
    }

    #[test]
    fn mouse_gate_malformed_report_passes_verbatim() {
        for seq in [&b"\x1b[<64;;5M"[..], b"\x1b[<64;10M", b"\x1b[<;1;1M"] {
            let (evs, rest) = gate_one(seq);
            assert!(evs.is_empty(), "{seq:?}");
            assert_eq!(rest, seq);
        }
    }

    #[test]
    fn mouse_gate_overlong_candidate_flushes() {
        let mut seq = b"\x1b[<".to_vec();
        seq.extend(std::iter::repeat_n(b'1', 40));
        let (evs, rest) = gate_one(&seq);
        assert!(evs.is_empty());
        assert_eq!(rest, seq);
    }

    #[test]
    fn mouse_event_classification() {
        let ev = |btn| MouseEvent {
            btn,
            col: 1,
            row: 1,
            press: true,
        };
        assert_eq!(ev(64).wheel(), Some(1)); // wheel up
        assert_eq!(ev(65).wheel(), Some(-1)); // wheel down
        assert_eq!(ev(68).wheel(), Some(1)); // shift+wheel up
        assert_eq!(ev(66).wheel(), None); // wheel left
        assert_eq!(ev(0).wheel(), None);
        assert!(ev(0).is_left_press());
        assert!(!ev(1).is_left_press()); // middle
        assert!(!ev(32).is_left_press()); // drag motion
        assert!(ev(32).is_motion());
        assert!(!ev(64).is_motion());
        let rel = MouseEvent {
            btn: 0,
            col: 1,
            row: 1,
            press: false,
        };
        assert!(!rel.is_left_press());
        assert_eq!(ev(65).encode_sgr(12, 3), b"\x1b[<65;12;3M");
        assert_eq!(rel.encode_sgr(1, 1), b"\x1b[<0;1;1m");
    }
}
