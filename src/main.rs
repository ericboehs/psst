//! psst — pi session tui.
//!
//! Browse, inspect, and resume pi coding-agent sessions:
//!   - lists every session under ~/.pi/agent/sessions (respects PI_CODING_AGENT_DIR)
//!   - live dot for sessions with an active pi process (cwd + recent-write match,
//!     exact when the process cmdline carries --session)
//!   - Enter: attach to the running tmux session, or spawn `pi --session <file>`
//!     in a new tmux window

use psst::config;
use psst::running::{self, detect, map_running};
use psst::scan::{scan, sessions_root, SessionMeta};
use psst::tui;

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::Command;

struct Args {
    list: bool,
    refresh: bool,
    debug_running: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        list: false,
        refresh: false,
        debug_running: false,
    };
    let it = std::env::args().skip(1);
        for a in it {
        match a.as_str() {
            "--list" => args.list = true,
            "--refresh" => args.refresh = true,
            "--debug-running" => args.debug_running = true,
            "-h" | "--help" => {
                println!(
                    "psst — pi session tui\n\n\
                     usage: psst [--list] [--refresh] [--debug-running]\n\n\
                     no flags opens the TUI. keys: /=search a=active j/k tab=group ^S=sort ^D=detail ^P=prompts enter=open q=quit\n\
                     config: ~/.config/psst/config.toml"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {} (try --help)", other),
        }
    }
    Ok(args)
}

fn print_list(sessions: &[SessionMeta], hits: &std::collections::HashMap<PathBuf, running::RunningHit>) {
    println!(
        "{:<2} {:<50} {:<28} {:>7}",
        "", "title", "project", "age"
    );
    for s in sessions {
        let hit = hits.get(&s.path);
        let run = if hit.is_some() { "* " } else { "  " };
        let age = chrono::Utc::now().timestamp() - s.last_user_or_activity();
        let title = hit
            .and_then(|h| h.peer_name.clone())
            .unwrap_or_else(|| s.title());
        let summary = hit
            .and_then(|h| h.summary.as_deref())
            .map(|sum| format!("  {}", crate::list_helpers::trunc(sum, 50)))
            .unwrap_or_default();
        println!(
            "{} {:<50} {:<28} {:>5}{}",
            run,
            crate::list_helpers::trunc(&title, 48),
            crate::list_helpers::trunc(&project_base(&s.cwd), 26),
            crate::list_helpers::fmt_age(age),
            summary,
        );
    }
}

fn project_base(cwd: &str) -> String {
    let c = cwd.trim_end_matches('/');
    c.rsplit('/').next().unwrap_or(c).to_string()
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let (sessions, hits) = load_sessions(args.refresh)?;

    if args.debug_running {
        let procs = detect();
        println!("pi processes: {}", serde_json::to_string_pretty(&procs)?);
        println!("hits: {}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }

    if args.list {
        print_list(&sessions, &hits);
        return Ok(());
    }

    let cfg = config::Config::load();
    let mut app = tui::App::new(sessions, cfg);
    match tui::run(&mut app)? {
        Some(action) => perform(action),
        None => Ok(()),
    }
}

fn load_sessions(force: bool) -> Result<(Vec<SessionMeta>, std::collections::HashMap<PathBuf, running::RunningHit>)> {
    let sessions = scan(&sessions_root(), force)?;
    let procs = detect();
    let cfg = config::Config::load();
    let hits = map_running(&procs, &sessions, cfg.active_secs());
    Ok((sessions, hits))
}

fn perform(action: tui::OpenAction) -> Result<()> {
    use std::os::unix::process::CommandExt;
    match action {
        tui::OpenAction::TmuxAttach { target, inside } => {
            if inside {
                let st = Command::new("tmux").args(["switch-client", "-t", &target]).status();
                match st {
                    Ok(s) if s.success() => Ok(()),
                    _ => bail!("tmux switch-client failed for {}", target),
                }
            } else {
                // replace this process with an attach
                let _ = Command::new("tmux")
                    .args(["attach-session", "-t", &target])
                    .exec();
                bail!("tmux attach failed for {}", target);
            }
        }
        tui::OpenAction::TmuxNew {
            cwd,
            name,
            command,
            inside,
        } => {
            if inside {
                let st = Command::new("tmux")
                    .args(["new-window", "-c", &cwd, "-n", &name, &command])
                    .status();
                match st {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => bail!("tmux new-window exited with {}", s),
                    Err(e) => bail!("tmux new-window: {}", e),
                }
            } else {
                // unique session name; if it exists, suffix it
                let mut sess = format!("psst-{}", name);
                for n in 2..50 {
                    let exists = Command::new("tmux")
                        .args(["has-session", "-t", &sess])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !exists {
                        break;
                    }
                    sess = format!("psst-{}-{}", name, n);
                }
                let st = Command::new("tmux")
                    .args(["new-session", "-d", "-s", &sess, "-c", &cwd, &command])
                    .status();
                if let Err(e) = st {
                    bail!("tmux new-session: {}", e);
                }
                let _ = Command::new("tmux").args(["attach-session", "-t", &sess]).exec();
                bail!("tmux attach failed");
            }
        }
    }
}

mod list_helpers {
    pub fn trunc(s: &str, w: usize) -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= w {
            return s.to_string();
        }
        let mut out: String = chars[..w.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }

    pub fn fmt_age(secs: i64) -> String {
        let s = secs.max(0);
        if s < 60 {
            format!("{}s", s)
        } else if s < 3600 {
            format!("{}m", s / 60)
        } else if s < 86_400 {
            format!("{}h", s / 3600)
        } else if s < 86_400 * 30 {
            format!("{}d", s / 86_400)
        } else {
            format!("{}mo", s / (86_400 * 30))
        }
    }
}