//! Detect running pi processes and map them to session files + tmux targets.
//!
//! Zero-config approach (per design): pi appends to session files with
//! open/write/close (`appendFileSync`), so `lsof` only catches a file mid-write.
//! Instead we:
//!   1. find pi-like processes via `ps`,
//!   2. resolve each process's cwd via `lsof`,
//!   3. find its tmux session/window by walking the parent chain to a tmux pane pid,
//!   4. pin a session file: exact when the cmdline carries `--session`,
//!      otherwise the most recently written session in that cwd within the
//!      active window (default 120s).

use crate::scan::SessionMeta;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct TmuxTarget {
    pub session: String,
    pub window: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PiProc {
    pub pid: u32,
    pub ppid: u32,
    pub cwd: Option<PathBuf>,
    pub cmd: String,
    /// `--session <arg>` from the cmdline, if present.
    pub session_arg: Option<String>,
    /// tmux target resolved via parent chain.
    pub tmux: Option<TmuxTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningHit {
    pub pid: u32,
    pub tmux: Option<TmuxTarget>,
    /// true when matched exactly via `--session`; false = cwd + recent-mtime heuristic.
    pub direct: bool,
}

/// Heuristic: does this command line look like the pi coding agent?
pub fn looks_like_pi(cmd: &str) -> bool {
    let arg0 = cmd.split_whitespace().next().unwrap_or("");
    let base = arg0.rsplit('/').next().unwrap_or(arg0);
    base == "pi"
        || cmd.contains("pi-coding-agent")
        || cmd.contains("/.bin/pi ")
        || cmd.ends_with("/.bin/pi")
        || cmd.contains("shims/pi ")
        || cmd.ends_with("/shims/pi")
}

/// Snapshot running pi processes, their cwds, and tmux targets.
pub fn detect() -> Vec<PiProc> {
    let mut procs = Vec::new();

    let ps = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output();
    let ps_out = match ps {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return procs,
    };

    // pid -> (ppid, cmd)
    let mut table: HashMap<u32, (u32, String)> = HashMap::new();
    for line in ps_out.lines() {
        let mut it = line.trim().splitn(3, char::is_whitespace);
        let (Some(p), Some(pp), Some(cmd)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Ok(pid) = p.trim().parse::<u32>() else { continue };
        let Ok(ppid) = pp.trim().parse::<u32>() else { continue };
        table.insert(pid, (ppid, cmd.trim().to_string()));
    }

    // tmux pane_pid -> (session, window_index)
    let mut panes: HashMap<u32, TmuxTarget> = HashMap::new();
    if let Ok(o) = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_pid}\t#{session_name}\t#{window_index}"])
        .output()
    {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() < 3 {
                    continue;
                }
                if let (Ok(ppid), Ok(win)) =
                    (parts[0].trim().parse::<u32>(), parts[2].trim().parse::<u32>())
                {
                    // #{pane_pid} is the pane's shell pid; pi spawned from that
                    // shell has ppid == pane_pid. (If the pane command exec'd
                    // pi directly, pane_pid == pi pid, handled below.)
                    panes.insert(
                        ppid,
                        TmuxTarget {
                            session: parts[1].to_string(),
                            window: Some(win),
                        },
                    );
                }
            }
        }
    }

    for (pid, (ppid, cmd)) in &table {
        if !looks_like_pi(cmd) {
            continue;
        }
        let cwd = proc_cwd(*pid);
        // Walk up a few levels (pi -> shell -> ...) looking for a tmux pane.
        let mut tmux = None;
        let mut cur = *ppid;
        for _ in 0..5 {
            if let Some(t) = panes.get(&cur) {
                tmux = Some(t.clone());
                break;
            }
            match table.get(&cur) {
                Some((next_ppid, _)) if *next_ppid != 0 && *next_ppid != cur => cur = *next_ppid,
                _ => break,
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
        procs.push(PiProc {
            pid: *pid,
            ppid: *ppid,
            cwd,
            cmd: cmd.clone(),
            session_arg,
            tmux,
        });
    }
    procs
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
    let now = chrono::Utc::now().timestamp();

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

    for p in procs {
        // 1. exact match from --session arg
        if let Some(arg) = &p.session_arg {
            let found = if arg.ends_with(".jsonl") {
                sessions.iter().find(|s| s.path.to_string_lossy().ends_with(arg))
            } else {
                by_id.get(arg.as_str()).copied()
            };
            if let Some(s) = found {
                hits.entry(s.path.clone()).or_insert_with(|| RunningHit {
                    pid: p.pid,
                    tmux: p.tmux.clone(),
                    direct: true,
                });
                continue;
            }
        }
        // 2. cwd + recent mtime
        if let Some(cwd) = &p.cwd {
            let cwd_str = cwd.to_string_lossy();
            if let Some(candidates) = by_cwd.get(cwd_str.as_ref()) {
                if let Some(s) = candidates.first() {
                    if now - s.mtime <= active_secs {
                        hits
                            .entry(s.path.clone())
                            .or_insert_with(|| RunningHit {
                                pid: p.pid,
                                tmux: p.tmux.clone(),
                                direct: false,
                            });
                    }
                }
            }
        }
    }
    hits
}