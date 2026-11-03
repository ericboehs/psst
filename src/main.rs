//! psst — pi session switcher for tmux.
//!
//! Browse, inspect, and resume pi coding-agent sessions:
//!   - lists every session under ~/.pi/agent/sessions (respects PI_CODING_AGENT_DIR)
//!     plus `[hosts.<name>]` remotes mirrored over ssh
//!   - live dot for sessions with an active pi process (cwd + recent-write match,
//!     exact when the process cmdline carries --session)
//!   - Enter: attach to the running tmux session, or spawn `pi --session <file>`
//!     in a new tmux window (remote sessions open via `ssh -t <host> ...`)

use psst::config;
use psst::hosts;
use psst::running::{
    self, attach_remote_enrichment, detect, detect_remote, fetch_remote_enrichment, live_mark,
    map_running,
};
use psst::scan::{in_scope, resolve_scope, scan, sessions_root, SessionKey, SessionMeta};
use psst::tui;

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `print!` that exits quietly when stdout's reader goes away
/// (`psst --list | head`). Rust ignores SIGPIPE, so `print!` would panic
/// with "failed printing to stdout: Broken pipe" instead.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = write!(std::io::stdout().lock(), $($arg)*) {
            exit_on_closed_stdout(e);
        }
    }};
}

/// `println!` counterpart of [`out!`].
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($arg)*) {
            exit_on_closed_stdout(e);
        }
    }};
}

fn exit_on_closed_stdout(e: std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("psst: writing to stdout: {e}");
    std::process::exit(1);
}

#[derive(Debug)]
struct Args {
    list: bool,
    json: bool,
    refresh: bool,
    debug_running: bool,
    debug_focus: bool,
    local: bool,
    open: Option<String>,
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
        debug_focus: false,
        local: false,
        open: None,
        scope: None,
    };
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => parsed.list = true,
            "--json" => parsed.json = true,
            "--local" => parsed.local = true,
            "--open" => {
                let value = args.next().filter(|s| !s.is_empty() && !s.starts_with('-'));
                if parsed.open.is_some() || value.is_none() {
                    bail!("--open requires one exact session ID or absolute path");
                }
                parsed.open = value;
            }
            "--refresh" => parsed.refresh = true,
            "--debug-running" => parsed.debug_running = true,
            "--debug-focus" => parsed.debug_focus = true,
            "-h" | "--help" => {
                outln!(
                    "psst — pi session switcher for tmux\n\n\
                     usage: psst [.] [--list] [--json] [--refresh] [--debug-running] [--debug-focus]\n\n\
                     .                 only this project (git root, else cwd)\n\
                     --list            plain table (host column; pipe-friendly)\n\
                     --json            JSON array (implies list; adds host)\n\
                     --local           local sessions only; never contacts configured hosts\n\
                     --open ID|PATH    with --local: open and focus one exact session, without a TUI\n\
                     --refresh         rebuild the metadata cache (re-mirrors hosts unless --local)\n\
                     --debug-running   dump detected pi processes (local + hosts)\n\
                     --debug-focus     show which terminal tab each host resolves to\n\n\
                     no flags opens the TUI. /=search, ?=keys, enter=open\n\
                     config: ~/.config/psst/config.toml ([hosts.coop] ssh=\"coop\")"
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
    if parsed.open.is_some()
        && (!parsed.local
            || parsed.list
            || parsed.json
            || parsed.refresh
            || parsed.debug_running
            || parsed.debug_focus
            || parsed.scope.is_some())
    {
        bail!("use --local --open ID|PATH alone; handoff is local-only");
    }
    Ok(parsed)
}

fn scoped<'a>(sessions: &'a [SessionMeta], scope: Option<&Path>) -> Vec<&'a SessionMeta> {
    sessions
        .iter()
        .filter(|s| scope.map(|sc| in_scope(&s.cwd, sc)).unwrap_or(true))
        .collect()
}

type Hits = HashMap<SessionKey, running::RunningHit>;

fn print_list(sessions: &[&SessionMeta], hits: &Hits) {
    out!("{}", list_text(sessions, hits));
}

fn list_text(sessions: &[&SessionMeta], hits: &Hits) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "{:<2} {:<42} {:<8} {:<22} {:>7}\n",
        "", "title", "host", "project", "age"
    );
    for s in sessions {
        let hit = hits.get(&s.key());
        let mark = live_mark(hit);
        let age = chrono::Utc::now().timestamp() - s.last_user_or_activity();
        let title = hit
            .and_then(|h| h.peer_name.clone())
            .unwrap_or_else(|| s.title());
        let _ = writeln!(
            out,
            "{} {:<42} {:<8} {:<22} {:>5}",
            mark.glyph(),
            crate::list_helpers::trunc(&title, 40),
            crate::list_helpers::trunc(&s.host, 8),
            crate::list_helpers::trunc(&project_base(&s.cwd), 20),
            crate::list_helpers::fmt_age(age),
        );
    }
    out
}

fn print_json(sessions: &[&SessionMeta], hits: &Hits) {
    outln!(
        "{}",
        serde_json::to_string_pretty(&sessions_json(sessions, hits))
            .unwrap_or_else(|_| "[]".into())
    );
}

fn sessions_json(sessions: &[&SessionMeta], hits: &Hits) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let hit = hits.get(&s.key());
            let mark = live_mark(hit);
            serde_json::json!({
                "path": s.path,
                "host": s.host,
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
    let cfg = config::Config::load();
    let cfg = if args.local { cfg.local_only() } else { cfg };
    if let Some(selector) = args.open.as_deref() {
        let app = tui::App::new(cfg, None);
        let session = exact_local_session(&app.sessions, selector)?;
        // Never resume a second copy of a process that has no known tmux home.
        if app
            .running
            .get(&session.key())
            .is_some_and(|h| h.tmux.is_none())
        {
            bail!("session is running without a known tmux target; open it manually");
        }
        let action = app
            .open_session(session)
            .ok_or_else(|| anyhow::anyhow!("no open action"))?;
        return perform_handoff(action, app.config.terminal());
    }
    // Answered before the archive scan: "where would Enter send me" has
    // nothing to do with sessions and should stay instant.
    if args.debug_focus {
        print_focus_debug(&cfg);
        return Ok(());
    }
    if args.debug_running {
        let (_, hits, _) = load_sessions(&cfg, args.refresh)?;
        let mut procs = detect();
        for h in cfg.hosts() {
            match detect_remote(&h.ssh_target, &h.name) {
                Ok(mut rp) => procs.append(&mut rp),
                Err(e) => eprintln!("psst: {}: {}", h.name, e),
            }
        }
        outln!("pi processes: {}", serde_json::to_string_pretty(&procs)?);
        // SessionKey serializes as a struct; JSON map keys must be strings.
        let hits_dbg: HashMap<String, &running::RunningHit> = hits
            .iter()
            .map(|(k, v)| (format!("{}:{}", k.host, k.path.to_string_lossy()), v))
            .collect();
        outln!("hits: {}", serde_json::to_string_pretty(&hits_dbg)?);
        return Ok(());
    }

    if args.json || args.list {
        let (sessions, hits, host_errors) = load_sessions(&cfg, args.refresh)?;
        for e in &host_errors {
            eprintln!("psst: {}", e);
        }
        let shown = scoped(&sessions, args.scope.as_deref());
        if args.json {
            print_json(&shown, &hits);
        } else {
            print_list(&shown, &hits);
        }
        return Ok(());
    }

    // The TUI draws its first frame from the cache and does every ssh call on
    // a worker thread, so loading synchronously here would only make that
    // frame wait for the network — and then be thrown away and redone.
    let mut app = tui::App::new(cfg, args.scope);
    match tui::run(&mut app, args.refresh)? {
        Some(action) => perform(action),
        None => Ok(()),
    }
}

/// Explain the tab hop for every configured host.
///
/// This path fails invisibly by nature: a revoked Accessibility grant, a
/// terminal that is not running, and a tag that matched nothing all look
/// identical from the outside — you just end up somewhere unexpected. So
/// each input is printed rather than inferred.
fn print_focus_debug(cfg: &config::Config) {
    let app = cfg.terminal();
    outln!("terminal:   {}", app);
    outln!(
        "tmux-focus: {}",
        psst::focus::tmux_focus_bin()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not on PATH — remote opens use a new ssh window".to_string())
    );
    let titles = psst::focus::tab_titles(app);
    if titles.is_empty() {
        outln!(
            "tabs:       none visible (not macOS, {app} not running, or no Accessibility grant)"
        );
    } else {
        outln!("tabs:");
        for t in &titles {
            outln!("  {}", t);
        }
    }
    if cfg.hosts().is_empty() {
        outln!("hosts:      none configured");
    }
    for h in cfg.hosts() {
        let tags = psst::focus::host_tags(&h.name, &h.ssh_target, h.tab.as_deref());
        match psst::focus::match_tab(&titles, &tags) {
            Some(tag) => outln!("{}: focuses tab matching {:?}", h.name, tag),
            None => outln!(
                "{}: no matching tab — opens a new ssh window (tried: {})",
                h.name,
                tags.join(", ")
            ),
        }
    }
}

fn load_sessions(
    cfg: &config::Config,
    force: bool,
) -> Result<(Vec<SessionMeta>, Hits, Vec<String>)> {
    let mut sessions = scan(&sessions_root(), force)?;
    let mut errors = Vec::new();
    let mut synced_ok = std::collections::HashSet::new();
    for sync in hosts::sync_all(cfg.hosts(), force) {
        if sync.error.is_none() {
            synced_ok.insert(sync.name.clone());
        }
        if let Some(e) = sync.error {
            errors.push(e);
        }
        sessions.extend(sync.sessions);
    }

    let mut procs = detect();
    for h in cfg.hosts() {
        if !synced_ok.contains(&h.name) {
            continue;
        }
        match detect_remote(&h.ssh_target, &h.name) {
            Ok(rp) => procs.extend(rp),
            Err(e) => errors.push(format!("{}: {}", h.name, e)),
        }
    }
    let mut hits = map_running(&procs, &sessions, cfg.active_secs());
    // Second pass: ssh enrichment for remote hits (names/status).
    for h in cfg.hosts() {
        let remote_pids: Vec<u32> = hits
            .iter()
            .filter(|(k, _)| k.host == h.name)
            .map(|(_, hit)| hit.pid)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if remote_pids.is_empty() {
            continue;
        }
        let (names, summaries) = fetch_remote_enrichment(&h.ssh_target, &remote_pids);
        // Restrict to this host's hits.
        let mut sub: HashMap<SessionKey, running::RunningHit> = hits
            .iter()
            .filter(|(k, _)| k.host == h.name)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        attach_remote_enrichment(&mut sub, &names, &summaries);
        for (k, v) in sub {
            hits.insert(k, v);
        }
    }
    Ok((sessions, hits, errors))
}

/// Exact matching only: no fuzzy names, prefix UUIDs, arbitrary files, or remotes.
fn exact_local_session<'a>(sessions: &'a [SessionMeta], selector: &str) -> Result<&'a SessionMeta> {
    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.is_local() && (s.id == selector || s.path == Path::new(selector)))
        .collect();
    match matches.as_slice() {
        [s] => Ok(s),
        [] => bail!("local session not found; refresh the session list"),
        _ => bail!("session ID is ambiguous; use its exact absolute path"),
    }
}

/// The browser never implements terminal mechanics. Reuse the TUI action planner
/// and agent-notify's tmux-focus, but do not attempt an interactive tmux attach.
fn perform_handoff(action: tui::OpenAction, terminal: &str) -> Result<()> {
    use anyhow::Context;
    let focus =
        psst::focus::tmux_focus_bin().context("install tmux-focus from agent-notify first")?;
    let out = Command::new("tmux")
        .args(["list-clients", "-F", "#{client_activity}|#{session_id}"])
        .output()
        .context("reading local tmux clients")?;
    if !out.status.success() {
        bail!("open a local tmux client in your terminal first");
    }
    let clients = String::from_utf8_lossy(&out.stdout);
    let session = clients
        .lines()
        .filter_map(|l| l.split_once('|'))
        .filter_map(|(age, id)| age.parse::<u64>().ok().map(|age| (age, id)))
        .max_by_key(|(age, _)| *age)
        .map(|(_, id)| id)
        .context("open a local tmux client in your terminal first")?;
    let target = match action {
        tui::OpenAction::TmuxAttach { target, .. } => target,
        tui::OpenAction::TmuxNew {
            cwd, name, command, ..
        } => {
            let out = Command::new("tmux")
                .args([
                    "new-window",
                    "-d",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "-t",
                    session,
                    "-c",
                    &cwd,
                    "-n",
                    &name,
                    &command,
                ])
                .output()
                .context("creating resume window")?;
            if !out.status.success() {
                bail!("tmux could not create a resume window");
            }
            let pane = String::from_utf8(out.stdout)?.trim().to_string();
            if !pane.starts_with('%') {
                bail!("tmux did not return a pane target");
            }
            pane
        }
        _ => bail!("noninteractive handoff is local-only"),
    };
    let st = Command::new(focus).args([&target, terminal]).status()?;
    if !st.success() {
        bail!("tmux-focus failed; the resume window may already exist");
    }
    // tmux-focus's accessibility hop is best-effort; do not claim focus was verified.
    outln!("Terminal handoff requested.");
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn perform(action: tui::OpenAction) -> Result<()> {
    use std::os::unix::process::CommandExt;
    match action {
        tui::OpenAction::TmuxAttach { target, inside } => {
            if inside {
                let st = Command::new("tmux")
                    .args(["switch-client", "-t", &target])
                    .status();
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
            tmux_new_local(&cwd, &name, &command, inside)?;
            Ok(())
        }
        tui::OpenAction::SshAttach {
            ssh_target,
            target,
            inside,
        } => {
            // `target` is `session[:window]` on the remote.
            let (session, window) = match target.split_once(':') {
                Some((s, w)) => (s.to_string(), Some(w.to_string())),
                None => (target.clone(), None),
            };
            // Select the window first (detached), then attach lands on it.
            let remote = match window {
                Some(w) => format!(
                    "tmux select-window -t {} \\; attach -t {}",
                    sh_quote(&format!("{}:{}", session, w)),
                    sh_quote(&session)
                ),
                None => format!("tmux attach -t {}", sh_quote(&session)),
            };
            if inside {
                let label = format!("ssh-{}", session);
                let st = Command::new("tmux")
                    .args([
                        "new-window",
                        "-n",
                        &label,
                        "ssh",
                        "-t",
                        &ssh_target,
                        &remote,
                    ])
                    .status();
                match st {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => bail!("tmux new-window exited with {}", s),
                    Err(e) => bail!("tmux new-window: {}", e),
                }
            } else {
                let _ = Command::new("ssh")
                    .args(["-t", &ssh_target, &remote])
                    .exec();
                bail!("ssh attach failed for {}:{}", ssh_target, target);
            }
        }
        tui::OpenAction::SshNew {
            ssh_target,
            cwd,
            name,
            command,
            inside,
        } => {
            let remote = format!("cd {} && exec {}", sh_quote(&cwd), command);
            if inside {
                let st = Command::new("tmux")
                    .args(["new-window", "-n", &name, "ssh", "-t", &ssh_target, &remote])
                    .status();
                match st {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => bail!("tmux new-window exited with {}", s),
                    Err(e) => bail!("tmux new-window: {}", e),
                }
            } else {
                let mut sess = format!("psst-{}-{}", ssh_target, name);
                sess = sess.replace(['.', ':'], "-");
                for n in 2..50 {
                    let probe = sess.clone();
                    let exists = Command::new("tmux")
                        .args(["has-session", "-t", &probe])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !exists {
                        break;
                    }
                    sess = format!("psst-{}-{}-{}", ssh_target, name, n).replace(['.', ':'], "-");
                }
                let st = Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &sess,
                        "ssh",
                        "-t",
                        &ssh_target,
                        &remote,
                    ])
                    .status();
                if let Err(e) = st {
                    bail!("tmux new-session: {}", e);
                }
                let _ = Command::new("tmux")
                    .args(["attach-session", "-t", &sess])
                    .exec();
                bail!("tmux attach failed");
            }
        }
        tui::OpenAction::SshFocus {
            focus,
            target,
            inside,
        } => {
            // The session is already on screen somewhere; move to it rather
            // than stacking a second ssh beside the tab that has it.
            match psst::focus::focusable_tab(&focus.app, &focus.tags) {
                Some(tag) => {
                    psst::focus::focus_tab(&focus.app, &tag, Some((&focus.ssh_target, &target)))
                }
                // No tab holds this host (or no tmux-focus, or no
                // Accessibility grant): opening one beats doing nothing.
                None => perform(tui::OpenAction::SshAttach {
                    ssh_target: focus.ssh_target,
                    target,
                    inside,
                }),
            }
        }
        tui::OpenAction::SshFocusNew {
            focus,
            cwd,
            name,
            command,
            inside,
        } => {
            // Put the window in the host's own tmux: it then outlives the
            // tab, and lands in the session that tab is already showing.
            if let Some(tag) = psst::focus::focusable_tab(&focus.app, &focus.tags) {
                if let Ok(target) =
                    hosts::new_remote_window(&focus.ssh_target, &cwd, &name, &command)
                {
                    return psst::focus::focus_tab(
                        &focus.app,
                        &tag,
                        Some((&focus.ssh_target, &target)),
                    );
                }
                // Tab exists but the host has no tmux server to put a
                // window in; fall through to a local ssh window.
            }
            perform(tui::OpenAction::SshNew {
                ssh_target: focus.ssh_target,
                cwd,
                name,
                command,
                inside,
            })
        }
    }
}

fn tmux_new_local(cwd: &str, name: &str, command: &str, inside: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if inside {
        let st = Command::new("tmux")
            .args(["new-window", "-c", cwd, "-n", name, command])
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
            .args(["new-session", "-d", "-s", &sess, "-c", cwd, command])
            .status();
        if let Err(e) = st {
            bail!("tmux new-session: {}", e);
        }
        let _ = Command::new("tmux")
            .args(["attach-session", "-t", &sess])
            .exec();
        bail!("tmux attach failed");
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
    use psst::scan::LOCAL_HOST;

    fn meta(id: &str, cwd: &str, ts: i64) -> SessionMeta {
        SessionMeta {
            path: PathBuf::from(format!("/tmp/{}.jsonl", id)),
            host: LOCAL_HOST.to_string(),
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
            cpu: 0.0,
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
        let argv = [
            "--list",
            "--json",
            "--refresh",
            "--debug-running",
            "--debug-focus",
        ]
        .iter()
        .map(|s| s.to_string());
        let args = parse_args_from(argv).unwrap();
        assert!(args.list && args.json && args.refresh && args.debug_running && args.debug_focus);
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
    fn local_handoff_args_and_exact_selection() {
        let args = parse_args_from(["--local", "--json"].map(String::from)).unwrap();
        assert!(args.local && args.json);
        let args = parse_args_from(["--local", "--open", "abc"].map(String::from)).unwrap();
        assert_eq!(args.open.as_deref(), Some("abc"));
        for argv in [
            vec!["--open"],
            vec!["--open", "a"],
            vec!["--local", "--open", "a", "--json"],
            vec!["--local", "--open", "--refresh"],
        ] {
            assert!(parse_args_from(argv.into_iter().map(String::from)).is_err());
        }
        let mut sessions = vec![meta("abc", "/fixture", 1), meta("remote", "/fixture", 1)];
        sessions[1].host = "fixture-remote".into();
        assert_eq!(exact_local_session(&sessions, "abc").unwrap().id, "abc");
        assert!(exact_local_session(&sessions, "ab").is_err());
        assert!(exact_local_session(&sessions, "remote").is_err());
        assert!(exact_local_session(&sessions, "/tmp/not-scanned.jsonl").is_err());
        sessions.push(meta("abc", "/other", 2));
        sessions[2].path = "/tmp/other.jsonl".into();
        assert!(exact_local_session(&sessions, "abc").is_err());
        assert_eq!(
            exact_local_session(&sessions, "/tmp/other.jsonl")
                .unwrap()
                .cwd,
            "/other"
        );
    }

    #[test]
    fn json_idle_session_row() {
        let sessions = [meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let v = sessions_json(&refs, &std::collections::HashMap::new());
        let row = &v.as_array().unwrap()[0];
        assert_eq!(row["title"], "a-name");
        assert_eq!(row["host"], "local");
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
        let sessions = [meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let hits = std::collections::HashMap::from([(sessions[0].key(), hit("pi-x-1"))]);
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
    fn json_idle_agent_with_busy_children_remains_running_but_waiting() {
        let session = meta("fictional", "/fictional/project", 1000);
        let mut running = hit("fictional-agent");
        running.status = Some("idle".into());
        running.cpu = 80.0;
        let hits = std::collections::HashMap::from([(session.key(), running)]);
        let value = sessions_json(&[&session], &hits);
        assert_eq!(value[0]["running"], true);
        assert_eq!(value[0]["status"], "idle");
        assert_eq!(value[0]["mark"], "waiting");
    }

    #[test]
    fn list_text_has_header_and_rows() {
        let sessions = [meta("a", "/proj/a", 1000)];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let out = list_text(&refs, &std::collections::HashMap::new());
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.contains("title") && header.contains("project") && header.contains("age"));
        assert!(header.contains("host"));
        let row = lines.next().unwrap();
        assert!(row.contains("a-name") && row.contains('a') && row.contains("○"));
        assert!(row.contains("local"));
        assert!(lines.next().is_none());
    }

    #[test]
    fn list_text_shows_remote_host() {
        let mut m = meta("a", "/home/agent", 1000);
        m.host = "coop".to_string();
        let sessions = [m];
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let out = list_text(&refs, &std::collections::HashMap::new());
        assert!(out.lines().nth(1).unwrap().contains("coop"));
    }
}
