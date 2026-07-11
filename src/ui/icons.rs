//! Glyph sets for the renderers: every symbol the tree/pane chrome draws,
//! resolved once at startup from `ui.icons` (ascii | nerd | auto).
//!
//! The ASCII set is exactly the glyphs the renderers hard-coded before this
//! abstraction existed, so the default UI stays pixel-identical. The NERD
//! set uses Nerd Font codepoints, each verified against the official
//! cheat-sheet data (nerd-fonts glyphnames.json). The Working spinner is
//! intentionally NOT here: braille frames render everywhere, so both sets
//! share `dashboard::SPINNER`.

use crate::config::IconMode;
use crate::types::AgentKind;

/// One resolved glyph set. Plain `&'static str`s so rendering never
/// allocates for a symbol lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icons {
    /// Agent mark shown next to claude sessions.
    pub claude: &'static str,
    /// Agent mark shown next to codex sessions.
    pub codex: &'static str,
    /// Folder row arrow while collapsed.
    pub folder_collapsed: &'static str,
    /// Folder row arrow while expanded.
    pub folder_expanded: &'static str,
    /// Prefix before the "Inbox" label; empty = no prefix (ascii default).
    pub inbox: &'static str,
    /// Prefix of the "+ new session" row.
    pub new_session: &'static str,
    /// Badge: turn finished while unviewed.
    pub badge_done_unread: &'static str,
    /// Badge: open and idle, already seen.
    pub badge_idle: &'static str,
    /// Badge: child process exited, pane still open.
    pub badge_exited: &'static str,
    /// Badge: running outside vag (external claude).
    pub badge_external: &'static str,
    /// "This machine" mark in the new-session location picker.
    pub local: &'static str,
    /// Remote (ssh) mark: location picker rows and remote session rows.
    pub remote: &'static str,
    /// Git-branch marker in the pane titlebar.
    pub branch: &'static str,
    /// Clock marker before the created-time in the pane titlebar; empty =
    /// spell it out ("created 3h ago", the ascii default).
    pub clock: &'static str,
    /// The "settings" row pinned at the top of the tree.
    pub settings: &'static str,
    /// Generic file mark in the diff view's file tree; empty in ascii mode
    /// (the status letter already anchors those rows).
    pub file: &'static str,
}

impl Icons {
    /// Exactly the glyphs the renderers used before the abstraction.
    pub const ASCII: Icons = Icons {
        claude: "✳",
        codex: "◆",
        folder_collapsed: "▸",
        folder_expanded: "▾",
        inbox: "",
        new_session: "+",
        badge_done_unread: "●",
        badge_idle: "◌",
        badge_exited: "✚",
        badge_external: "▲",
        local: "•",
        remote: "@",
        branch: "⎇",
        clock: "",
        settings: "⚙",
        file: "",
    };

    /// Nerd Font glyphs; codepoints verified against the nerd-fonts
    /// cheat-sheet data (glyphnames.json, nerdfonts.com/cheat-sheet).
    pub const NERD: Icons = Icons {
        claude: "\u{F0674}",           // nf-md-creation
        codex: "\u{F06A9}",            // nf-md-robot
        folder_collapsed: "\u{F07B}",  // nf-fa-folder
        folder_expanded: "\u{F07C}",   // nf-fa-folder_open
        inbox: "\u{F48D}",             // nf-oct-inbox
        new_session: "\u{F055}",       // nf-fa-plus_circle
        badge_done_unread: "\u{F0E0}", // nf-fa-envelope
        badge_idle: "\u{F10C}",        // nf-fa-circle_o
        badge_exited: "\u{F068C}",     // nf-md-skull
        badge_external: "\u{F08E}",    // nf-fa-external_link
        local: "\u{F0322}",            // nf-md-laptop
        remote: "\u{F015F}",           // nf-md-cloud
        branch: "\u{F418}",            // nf-oct-git_branch
        clock: "\u{F0150}",            // nf-md-clock_outline
        settings: "\u{F0493}",         // nf-md-cog
        file: "\u{F016}",              // nf-fa-file_o
    };

    /// Resolve the set for a configured mode (`IconMode::use_nerd` handles
    /// the `auto` heuristics).
    pub fn for_mode(mode: IconMode) -> Icons {
        // The TUI's ascii agent marks must stay in lock-step with
        // `AgentKind::icon()`, the plain fallback used outside the
        // renderers.
        debug_assert_eq!(Icons::ASCII.claude, AgentKind::Claude.icon());
        debug_assert_eq!(Icons::ASCII.codex, AgentKind::Codex.icon());
        if mode.use_nerd() {
            Icons::NERD
        } else {
            Icons::ASCII
        }
    }

    /// The agent mark for a session's agent.
    pub fn agent(&self, agent: AgentKind) -> &'static str {
        match agent {
            AgentKind::Claude => self.claude,
            AgentKind::Codex => self.codex,
            // Shell panes use the plain glyph in both icon sets.
            AgentKind::Shell => AgentKind::Shell.icon(),
        }
    }

    /// Per-filetype mark for the diff view's file tree. ASCII mode returns
    /// "" (rows keep their pre-icon look); nerd mode maps common extensions
    /// and falls back to the generic file glyph. Codepoints verified
    /// against glyphnames.json like the rest of the NERD set.
    pub fn file_icon(&self, name: &str) -> &'static str {
        if self.file.is_empty() {
            return "";
        }
        let lower = name.to_ascii_lowercase();
        if lower.starts_with(".git") {
            return "\u{E702}"; // nf-dev-git
        }
        if lower.ends_with(".lock") || lower == "package-lock.json" {
            return "\u{F023}"; // nf-fa-lock
        }
        let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        match ext {
            "rs" => "\u{E7A8}",                                // nf-dev-rust
            "py" => "\u{E73C}",                                // nf-dev-python
            "js" | "jsx" | "mjs" | "cjs" => "\u{E718}",        // nf-dev-nodejs_small
            "ts" | "tsx" => "\u{E628}",                        // nf-seti-typescript
            "json" => "\u{E60B}",                              // nf-seti-json
            "md" | "markdown" => "\u{F48A}",                   // nf-oct-markdown
            "toml" | "ini" | "conf" | "cfg" => "\u{E615}",     // nf-seti-config
            "yml" | "yaml" => "\u{E6A8}",                      // nf-seti-yml
            "sh" | "bash" | "zsh" | "fish" => "\u{F489}",      // nf-oct-terminal
            "html" | "htm" => "\u{E736}",                      // nf-dev-html5
            "css" | "scss" | "less" => "\u{E749}",             // nf-dev-css3
            "go" => "\u{E627}",                                // nf-seti-go
            "swift" => "\u{E755}",                             // nf-dev-swift
            "c" | "h" => "\u{E61E}",                           // nf-custom-c
            "cpp" | "cc" | "hpp" | "cxx" => "\u{E61D}",        // nf-custom-cpp
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" => "\u{F1C5}", // nf-fa-file_image_o
            "diff" | "patch" => "\u{F4D2}",                    // nf-oct-file_diff
            _ => self.file,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_set_is_pixel_identical_to_legacy_glyphs() {
        let a = Icons::ASCII;
        assert_eq!(a.claude, "✳");
        assert_eq!(a.codex, "◆");
        assert_eq!(a.folder_collapsed, "▸");
        assert_eq!(a.folder_expanded, "▾");
        assert_eq!(a.inbox, "", "inbox had no prefix before the abstraction");
        assert_eq!(a.new_session, "+");
        assert_eq!(a.badge_done_unread, "●");
        assert_eq!(a.badge_idle, "◌");
        assert_eq!(a.badge_exited, "✚");
        assert_eq!(a.badge_external, "▲");
        assert_eq!(a.local, "•");
        assert_eq!(a.remote, "@");
        // …and the agent marks match the non-UI fallback in types.rs.
        assert_eq!(a.claude, AgentKind::Claude.icon());
        assert_eq!(a.codex, AgentKind::Codex.icon());
    }

    #[test]
    fn nerd_glyphs_are_one_or_two_chars() {
        let n = Icons::NERD;
        for (name, g) in [
            ("claude", n.claude),
            ("codex", n.codex),
            ("folder_collapsed", n.folder_collapsed),
            ("folder_expanded", n.folder_expanded),
            ("inbox", n.inbox),
            ("new_session", n.new_session),
            ("badge_done_unread", n.badge_done_unread),
            ("badge_idle", n.badge_idle),
            ("badge_exited", n.badge_exited),
            ("badge_external", n.badge_external),
            ("local", n.local),
            ("remote", n.remote),
        ] {
            let chars = g.chars().count();
            assert!(
                (1..=2).contains(&chars),
                "{name}: want 1-2 chars, got {chars} ({g:?})"
            );
        }
    }

    #[test]
    fn location_glyphs_use_verified_codepoints() {
        // Verified against glyphnames.json: md-laptop = f0322, md-cloud = f015f.
        assert_eq!(Icons::NERD.local, "\u{F0322}");
        assert_eq!(Icons::NERD.remote, "\u{F015F}");
    }

    #[test]
    fn file_icons_map_extensions_and_ascii_stays_bare() {
        let n = Icons::NERD;
        assert_eq!(n.file_icon("main.rs"), "\u{E7A8}");
        assert_eq!(n.file_icon("app.TSX"), "\u{E628}");
        assert_eq!(n.file_icon("Cargo.lock"), "\u{F023}");
        assert_eq!(n.file_icon(".gitignore"), "\u{E702}");
        assert_eq!(n.file_icon("notes.md"), "\u{F48A}");
        assert_eq!(n.file_icon("Makefile"), n.file, "unknown → generic file");
        // ASCII mode: no file icons at all — rows stay pixel-identical.
        assert_eq!(Icons::ASCII.file_icon("main.rs"), "");
    }

    #[test]
    fn agent_lookup_matches_fields() {
        assert_eq!(Icons::ASCII.agent(AgentKind::Claude), Icons::ASCII.claude);
        assert_eq!(Icons::NERD.agent(AgentKind::Codex), Icons::NERD.codex);
    }

    #[test]
    fn explicit_modes_resolve_without_the_environment() {
        assert_eq!(Icons::for_mode(IconMode::Ascii), Icons::ASCII);
        assert_eq!(Icons::for_mode(IconMode::Nerd), Icons::NERD);
    }

    /// Auto-mode heuristics read the environment, so every scenario runs
    /// sequentially inside this single test (parallel test threads must
    /// never race on set_var) and the original values are restored.
    #[test]
    fn auto_mode_heuristics() {
        const VARS: [&str; 3] = ["NERD_FONT", "USE_NERD_FONT", "TERM_PROGRAM"];
        let saved: Vec<Option<std::ffi::OsString>> = VARS.iter().map(std::env::var_os).collect();
        let clear_all = || {
            for v in VARS {
                // SAFETY: test-only; no other test in this crate touches
                // these variables.
                unsafe { std::env::remove_var(v) };
            }
        };

        clear_all();
        assert_eq!(
            Icons::for_mode(IconMode::Auto),
            Icons::ASCII,
            "bare environment falls back to ascii"
        );

        unsafe { std::env::set_var("TERM_PROGRAM", "WezTerm") };
        assert_eq!(
            Icons::for_mode(IconMode::Auto),
            Icons::NERD,
            "nerd-font-friendly terminal"
        );

        unsafe { std::env::set_var("TERM_PROGRAM", "SomethingElse") };
        assert_eq!(Icons::for_mode(IconMode::Auto), Icons::ASCII);

        unsafe { std::env::set_var("NERD_FONT", "1") };
        assert_eq!(
            Icons::for_mode(IconMode::Auto),
            Icons::NERD,
            "explicit NERD_FONT wins over an unknown terminal"
        );

        clear_all();
        unsafe { std::env::set_var("USE_NERD_FONT", "true") };
        assert_eq!(Icons::for_mode(IconMode::Auto), Icons::NERD);

        // restore whatever the harness had
        for (name, val) in VARS.iter().zip(saved) {
            match val {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}
