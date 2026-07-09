# vag

**One keyboard-driven dashboard for every Claude Code and Codex session on your machine.**

[![CI](https://img.shields.io/github/actions/workflow/status/OWNER/vag/ci.yml?branch=main)](https://github.com/OWNER/vag/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

vag is **not a replacement** for Claude Code or Codex — it's an expansion. It launches
the **real** `claude` and `codex` CLIs and embeds them in a terminal pane, so everything
those tools do keeps working (including features shipped after vag), and adds
organization, turn tracking, and navigation on top. Think lazygit for your agent sessions.

```
┌─ vag ─────────────────────┬──────────────────────────────────┐
│   + new session           │ ✳ Claude Code                    │
│ ▾ work/                   │                                  │
│   ⠹ auth-fix    working 4m│ > fix the flaky auth test        │
│   ● parser-v2   done 2m   │   ⏺ Running tests…               │
│ ▸ experiments/            │                                  │
│                           │ (the actual claude TUI, rendered │
│ n:new F:fork e:edit ?:help│  in an embedded terminal pane)   │
└───────────────────────────┴──────────────────────────────────┘
```

Prefer full-width sessions? The floating tree (`ui.tree = "float"`) overlays on demand:

```
┌─ vag ────────────────────────────────────────────────────────┐
│ > codex is refactoring, full width…                          │
│         ┌─ sessions ──────────────────────┐                  │
│         │ ▾ work/                         │                  │
│         │   ⠹ auth-fix        working 4m  │                  │
│         │   ● parser-v2       done 2m     │                  │
│         └─────────────────────────────────┘                  │
└──────────────────────────────────────────────────────────────┘
```

## Install

**Homebrew** — *not published yet, coming soon* (the formula lives in [`Formula/vag.rb`](Formula/vag.rb)):

```sh
brew install OWNER/vag/vag
```

**curl** (downloads a release binary, falls back to a cargo build — script is [`install.sh`](install.sh)):

```sh
curl -fsSL https://raw.githubusercontent.com/OWNER/vag/main/install.sh | sh
```

**From source** (requires Rust):

```sh
cargo install --path .
```

vag drives the real agent CLIs — install at least one of
[Claude Code](https://docs.anthropic.com/en/docs/claude-code) or
[Codex](https://github.com/openai/codex), then run `vag doctor`.

Those two CLIs are the only requirement: everything else (including the fuzzy
directory picker in the new-session flow) is built into the `vag` binary — no
`fzf` or other external tools needed.

*`OWNER` is a placeholder for the GitHub org/user until the repository is published.*

## Quickstart

```sh
vag        # dashboard of every claude/codex session on this machine
```

`enter` opens the session under the cursor (the real CLI, embedded). `ctrl-q` detaches
back to the tree — the agent keeps working. `?` shows every key.

## Features

- **Dashboard of everything** — every Claude Code and Codex session, discovered read-only
  from the CLIs' own stores (`~/.claude`, `~/.codex`), grouped into **folders you define**.
  Session files are **never moved or edited** — organization lives in vag's own state
  file, so resume always works and agent CLI updates can't break your layout.
- **The real CLIs, embedded** — opening a session runs the actual `claude --resume` /
  `codex resume` in a PTY and renders it in a pane. Inside the pane every key goes to the
  agent — including `ctrl-c` — except the detach hotkey.
- **Session lifecycle** — create (either agent, any directory — picked with a built-in
  fzf-style fuzzy finder), fork (`--fork-session` / `codex fork`), rename, hide,
  archive (codex-native).
- **Turn tracking** — switch away mid-turn and the tree shows what every session is doing:
  `⠹ working 4m32s` (animated) while a command is in flight, a bold `● done 2m` when a
  turn finished while you weren't looking (cleared when you view it), `◌` idle, `✚` exited,
  and `▲ working 3m` for claude sessions running in *other* terminals (detected via
  transcript activity). Timestamps refresh live.
- **Repo scoping** — launched inside a git repo, vag shows only that repo's sessions and
  folders by default (`g` toggles the cross-project view). A pinned `+ new session` row
  defaults new sessions to the repo root.
- **Zoom** (`z`) — hand the whole terminal to the session for full fidelity, one hotkey back.
- **Agent detection** — agents missing from PATH appear grayed out in the new-session
  picker; `vag doctor` reports exactly what was found.
- **In-app settings** (`⚙` pinned at the bottom of the tree; `End` jumps to it) —
  themes with **live preview**,
  icons, pane style, tree mode, launch behavior, and fully rebindable keys; every
  change applies instantly and is saved to `config.toml` (see [Settings](#settings)).

### Edit mode (oil.nvim-style)

Press `e` in the tree (dashboard, sidebar, or floating tree) to turn it into an editable
text buffer, [oil.nvim](https://github.com/stevearc/oil.nvim)-style: reorganize sessions
by editing lines, then `:w` to review and apply the changes as one batch.

- **Vim subset** — Normal: `h j k l` (+arrows), `0 $ gg G`, counts for `j`/`k`,
  `i a I A` enter Insert, `x`, `dd` (cut line), `yy` (yank), `p`/`P` (paste below/above),
  `o`/`O` (open line), `u` / `ctrl-r` (undo/redo). Cmdline: `:w`, `:q`, `:q!`, `:wq`.
  `enter` on a session line opens it (only when the buffer has no unsaved changes).
- **Editing a line renames** the session or folder (folders keep a trailing `/`).
  Deleting all of a session's text resets its name to the agent's default.
- **`dd` + `p` moves** a session (paste on a folder line drops it *inside* that folder);
  `dd` without a re-paste **hides** the session; `dd` on a folder line **deletes** the
  folder (contents re-parent).
- **Fork by copy-paste**: `yy` a session line and `p`aste it somewhere — on `:w` the
  duplicate becomes a **fork** of the session into the paste location's folder.
- **`o` then a name ending in `/`** creates a folder there; other typed text is ignored
  with a warning (sessions can't be typed into existence).
- `:w` shows the planned actions in a confirm box before anything is applied; `:q`
  refuses to leave with unsaved changes (`:q!` forces). While editing, every key —
  including the detach key — goes to the buffer, so `:q` is the only way out.

### Floating tree

With `ui.tree = "float"` a session takes the full width and the detach key (default
`ctrl-q`) toggles a centered floating tree over it — press it again (or `esc`) to close,
`b` to return to the full dashboard, and `e` inside the float to edit. The default
`"sidebar"` keeps the persistent left sidebar.

### Remote (SSH) sessions

Machines are dashboard groups. Press `R` inside vag (or run
`vag remote add gpu-box user@host`) to add one; `n` on a machine creates a session there,
and `s` opens a plain shell on it — or a local `$SHELL` anywhere else — so shells and
agent sessions mix in one dashboard. The real agent CLI runs over `ssh -t` inside the
same embedded pane, so the first connection's password and host-key prompts just work —
vag never stores credentials, it rides your `~/.ssh/config` (the host field accepts your
ssh aliases, and the add-machine dialog suggests them). Turn tracking comes along. Remote
claude sessions get a pre-assigned session id, so they persist and reopen from vag like
local ones; remote codex sessions are attach-only for now (they stay resumable on the box
itself). Remote rows carry an `@host` label in the tree.

## CLI commands

`vag doctor` — check agent CLIs, stores, vag's own files, and the reachability of any
configured `[[remotes]]`:

```
  claude  ✓ claude  (2.1.197 (Claude Code))
  codex   ✓ codex  (codex-cli 0.142.5)
  claude store   /Users/guigaribaldi/.claude (163 sessions)
  codex store    /Users/guigaribaldi/.codex (59 sessions)
  config         /Users/guigaribaldi/.config/vag/config.toml  (missing — defaults active)
  state          /Users/guigaribaldi/.local/share/vag/state.json  (0 folders, 1 tracked sessions)
```

`vag list [--json]` — print every discovered session (`--json` for machines):

```
claude  39212683 Build vag CLI aggregator for Claude sessions       vibe-aggregator      29s
claude  da90a1ec voice lab                                          passport-ts          1h
```

`vag config` — print file locations and the fully resolved configuration:

```
config file:  /Users/guigaribaldi/.config/vag/config.toml
state file:   /Users/guigaribaldi/.local/share/vag/state.json
resolved configuration:
[keys]
detach = "ctrl-q"
```

`vag remote add <name> <host> [--dir <d>]` / `vag remote list` / `vag remote remove <name>`
— manage the `[[remotes]]` SSH machines without opening the config.

`vag --icons <nerd|ascii|auto>` — per-run icon set override for the TUI (also `VAG_ICONS=nerd`).

`vag --tree <sidebar|float>` / `vag --float` — per-run tree placement (also `VAG_TREE=float`).

`vag --pane <border|titlebar>` — pane chrome (default: tmux-style titlebar; also `VAG_PANE=border`).

`vag --theme <night|mocha|gruvbox|transparent>` — color theme (default: `night`, a solid
dark background; `transparent` keeps the terminal's own background — the pre-theme look).
The agent pane joins the theme: the embedded emulator answers claude/codex's color
queries with the theme's pane colors, so agents render palettes that match.
The titlebar shows the session's context live: project (or `@machine`), git branch,
turn state (`working 4m12s` / `done 2m`), and creation time — dropping the least
important pieces first on narrow terminals.

`vag --edit` — start the tree in nvim edit mode for this run (also `VAG_EDIT=1`).

## Settings

The `⚙ settings` row is pinned at the **bottom** of the tree — outside the scrolling
list, so it never takes a slot from your sessions and stays visible however long the
tree gets — and shows its shortcut: `settings (,)`. Press `,` to open the page from
anywhere in the tree, or `End` (or `j` past the last row) then `enter`. The page covers: theme,
icons, pane style, tree mode, sidebar width, launch behavior — and every key
binding. Changes apply **immediately** and are written to `config.toml` for you
(comments and formatting in the file survive). The theme row opens a picker that
**previews live** as you move over the options: `enter` keeps the hovered theme,
`esc` puts the old one back. On a key row, `enter` waits for the next keypress to
rebind (navigation keys — `j/k/h/l`, space, `/`, digits — are reserved and refused;
so is a char another action already holds).

Per-run flags (`--theme`, `--icons`, …) still win at launch over whatever is saved.

## Configuration

Create `~/.config/vag/config.toml` (`XDG_CONFIG_HOME` respected). Every key is optional —
any key you set overrides the default. The settings page writes this same file, so
you never *have* to edit it by hand. These are the defaults:

```toml
[keys]
detach = "ctrl-q"               # pane -> tree (double press sends the literal chord through)
toggle_sidebar = "ctrl-e"       # show/hide the sidebar while a session pane has focus
focus_tree = "ctrl-h"           # pane -> tree (plain alias, no double-press escape hatch)
focus_pane = "ctrl-l"           # tree -> the active session's pane (not "enter" — never
                                 # opens the row under the cursor)
# every single-char command is rebindable (defaults shown; one printable,
# non-reserved char each — j/k/h/l, space, `/` and digits are navigation):
quit = "q"
help = "?"
new_session = "n"
new_folder = "N"
fork = "F"
edit = "e"
move = "m"
rename = "r"
add_machine = "R"
shell = "s"
bind_dir = "b"
color = "c"
hide = "d"
show_hidden = "H"
scope = "g"
archive = "A"
delete = "x"
close = "w"
zoom = "z"
settings = ","

[agents.claude]
command = "claude"              # binary name or path
extra_args = []                 # appended to every spawn of this agent

[agents.codex]
command = "codex"               # binary name or path
extra_args = []                 # appended to every spawn of this agent

[ui]
sidebar_width = 34              # columns for the session tree
tree = "sidebar"                # or "float": full-width pane, detach key toggles a floating tree
pane = "titlebar"               # tmux-style full-width title bar (default); or
                                # "border": a bordered box (per run: vag --pane border)
theme = "night"                 # solid dark background (default). Also: "mocha",
                                # "gruvbox", "dracula", "nord", "onedark", "solarized",
                                # "rose-pine", or "transparent" (terminal shows through).
                                # ALL tree text (folders, project labels, timestamps)
                                # follows the theme's palette. Switch live from the
                                # settings page (previews as you move); per run:
                                # vag --theme <name> or VAG_THEME
                                # The sidebar/dashboard tree paints its own shade
                                # (sidebar_bg), distinct per theme, so split view
                                # reads as two panels instead of one flat surface.
# [theme]                       # fine-tune any key over the named base:
# bg = "#1a1b26"                # app background
# surface = "#24283b"           # bars / raised rows
# sidebar_bg = "#1f2231"        # tree/sidebar panel — distinct from the pane bg
# sel = "#3b4261"               # cursor-row highlight (tree + pickers)
# accent = "cyan"               # highlights: folders, buttons (names or #rrggbb)
# info = "#7dcfff"              # secondary accent: project labels, machine names
# dim = "#565f89"               # hints, timestamps
# pane_fg = "#c0caf5"           # agent pane defaults — also answered to the
# pane_bg = "#1a1b26"           # agents' OSC theme queries so their colors match
icons = "ascii"                 # or "nerd" | "auto" — auto detects common nerd-font terminals;
                                # per run: `vag --icons nerd` or VAG_ICONS=nerd
edit_default = false            # start the tree in vim edit mode

[behavior]
repo_scope = true               # scope to the current git repo by default when inside one (g toggles)
show_hidden = false             # show hidden sessions
codex_show_automation = false   # show codex automation threads
# claude_config_dir = "/path/to/.claude"   # store override; $CLAUDE_CONFIG_DIR works without config
# codex_home = "/path/to/.codex"           # store override; $CODEX_HOME works without config

# [[remotes]]                   # SSH machines sessions can be created on; none by default
# name = "gpu-box"              # shown in the UI
# host = "user@10.0.0.5"        # anything `ssh` accepts (incl. config aliases)
# default_dir = "~/work"        # optional: prefill for new sessions
# claude_command = "claude"     # optional: binary path on the remote
# codex_command = "codex"
```

The `R` dialog and `vag remote add` write the `[[remotes]]` section for you.

Folder organization lives in `~/.local/share/vag/state.json` (`XDG_DATA_HOME` respected) —
written atomically and never touched by the agent CLIs.

## Keys

Defaults — every single-letter command (and every ctrl chord below) is rebindable
from the settings page (or the `[keys]` table); navigation keys are fixed.

| Key | Action |
|---|---|
| `j`/`k`, arrows | move |
| `enter` | open session / toggle folder / open settings (top row) |
| `tab` | switch focus tree ⇄ pane |
| `h` / `l` | collapse folder / focus pane |
| `ctrl-q` | detach from pane back to the tree (twice quickly = send literal ctrl-q) |
| `ctrl-e` | toggle the sidebar's visibility while a session pane has focus (view-only — doesn't change `ui.tree`) |
| `ctrl-h` | focus the tree from the pane (plain alias for `ctrl-q`, no double-press escape hatch) |
| `ctrl-l` | focus the active session's pane from the tree — unlike `enter`, never opens/activates the row under the cursor |
| `esc` | clear filter / back to full dashboard (sessions keep running) |
| `n` / `N` | new session / new folder |
| `F` | fork session |
| `s` | open a shell — local `$SHELL` here, or `ssh` on the selected machine |
| `R` | add a machine (SSH remote) |
| `e` | edit the tree as a buffer (vim keys — see Edit mode) |
| `m` | move session to folder |
| `r` | rename session or folder |
| `b` | bind a default directory to a folder / back to dashboard |
| `c` | set a session's accent color (tints its tree row and titlebar) |
| `d` | hide/unhide session |
| `H` | show hidden/archived sessions (dimmed) — press `d` on one to unhide |
| `g` | toggle git-repo scope (on by default inside a repo) |
| `A` | archive/unarchive (codex sessions, via `codex archive`) |
| `x` | delete folder / machine / session — codex sessions are truly deleted via `codex delete`; claude sessions are removed from the list (claude has no delete command); remote ones are dropped from vag only |
| `w` | close a session's process |
| `space` | collapse folder |
| `z` | zoom the active session full-screen |
| `1..9` | jump to open session |
| `,` | open settings |
| `end` | jump to the pinned `⚙ settings` row |
| `pgup`/`pgdn` | scroll the active pane (from tree focus) |
| `/` | filter |
| `?` | help |
| `q` | quit |

### tmux (vim-tmux-navigator users)

If your tmux binds `C-h`/`C-l` to `select-pane` (the vim-tmux-navigator setup), tmux
swallows those keys before vag sees them — the same reason the plugin special-cases vim.
Add `vag` to the pass-through detection in your `~/.tmux.conf`:

```tmux
# vim-tmux-navigator's is_vim check, with vag added alongside vim/fzf:
is_vim="ps -o state= -o comm= -t '#{pane_tty}' | grep -iqE '^[^TXZ ]+ +(\\S+\\/)?g?(view|l?n?vim?x?|fzf|vag)(diff)?$'"
bind-key -n 'C-h' if-shell "$is_vim" 'send-keys C-h' 'select-pane -L'
bind-key -n 'C-l' if-shell "$is_vim" 'send-keys C-l' 'select-pane -R'
```

(If you load the plugin via TPM, define these two bindings *after* the plugin line so
they win, or patch the plugin's `is_vim` variable the same way.)

vag speaks the navigator protocol back: when a focus key runs off vag's edge —
`ctrl-h` with the tree already focused, or `ctrl-l` with no session to focus — vag
forwards the motion to `tmux select-pane -L`/`-R`, so `C-h`/`C-l` walk seamlessly
across your tmux panes, vag's tree, and the embedded session as one continuum,
exactly like vim splits do. Outside tmux the edge presses are quiet no-ops. One
deliberate exception: `ctrl-l` while the *session pane* has focus is forwarded to
the agent (it's clear-screen in shells), not to tmux.

## Notes & limitations (v1)

- The embedded pane advertises a standard xterm-256color surface. Kitty-keyboard-protocol
  keys (e.g. claude's shift+enter) aren't forwarded — use `\` + enter, or zoom (`z`) in a
  kitty-capable terminal. Sync output (DEC 2026), theme queries (OSC 10/11), DA1/DSR are
  fully handled — claude and codex render flicker-free.
- Sessions end when vag exits (no daemon). `q` asks for confirmation if agents are running.
- Claude Code has no per-session delete CLI, so `d` hides sessions at the vag level;
  codex `A` uses the real `codex archive` so its own index stays consistent.
- Both CLIs' storage formats are officially internal; vag parses defensively and treats
  its scan as read-only, but a CLI update can temporarily blank titles until a fix.
- Remote codex sessions are attach-only from vag (still resumable on the box itself), and
  forking isn't available on remotes yet.

## How it works

Each open session is a real CLI process on a PTY; its output feeds
[alacritty_terminal](https://crates.io/crates/alacritty_terminal)'s emulator, and the
resulting grid is drawn as a [ratatui](https://ratatui.rs) pane. Keyboard input reaches
the agent as raw bytes — vag intercepts exactly one chord, the detach key. See `PLAN.md`
for the full architecture.

## Development

```sh
cargo test                       # 244+ unit tests (tempdir fixtures, no real data)
cargo test -- --ignored          # opt-in: read-only smoke against your real stores
cargo run --example spike -- -- claude   # fidelity harness: real CLI in a pane
cargo run --example spike -- --headless 10 -- codex   # headless grid dump
```

## License

[MIT](LICENSE)
