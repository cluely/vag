//! Render a SessionRuntime's emulator grid into a ratatui area, and
//! serialize the grid to ANSI for zoom-mode handoff.
//!
//! CONTRACT:
//! - `render(rt, area, buf, focused)`: paint the visible grid (respecting
//!   the current display offset for scrollback) cell-by-cell into the
//!   ratatui Buffer: fg/bg/underline-color mapped from alacritty's
//!   Named/Indexed/Spec colors to ratatui Colors (indexed → Color::Indexed,
//!   spec → Color::Rgb, named → the standard 16 / default fg/bg), flags
//!   (BOLD, ITALIC, UNDERLINE*, INVERSE, DIM, STRIKEOUT, HIDDEN) mapped to
//!   Modifiers. Wide chars: paint the leading cell with the char and skip
//!   spacer cells. The cursor (when visible, on-screen, and `focused`) is
//!   drawn as a REVERSED cell — the host cursor stays hidden.
//! - Performance: this runs on every visible wakeup; avoid per-cell
//!   allocations (no String per cell — use char). Target: full 200x60 grid
//!   paint well under a millisecond.
//! - `render` must not hold the Term lock across the whole frame draw if
//!   avoidable; lock, copy what's needed, release (renderable_content
//!   iteration under the lock is acceptable for v1 — measure in the spike).
//! - `serialize_screen(rt) -> Vec<u8>`: ANSI bytes that repaint the CURRENT
//!   visible screen on a real terminal (cursor home, per-line SGR runs,
//!   final cursor position + SGR reset). Used when entering zoom so the user
//!   instantly sees the session content before live bytes flow.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use crate::runtime::SessionRuntime;

pub fn render(rt: &SessionRuntime, area: Rect, buf: &mut Buffer, focused: bool) {
    let term = rt.term().lock();
    paint(&term, area, buf, focused);
}

pub fn serialize_screen(rt: &SessionRuntime) -> Vec<u8> {
    let term = rt.term().lock();
    serialize_term(&term)
}

/// Grid → Buffer painter. Generic over the event listener so tests can use
/// a directly-constructed `Term<VoidListener>`.
pub(crate) fn paint<T: EventListener>(term: &Term<T>, area: Rect, buf: &mut Buffer, focused: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let content = term.renderable_content();
    let display_offset = content.display_offset as i32;
    let colors = content.colors;

    for indexed in content.display_iter {
        // display_iter yields grid coordinates: line -display_offset .. end.
        let row = indexed.point.line.0 + display_offset;
        let col = indexed.point.column.0;
        if row < 0 || row >= area.height as i32 || col >= area.width as usize {
            continue;
        }
        let pos = (area.x + col as u16, area.y + row as u16);
        let cell = &indexed.cell;
        let flags = cell.flags;

        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            // Hidden behind the wide glyph to the left (ratatui skips the
            // cell after a 2-wide symbol); reset so stale content clears.
            if let Some(bc) = buf.cell_mut(pos) {
                bc.reset();
            }
            continue;
        }

        let Some(bc) = buf.cell_mut(pos) else {
            continue;
        };
        let wide = flags.contains(Flags::WIDE_CHAR);
        if wide && col + 1 >= area.width as usize {
            // A 2-wide glyph in the last column would bleed outside the pane.
            bc.set_char(' ');
        } else if let Some(zerowidth) = cell.zerowidth() {
            // Rare: combining chars; the one allocating path.
            let mut s = String::with_capacity(8);
            s.push(cell.c);
            s.extend(zerowidth);
            bc.set_symbol(&s);
        } else {
            bc.set_char(cell.c);
        }
        bc.fg = map_color(cell.fg, colors);
        bc.bg = map_color(cell.bg, colors);
        bc.modifier = map_flags(flags);
    }

    // Cursor: reversed cell, only for the focused pane on the live screen.
    let cursor = content.cursor;
    if focused && display_offset == 0 && !matches!(cursor.shape, CursorShape::Hidden) {
        let (line, col) = (cursor.point.line.0, cursor.point.column.0);
        if line >= 0
            && line < area.height as i32
            && col < area.width as usize
            && let Some(bc) = buf.cell_mut((area.x + col as u16, area.y + line as u16))
        {
            bc.modifier.toggle(Modifier::REVERSED);
        }
    }
}

fn map_color(color: AnsiColor, colors: &Colors) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => match colors[i as usize] {
            // The child re-defined this palette slot via OSC 4.
            Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            None => Color::Indexed(i),
        },
        AnsiColor::Named(n) => match colors[n as usize] {
            Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            None => named_color(n),
        },
    }
}

fn named_color(n: NamedColor) -> Color {
    match n {
        NamedColor::Black | NamedColor::DimBlack => Color::Black,
        NamedColor::Red | NamedColor::DimRed => Color::Red,
        NamedColor::Green | NamedColor::DimGreen => Color::Green,
        NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
        NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
        NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
        NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
        NamedColor::White | NamedColor::DimWhite => Color::Gray,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
        // Default fg/bg (and cursor/dim/bright aliases of them): themed
        // panes paint the theme's pane colors (matching the OSC answers the
        // emulator gave the agent); the transparent theme maps to the host
        // default so the terminal shows through — the classic behavior.
        NamedColor::Foreground
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => pane_default_fg(),
        NamedColor::Background => pane_default_bg(),
    }
}

/// Theme pane colors, set by the app at startup and again on every in-app
/// theme switch (None = transparent).
static PANE_COLORS: std::sync::RwLock<Option<(Color, Color)>> = std::sync::RwLock::new(None);

/// (fg, bg) for cells the child leaves at the terminal default. Must match
/// the OSC 10/11 answers (`runtime::set_theme_colors`) or agents pick
/// palettes for a background they aren't actually on.
pub fn set_pane_colors(colors: Option<(Color, Color)>) {
    *PANE_COLORS.write().unwrap() = colors;
}

fn pane_default_fg() -> Color {
    PANE_COLORS
        .read()
        .unwrap()
        .map(|(f, _)| f)
        .unwrap_or(Color::Reset)
}

fn pane_default_bg() -> Color {
    PANE_COLORS
        .read()
        .unwrap()
        .map(|(_, b)| b)
        .unwrap_or(Color::Reset)
}

fn map_flags(flags: Flags) -> Modifier {
    let mut m = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        m |= Modifier::BOLD;
    }
    if flags.contains(Flags::DIM) {
        m |= Modifier::DIM;
    }
    if flags.contains(Flags::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        m |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::INVERSE) {
        m |= Modifier::REVERSED;
    }
    if flags.contains(Flags::HIDDEN) {
        m |= Modifier::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        m |= Modifier::CROSSED_OUT;
    }
    m
}

/// Style bits that participate in SGR runs when serializing.
const SGR_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

pub(crate) fn serialize_term<T: EventListener>(term: &Term<T>) -> Vec<u8> {
    let content = term.renderable_content();
    let display_offset = content.display_offset as i32;
    let mut out = Vec::with_capacity(term.columns() * term.screen_lines() * 4);
    out.extend_from_slice(b"\x1b[H\x1b[2J");

    let mut current: Option<(AnsiColor, AnsiColor, Flags)> = None;
    let mut last_row = 0i32;
    let mut utf8 = [0u8; 4];
    for indexed in content.display_iter {
        let row = indexed.point.line.0 + display_offset;
        if row < 0 {
            continue;
        }
        while last_row < row {
            out.extend_from_slice(b"\r\n");
            last_row += 1;
        }
        let cell = &indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue; // the real terminal advances 2 columns for wide glyphs
        }
        let attrs = (cell.fg, cell.bg, cell.flags & SGR_FLAGS);
        if current != Some(attrs) {
            write_sgr(&mut out, attrs.0, attrs.1, attrs.2);
            current = Some(attrs);
        }
        out.extend_from_slice(cell.c.encode_utf8(&mut utf8).as_bytes());
        if let Some(zerowidth) = cell.zerowidth() {
            for &c in zerowidth {
                out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }

    out.extend_from_slice(b"\x1b[0m");
    let cursor = content.cursor;
    let row = (cursor.point.line.0 + 1).max(1);
    let col = cursor.point.column.0 + 1;
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    if matches!(cursor.shape, CursorShape::Hidden) {
        out.extend_from_slice(b"\x1b[?25l");
    } else {
        out.extend_from_slice(b"\x1b[?25h");
    }
    out
}

/// Emit a full SGR (reset + attributes + colors) — simple and unambiguous.
fn write_sgr(out: &mut Vec<u8>, fg: AnsiColor, bg: AnsiColor, flags: Flags) {
    out.extend_from_slice(b"\x1b[0");
    if flags.contains(Flags::BOLD) {
        out.extend_from_slice(b";1");
    }
    if flags.contains(Flags::DIM) {
        out.extend_from_slice(b";2");
    }
    if flags.contains(Flags::ITALIC) {
        out.extend_from_slice(b";3");
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        out.extend_from_slice(b";4");
    }
    if flags.contains(Flags::INVERSE) {
        out.extend_from_slice(b";7");
    }
    if flags.contains(Flags::HIDDEN) {
        out.extend_from_slice(b";8");
    }
    if flags.contains(Flags::STRIKEOUT) {
        out.extend_from_slice(b";9");
    }
    push_sgr_color(out, fg, true);
    push_sgr_color(out, bg, false);
    out.push(b'm');
}

/// ANSI 0-15 index for the named colors that have one.
fn named_base_index(n: NamedColor) -> Option<u8> {
    Some(match n {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    })
}

fn push_sgr_color(out: &mut Vec<u8>, color: AnsiColor, is_fg: bool) {
    match color {
        AnsiColor::Named(n) => match named_base_index(n) {
            Some(i @ 0..=7) => {
                let base = if is_fg { 30 } else { 40 };
                out.extend_from_slice(format!(";{}", base + i as u16).as_bytes());
            }
            Some(i) => {
                let base = if is_fg { 90 } else { 100 };
                out.extend_from_slice(format!(";{}", base + (i - 8) as u16).as_bytes());
            }
            // Default fg/bg.
            None => out.extend_from_slice(if is_fg { b";39" } else { b";49" }),
        },
        AnsiColor::Indexed(i) => {
            let sel = if is_fg { 38 } else { 48 };
            out.extend_from_slice(format!(";{sel};5;{i}").as_bytes());
        }
        AnsiColor::Spec(rgb) => {
            let sel = if is_fg { 38 } else { 48 };
            out.extend_from_slice(format!(";{sel};2;{};{};{}", rgb.r, rgb.g, rgb.b).as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Scroll;
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::Processor;

    struct Size {
        rows: usize,
        cols: usize,
    }
    impl Dimensions for Size {
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

    fn term_with(cols: usize, rows: usize, bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(TermConfig::default(), &Size { rows, cols }, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, bytes);
        term
    }

    fn painted(term: &Term<VoidListener>, w: u16, h: u16, focused: bool) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        paint(term, area, &mut buf, focused);
        buf
    }

    #[test]
    fn plain_and_colored_text() {
        let term = term_with(20, 4, b"\x1b[31mred\x1b[0m plain");
        let buf = painted(&term, 20, 4, false);
        assert_eq!(buf[(0, 0)].symbol(), "r");
        assert_eq!(buf[(0, 0)].fg, Color::Red);
        assert_eq!(buf[(2, 0)].symbol(), "d");
        assert_eq!(buf[(4, 0)].symbol(), "p");
        // Default-fg cells follow the process-wide pane theme (other tests
        // in this binary may have primed it) — assert against it, not a
        // hardcoded Reset.
        assert_eq!(buf[(4, 0)].fg, pane_default_fg());
        assert_eq!(buf[(4, 0)].modifier, Modifier::empty());
    }

    #[test]
    fn sgr_attribute_combos() {
        let term = term_with(20, 2, b"\x1b[1;3;4;7mX\x1b[0m\x1b[2;9;8mY");
        let buf = painted(&term, 20, 2, false);
        let x = &buf[(0, 0)];
        assert!(x.modifier.contains(Modifier::BOLD));
        assert!(x.modifier.contains(Modifier::ITALIC));
        assert!(x.modifier.contains(Modifier::UNDERLINED));
        assert!(x.modifier.contains(Modifier::REVERSED));
        let y = &buf[(1, 0)];
        assert!(y.modifier.contains(Modifier::DIM));
        assert!(y.modifier.contains(Modifier::CROSSED_OUT));
        assert!(y.modifier.contains(Modifier::HIDDEN));
    }

    #[test]
    fn indexed_and_rgb_colors() {
        let term = term_with(20, 2, b"\x1b[38;5;196mA\x1b[48;2;10;20;30mB");
        let buf = painted(&term, 20, 2, false);
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(196));
        assert_eq!(buf[(1, 0)].bg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn wide_char_spacer_skipped() {
        let term = term_with(20, 2, "你a".as_bytes());
        let buf = painted(&term, 20, 2, false);
        assert_eq!(buf[(0, 0)].symbol(), "你");
        assert_eq!(buf[(1, 0)].symbol(), " "); // spacer reset, not painted
        assert_eq!(buf[(2, 0)].symbol(), "a");
    }

    #[test]
    fn cursor_reversed_only_when_focused() {
        let term = term_with(20, 2, b"ab");
        let focused = painted(&term, 20, 2, true);
        assert!(focused[(2, 0)].modifier.contains(Modifier::REVERSED));
        let unfocused = painted(&term, 20, 2, false);
        assert!(!unfocused[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn hidden_cursor_not_drawn() {
        // DECTCEM reset hides the cursor.
        let term = term_with(20, 2, b"ab\x1b[?25l");
        let buf = painted(&term, 20, 2, true);
        assert!(!buf[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn clamps_to_area() {
        // Grid larger than the paint area: no panic, content clipped.
        let term = term_with(40, 10, b"0123456789012345678901234567890123456789");
        let buf = painted(&term, 5, 2, true);
        assert_eq!(buf[(4, 0)].symbol(), "4");
    }

    #[test]
    fn offset_area_paints_at_offset() {
        let term = term_with(10, 2, b"hi");
        let area = Rect::new(3, 1, 10, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 15, 4));
        paint(&term, area, &mut buf, false);
        assert_eq!(buf[(3, 1)].symbol(), "h");
        assert_eq!(buf[(4, 1)].symbol(), "i");
    }

    #[test]
    fn scrollback_display_offset() {
        // 5 rows tall; write 8 numbered lines, scroll back 3: top row is "1".
        let mut term = term_with(10, 5, b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8");
        term.scroll_display(Scroll::Delta(3));
        let buf = painted(&term, 10, 5, true);
        assert_eq!(buf[(0, 0)].symbol(), "1");
        assert_eq!(buf[(0, 4)].symbol(), "5");
        // Cursor must not be drawn while scrolled back.
        for y in 0..5 {
            for x in 0..10 {
                assert!(!buf[(x, y)].modifier.contains(Modifier::REVERSED));
            }
        }
    }

    #[test]
    fn serialize_basic_screen() {
        let term = term_with(10, 3, b"\x1b[31mAB\x1b[0m c");
        let bytes = serialize_term(&term);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("\x1b[H\x1b[2J"), "clear+home prefix: {s:?}");
        assert!(s.contains(";31"), "red fg SGR: {s:?}");
        assert!(s.contains("AB"));
        assert!(s.contains("\r\n"));
        assert!(s.contains("\x1b[0m"));
        // Cursor after " c" → row 1, col 5 (1-based).
        assert!(s.ends_with("\x1b[1;5H\x1b[?25h"), "cursor tail: {s:?}");
    }

    #[test]
    fn serialize_minimal_sgr_runs() {
        // Same style across a run → exactly one SGR for the red run.
        let term = term_with(20, 2, b"\x1b[31mrrr\x1b[0mplain");
        let bytes = serialize_term(&term);
        let s = String::from_utf8_lossy(&bytes);
        let red_count = s.matches(";31").count();
        assert_eq!(red_count, 1, "one SGR per run: {s:?}");
    }

    #[test]
    fn serialize_wide_chars_once() {
        let term = term_with(10, 2, "你x".as_bytes());
        let bytes = serialize_term(&term);
        let s = String::from_utf8_lossy(&bytes);
        assert_eq!(s.matches('你').count(), 1);
        assert!(s.contains("你x") || s.contains('你'));
    }

    #[test]
    fn serialize_hidden_cursor() {
        let term = term_with(10, 2, b"x\x1b[?25l");
        let bytes = serialize_term(&term);
        assert!(String::from_utf8_lossy(&bytes).ends_with("\x1b[?25l"));
    }
}
