mod actions;
mod config;
mod dirscan;
mod discovery;
mod runtime;
mod state;
mod types;
mod ui;

use anyhow::Result;

use crate::config::Config;
use crate::types::AgentKind;

/// Print-style subcommands die quietly when their pipe closes (`vag list |
/// head`); the TUI keeps Rust's default ignore — its PTY writers handle
/// EPIPE/EIO explicitly and must never be killed by a signal.
fn default_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Global TUI flags, parsed out before command dispatch. Each maps to an
    // env override consumed by Config::load (usable directly as env vars).
    // SAFETY of set_var: single-threaded here — no other threads exist yet.
    if let Some(i) = args.iter().position(|a| a == "--icons") {
        if i + 1 >= args.len() {
            eprintln!("vag: --icons needs a value: nerd | ascii | auto");
            std::process::exit(2);
        }
        let v = args.remove(i + 1);
        args.remove(i);
        if !matches!(v.as_str(), "nerd" | "ascii" | "auto") {
            eprintln!("vag: invalid --icons `{v}` (want nerd | ascii | auto)");
            std::process::exit(2);
        }
        unsafe { std::env::set_var("VAG_ICONS", v) };
    }
    if let Some(i) = args.iter().position(|a| a == "--tree") {
        if i + 1 >= args.len() {
            eprintln!("vag: --tree needs a value: sidebar | float");
            std::process::exit(2);
        }
        let v = args.remove(i + 1);
        args.remove(i);
        if !matches!(v.as_str(), "sidebar" | "float") {
            eprintln!("vag: invalid --tree `{v}` (want sidebar | float)");
            std::process::exit(2);
        }
        unsafe { std::env::set_var("VAG_TREE", v) };
    }
    if let Some(i) = args.iter().position(|a| a == "--theme") {
        if i + 1 >= args.len() {
            eprintln!("vag: --theme needs a value: night | mocha | gruvbox | transparent");
            std::process::exit(2);
        }
        let v = args.remove(i + 1);
        args.remove(i);
        unsafe { std::env::set_var("VAG_THEME", v) };
    }
    if let Some(i) = args.iter().position(|a| a == "--pane") {
        if i + 1 >= args.len() {
            eprintln!("vag: --pane needs a value: border | titlebar");
            std::process::exit(2);
        }
        let v = args.remove(i + 1);
        args.remove(i);
        if !matches!(v.as_str(), "border" | "titlebar") {
            eprintln!("vag: invalid --pane `{v}` (want border | titlebar)");
            std::process::exit(2);
        }
        unsafe { std::env::set_var("VAG_PANE", v) };
    }
    if let Some(i) = args.iter().position(|a| a == "--float") {
        args.remove(i);
        unsafe { std::env::set_var("VAG_TREE", "float") };
    }
    if let Some(i) = args.iter().position(|a| a == "--edit") {
        args.remove(i);
        unsafe { std::env::set_var("VAG_EDIT", "1") };
    }
    if !args.is_empty() {
        default_sigpipe();
    }
    match args.first().map(String::as_str) {
        None => ui::app::run(),
        Some("--version") | Some("-V") | Some("version") => {
            println!("vag {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            Ok(())
        }
        Some("doctor") => doctor(),
        Some("list") => list(args.iter().any(|a| a == "--json")),
        Some("config") => config_cmd(),
        Some("remote") => remote_cmd(&args[1..]),
        Some(other) => {
            eprintln!("vag: unknown command `{other}`\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "vag {} — organize and drive your Claude Code & Codex sessions

vag is not a replacement for the agent CLIs: it launches the real
`claude` and `codex` binaries and embeds them, so every feature of
those tools keeps working.

USAGE:
    vag [OPTIONS] [COMMAND]

COMMANDS:
    (none)     open the dashboard TUI
    doctor     check agent CLIs, stores, and vag's own files
    list       print every discovered session (--json for machines)
    config     print the resolved configuration and file locations
    remote     manage ssh machines: add <name> <host> [--dir <d>] | list | remove <name>
    help       this help
    version    print version

DASHBOARD KEYS (press ? inside vag for the full list):
    enter open · n new · N folder · F fork · m move · r rename
    d hide · g repo-scope · e edit tree as a vim buffer · z zoom
    ctrl-q detach from a session pane back to the tree · q quit

OPTIONS (per-run overrides of ~/.config/vag/config.toml):
    --icons <nerd|ascii|auto>   icon set (default: `ui.icons` = ascii)
    --tree <sidebar|float>      tree placement (default: `ui.tree` = sidebar)
    --float                     shorthand for --tree float
    --pane <border|titlebar>    pane chrome: tmux-style title bar (default)
                                or a bordered box
    --theme <name>              color theme: night (default) | mocha |
                                gruvbox | transparent
    --edit                      start the tree in nvim edit mode

FILES:
    ~/.config/vag/config.toml       configuration (all keys optional)
    ~/.local/share/vag/state.json   folders & organization

Docs & source: https://github.com/OWNER/vag",
        env!("CARGO_PKG_VERSION")
    );
}

fn agent_version(program: &str) -> Option<String> {
    let out = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout);
    Some(v.lines().next().unwrap_or("").trim().to_string())
}

fn doctor() -> Result<()> {
    let cfg = Config::load()?;
    println!("vag {} — doctor\n", env!("CARGO_PKG_VERSION"));

    let mut any = false;
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        match actions::check_agent_available(&cfg, agent) {
            Ok(path) => {
                any = true;
                let ver = agent_version(&path).unwrap_or_else(|| "version unknown".into());
                println!("  {:<7} ✓ {}  ({})", agent.label(), path, ver);
            }
            Err(e) => println!("  {:<7} ✗ {}", agent.label(), e),
        }
    }
    if !any {
        println!(
            "\n  neither agent CLI was found — install at least one:\n    \
             claude code:  https://code.claude.com\n    \
             codex:        https://developers.openai.com/codex"
        );
    }

    println!();
    let scan = discovery::scan_all(&cfg);
    let claude_n = scan
        .sessions
        .iter()
        .filter(|m| m.key.agent == AgentKind::Claude)
        .count();
    let codex_n = scan.sessions.len() - claude_n;
    println!(
        "  claude store   {} ({} sessions)",
        cfg.claude_dir().display(),
        claude_n
    );
    println!(
        "  codex store    {} ({} sessions)",
        cfg.codex_home().display(),
        codex_n
    );
    for w in &scan.warnings {
        println!("  ⚠ {w}");
    }

    println!();
    let cfg_path = Config::config_path();
    println!(
        "  config         {}{}",
        cfg_path.display(),
        if cfg_path.exists() {
            ""
        } else {
            "  (missing — defaults active)"
        }
    );
    match crate::state::VagState::load() {
        Ok(st) => println!(
            "  state          {}  ({} folders, {} tracked sessions)",
            Config::data_dir().join("state.json").display(),
            st.folders.len(),
            st.sessions.len()
        ),
        Err(e) => println!("  state          ✗ {e:#}"),
    }

    // Configured ssh remotes with a reachability probe. VAG_DOCTOR_NO_PROBE
    // skips the (network-touching) probe — set it in tests/CI.
    if !cfg.remotes.is_empty() {
        println!();
        let skip_probe = std::env::var_os("VAG_DOCTOR_NO_PROBE").is_some();
        for r in &cfg.remotes {
            let probe = if skip_probe {
                None
            } else {
                Some(probe_remote(&r.host))
            };
            println!("{}", remote_doctor_line(&r.name, &r.host, probe));
        }
    }
    Ok(())
}

/// `ssh -o BatchMode=yes -o ConnectTimeout=3 <host> true` — Ok(()) when the
/// host answered, Err(short reason) otherwise. BatchMode keeps the probe
/// from hanging on a password prompt.
fn probe_remote(host: &str) -> std::result::Result<(), String> {
    let out = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            host,
            "true",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // ssh's actual failure reason is the last non-empty stderr line
            // (banners/warnings come first).
            let reason: String = stderr
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            if reason.is_empty() {
                Err(format!("ssh exited {}", o.status))
            } else {
                Err(reason)
            }
        }
        Err(e) => Err(format!("couldn't run ssh: {e}")),
    }
}

/// One doctor output line per remote; `probe` None = probing skipped.
fn remote_doctor_line(
    name: &str,
    host: &str,
    probe: Option<std::result::Result<(), String>>,
) -> String {
    let status = match probe {
        None => "(probe skipped)".to_string(),
        Some(Ok(())) => "✓ reachable".to_string(),
        Some(Err(reason)) => format!("✗ {reason}"),
    };
    format!("  remote {name:<12} {host:<24} {status}")
}

fn list(json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let st = crate::state::VagState::load().unwrap_or_default();
    let scan = discovery::scan_all(&cfg);
    if json {
        let items: Vec<serde_json::Value> = scan
            .sessions
            .iter()
            .map(|m| {
                let r = st.session(&m.key);
                serde_json::json!({
                    "agent": m.key.agent.label(),
                    "id": m.key.id,
                    "title": ui::dashboard::display_title(&st, m),
                    "cwd": m.cwd,
                    "last_activity": m.last_activity.map(|t| t.to_rfc3339()),
                    "archived": m.archived,
                    "hidden": r.map(|r| r.hidden).unwrap_or(false),
                    "folder": r.and_then(|r| r.folder.as_ref()).and_then(|id| {
                        st.folder(id).map(|f| f.name.clone())
                    }),
                    "transcript": m.source_path,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "sessions": items,
                "warnings": scan.warnings,
            }))?
        );
        return Ok(());
    }
    let now = chrono::Utc::now();
    for m in &scan.sessions {
        let title = ui::dashboard::display_title(&st, m);
        let title: String = title.chars().take(48).collect();
        println!(
            "{:<7} {:<8} {:<50} {:<20} {}",
            m.key.agent.label(),
            &m.key.id[..m.key.id.len().min(8)],
            title,
            m.project_label().chars().take(20).collect::<String>(),
            ui::dashboard::rel_time(m.last_activity, now),
        );
    }
    for w in &scan.warnings {
        eprintln!("⚠ {w}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `vag remote <add|list|remove>` — manage the [[remotes]] machines in
// config.toml without opening an editor (the same file the in-app R flow
// writes). Usage errors exit 2, like the rest of the CLI surface.

fn remote_usage() -> ! {
    eprintln!(
        "usage: vag remote add <name> <host> [--dir <d>]\n       \
         vag remote list\n       \
         vag remote remove <name>"
    );
    std::process::exit(2);
}

fn remote_cmd(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") => remote_add(&args[1..]),
        Some("list") => remote_list(),
        Some("remove") => remote_remove(&args[1..]),
        _ => remote_usage(),
    }
}

fn remote_add(args: &[String]) -> Result<()> {
    let (name, host, dir) = match parse_remote_add_args(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("vag: {msg}");
            remote_usage();
        }
    };
    let r = config::RemoteConfig {
        name: name.clone(),
        host: host.clone(),
        default_dir: dir,
        claude_command: String::new(),
        codex_command: String::new(),
    };
    let path = Config::config_path();
    config::add_remote_to_file(&path, &r)?;
    println!("added remote `{name}` ({host}) to {}", path.display());
    println!("open vag — your machine shows as a group; or: vag remote list");
    Ok(())
}

/// `(name, host, default_dir)` from `remote add` args; Err(message) on any
/// shape problem (caller prints it and exits 2).
fn parse_remote_add_args(
    args: &[String],
) -> std::result::Result<(String, String, Option<String>), String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                if i + 1 >= args.len() {
                    return Err("--dir needs a value".into());
                }
                dir = Some(args[i + 1].clone());
                i += 2;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            p => {
                positional.push(p);
                i += 1;
            }
        }
    }
    match positional[..] {
        [name, host] => Ok((name.to_string(), host.to_string(), dir)),
        _ => Err("remote add takes exactly <name> <host>".into()),
    }
}

fn remote_list() -> Result<()> {
    let cfg = Config::load()?;
    if cfg.remotes.is_empty() {
        println!(
            "no remotes configured — add one: vag remote add <name> <user@host> — \
             or press R inside vag"
        );
        return Ok(());
    }
    println!("{:<14} {:<28} DEFAULT DIR", "NAME", "HOST");
    for r in &cfg.remotes {
        println!(
            "{:<14} {:<28} {}",
            r.name,
            r.host,
            r.default_dir.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn remote_remove(args: &[String]) -> Result<()> {
    let [name] = args else {
        eprintln!("vag: remote remove takes exactly <name>");
        remote_usage();
    };
    let path = Config::config_path();
    if config::remove_remote_from_file(&path, name)? {
        println!("removed remote `{name}` from {}", path.display());
    } else {
        println!(
            "no remote named `{name}` in {} — see: vag remote list",
            path.display()
        );
    }
    Ok(())
}

fn config_cmd() -> Result<()> {
    let cfg = Config::load()?;
    println!("config file:  {}", Config::config_path().display());
    println!(
        "state file:   {}",
        Config::data_dir().join("state.json").display()
    );
    println!("claude store: {}", cfg.claude_dir().display());
    println!("codex store:  {}", cfg.codex_home().display());
    println!("\nresolved configuration:");
    print!("{}", toml::to_string_pretty(&cfg)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_remote_add_args_shapes() {
        assert_eq!(
            parse_remote_add_args(&s(&["gpu", "user@host"])),
            Ok(("gpu".into(), "user@host".into(), None))
        );
        assert_eq!(
            parse_remote_add_args(&s(&["gpu", "--dir", "~/work", "user@host"])),
            Ok(("gpu".into(), "user@host".into(), Some("~/work".into())))
        );
        assert!(parse_remote_add_args(&s(&["gpu"])).is_err());
        assert!(parse_remote_add_args(&s(&["a", "b", "c"])).is_err());
        assert!(parse_remote_add_args(&s(&["gpu", "user@host", "--dir"])).is_err());
        assert!(parse_remote_add_args(&s(&["gpu", "user@host", "--bogus"])).is_err());
    }

    #[test]
    fn remote_doctor_line_covers_all_probe_outcomes() {
        assert_eq!(
            remote_doctor_line("gpu", "user@gpu.example", None),
            "  remote gpu          user@gpu.example         (probe skipped)"
        );
        assert_eq!(
            remote_doctor_line("gpu", "user@gpu.example", Some(Ok(()))),
            "  remote gpu          user@gpu.example         ✓ reachable"
        );
        assert_eq!(
            remote_doctor_line("gpu", "h", Some(Err("Connection refused".into()))),
            "  remote gpu          h                        ✗ Connection refused"
        );
    }
}
