//! psst — pi session tui.
//!
//! Browse, inspect, and resume pi coding-agent sessions:
//!   - lists every session under ~/.pi/agent/sessions (respects PI_CODING_AGENT_DIR)
//!   - live dot for sessions with an active pi process (cwd + recent-write match,
//!     exact when the process cmdline carries --session)
//!   - Enter: attach to the running tmux session, or spawn `pi --session <file>`
//!     in a new tmux window

use psst::config;
use psst::running::{self, detect, live_mark, map_running};
use psst::scan::{in_scope, resolve_scope, scan, sessions_root, SessionMeta};
use psst::tui;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
struct Args {
    list: bool,
    json: bool,
    refresh: bool,
    debug_running: bool,
    scope: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut parsed = Args {
        list: false,
        json: false,
        refresh: false,
        debug_running: false,
        scope: None,
    };
    for a in args {
        match a.as_str() {
            "--list" => parsed.list = true,
            "--json" => parsed.json = true,
            "--refresh" => parsed.refresh = true,
            "--debug-running" => parsed.debug_running = true,
            "-h" | "--help" => {
                println!(
                    "psst — pi session tui\n\n\
                     usage: psst [.] [--list] [--json] [--refresh] [--debug-running]\n\n\
                     .                 only this project (git root, else cwd)\n\
                     --list            plain table\n\
                     --json            JSON array (implies list)\n\
                     --refresh         rebuild the metadata cache\n\
                     --debug-running   dump detected pi processes\n\n\
                     no flags opens the TUI. /=search, ?=keys, enter=open\n\
                     config: ~/.config/psst/config.toml"
                );
                std::process::exit(0);
            }
            other => {
                let p = PathBuf::from(other);
                if other == "." || p.is_dir() {
                    parsed.scope = Some(resolve_scope(&p));
                } else {
                    bail!("unknown argument: {} (try --help)", other);
                }
            }
        }
    }
    Ok(parsed)
}

fn scoped<'a>(sessions: &'a [SessionMeta], scope: Option<&Path>) -> Vec<&'a SessionMeta> {
    sessions
        .iter()
        .filter(|s| scope.map(|sc| in_scope(&s.cwd, sc)).unwrap_or(true))
        .collect()
}

fn print_list(sessions: &[&SessionMeta], hits: &std::collections::HashMap<PathBuf, running::RunningHit>) {
    print!("{}", list_text(sessions, hits));
}

fn list_text(
    sessions: &[&SessionMeta],
    hits: &std::collections::HashMap<PathBuf, running::RunningHit>,
) -> String {
    use std::fmt::Write;
    let mut out = format!("{:<2} {:<50} {:<28} {:>7}\n", "", "title", "project", "age");
    for s in sessions {
        let hit = hits.get(&s.path);
        let mark = live_mark(hit);
        let age = chrono::Utc::now().timestamp() - s.last_user_or_activity();
        let title = hit
            .and_then(|h| h.peer_name.clone())
            .unwrap_or_else(|| s.title());
        let _ = writeln!(
            out,
            "{} {:<50} {:<28} {:>5}",
            mark.glyph(),
            crate::list_helpers::trunc(&title, 48),
            crate::list_helpers::trunc(&project_base(&s.cwd), 26),
            crate::list_helpers::fmt_age(age),
        );
    }
    out
}

fn print_json(sessions: &[&SessionMeta], hits: &std::collections::HashMap<PathBuf, running::RunningHit>) {
    println!(
        "{}",
        serde_json::to_string_pretty(&sessions_json(sessions, hits))
            .unwrap_or_else(|_| "[]".into())
    );
}

fn sessions_json(
    sessions: &[&SessionMeta],
    hits: &std::collections::HashMap<PathBuf, running::RunningHit>,
) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let hit = hits.get(&s.path);
            let mark = live_mark(hit);
            serde_json::json!({
                "path": s.path,
                "id": s.id,
                "cwd": s.cwd,
                "name": s.name,
                "title": hit.and_then(|h| h.peer_name.clone()).unwrap_or_else(|| s.title()),
                "running": hit.is_some(),
                "pid": hit.map(|h| h.pid),
                "tmux": hit.and_then(|h| h.tmux.as_ref()).map(|t| {
                    format!(
                        "{}{}",
                        t.session,
                        t.window.map(|w| format!(":{}", w)).unwrap_or_default()
                    )
                }),
                "status": hit.and_then(|h| h.status.clone()),
                "mark": mark.as_str(),
                "forked": s.parent_session.is_some(),
                "last_user": s.last_user,
                "last_activity": s.last_activity,
                "age_secs": now - s.last_user_or_activity(),
                "messages": s.messages,
                "cost_total": s.cost_total,
            })
        })
        .collect();
    serde_json::Value::Array(rows)
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

    let shown = scoped(&sessions, args.scope.as_deref());
    if args.json {
        print_json(&shown, &hits);
        return Ok(());
    }
    if args.list {
        print_list(&shown, &hits);
        return Ok(());
    }

    let cfg = config::Config::load();
    let mut app = tui::App::new(sessions, cfg, args.scope);
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

#[cfg(test)]
mod tests {
    use super::*;
    use psst::running::{RunningHit, TmuxTarget};

    fn meta(id: &str, cwd: &str, ts: i64) -> SessionMeta {
        SessionMeta {
            path: PathBuf::from(format!("/tmp/{}.jsonl", id)),
            id: id.into(),
            cwd: cwd.into(),
            created: Some(ts),
            last_activity: Some(ts),
            last_user: Some(ts),
            name: Some(format!("{}-name", id)),
            first_prompt: None,
            last_prompt: None,
            model: None,
            cost_total: 1.5,
            tokens_in: 10,
            tokens_out: 20,
            messages: 7,
            branches: 0,
            parent_session: None,
            mtime: ts,
            size: 100,
        }
    }

    fn hit(peer: &str) -> RunningHit {
        RunningHit {
            pid: 4242,
            tmux: Some(TmuxTarget {
                session: "s".into(),
                window: Some(3),
                attention: false,
            }),
            direct: true,
            summary: None,
            status: Some("thinking".into()),
            peer_name: Some(peer.into()),
        }
    }

    #[test]
    fn trunc_short_exact_and_long() {
        assert_eq!(list_helpers::trunc("abc", 5), "abc");
        assert_eq!(list_helpers::trunc("abcde", 5), "abcde");
        assert_eq!(list_helpers::trunc("hello world", 5), "hell…");
        assert_eq!(list_helpers::trunc("héllo", 4), "hél…");
    }

    #[test]
    fn fmt_age_boundaries() {
        let cases = [
            (-5, "0s"),
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (86399, "23h"),
            (86400, "1d"),
            (29 * 86400, "29d"),
            (30 * 86400, "1mo"),
            (300 * 86400, "10mo"),
        ];
        for (secs, want) in cases {
            assert_eq!(list_helpers::fmt_age(secs), want, "secs={}", secs);
        }
    }

    #[test]
    fn project_base_strips_path() {
        assert_eq!(project_base("/a/b/c"), "c");
        assert_eq!(project_base("/a/b/"), "b");
        assert_eq!(project_base("solo"), "solo");
    }

    #[test]
    fn scoped_filters_to_scope_subtree() {
        let sessions = vec![
            meta("x", "/proj/a", 1000),
            meta("y", "/proj/a/sub", 1000),
            meta("z", "/other", 1000),
        ];
        assert_eq!(scoped(&sessions, None).len(), 3);
        let shown = scoped(&sessions, Some(Path::new("/proj/a")));
        let ids: Vec<&str> = shown.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["x", "y"]);
    }

    #[test]
    fn parse_args_flags_scope_and_errors() {
        let args = parse_args_from(Vec::<String>::new()).unwrap();
        assert!(!args.list && !args.json && !args.refresh && !args.debug_running);
        assert_eq!(args.scope, None);
        let argv = ["--list", "--json", "--refresh", "--debug-running"]
            .iter()
            .map(|s| s.to_string());
        let args = parse_args_from(argv).unwrap();
        assert!(args.list && args.json && args.refresh && args.debug_running);
        let err = parse_args_from(["--bogus".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"));
        // an existing dir resolves through canonicalization
        let tmp = std::env::temp_dir();
        let args = parse_args_from([tmp.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(args.scope, Some(std::fs::canonicalize(&tmp).unwrap()));
        // `.` walks up to the enclosing git root (tests run in the repo)
        let args = parse_args_from([".".to_string()]).unwrap();
        let scope = args.scope.unwrap();
        assert!(scope.join(".git").exists());
    }

    #[test]
    fn json_idle_session_row() {
        let sessions = vec![meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let v = sessions_json(&refs, &std::collections::HashMap::new());
        let row = &v.as_array().unwrap()[0];
        assert_eq!(row["title"], "a-name");
        assert_eq!(row["running"], false);
        assert!(row["pid"].is_null());
        assert!(row["tmux"].is_null());
        assert!(row["status"].is_null());
        assert_eq!(row["mark"], "exited");
        assert_eq!(row["forked"], false);
        assert_eq!(row["messages"], 7);
        assert_eq!(row["cost_total"], 1.5);
        assert_eq!(row["cwd"], "/proj/a");
    }

    #[test]
    fn json_running_session_prefers_peer_and_tmux() {
        let sessions = vec![meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let hits = std::collections::HashMap::from([(sessions[0].path.clone(), hit("pi-x-1"))]);
        let v = sessions_json(&refs, &hits);
        let row = &v.as_array().unwrap()[0];
        assert_eq!(row["title"], "pi-x-1");
        assert_eq!(row["running"], true);
        assert_eq!(row["pid"], 4242);
        assert_eq!(row["tmux"], "s:3");
        assert_eq!(row["status"], "thinking");
        assert_eq!(row["mark"], "working");
    }

    #[test]
    fn list_text_has_header_and_rows() {
        let sessions = vec![meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let out = list_text(&refs, &std::collections::HashMap::new());
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.contains("title") && header.contains("project") && header.contains("age"));
        let row = lines.next().unwrap();
        assert!(row.contains("a-name") && row.contains('a') && row.contains("○"));
        assert!(lines.next().is_none());
    }
}