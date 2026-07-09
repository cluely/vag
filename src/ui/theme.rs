//! Color themes. The historical look is the `transparent` theme: no
//! background painted anywhere, the terminal's own colors show through.
//! Every other theme paints a solid background (nvim-style) and keys the
//! chrome colors off a small palette.
//!
//! The agent pane joins the theme: cells the child leaves at the DEFAULT
//! background/foreground render in `pane_bg`/`pane_fg`, and the embedded
//! emulator answers the agents' OSC 10/11 theme queries with the SAME
//! values — so claude/codex pick colors that match the theme they're
//! actually sitting on.

use ratatui::style::Color;

use crate::config::Config;
use crate::ui::dashboard::parse_session_color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// App background. `Color::Reset` = don't paint (transparent theme).
    pub bg: Color,
    /// Default foreground for chrome text.
    pub fg: Color,
    /// Raised surfaces: unfocused titlebar, unfocused cursor row, modals.
    pub surface: Color,
    /// Tree/browser chrome background (dashboard body, sidebar column,
    /// floating tree box) — deliberately distinct from `bg` (the agent
    /// pane) so split view reads as two panels, not one surface.
    /// `Color::Reset` = don't paint (transparent theme).
    pub sidebar_bg: Color,
    /// Focused cursor-row background (tree + pickers): a solid bar that
    /// keeps each span's own text color. Never use REVERSED for row
    /// highlights — it flips colored spans (icons, badges, accents) into
    /// mismatched background patches.
    pub sel: Color,
    /// Secondary text: hints, timestamps, separators.
    pub dim: Color,
    /// Highlights: titles, focused chrome, pickers.
    pub accent: Color,
    /// Secondary accent: project labels, remote/machine names — metadata
    /// that should read apart from both `accent` chrome and `dim` hints.
    pub info: Color,
    /// Default colors inside the agent pane, ALSO answered to the agents'
    /// OSC 10/11 queries. None = classic behavior (terminal default cells,
    /// queries answered with the fixed dark palette).
    pub pane: Option<(u8, u8, u8, u8, u8, u8)>, // (fg r,g,b, bg r,g,b)
}

impl Theme {
    /// Exactly the pre-theme rendering: nothing painted, terminal shows
    /// through, chrome uses the historical fixed colors.
    pub const TRANSPARENT: Theme = Theme {
        bg: Color::Reset,
        fg: Color::Reset,
        surface: Color::Rgb(45, 45, 55),
        sidebar_bg: Color::Reset,
        sel: Color::Rgb(60, 62, 78),
        dim: Color::DarkGray,
        accent: Color::Cyan,
        info: Color::Blue,
        pane: None,
    };

    /// The default solid theme (tokyonight-flavored dark).
    pub const NIGHT: Theme = Theme {
        bg: Color::Rgb(0x1a, 0x1b, 0x26),
        fg: Color::Rgb(0xc0, 0xca, 0xf5),
        surface: Color::Rgb(0x24, 0x28, 0x3b),
        sidebar_bg: Color::Rgb(0x1f, 0x22, 0x31),
        sel: Color::Rgb(0x3b, 0x42, 0x61),
        dim: Color::Rgb(0x56, 0x5f, 0x89),
        accent: Color::Rgb(0x7a, 0xa2, 0xf7),
        info: Color::Rgb(0x7d, 0xcf, 0xff),
        pane: Some((0xc0, 0xca, 0xf5, 0x1a, 0x1b, 0x26)),
    };

    /// Catppuccin-mocha-flavored.
    pub const MOCHA: Theme = Theme {
        bg: Color::Rgb(0x1e, 0x1e, 0x2e),
        fg: Color::Rgb(0xcd, 0xd6, 0xf4),
        surface: Color::Rgb(0x31, 0x32, 0x44),
        sidebar_bg: Color::Rgb(0x28, 0x28, 0x39),
        sel: Color::Rgb(0x45, 0x47, 0x5a),
        dim: Color::Rgb(0x6c, 0x70, 0x86),
        accent: Color::Rgb(0x89, 0xb4, 0xfa),
        info: Color::Rgb(0x89, 0xdc, 0xeb),
        pane: Some((0xcd, 0xd6, 0xf4, 0x1e, 0x1e, 0x2e)),
    };

    /// Gruvbox-dark-flavored.
    pub const GRUVBOX: Theme = Theme {
        bg: Color::Rgb(0x28, 0x28, 0x28),
        fg: Color::Rgb(0xeb, 0xdb, 0xb2),
        surface: Color::Rgb(0x3c, 0x38, 0x36),
        sidebar_bg: Color::Rgb(0x32, 0x30, 0x2f),
        sel: Color::Rgb(0x50, 0x49, 0x45),
        dim: Color::Rgb(0x92, 0x83, 0x74),
        accent: Color::Rgb(0x83, 0xa5, 0x98),
        info: Color::Rgb(0x8e, 0xc0, 0x7c),
        pane: Some((0xeb, 0xdb, 0xb2, 0x28, 0x28, 0x28)),
    };

    /// Dracula-flavored.
    pub const DRACULA: Theme = Theme {
        bg: Color::Rgb(0x28, 0x2a, 0x36),
        fg: Color::Rgb(0xf8, 0xf8, 0xf2),
        surface: Color::Rgb(0x34, 0x37, 0x46),
        sidebar_bg: Color::Rgb(0x2e, 0x31, 0x3e),
        sel: Color::Rgb(0x44, 0x47, 0x5a),
        dim: Color::Rgb(0x62, 0x72, 0xa4),
        accent: Color::Rgb(0xbd, 0x93, 0xf9),
        info: Color::Rgb(0x8b, 0xe9, 0xfd),
        pane: Some((0xf8, 0xf8, 0xf2, 0x28, 0x2a, 0x36)),
    };

    /// Nord-flavored.
    pub const NORD: Theme = Theme {
        bg: Color::Rgb(0x2e, 0x34, 0x40),
        fg: Color::Rgb(0xd8, 0xde, 0xe9),
        surface: Color::Rgb(0x3b, 0x42, 0x52),
        sidebar_bg: Color::Rgb(0x35, 0x3b, 0x49),
        sel: Color::Rgb(0x43, 0x4c, 0x5e),
        dim: Color::Rgb(0x61, 0x6e, 0x88),
        accent: Color::Rgb(0x88, 0xc0, 0xd0),
        info: Color::Rgb(0x81, 0xa1, 0xc1),
        pane: Some((0xd8, 0xde, 0xe9, 0x2e, 0x34, 0x40)),
    };

    /// One-Dark-flavored (Atom).
    pub const ONEDARK: Theme = Theme {
        bg: Color::Rgb(0x28, 0x2c, 0x34),
        fg: Color::Rgb(0xab, 0xb2, 0xbf),
        surface: Color::Rgb(0x31, 0x35, 0x3f),
        sidebar_bg: Color::Rgb(0x2d, 0x31, 0x3a),
        sel: Color::Rgb(0x3e, 0x44, 0x51),
        dim: Color::Rgb(0x5c, 0x63, 0x70),
        accent: Color::Rgb(0x61, 0xaf, 0xef),
        info: Color::Rgb(0x56, 0xb6, 0xc2),
        pane: Some((0xab, 0xb2, 0xbf, 0x28, 0x2c, 0x34)),
    };

    /// Solarized-dark-flavored.
    pub const SOLARIZED: Theme = Theme {
        bg: Color::Rgb(0x00, 0x2b, 0x36),
        fg: Color::Rgb(0x93, 0xa1, 0xa1),
        surface: Color::Rgb(0x07, 0x36, 0x42),
        sidebar_bg: Color::Rgb(0x04, 0x31, 0x3c),
        sel: Color::Rgb(0x15, 0x4a, 0x57),
        dim: Color::Rgb(0x58, 0x6e, 0x75),
        accent: Color::Rgb(0x26, 0x8b, 0xd2),
        info: Color::Rgb(0x2a, 0xa1, 0x98),
        pane: Some((0x93, 0xa1, 0xa1, 0x00, 0x2b, 0x36)),
    };

    /// Rosé-Pine-flavored.
    pub const ROSE_PINE: Theme = Theme {
        bg: Color::Rgb(0x19, 0x17, 0x24),
        fg: Color::Rgb(0xe0, 0xde, 0xf4),
        surface: Color::Rgb(0x26, 0x23, 0x3a),
        sidebar_bg: Color::Rgb(0x20, 0x1d, 0x2f),
        sel: Color::Rgb(0x40, 0x3d, 0x52),
        dim: Color::Rgb(0x6e, 0x6a, 0x86),
        accent: Color::Rgb(0xc4, 0xa7, 0xe7),
        info: Color::Rgb(0x9c, 0xcf, 0xd8),
        pane: Some((0xe0, 0xde, 0xf4, 0x19, 0x17, 0x24)),
    };

    /// Every selectable theme as (canonical name, value) — the settings
    /// picker and `by_name` both derive from this, so they can't drift.
    pub const ALL: [(&'static str, Theme); 9] = [
        ("night", Theme::NIGHT),
        ("mocha", Theme::MOCHA),
        ("gruvbox", Theme::GRUVBOX),
        ("dracula", Theme::DRACULA),
        ("nord", Theme::NORD),
        ("onedark", Theme::ONEDARK),
        ("solarized", Theme::SOLARIZED),
        ("rose-pine", Theme::ROSE_PINE),
        ("transparent", Theme::TRANSPARENT),
    ];

    pub fn by_name(name: &str) -> Option<Theme> {
        let name = name.trim().to_ascii_lowercase();
        // aliases first, then the canonical table
        let canonical = match name.as_str() {
            "default" | "tokyonight" => "night",
            "catppuccin" => "mocha",
            "one" | "one-dark" | "atom" => "onedark",
            "solarized-dark" => "solarized",
            "rosepine" | "rosé-pine" => "rose-pine",
            other => other,
        };
        Theme::ALL
            .iter()
            .find(|(n, _)| *n == canonical)
            .map(|(_, t)| *t)
    }

    /// Resolve from config: named base + per-key `[theme]` hex overrides.
    /// Unknown names warn and fall back to the default solid theme.
    pub fn from_config(cfg: &Config) -> Theme {
        let mut t = Theme::by_name(&cfg.ui.theme).unwrap_or_else(|| {
            if !cfg.ui.theme.trim().is_empty() {
                let names: Vec<&str> = Theme::ALL.iter().map(|(n, _)| *n).collect();
                eprintln!(
                    "vag: unknown ui.theme `{}` ({}); using night",
                    cfg.ui.theme,
                    names.join(" | ")
                );
            }
            Theme::NIGHT
        });
        let o = &cfg.theme;
        let set = |slot: &mut Color, v: &Option<String>| {
            if let Some(c) = v.as_deref().and_then(parse_session_color) {
                *slot = c;
            }
        };
        set(&mut t.bg, &o.bg);
        set(&mut t.fg, &o.fg);
        set(&mut t.surface, &o.surface);
        set(&mut t.sidebar_bg, &o.sidebar_bg);
        set(&mut t.sel, &o.sel);
        set(&mut t.dim, &o.dim);
        set(&mut t.accent, &o.accent);
        set(&mut t.info, &o.info);
        if let (Some(Color::Rgb(fr, fg_, fb)), Some(Color::Rgb(br, bg_, bb))) = (
            o.pane_fg.as_deref().and_then(parse_session_color),
            o.pane_bg.as_deref().and_then(parse_session_color),
        ) {
            t.pane = Some((fr, fg_, fb, br, bg_, bb));
        }
        // An overridden transparent bg means "don't paint": keep pane
        // classic too unless explicitly overridden above.
        if t.bg == Color::Reset && o.pane_bg.is_none() {
            t.pane = None;
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_and_unknown_falls_back() {
        assert_eq!(Theme::by_name("transparent"), Some(Theme::TRANSPARENT));
        assert_eq!(Theme::by_name("Night"), Some(Theme::NIGHT));
        assert_eq!(Theme::by_name("default"), Some(Theme::NIGHT));
        assert_eq!(Theme::by_name("catppuccin"), Some(Theme::MOCHA));
        assert_eq!(Theme::by_name("gruvbox"), Some(Theme::GRUVBOX));
        assert_eq!(Theme::by_name("dracula"), Some(Theme::DRACULA));
        assert_eq!(Theme::by_name("nord"), Some(Theme::NORD));
        assert_eq!(Theme::by_name("one-dark"), Some(Theme::ONEDARK));
        assert_eq!(Theme::by_name("solarized-dark"), Some(Theme::SOLARIZED));
        assert_eq!(Theme::by_name("rosepine"), Some(Theme::ROSE_PINE));
        assert_eq!(Theme::by_name("hotdog-stand"), None);
    }

    #[test]
    fn all_themes_are_selectable_solid_and_distinct() {
        for (name, t) in Theme::ALL {
            // every canonical picker name resolves to its own value
            assert_eq!(Theme::by_name(name), Some(t), "{name}");
            if name == "transparent" {
                assert_eq!(t.pane, None);
                assert_eq!(t.sidebar_bg, Color::Reset, "{name}");
                continue;
            }
            // solid themes: painted bg, pane joins the theme, and the pane
            // bg matches the app bg so the split reads as one surface
            let Some((_, _, _, br, bgc, bb)) = t.pane else {
                panic!("{name} must theme the pane");
            };
            assert_eq!(t.bg, Color::Rgb(br, bgc, bb), "{name} pane bg == app bg");
            // sel must differ from surface or focus becomes invisible
            assert_ne!(t.sel, t.surface, "{name}");
            // sidebar_bg is a distinct third shade between bg and surface
            assert_ne!(t.sidebar_bg, t.bg, "{name}");
            assert_ne!(t.sidebar_bg, t.surface, "{name}");
        }
        // no duplicate backgrounds — each theme must look like itself
        let mut bgs: Vec<Color> = Theme::ALL.iter().map(|(_, t)| t.bg).collect();
        bgs.sort_by_key(|c| format!("{c:?}"));
        bgs.dedup();
        assert_eq!(bgs.len(), Theme::ALL.len());
    }

    #[test]
    fn config_overrides_apply_per_key() {
        let mut cfg = Config::default();
        cfg.ui.theme = "night".into();
        cfg.theme.accent = Some("#ff0000".into());
        cfg.theme.bg = Some("blue".into());
        cfg.theme.sel = Some("#334455".into());
        cfg.theme.info = Some("#00ff00".into());
        cfg.theme.sidebar_bg = Some("#112233".into());
        let t = Theme::from_config(&cfg);
        assert_eq!(t.accent, Color::Rgb(0xff, 0, 0));
        assert_eq!(t.bg, Color::Blue);
        assert_eq!(t.sel, Color::Rgb(0x33, 0x44, 0x55));
        assert_eq!(t.info, Color::Rgb(0, 0xff, 0));
        assert_eq!(t.sidebar_bg, Color::Rgb(0x11, 0x22, 0x33));
        // untouched keys keep the base theme
        assert_eq!(t.dim, Theme::NIGHT.dim);
        // pane defaults survive
        assert_eq!(t.pane, Theme::NIGHT.pane);
    }

    #[test]
    fn transparent_stays_classic() {
        let mut cfg = Config::default();
        cfg.ui.theme = "transparent".into();
        let t = Theme::from_config(&cfg);
        assert_eq!(t, Theme::TRANSPARENT);
        assert_eq!(t.pane, None);
    }
}
