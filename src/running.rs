//! Detect running pi processes and map them to session files + tmux targets.
//!
//! Zero-config approach (per design): pi appends to session files with
//! open/write/close (`appendFileSync`), so `lsof` only catches a file mid-write.
//! Instead we:
//!   1. find pi-like processes via `ps`,
//!   2. resolve each process's cwd via `lsof`,
//!   3. find its tmux session/window via the inherited `TMUX_PANE` env var
//!      (exact), falling back to walking the parent chain to a tmux pane pid,
//!   4. pin a session file: exact when the cmdline carries `--session`;
//!      otherwise the session whose `created` timestamp matches the process
//!      start (a bare `pi` writes its header at launch); falling back to the
//!      newest unclaimed session in that cwd. Idle agents at a prompt do not
//!      write, so there is no recency gate — a live process is enough.
//!      Parallel agents in one project claim distinct files (newest-first).

use crate::scan::SessionMeta;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct TmuxTarget {
    pub session: String,
    pub window: Option<u32>,
    /// Window still has `@special_activity` (tmux-attention); cleared on focus.
    #[serde(default)]
    pub attention: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PiProc {
    pub pid: u32,
    pub ppid: u32,
    pub cwd: Option<PathBuf>,
    pub cmd: String,
    /// `--session <arg>` from the cmdline, if present.
    pub session_arg: Option<String>,
    /// `-c` / `--continue` on the cmdline: owns the newest session in cwd.
    pub continue_flag: bool,
    /// Epoch seconds when the process started (`now - etime`), if known.
    pub started_at: Option<i64>,
    /// tmux target resolved via parent chain.
    pub tmux: Option<TmuxTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningHit {
    pub pid: u32,
    pub tmux: Option<TmuxTarget>,
    /// true when matched exactly via `--session`; false = cwd + recent-mtime heuristic.
    pub direct: bool,
    /// Live activity summary from the shared agent-link cache
    /// (~/.cache/agent-link/summaries/<pid>.json, written by pi-agent-link).
    pub summary: Option<String>,
    /// Cache status (idle / thinking / tool:<name>).
    pub status: Option<String>,
    /// Live peer name from the cache (`pi-dotfiles-2`), assigned by pi-agent-link.
    pub peer_name: Option<String>,
}

/// Row glyph: ○ exited, ◎ waiting (looked at), ● waiting unread / working.
///
/// JetBrains Mono Nerd Font has no ◐ (U+25D0); the terminal would fall back
/// to another font and the column would not line up. ●/◎/○ all exist in JBM
/// with the same advance and bounding box. Unread uses the same solid disc as
/// working (tmux `@special_activity` until the pane is focused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMark {
    Exited,
    Waiting,
    Unread,
    Prompt,
    Working,
}

impl LiveMark {
    pub fn glyph(self) -> &'static str {
        match self {
            LiveMark::Exited => "○",
            LiveMark::Waiting => "◎",
            LiveMark::Unread | LiveMark::Prompt | LiveMark::Working => "●",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LiveMark::Exited => "exited",
            LiveMark::Waiting => "waiting",
            LiveMark::Unread => "unread",
            LiveMark::Prompt => "prompt",
            LiveMark::Working => "working",
        }
    }
}

fn is_prompt_status(st: &str) -> bool {
    matches!(st, "tool:ask" | "tool:confirm" | "tool:input")
}

/// Running + ask/confirm/input → prompt; thinking/other tools → working;
/// tmux `@special_activity` → unread; otherwise waiting on you.
pub fn live_mark(hit: Option<&RunningHit>) -> LiveMark {
    match hit {
        None => LiveMark::Exited,
        Some(h) => {
            let st = h.status.as_deref().unwrap_or("");
            if is_prompt_status(st) {
                LiveMark::Prompt
            } else if st == "thinking" || st.starts_with("tool") {
                LiveMark::Working
            } else if h.tmux.as_ref().is_some_and(|t| t.attention) {
                LiveMark::Unread
            } else {
                LiveMark::Waiting
            }
        }
    }
}

/// Parse `ps -o etime=` (`[[dd-]hh:]mm:ss`) into elapsed seconds.
fn parse_etime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [m, sec] => (0, m.parse::<i64>().ok()?, sec.parse::<i64>().ok()?),
        [h, m, sec] => (
            h.parse::<i64>().ok()?,
            m.parse::<i64>().ok()?,
            sec.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    Some(days * 86400 + h * 3600 + m * 60 + sec)
}

/// Heuristic: does this command line look like the pi coding agent?
pub fn looks_like_pi(cmd: &str) -> bool {
    let arg0 = cmd.split_whitespace().next().unwrap_or("");
    let base = arg0.rsplit('/').next().unwrap_or(arg0);
    base == "pi"
        || cmd.contains("pi-coding-agent/")
        || cmd.contains("/.bin/pi ")
        || cmd.ends_with("/.bin/pi")
        || cmd.contains("shims/pi ")
        || cmd.ends_with("/shims/pi")
}

/// Snapshot running pi processes, their cwds, and tmux targets.
pub fn detect() -> Vec<PiProc> {
    let mut procs = Vec::new();

    let ps = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,command="])
        .output();
    let ps_out = match ps {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return procs,
    };

    // pid -> (ppid, cmd, started_at)
    let now = chrono::Utc::now().timestamp();
    let mut table: HashMap<u32, (u32, String, Option<i64>)> = HashMap::new();
    for line in ps_out.lines() {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let Ok(pid) = parts[0].parse::<u32>() else { continue };
        let Ok(ppid) = parts[1].parse::<u32>() else { continue };
        let started_at = parse_etime(parts[2]).map(|secs| now - secs);
        let cmd = parts[3..].join(" ");
        table.insert(pid, (ppid, cmd, started_at));
    }

    // tmux panes: pane_pid (shell) -> target, pane_id ("%N") -> target
    let mut panes: HashMap<u32, TmuxTarget> = HashMap::new();
    let mut panes_by_id: HashMap<String, TmuxTarget> = HashMap::new();
    if let Ok(o) = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_pid}\t#{pane_id}\t#{session_name}\t#{window_index}\t#{@special_activity}",
        ])
        .output()
    {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() < 4 {
                    continue;
                }
                if let (Ok(ppid), Ok(win)) =
                    (parts[0].trim().parse::<u32>(), parts[3].trim().parse::<u32>())
                {
                    // #{pane_pid} is the pane's shell pid; pi spawned from that
                    // shell has ppid == pane_pid. (If the pane command exec'd
                    // pi directly, pane_pid == pi pid, handled below.)
                    let attention = parts.get(4).map(|s| s.trim() == "on").unwrap_or(false);
                    let t = TmuxTarget {
                        session: parts[2].to_string(),
                        window: Some(win),
                        attention,
                    };
                    panes.insert(ppid, t.clone());
                    panes_by_id.insert(parts[1].to_string(), t);
                }
            }
        }
    }

    for (pid, (ppid, cmd, started_at)) in &table {
        if !looks_like_pi(cmd) {
            continue;
        }
        let cwd = proc_cwd(*pid);
        // Exact pane via TMUX_PANE inherited from the pane environment
        // (survives shim/exec chains that defeat parent walking).
        let mut tmux = proc_tmux_pane(*pid).and_then(|pane| panes_by_id.get(&pane).cloned());
        // Fallback: walk up a few levels (pi -> shell -> ...) to a pane shell.
        if tmux.is_none() {
            let mut cur = *ppid;
            for _ in 0..5 {
                if let Some(t) = panes.get(&cur) {
                    tmux = Some(t.clone());
                    break;
                }
                match table.get(&cur) {
                    Some((next_ppid, _, _)) if *next_ppid != 0 && *next_ppid != cur => {
                        cur = *next_ppid
                    }
                    _ => break,
                }
            }
        }
        // Also match the process's own pid directly (pi exec'd as pane command).
        if tmux.is_none() {
            if let Some(t) = panes.get(pid) {
                tmux = Some(t.clone());
            }
        }
        let session_arg = cmd
            .split_whitespace()
            .skip(1)
            .zip(cmd.split_whitespace().skip(2))
            .find(|(flag, _)| *flag == "--session")
            .map(|(_, val)| val.to_string());
        let continue_flag = cmd
            .split_whitespace()
            .skip(1)
            .any(|t| t == "-c" || t == "--continue");
        procs.push(PiProc {
            pid: *pid,
            ppid: *ppid,
            cwd,
            cmd: cmd.clone(),
            session_arg,
            continue_flag,
            started_at: *started_at,
            tmux,
        });
    }
    procs
}

/// Read TMUX_PANE from a process environment (macOS/Linux `ps eww`).
fn proc_tmux_pane(pid: u32) -> Option<String> {
    let o = std::process::Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .find_map(|t| t.strip_prefix("TMUX_PANE=").map(|v| v.to_string()))
}

fn proc_cwd(pid: u32) -> Option<PathBuf> {
    let o = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(p) = line.strip_prefix('n') {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

/// Shared activity-summary cache. pi-agent-link mirrors each live pi session's
/// name/status/summary here because psst must not read ~/.claude (guarded on
/// this machine). Layout root must match the extension:
/// `$XDG_CACHE_HOME/agent-link/summaries` (else `~/.cache/...`), one
/// `<pid>.json` per session: { pid, name, cwd, status, summary, updatedAt }.
fn summary_cache_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Some(
                PathBuf::from(x).join("agent-link").join("summaries"),
            );
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("agent-link")
            .join("summaries"),
    )
}

/// (summary, status) for the detected pids, from the shared activity cache.
#[derive(Debug, Clone, PartialEq)]
struct CacheFields {
    name: Option<String>,
    summary: Option<String>,
    status: Option<String>,
}

/// Unique peer name from `~/.claude/sessions/<pid>.json` (pi-agent-link writes
/// this even when the summary cache is missing, e.g. a session started before
/// the cache existed).
fn registry_peer_names(pids: &[u32]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return out,
    };
    let dir = PathBuf::from(home).join(".claude").join("sessions");
    for pid in pids {
        let raw = match std::fs::read_to_string(dir.join(format!("{}.json", pid))) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        if let Some(n) = v
            .get("name")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            out.insert(*pid, n);
        }
    }
    out
}

fn peer_slug(cwd: &str) -> String {
    let base = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("pi");
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    format!("pi-{}", if slug.is_empty() { "pi".into() } else { slug })
}

fn first_free_name(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2..=99 {
        let candidate = format!("{}-{}", base, n);
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{}-x", base)
}

fn fill_missing_peer_names(
    hits: &mut HashMap<PathBuf, RunningHit>,
    procs: &[PiProc],
    sessions: &[SessionMeta],
) {
    let cwd_by_path: HashMap<&PathBuf, &str> =
        sessions.iter().map(|s| (&s.path, s.cwd.as_str())).collect();
    let start_by_pid: HashMap<u32, i64> = procs
        .iter()
        .filter_map(|p| p.started_at.map(|t| (p.pid, t)))
        .collect();
    let mut taken: HashSet<String> = hits
        .values()
        .filter_map(|h| h.peer_name.clone())
        .collect();
    let mut unnamed: Vec<PathBuf> = hits
        .iter()
        .filter(|(_, h)| h.peer_name.as_ref().map_or(true, |n| n.is_empty()))
        .map(|(p, _)| p.clone())
        .collect();
    unnamed.sort_by_key(|path| {
        hits.get(path)
            .and_then(|h| start_by_pid.get(&h.pid).copied())
            .unwrap_or(0)
    });
    for path in unnamed {
        let cwd = cwd_by_path.get(&path).copied().unwrap_or("");
        let name = first_free_name(&peer_slug(cwd), &taken);
        taken.insert(name.clone());
        if let Some(h) = hits.get_mut(&path) {
            h.peer_name = Some(name);
        }
    }
}

fn summary_cache_lookup(pids: &[u32]) -> HashMap<u32, CacheFields> {
    let mut out: HashMap<u32, CacheFields> = HashMap::new();
    let dir = match summary_cache_dir() {
        Some(d) => d,
        None => return out,
    };
    for pid in pids {
        let raw = match std::fs::read_to_string(dir.join(format!("{}.json", pid))) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        let get = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        out.insert(
            *pid,
            CacheFields {
                name: get("name"),
                summary: get("summary"),
                status: get("status"),
            },
        );
    }
    out
}

/// Reap cache files whose pid is no longer alive. Files fresher than 120s are
/// kept: a just-started pi may not appear in our ps snapshot yet.
fn prune_summary_cache(live: &HashSet<u32>) {
    let dir = match summary_cache_dir() {
        Some(d) => d,
        None => return,
    };
    let now = chrono::Utc::now().timestamp();
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for f in rd.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        let Some(pid_str) = name.strip_suffix(".json") else { continue };
        let Ok(fpid) = pid_str.parse::<u32>() else { continue };
        if live.contains(&fpid) {
            continue;
        }
        let updated: i64 = std::fs::read_to_string(f.path())
            .ok()
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
            .and_then(|v| v.get("updatedAt").and_then(|u| u.as_i64()))
            .unwrap_or(0);
        if now - updated > 120 {
            let _ = std::fs::remove_file(f.path());
        }
    }
}

/// Map detected processes to concrete session files.
pub fn map_running(
    procs: &[PiProc],
    sessions: &[SessionMeta],
    active_secs: i64,
) -> HashMap<PathBuf, RunningHit> {
    let mut hits: HashMap<PathBuf, RunningHit> = HashMap::new();
    if procs.is_empty() {
        return hits;
    }
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    // sessions by cwd for the cwd+recency heuristic
    let mut by_cwd: HashMap<&str, Vec<&SessionMeta>> = HashMap::new();
    for s in sessions {
        by_cwd.entry(s.cwd.as_str()).or_default().push(s);
    }
    for v in by_cwd.values_mut() {
        v.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    }

    // index sessions by uuid for `--session <id>` resolution
    let mut by_id: HashMap<&str, &SessionMeta> = HashMap::new();
    for s in sessions {
        let short = &s.id[..s.id.len().min(8)];
        by_id.insert(short, s);
        by_id.insert(s.id.as_str(), s);
    }

    let claim = |hits: &mut HashMap<PathBuf, RunningHit>,
                 claimed: &mut HashSet<PathBuf>,
                 s: &SessionMeta,
                 p: &PiProc,
                 direct: bool| {
        if claimed.contains(&s.path) {
            return false;
        }
        claimed.insert(s.path.clone());
        hits.insert(
            s.path.clone(),
            RunningHit {
                pid: p.pid,
                tmux: p.tmux.clone(),
                direct,
                summary: None,
                status: None,
                peer_name: None,
            },
        );
        true
    };

    let mut unmatched: Vec<&PiProc> = Vec::new();
    for p in procs {
        // 1. exact match from --session arg
        if let Some(arg) = &p.session_arg {
            let found = if arg.ends_with(".jsonl") {
                sessions.iter().find(|s| s.path.to_string_lossy().ends_with(arg))
            } else {
                by_id.get(arg.as_str()).copied()
            };
            if let Some(s) = found {
                claim(&mut hits, &mut claimed, s, p, true);
                continue;
            }
        }
        unmatched.push(p);
    }

    // 2. pair a live process with the session it created at launch (bare `pi`).
    let mut still: Vec<&PiProc> = Vec::new();
    for p in unmatched {
        let mut paired = false;
        if let (Some(cwd), Some(start)) = (&p.cwd, p.started_at) {
            let cwd_str = cwd.to_string_lossy();
            if let Some(candidates) = by_cwd.get(cwd_str.as_ref()) {
                let mut best: Option<(&SessionMeta, i64)> = None;
                for s in candidates {
                    if claimed.contains(&s.path) {
                        continue;
                    }
                    let Some(created) = s.created else { continue };
                    let delta = (created - start).abs();
                    if delta > active_secs {
                        continue;
                    }
                    if best.map_or(true, |(_, d)| delta < d) {
                        best = Some((s, delta));
                    }
                }
                if let Some((s, _)) = best {
                    claim(&mut hits, &mut claimed, s, p, false);
                    paired = true;
                }
            }
        }
        if !paired {
            still.push(p);
        }
    }

    // 3. leftover processes (idle, `-c`, unmatched start times): newest
    //    unclaimed session in that cwd. No recency gate — a live pid is enough.
    for p in still {
        if let Some(cwd) = &p.cwd {
            let cwd_str = cwd.to_string_lossy();
            if let Some(candidates) = by_cwd.get(cwd_str.as_ref()) {
                if let Some(s) = candidates.iter().find(|s| !claimed.contains(&s.path)).copied() {
                    claim(&mut hits, &mut claimed, s, p, p.continue_flag);
                }
            }
        }
    }

    // Attach live activity summaries / names. Prefer the cache (written by
    // current pi-agent-link); fall back to Claude's registry entry, which
    // older in-memory sessions still update with their unique peer name.
    let pids: Vec<u32> = hits.values().map(|h| h.pid).collect();
    let cache = summary_cache_lookup(&pids);
    let registry = registry_peer_names(&pids);
    for h in hits.values_mut() {
        if let Some(c) = cache.get(&h.pid) {
            h.summary = c.summary.clone();
            h.status = c.status.clone();
            h.peer_name = c.name.clone();
        }
        if h.peer_name.is_none() {
            h.peer_name = registry.get(&h.pid).cloned();
        }
    }
    // Last resort: mint unique pi-<cwd> names in start-time order so two live
    // agents in one repo don't both show up as "pi-dotfiles".
    fill_missing_peer_names(&mut hits, procs, sessions);
    // Reap cache files for sessions that are gone.
    prune_summary_cache(&procs.iter().map(|p| p.pid).collect());
    hits
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_cache_lookup_and_prune() {
        // Point HOME at a throwaway dir so the test never touches real caches.
        let tmp = std::env::temp_dir().join(format!("psst-test-{}", std::process::id()));
        let dir = tmp.join(".cache").join("agent-link").join("summaries");
        std::fs::create_dir_all(&dir).unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp);

        // Fake entries: one live-ish (pid we query), one stale, one fresh-but-dead.
        let now = chrono::Utc::now().timestamp();
        let live_pid = 424242u32;
        std::fs::write(
            dir.join(format!("{}.json", live_pid)),
            serde_json::json!({
                "pid": live_pid, "name": "pi-x", "status": "thinking",
                "summary": "▸ do the thing", "updatedAt": now,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("111.json"),
            serde_json::json!({ "summary": "old", "updatedAt": now - 9999 }).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("222.json"),
            serde_json::json!({ "summary": "new", "updatedAt": now }).to_string(),
        )
        .unwrap();

        let got = summary_cache_lookup(&[live_pid, 999999]);
        assert_eq!(
            got.get(&live_pid),
            Some(&CacheFields {
                name: Some("pi-x".to_string()),
                summary: Some("▸ do the thing".to_string()),
                status: Some("thinking".to_string()),
            })
        );
        assert!(!got.contains_key(&999999));

        // Prune: stale 111.json goes, fresh 222.json stays despite dead pid.
        let mut live = HashSet::new();
        live.insert(live_pid);
        prune_summary_cache(&live);
        assert!(dir.join(format!("{}.json", live_pid)).exists());
        assert!(!dir.join("111.json").exists());
        assert!(dir.join("222.json").exists());

        if let Some(h) = prev_home {
            std::env::set_var("HOME", h);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn peer_names_suffix_unclaimed() {
        assert_eq!(peer_slug("/Users/x/Code/github.com/ericboehs/dotfiles"), "pi-dotfiles");
        let mut taken = HashSet::new();
        taken.insert("pi-dotfiles".into());
        assert_eq!(first_free_name("pi-dotfiles", &taken), "pi-dotfiles-2");
        taken.insert("pi-dotfiles-2".into());
        assert_eq!(first_free_name("pi-dotfiles", &taken), "pi-dotfiles-3");
    }

    #[test]
    fn live_mark_from_status() {
        assert_eq!(live_mark(None), LiveMark::Exited);
        let idle = RunningHit {
            pid: 1,
            tmux: None,
            direct: true,
            summary: None,
            status: Some("idle".into()),
            peer_name: None,
        };
        assert_eq!(live_mark(Some(&idle)), LiveMark::Waiting);
        let mut thinking = idle.clone();
        thinking.status = Some("thinking".into());
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Working);
        thinking.status = Some("tool:bash".into());
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Working);
        thinking.status = None;
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Waiting);
        thinking.tmux = Some(TmuxTarget {
            session: "agents".into(),
            window: Some(3),
            attention: true,
        });
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Unread);
        thinking.status = Some("thinking".into());
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Working);
        thinking.status = Some("tool:ask".into());
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Prompt);
        thinking.tmux = None;
        thinking.status = Some("tool:confirm".into());
        assert_eq!(live_mark(Some(&thinking)), LiveMark::Prompt);
    }

    #[test]
    fn etime_parses() {
        assert_eq!(parse_etime("05:22"), Some(5 * 60 + 22));
        assert_eq!(parse_etime("01:02:03"), Some(3600 + 120 + 3));
        assert_eq!(parse_etime("2-03:04:05"), Some(2 * 86400 + 3 * 3600 + 4 * 60 + 5));
        assert_eq!(parse_etime("bogus"), None);
    }

    #[test]
    fn pi_process_detection() {
        assert!(looks_like_pi("/Users/ericboehs/.local/share/mise/shims/pi"));
        assert!(looks_like_pi("pi --session /tmp/x.jsonl"));
        assert!(looks_like_pi(
            "node /Users/x/.local/share/mise/installs/npm-earendil-works-pi-coding-agent/0.84.4/node_modules/.bin/pi"
        ));
        assert!(looks_like_pi("node /x/node_modules/.bin/pi"));
        assert!(!looks_like_pi("vim src/main.rs"));
        assert!(!looks_like_pi("rg -r pi something"));
        assert!(!looks_like_pi("grep pi-coding-agent README.md"));
    }
}
