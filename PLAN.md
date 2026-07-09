# vag — implementation plan

A keyboard-driven TUI (lazygit-style) that organizes Claude Code and Codex CLI sessions into
folders and opens them as the **real** `claude`/`codex` TUIs in an embedded pane, with a
persistent sidebar. No tmux required (but works fine inside tmux).

Decisions (2026-07-08): Rust · embedded pane from day one · global folders with optional
directory binding · persistent background sessions (PTYs stay alive while vag runs).

---

## 1. Core principles (derived from research, see §8)

1. **Wrap, never reimplement.** Every UI reimplementor (opcode, vibe-kanban, Crystal) fights
   permanent drift against CLI updates, plus `claude -p` billing-policy risk. PTY-wrapping the
   real TUIs gets every new CLI feature for free.
2. **Session files never move.** Claude resume is cwd-scoped
   (`~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`); Codex's SQLite index stores absolute
   `rollout_path`. Folders are **vag-owned metadata** mapping `(agent, session_id) → folder`.
3. **Mutate via native CLI verbs, discover via store content.** Resume/fork/archive through
   `claude`/`codex` flags & subcommands. Discovery parses store contents defensively (both
   formats are officially internal and drift between releases).
4. **No screen-scraping for status.** It breaks on nearly every agent release (ccmanager's
   dominant bug class). Use process liveness + PTY output activity instead.

## 2. Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | Rust (edition 2024) | single static binary, `brew`/`cargo install` distribution |
| TUI | `ratatui` + `crossterm` | lazygit-style panels, keybinding table |
| PTY | `portable-pty` (wezterm) | cross-platform, `MasterPty::resize` → auto SIGWINCH |
| Terminal emulator | `alacritty_terminal` (pinned) behind an internal `Emulator` trait; evaluate `libghostty-vt` in the M0 spike | alacritty_terminal powers Zed's terminal; vte handles sync output (mode 2026). libghostty-vt is what turborepo migrated to for ratatui-embedded terminals. Trait keeps it swappable — both crates have API-stability caveats |
| Concurrency | std threads + crossbeam channels | one PTY-reader thread per session feeding its emulator; UI thread renders on damage/tick. No tokio needed |
| Codex index | `rusqlite` (bundled) | read-only snapshot of `state_5.sqlite` |
| Parsing | `serde`/`serde_json`, unknown-field tolerant | fixture tests pinned per CLI version |
| Own state | JSON file, atomic write | small data; no DB needed |

License hygiene: MIT references OK (ccmanager, agent-deck). **Do not copy code** from AGPL
projects (claude-squad, opcode). Check tuimux's license before borrowing.

## 3. Architecture

Single crate, modules (split into workspace later if warranted):

```
src/
  main.rs
  config.rs            // ~/.config/vag/config.toml
  state.rs             // folder tree + session→folder map, atomic JSON
  discovery/
    mod.rs             // SessionSource trait, unified SessionMeta
    claude.rs          // ~/.claude scanner
    codex.rs           // state_5.sqlite snapshot + jsonl fallback
  runtime/
    pty.rs             // spawn, resize, reader thread, input writer
    emulator.rs        // Emulator trait; alacritty_terminal impl
    session.rs         // SessionRuntime: PTY + emulator + child + activity clock
  ui/
    app.rs             // event loop, focus model, mode switching
    dashboard.rs       // full-screen folder/session browser
    sidebar.rs         // compact tree while a session is open
    pane.rs            // emulator-grid → ratatui::Buffer painter (damage-tracked)
    prompts.rs         // new-session / move / rename / new-folder dialogs
  actions.rs           // open/new/fork/move/archive, spawning argv builders
```

### Discovery — Claude backend
- Scan `~/.claude/projects/*/*.jsonl` (top level only; everything under `<uuid>/` subdirs is
  subagent/workflow/tool-result data, not sessions). Respect `CLAUDE_CONFIG_DIR`.
- Real cwd: **never decode dir names** (lossy: `/`, `.`, `_`, `-` all become `-`). Read the
  first record containing a `cwd` field (first line is often a timestamp-less `mode` /
  `file-history-snapshot` sidecar — skip those).
- Title precedence: last `custom-title` → last `ai-title` → first non-`isMeta` user message
  (skip `<command-name>`/`<local-command-caveat>` content). Read file head for meta + tail for
  titles; **never slurp whole files** (up to 30MB, perms 0600).
- `sessions-index.json` (per project dir): use as accelerator only — lazily created, goes
  stale; always `stat()` `fullPath` before trusting.
- Cache parsed metadata in vag state keyed by `(path, mtime, size)`.
- Running badge: `~/.claude/sessions/<pid>.json` registry (has `sessionId`, `cwd`, `name`;
  **no** `status` key on 2.1.197). Validate liveness via `kill(pid, 0)` — don't trust stale
  files; note `procStart` string is UTC while `ps lstart` is local, so avoid string comparison.
- Tolerate vanishing sessions: `cleanupPeriodDays` (default 30) deletes transcripts at claude
  startup. Format is officially internal — unknown record types are ignored, all fields optional.

### Discovery — Codex backend
- Primary: copy `~/.codex/state_5.sqlite` (+`-wal`/`-shm` if present) to scratch, query
  `threads`: `id, title, preview, first_user_message, cwd, rollout_path, updated_at, archived,
  thread_source, has_user_event`. Glob `state_*.sqlite` and pick the highest N (schema bumps
  rename the file). Respect `$CODEX_HOME`.
- **Filter noise**: default `thread_source IN ('user','') OR has_user_event=1` — on the
  reference machine 769/829 threads are `automation`.
- Fallback (sqlite unreadable): walk `sessions/**/rollout-*.jsonl` **and `*.jsonl.zst`**
  (compression is implemented behind a flag and will ship); line 1 = `session_meta` with
  `id`, `cwd`; first `event_msg/user_message` = preview; names joined from
  `session_index.jsonl` (last entry per id wins).

### Actions (argv builders)
| Action | Claude | Codex |
|---|---|---|
| Open/resume | child cwd = session's project path, `claude --resume <id>` (cwd-scoped lookup!) | `codex resume <uuid> --cd <stored cwd>` (id resolves from any cwd; working root must be set explicitly) |
| New | child cwd = target dir, `claude --session-id <vag-generated uuid> [-n <name>]` → id known upfront, folder mapping immediate | child cwd = target dir (or `--cd`), plain `codex`; learn id by watching for the new rollout file (match cwd + spawn time), confirm via sqlite |
| Fork | `claude --resume <id> --fork-session`; learn new id from `~/.claude/sessions/<pid>.json` (we know the child pid), fallback: watch project dir for new jsonl | `codex fork <uuid>` (**UUID-only**, no name); learn new id from new rollout / `session_meta.forked_from_id` |
| Archive | no per-session delete CLI → vag-level `hidden` flag only (v1) | shell out to `codex archive <uuid>` (keeps sqlite + Desktop app consistent — never move rollout files ourselves) |
| Rename | vag display-name override in state (uniform for both agents); claude also supports native `-n`/`--resume <name>` later | vag display-name override |
| Second attach to a running session | switch to the existing SessionRuntime — never spawn a duplicate (unforked double-resume interleaves into one transcript) | same |

New-session directory: folder's bound `default_dir` if set, else a path prompt with completion.
Expect codex's first-run **trust prompt** for untrusted dirs inside the pane (it's interactive —
fine, since we show the real TUI).

### Runtime & rendering
- `SessionRuntime` per opened session: PTY (sized to pane) + child + reader thread → emulator +
  `last_output_at` clock. Persistent: switching away keeps it running; all runtimes die with vag
  (no daemon in v1).
- Emulator must **answer terminal queries** (DA1, DSR/CPR, DECRQM — critically `?2026$p` sync
  output, OSC 10/11 theme colors) via alacritty_terminal's `Event::PtyWrite`/`ColorRequest`
  plumbing. Unanswered queries → wrong theme, startup delays; wrong answers → escape garbage in
  the composer (codex kitty-leak precedent).
- **Do not advertise kitty keyboard protocol** until fully implemented — codex falls back to
  legacy keys gracefully; half-support produces visible CSI-u garbage. Known v1 limitation:
  claude's shift+enter needs kitty; backslash+enter / option+enter still work.
- Child env: `TERM=xterm-256color`, `COLORTERM=truecolor`; never lie beyond what the emulator
  implements.
- Painter: damage-tracked grid → `ratatui::Buffer`; gate repaints on sync-update (2026) commit
  boundaries + a safety timeout (naive per-cell copy was a measured hotspot in turborepo).
- Resize: debounce; `MasterPty::resize` delivers SIGWINCH automatically; force a full repaint
  after resize/re-attach (claude "squished UI" bug; `CLAUDE_CODE_ALT_SCREEN_FULL_REPAINT` exists
  as a knob if needed).
- Scrollback: emulator-owned; pane scroll mode on `[` (v1: view-only copy mode).
- Mouse: none in v1 (avoids capture/translation complexity).

### Focus & keyboard model (modal, tuimux-style)
- **Pane focus**: forward *everything* to the child — including Ctrl+C — except one reserved,
  configurable hotkey (default `ctrl+q`): jump to sidebar. Double-press sends the literal byte.
- **Sidebar/dashboard focus**: `j/k` move · `enter` open · `n` new session · `N` new folder ·
  `F` fork · `m` move to folder · `r` rename · `d` archive/hide · `space` collapse ·
  `/` filter · `1..9` quick-jump to running sessions · `z` zoom · `?` help · `q` quit.
- **Zoom (`z`)**: full-screen handoff — leave alt screen, raw byte passthrough between real
  terminal and PTY (resized to full), hotkey returns. This is both a power feature and the
  fidelity escape hatch if a pane renders imperfectly. On return: reset leaked modes
  (`ESC[>0u` kitty pop, `ESC[>4;0m`, focus reporting off, bracketed paste off, DECAWM on —
  ccmanager's hard-won list), resize PTY back, force repaint.

### Status badges (no screen-scraping)
- `●` running + output in last ~3s (agent working) · `◌` running, quiet (likely waiting for
  input) · no badge: not attached. Claude cross-check via the `~/.claude/sessions/<pid>.json`
  registry. Later (opt-in): claude Notification/Stop hooks pinging a vag socket for precise
  "needs attention" states.

## 4. UX flow

1. `vag` → **dashboard**: full-screen folder tree with sessions (title, agent icon, project
   badge, relative time, status badge). Ungrouped sessions land in a virtual **Inbox**,
   grouped by project path.
2. `enter` on a session → layout switches to **sidebar (compact tree) + pane** running the real
   CLI; focus lands in the pane.
3. `ctrl+q` → focus sidebar; navigate; `enter` on another session switches panes (previous keeps
   running); `ctrl+q` from sidebar → back to full dashboard.
4. `n` → dialog: agent (claude/codex) → directory (folder default or completion prompt) →
   optional name → opens in pane, mapped to the current folder.
5. `F` on a session → fork → new session opens in pane, mapped to same folder as the source.
6. `m` → folder picker; `q` from dashboard quits (confirm if sessions are running).

## 5. Config & state

`~/.config/vag/config.toml` (XDG; all optional):
```toml
[keys]
detach = "ctrl-q"          # the one reserved hotkey

[agents.claude]
command = "claude"          # override binary / add default args
extra_args = []

[agents.codex]
command = "codex"
extra_args = []

[ui]
sidebar_width = 34

[behavior]
show_hidden = false
codex_show_automation = false   # thread_source != 'user'
claude_config_dir = ""          # honor CLAUDE_CONFIG_DIR override
codex_home = ""                 # honor CODEX_HOME override
```

`~/.local/share/vag/state.json` (atomic temp+rename writes):
```json
{
  "version": 1,
  "folders": [
    {"id": "f1", "name": "work", "parent": null, "default_dir": "/Users/me/Developer/passport-ts"}
  ],
  "sessions": {
    "claude:39212683-afb1-...": {"folder": "f1", "name_override": null, "hidden": false, "last_opened": "..."},
    "codex:019f2a4c-72af-...": {"folder": null, "hidden": false}
  }
}
```
Mappings whose sessions have vanished (claude 30-day cleanup, codex delete) are kept ~30 days
then GC'd, so a transient read failure doesn't destroy organization.

## 6. Milestones

**M0 — Fidelity spike (the gate, ~few days).** Standalone `examples/spike.rs`:
1. Scripted-PTY byte logger: capture exact startup query sequences of claude 2.1.197 and
   codex 0.142.5 (DA1, DECRQM list, OSC 10/11, XTGETTCAP, kitty `CSI ? u`, OSC 1337;File).
2. Render both CLIs in a minimal ratatui pane via alacritty_terminal; evaluate libghostty-vt on
   the same harness.
3. Exit criteria: no escape garbage in composer · no flicker during streaming (2026 honored) ·
   correct theme detection · typing/paste/Ctrl+C correct · resize clean · claude `/resume`
   picker works *inside* the pane (ccmanager #196 regression) · acceptable CPU during redraw
   storms. **If the spike fails both emulators, fall back to full-screen handoff for v1** (all
   other plumbing is unchanged — this is the pre-agreed plan B, not a redesign.)

**M1 — Core (usable dashboard).** Discovery (both backends, defensive parsers + fixture tests),
state store, config, dashboard UI, folder CRUD, move/hide/rename, open/new/fork spawning with
correct cwd handling, persistent runtimes, quit confirmation.

**M2 — Embedded pane.** Pane painter with damage/2026 gating, focus model + input forwarding,
resize pipeline, sidebar layout, session switching, status badges, scroll mode.

**M3 — Polish & ship v0.1.** Zoom/handoff mode + mode-reset hygiene, filter/search, help
overlay, codex archive integration, error surfaces (missing project dir on resume, CLI not
installed, version too old — codex fork needs ≥0.137), README, brew tap + `cargo install`, CI
(macOS + Linux).

Later: fs-watch live refresh (`notify`), claude hooks for precise attention badges, kitty
keyboard passthrough, mouse support, session search across content (`history.jsonl` sources),
worktree-aware workflows, codex `--remote`/app-server structured events.

## 7. Top risks & mitigations

| Risk | Mitigation |
|---|---|
| Embedded-pane fidelity (least-proven pattern; only tiny prior art) | M0 gate with hard exit criteria; two emulator candidates; zoom/handoff always available per session |
| Both storage formats officially internal, drift on any release | content-based discovery, unknown-field tolerance, per-version fixture tests, sqlite/index used as cache never truth |
| Kitty keyboard (codex) / shift+enter (claude) | don't advertise kitty in v1; document fallbacks; translation layer later |
| `state_5.sqlite` WAL/staleness/renames | snapshot-copy before read; `state_*` glob; jsonl-walk fallback path kept working |
| Sessions vanish (claude 30-day cleanup) | stat-before-trust, stale-mapping GC, graceful "session gone" UI |
| Terminal-mode bleed on zoom/detach | ccmanager's reset sequence on every detach; force repaint on re-attach |
| Double-attach transcript interleaving (claude) | vag switches to the existing runtime instead of re-spawning |
| First-party features absorbing vag's value (claude `--tmux`, codex desktop) | vag's moat is cross-agent folders + keyboard workflow; keep scope tight |

## 8. Research provenance (2026-07-08)

Verified against claude 2.1.197 + codex-cli 0.142.5 and live data in `~/.claude` / `~/.codex`;
CLI semantics from `--help`, official docs (code.claude.com/docs/en/sessions — resume is
cwd-scoped, format internal; developers.openai.com/codex), and source (openai/codex:
`fork_thread` creates new uuid+file with `forked_from_id`; resume opens rollout in append mode).
Prior art: ccmanager (PTY + headless-xterm + handoff, MIT), agent-deck (groups model, MIT,
tmux-based), tuimux (embedded ghostty panes, focus toggle), Crystal (folders UX, reimplemented
UI), opcode/vibe-kanban (reimplementation drift cautionary tales), Zed (ACP = reimplemented UI —
explicitly rejected for vag's core; the real-TUI constraint requires PTY wrapping). No existing
tool combines folders + both CLIs + no tmux + real embedded CLI + sidebar.
