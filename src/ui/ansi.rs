//! Minimal ANSI-SGR → ratatui converter for captured tool output (delta's
//! colored diffs). Only styling is honored: SGR color/attribute sequences
//! become `Span` styles, every other escape (cursor movement, OSC titles,
//! modes) is stripped. This is NOT a terminal emulator — the input is a
//! byte stream a formatter printed top-to-bottom, not an interactive grid,
//! so a ~100-line scanner beats dragging the pane emulator in sideways.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert captured output into one `Line` per input line. SGR state
/// carries across newlines (formatters may open a color once for a block).
pub fn lines(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut style = Style::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (line, next_style) = parse_line(raw, style);
        style = next_style;
        out.push(line);
    }
    // split('\n') yields one trailing empty entry for text ending in \n —
    // that's the formatter's final newline, not a real empty line.
    if text.ends_with('\n') {
        out.pop();
    }
    out
}

fn parse_line(raw: &str, mut style: Style) -> (Line<'static>, Style) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            // Tabs would render as a replacement glyph in a Span.
            if c == '\t' {
                cur.push_str("    ");
            } else if !c.is_control() {
                cur.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: consume parameters, apply only the `m` (SGR) final byte.
            Some('[') => {
                let mut params = String::new();
                let mut fin = '\0';
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        fin = c;
                        break;
                    }
                    params.push(c);
                }
                if fin == 'm' {
                    if !cur.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut cur), style));
                    }
                    style = apply_sgr(style, &params);
                }
            }
            // OSC: swallow until BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-char escapes (ESC ( B charset picks etc.): drop both.
            Some(_) | None => {}
        }
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, style));
    }
    (Line::from(spans), style)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let mut it = params
        .split([';', ':'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    while let Some(p) = it.next() {
        style = match p {
            0 => Style::new(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            9 => style.add_modifier(Modifier::CROSSED_OUT),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            29 => style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg(basic_color(p - 30)),
            39 => style.fg(Color::Reset),
            40..=47 => style.bg(basic_color(p - 40)),
            49 => style.bg(Color::Reset),
            90..=97 => style.fg(bright_color(p - 90)),
            100..=107 => style.bg(bright_color(p - 100)),
            38 | 48 => {
                let color = match it.next() {
                    Some(5) => it.next().map(|n| Color::Indexed(n as u8)),
                    Some(2) => {
                        let (r, g, b) = (it.next(), it.next(), it.next());
                        match (r, g, b) {
                            (Some(r), Some(g), Some(b)) => {
                                Some(Color::Rgb(r as u8, g as u8, b as u8))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match (p, color) {
                    (38, Some(c)) => style.fg(c),
                    (48, Some(c)) => style.bg(c),
                    _ => style,
                }
            }
            _ => style,
        };
    }
    style
}

fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn bright_color(n: u16) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn sgr_colors_and_resets_split_spans() {
        let ls = lines("\u{1b}[32m+added\u{1b}[0m rest\n\u{1b}[31m-gone\u{1b}[m\n");
        assert_eq!(ls.len(), 2);
        assert_eq!(text_of(&ls[0]), "+added rest");
        assert_eq!(ls[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(ls[0].spans[1].style.fg, None);
        assert_eq!(ls[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn indexed_truecolor_and_attributes() {
        let ls = lines("\u{1b}[38;5;28m\u{1b}[1mbold green\u{1b}[22m\u{1b}[48;2;10;20;30mbg\n");
        let s0 = ls[0].spans[0].style;
        assert_eq!(s0.fg, Some(Color::Indexed(28)));
        assert!(s0.add_modifier.contains(Modifier::BOLD));
        let s1 = ls[0].spans[1].style;
        assert!(!s1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(s1.bg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn state_carries_across_lines_and_non_sgr_is_stripped() {
        let ls = lines("\u{1b}[33mline one\nline two\u{1b}[0m\n\u{1b}]0;title\u{7}plain\u{1b}[2Kx\n");
        assert_eq!(ls[0].spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(ls[1].spans[0].style.fg, Some(Color::Yellow), "carried");
        assert_eq!(text_of(&ls[2]), "plainx", "OSC + erase-line stripped");
    }

    #[test]
    fn tabs_expand_and_trailing_newline_does_not_add_a_line() {
        let ls = lines("a\tb");
        assert_eq!(text_of(&ls[0]), "a    b");
        assert_eq!(lines("x\n").len(), 1);
        assert_eq!(lines("x\ny").len(), 2);
        assert_eq!(lines("").len(), 1);
    }
}
