<h1 align="center">vag</h1>

<p align="center">
  <strong>One keyboard-driven dashboard for every Claude Code and Codex session on your machine.</strong>
</p>

<p align="center">
  <a href="https://github.com/cluely/vag/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/cluely/vag?style=flat-square&label=release"></a>
  <a href="https://github.com/cluely/vag/blob/main/LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square"></a>
  <img alt="macOS and Linux" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg?style=flat-square">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-dea584.svg?style=flat-square&logo=rust">
</p>

<p align="center">
  <a href="#installation">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#workflows">Workflows</a> ·
  <a href="#cli-reference">CLI</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#limitations">Limitations</a> ·
  <a href="#development">Development</a>
</p>

```text
┌─ vag ─────────────────────┬──────────────────────────────────┐
│   + new session           │ ✳ Claude Code                    │
│ ▾ work/                   │                                  │
│   ⠹ auth-fix    working 4m│ > fix the flaky auth test        │
│   ● parser-v2   done 2m   │   ⏺ Running tests…               │
│ ▸ experiments/            │                                  │
│                           │   the real agent TUI, running     │
│ n:new F:fork e:edit ?:help│   in an embedded terminal pane   │
└───────────────────────────┴──────────────────────────────────┘
```

_Illustrative split view; exact chrome depends on the theme, tree mode, and
terminal width._

`vag` puts your coding-agent sessions in one terminal workspace. Open a
session in the real agent UI, detach to the dashboard while it keeps running,
see which turns need attention, and inspect the working-tree diff associated
with each session.

It does not replace or reimplement either agent. `vag` launches your installed
`claude` and `codex` binaries in PTYs and keeps its folders, colors, and activity
metadata separate from their session stores. Think
[lazygit](https://github.com/jesseduffield/lazygit) for agent sessions.

## Highlights

- **One tree for every session** — discover Claude Code and Codex sessions
  automatically, group them into folders, filter them, or scope the view to the
  current Git repository. Agent session files are scanned read-only and never
  moved.
- **The real agent UI** — run the installed CLIs in embedded terminal panes.
  Detach to the dashboard, return later, keep several sessions open, or zoom one
  to the full terminal.
- **Attention at a glance** — see when a session is working, done, waiting for
  approval, waiting for input, idle, or exited. Native agent events are used
  when available, with PTY and transcript activity as fallbacks.
- **Session-scoped diffs** — press `D` to review the live diff for files a
  session touched. Use the built-in renderer or your existing
  [delta](https://github.com/dandavison/delta) setup.
- **Organize at terminal speed** — create sessions with a built-in fuzzy
  directory picker, or press `e` and edit the whole session tree as an
  [oil.nvim](https://github.com/stevearc/oil.nvim)-style text buffer.
- **Activity without inflated timers** — dashboard cards and a calendar heatmap
  record genuine streaming-output time, excluding idle time and approval waits.
- **Local and remote** — mix Claude, Codex, local shells, and SSH machines in the
  same dashboard.
- **Batteries included** — themes, mouse support, Nerd Font icons, a floating-tree
  layout, and in-app settings ship in one binary. The fuzzy finder is built in;
  `fzf` is not required.

## Installation

`vag` supports macOS and Linux on `x86_64` and `aarch64`. Install at least one
of [Claude Code](https://code.claude.com) or
[Codex](https://developers.openai.com/codex) first.

### Homebrew

```sh
brew install cluely/vag/vag
```

The Homebrew formula also installs `git-delta` for richer diff rendering.

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/cluely/vag/main/install.sh | sh
```

The script downloads a release binary when one is available and otherwise
builds from source with Cargo. The fallback requires a current stable Rust
toolchain. Set `VAG_INSTALL_DIR` to choose the destination.

### Cargo

```sh
cargo install --git https://github.com/cluely/vag
```

Building from source requires a current stable Rust toolchain. Git is used for
repository scoping and diffs; `delta` and SSH are optional.

After installing, check the setup:

```sh
vag doctor
```

## Quick start

Start `vag` inside a repository to see that project's sessions, or anywhere
else to see every discovered session:

```sh
cd ~/code/my-project
vag
```

| Key | Action |
|---|---|
| `enter` | Open or resume the selected session |
| `n` | Create a session |
| `ctrl-q` | Detach from the agent pane to the tree |
| `D` in the tree / `ctrl-g` anywhere | Toggle the active session's diff |
| `e` | Edit the session tree as a text buffer |
| `/` | Filter sessions |
| `z` | Zoom the active session |
| `,` | Open settings |
| `?` | Show the full in-app keymap |
| `q` | Quit |

> [!IMPORTANT]
> Sessions keep running while you move around inside `vag`, but `vag` is not a
> daemon. Quitting it ends the processes it launched.

## Workflows

### Run several agents without losing the thread

Press `n`, choose Claude or Codex, and pick a working directory with the
built-in fuzzy finder. While the agent works, press `ctrl-q` to return to the
tree and open another session. The row status updates live:

| Status | Meaning |
|---|---|
| `⠹ working 4m` | A turn is in flight |
| `● done 2m` | A turn completed while the session was out of view |
| `approval` / `input` | The agent needs attention |
| `◌ idle` | The session is ready |
| `✚ exited` | The child process ended |
| `▲ working 3m` | A Claude session is active in another terminal |

Unfiled Inbox sessions with no activity for more than three days move into a
collapsed **Archived** smart group. This only changes the dashboard view; it
does not edit either agent's store.

### Review a session's working-tree diff

Press `D` from the tree or `ctrl-g` from a pane. `vag` shows a collapsible file
tree beside a live diff anchored to the commit that was `HEAD` when the session
first opened.

By default, the diff is limited to files the agent's transcript says it
touched. Press `a` to widen it to the whole repository, `B` to re-anchor the
base to the current `HEAD`, or `r` to refresh. If `delta` is on `PATH`, its
syntax themes, line numbers, and side-by-side settings apply automatically;
otherwise `vag` uses its built-in unified renderer.

### Organize sessions by editing text

Press `e` in any tree to enter edit mode. Rename a session by editing its line,
move it with `dd` and `p`, create a folder with `o` and a trailing `/`, or copy
and paste a session to fork it. `:w` previews the actions before applying them.

<details>
<summary><strong>Edit mode reference</strong></summary>

- Normal mode: `h j k l`, arrows, `0`, `$`, `gg`, `G`, and counts for `j`/`k`.
- Enter Insert mode with `i`, `a`, `I`, or `A`.
- Edit with `x`, `dd`, `yy`, `p`, `P`, `o`, `O`, `u`, and `ctrl-r`.
- Save or leave with `:w`, `:q`, `:q!`, or `:wq`.
- Editing a line renames the session or folder. Folders keep a trailing `/`.
- `dd` then `p` moves a session; pasting on a folder drops it inside.
- Deleting a session line hides it. Deleting a folder re-parents its contents.
- `yy` then `p` duplicates a session as a fork.
- `o` followed by a name ending in `/` creates a folder.
- `enter` opens a session only when the buffer has no unsaved changes.

</details>

### Work across machines

Add an SSH machine from the UI with `R`, or from the shell:

```sh
vag remote add gpu-box user@host --dir '~/work'
```

Press `n` on a machine to create an agent session there, or `s` to open a plain
shell. Connections use your normal `ssh` command and `~/.ssh/config`; `vag`
does not store credentials.

Remote Claude sessions created by `vag` persist and can be reopened from the
dashboard. A remote Codex session works only while its original `vag` pane is
open; after that, resume it on the remote machine with `codex resume` because
`vag` cannot reattach it yet.

### Choose a layout

The default sidebar keeps the session tree beside the active agent. Floating
mode gives the agent the full width and opens the tree as an overlay when you
press the detach key:

```sh
vag --float
```

Use `b` from the floating tree to return to the full dashboard, or `e` to edit
the overlay directly.

## Keybindings

Press `?` inside `vag` for the contextual keymap. Dashboard command bindings
and its five control chords are rebindable from the settings page or
`config.toml`; navigation and view-specific scrolling keys stay fixed.

<details>
<summary><strong>Default keymap overview</strong></summary>

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move |
| `enter` | Open session, toggle folder, or activate settings |
| `tab` | Focus the pane from the tree; inside a pane, Tab goes to the child |
| `h` / `l` | Collapse folder / focus pane |
| `ctrl-q` | Detach to the tree; press twice quickly to send a literal `ctrl-q` |
| `ctrl-e` | Show or hide the sidebar while a pane has focus |
| `ctrl-h` / `ctrl-l` | Focus tree / focus active pane |
| `esc` | Clear filter, close the floating tree, or return to the dashboard |
| `n` / `N` | New session / new folder |
| `F` | Fork session |
| `s` | Open a local or remote shell |
| `R` | Add an SSH machine |
| `e` | Edit the tree as a buffer |
| `m` | Move session to a folder |
| `r` | Rename session or folder |
| `b` | Bind a default directory to a folder / return to dashboard |
| `c` | Set a session accent color |
| `d` | Hide or unhide session |
| `H` | Show hidden and archived sessions |
| `g` | Toggle current-repository scope |
| `A` | Archive or unarchive a Codex session with the Codex CLI |
| `x` | Remove the selected folder, machine, or session; behavior is provider-specific |
| `w` | Close a session process |
| `space` | Collapse or expand a folder |
| `z` | Zoom the active session |
| `D` in the tree / `ctrl-g` in any view | Toggle the active session's agent and diff tabs |
| `1`…`9` | Jump to an open session |
| `,` | Open settings |
| `end` | Jump to the bottom-pinned settings row |
| `pgup` / `pgdn` | Scroll pane history |
| Mouse wheel | Scroll the tree or pane under the pointer |
| `/` | Filter |
| `?` | Help |
| `q` | Quit |

`x` permanently deletes a local Codex session through the Codex CLI. It hides
a local Claude session, removes a remote session only from `vag`, and removes a
machine's configuration without deleting data on that machine.

Diff view adds these controls:

| Key | Action |
|---|---|
| `j` / `k` | Scroll |
| `ctrl-d` / `ctrl-u` | Scroll half a page |
| `ctrl-f` / `ctrl-b`, `pgup` / `pgdn` | Scroll a full page |
| `tab` | Switch between the file tree and diff |
| `enter` | Jump to the selected file |
| `space` | Collapse or expand a directory |
| `g` / `G` | Jump to top / bottom |
| `a` | Toggle agent-touched files / whole repository |
| `r` | Refresh |
| `B` | Re-anchor the base to current `HEAD` |
| `esc` | Return to the agent pane |

</details>

## CLI reference

| Command | Purpose |
|---|---|
| `vag` | Open the dashboard |
| `vag doctor` | Check agent CLIs, stores, local files, and configured remotes |
| `vag list [--json]` | List discovered sessions |
| `vag config` | Print file locations and resolved configuration |
| `vag remote add <name> <host> [--dir <path>]` | Add an SSH machine |
| `vag remote list` | List configured SSH machines |
| `vag remote remove <name>` | Remove an SSH machine |
| `vag --help` / `vag --version` | Print help / version |

Per-run UI overrides:

```text
--icons <ascii|nerd|auto>
--tree <sidebar|float>
--float
--pane <titlebar|border>
--theme <name>
--edit
```

## Configuration

The settings row is pinned at the bottom of the tree. Press `,` to open it and
change the theme, icons, pane style, tree mode, sidebar width, launch behavior,
or any rebindable key. Changes preview live and are written without discarding
comments in the config file.

For manual configuration, create
`~/.config/vag/config.toml` (`XDG_CONFIG_HOME` is respected). Every field is
optional:

```toml
[ui]
theme = "mocha"          # night, mocha, gruvbox, dracula, nord, onedark,
                         # solarized, rose-pine, or transparent
tree = "float"           # sidebar or float
pane = "titlebar"        # titlebar or border
icons = "auto"           # ascii, nerd, or auto
sidebar_width = 38
mouse = true
edit_default = false

[keys]
detach = "ctrl-q"
toggle_sidebar = "ctrl-e"
focus_tree = "ctrl-h"
focus_pane = "ctrl-l"
toggle_diff = "ctrl-g"
new_session = "n"
diff = "D"
settings = ","

[agents.claude]
command = "claude"
extra_args = []

[agents.codex]
command = "codex"
extra_args = []

[diff]
use_delta = true
delta_args = []           # for example: ["--side-by-side"]

[behavior]
repo_scope = true
show_hidden = false
codex_show_automation = false
```

Override individual theme colors with `red`, `orange`, `yellow`, `green`,
`cyan`, `blue`, `magenta`, `pink`, or a `#rrggbb` value:

```toml
[theme]
bg = "#1a1b26"
fg = "#c0caf5"
surface = "#24283b"
sidebar_bg = "#1f2231"
sel = "#3b4261"
dim = "#565f89"
accent = "cyan"
info = "#7dcfff"
pane_fg = "#c0caf5"
pane_bg = "#1a1b26"
```

Set `pane_fg` and `pane_bg` together, using hex colors, to override the embedded
terminal palette.

Remotes can also be declared directly:

```toml
[[remotes]]
name = "gpu-box"
host = "user@example.com"
default_dir = "~/work"
# claude_command = "claude"
# codex_command = "codex"
```

Configuration and local state live at:

| Data | Default path |
|---|---|
| Configuration | `~/.config/vag/config.toml` |
| Folder and session metadata | `~/.local/share/vag/state.json` |
| Activity totals and heatmap | `~/.local/share/vag/activity_stats.json` |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` change those roots. Folder organization
and activity stats are written atomically. Discovery reads agent stores without
writing to them; explicit Codex archive and delete requests are delegated to
the native Codex CLI.

Flags take precedence over the file. The equivalent environment overrides are
`VAG_ICONS`, `VAG_TREE`, `VAG_PANE`, `VAG_THEME`, and `VAG_EDIT`. Native
`CLAUDE_CONFIG_DIR` and `CODEX_HOME` overrides are also respected.

## Terminal integration

Mouse support works in tmux without additional configuration. Because terminal
mouse reporting takes over drag events, hold Shift while dragging to select
text, or set `ui.mouse = false`.

<details>
<summary><strong>vim-tmux-navigator setup</strong></summary>

If tmux binds `C-h` and `C-l` to `select-pane`, it consumes those keys before
`vag` sees them. Add `vag` to the plugin's pass-through check and place these
bindings after the plugin line:

```tmux
is_vim="ps -o state= -o comm= -t '#{pane_tty}' | grep -iqE '^[^TXZ ]+ +(\\S+\\/)?g?(view|l?n?vim?x?|fzf|vag)(diff)?$'"
bind-key -n 'C-h' if-shell "$is_vim" 'send-keys C-h' 'select-pane -L'
bind-key -n 'C-l' if-shell "$is_vim" 'send-keys C-l' 'select-pane -R'
```

When `ctrl-h` is pressed with the tree already focused, `vag` forwards the move
left to tmux. It forwards `ctrl-l` to the right only when there is no active
session to focus; inside an active agent pane, `ctrl-l` reaches the child.

</details>

## How it works

Each open session is a real CLI process running on a PTY. Its output is fed to
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal), then the
emulated grid is rendered by [`ratatui`](https://ratatui.rs). Input stays as raw
terminal bytes; `vag` handles only its configured navigation, scrolling, and
view shortcuts around the child process.

Discovery is defensive because both agents' on-disk formats are internal. A
failed backend produces a warning instead of taking down the other agent or the
dashboard.

## Limitations

- `vag` has no daemon. Quitting ends the sessions it launched.
- The embedded pane exposes an xterm-256color surface. Kitty keyboard protocol
  sequences are not forwarded, so use an agent's alternate binding or zoom with
  `z` when a key depends on that protocol.
- Claude Code has no per-session delete command. `vag` can hide Claude sessions;
  Codex archive and delete actions use the native Codex CLI.
- Claude and Codex storage formats are internal. A CLI update may require a
  compatibility fix even though discovery is read-only and defensive.
- Remote Codex sessions cannot be reattached from `vag`; after the original
  pane closes, resume them on the remote machine. Remote sessions also cannot
  currently be forked from `vag`.

## Development

```sh
git clone https://github.com/cluely/vag.git
cd vag
cargo test
cargo run
```

Read-only smoke tests against the agent stores on your machine are opt-in:

```sh
cargo test -- --ignored
```

The terminal fidelity harness can run an agent in isolation or dump its
headless terminal grid:

```sh
cargo run --example spike -- -- claude
cargo run --example spike -- --headless 10 -- codex
```

Issues and pull requests are welcome. Please run `cargo test` before submitting
a change.

## Inspiration

`vag` borrows interaction ideas from tools that make complicated terminal
workflows feel direct: [lazygit](https://github.com/jesseduffield/lazygit),
[`fzf`](https://github.com/junegunn/fzf),
[`oil.nvim`](https://github.com/stevearc/oil.nvim), and
[Neovim](https://github.com/neovim/neovim).

## License

[MIT](LICENSE) © 2026 Guilherme Garibaldi
