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

use crate::scan::{SessionKey, SessionMeta, LOCAL_HOST};
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

/// Subtree %CPU (process plus descendants) at or above which a session
/// counts as working. Idle pi nodes sit at exactly 0.0% sample after sample;
/// a streaming agent holds mid-to-high single digits and up (observed 9-11%
/// sustained on a working session), tool children spike far higher. Kept low
/// on purpose: ps %CPU is a decaying/lifetime average, so this also catches
/// "just finished a burst" rather than only mid-token instants. A stale
/// "idle" cache entry loses to live CPU — same as a fresh hook event.
pub const CPU_WORKING_PCT: f64 = 5.0;

#[derive(Debug, Clone, Serialize)]
pub struct PiProc {
    /// Host owning the process: "local" or a `[hosts.<name>]` key.
    pub host: String,
    /// Subtree %CPU (self plus descendants) sampled at detect time.
    /// Lets hosts without the agent-link status hook (e.g. gfew, coop)
    /// still report working vs waiting.
    pub cpu: f64,
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
    /// Subtree %CPU carried over from the process (see [`CPU_WORKING_PCT`]).
    #[serde(default)]
    pub cpu: f64,
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
/// hot CPU (no hook status on this host) → working; tmux `@special_activity`
/// → unread; otherwise waiting on you.
pub fn live_mark(hit: Option<&RunningHit>) -> LiveMark {
    match hit {
        None => LiveMark::Exited,
        Some(h) => {
            let st = h.status.as_deref().unwrap_or("");
            if is_prompt_status(st) {
                LiveMark::Prompt
            } else if st == "thinking" || st.starts_with("tool") {
                LiveMark::Working
            } else if h.cpu >= CPU_WORKING_PCT {
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

/// Parse `ps -axo pid=,ppid=,etime=,command=` output into
/// pid -> (ppid, cmd, started_at). Pure for tests and remote reuse.
pub(crate) fn parse_ps_table(ps_out: &str, now: i64) -> HashMap<u32, (u32, String, Option<i64>)> {
    let mut table: HashMap<u32, (u32, String, Option<i64>)> = HashMap::new();
    for line in ps_out.lines() {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let Ok(pid) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(ppid) = parts[1].parse::<u32>() else {
            continue;
        };
        let started_at = parse_etime(parts[2]).map(|secs| now - secs);
        let cmd = parts[3..].join(" ");
        table.insert(pid, (ppid, cmd, started_at));
    }
    table
}

/// Parse `tmux list-panes -a -F ...` output into
/// (pane_pid -> target, pane_id -> target). Pure for tests and remote reuse.
pub(crate) fn parse_tmux_panes(
    tmux_out: &str,
) -> (HashMap<u32, TmuxTarget>, HashMap<String, TmuxTarget>) {
    let mut panes: HashMap<u32, TmuxTarget> = HashMap::new();
    let mut panes_by_id: HashMap<String, TmuxTarget> = HashMap::new();
    for line in tmux_out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        if let (Ok(ppid), Ok(win)) = (
            parts[0].trim().parse::<u32>(),
            parts[3].trim().parse::<u32>(),
        ) {
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
    (panes, panes_by_id)
}

/// Parse `ps -axo pid=,pcpu=` output (macOS and Linux) into pid → %CPU.
/// Pure for tests; unparseable lines are skipped.
pub(crate) fn parse_ps_cpu(out: &str) -> HashMap<u32, f64> {
    let mut map = HashMap::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        if let (Ok(pid), Ok(cpu)) = (parts[0].parse::<u32>(), parts[1].parse::<f64>()) {
            map.insert(pid, cpu);
        }
    }
    map
}

/// %CPU for `root` plus all descendants, using parent links from the ps
/// table. Catches tool children (rg, tsc, cargo) doing the agent's work
/// while the node parent waits. Cycle-guarded; unknown pids read 0.
pub(crate) fn subtree_cpu(
    table: &HashMap<u32, (u32, String, Option<i64>)>,
    cpu_map: &HashMap<u32, f64>,
    root: u32,
) -> f64 {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, (ppid, _, _)) in table {
        kids.entry(*ppid).or_default().push(*pid);
    }
    let mut total = 0.0;
    let mut visited: HashSet<u32> = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        total += cpu_map.get(&pid).copied().unwrap_or(0.0);
        if let Some(children) = kids.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    total
}

pub(crate) fn session_arg_from_cmd(cmd: &str) -> Option<String> {
    cmd.split_whitespace()
        .skip(1)
        .zip(cmd.split_whitespace().skip(2))
        .find(|(flag, _)| *flag == "--session")
        .map(|(_, val)| val.to_string())
}

pub(crate) fn continue_flag_from_cmd(cmd: &str) -> bool {
    cmd.split_whitespace()
        .skip(1)
        .any(|t| t == "-c" || t == "--continue")
}

/// Resolve a process's tmux target: exact `TMUX_PANE` first, then a walk up
/// the parent chain to a pane shell, then the process's own pid.
fn resolve_tmux(
    pid: u32,
    ppid: u32,
    table: &HashMap<u32, (u32, String, Option<i64>)>,
    panes: &HashMap<u32, TmuxTarget>,
    panes_by_id: &HashMap<String, TmuxTarget>,
    tmux_pane: Option<String>,
) -> Option<TmuxTarget> {
    if let Some(t) = tmux_pane.and_then(|pane| panes_by_id.get(&pane).cloned()) {
        return Some(t);
    }
    let mut cur = ppid;
    for _ in 0..5 {
        if let Some(t) = panes.get(&cur) {
            return Some(t.clone());
        }
        match table.get(&cur) {
            Some((next_ppid, _, _)) if *next_ppid != 0 && *next_ppid != cur => cur = *next_ppid,
            _ => break,
        }
    }
    panes.get(&pid).cloned()
}

/// Snapshot local running pi processes, their cwds, and tmux targets.
pub fn detect() -> Vec<PiProc> {
    let ps_out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,command="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    if ps_out.is_empty() {
        return Vec::new();
    }
    let now = chrono::Utc::now().timestamp();
    let table = parse_ps_table(&ps_out, now);
    let tmux_out = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_pid}\t#{pane_id}\t#{session_name}\t#{window_index}\t#{@special_activity}",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let (panes, panes_by_id) = parse_tmux_panes(&tmux_out);
    let cpu_map = std::process::Command::new("ps")
        .args(["-axo", "pid=,pcpu="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_ps_cpu(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();

    let mut procs = Vec::new();
    for (pid, (ppid, cmd, started_at)) in &table {
        if !looks_like_pi(cmd) {
            continue;
        }
        let tmux = resolve_tmux(
            *pid,
            *ppid,
            &table,
            &panes,
            &panes_by_id,
            proc_tmux_pane(*pid),
        );
        procs.push(PiProc {
            host: LOCAL_HOST.to_string(),
            cpu: subtree_cpu(&table, &cpu_map, *pid),
            pid: *pid,
            ppid: *ppid,
            cwd: proc_cwd(*pid),
            cmd: cmd.clone(),
            session_arg: session_arg_from_cmd(cmd),
            continue_flag: continue_flag_from_cmd(cmd),
            started_at: *started_at,
            tmux,
        });
    }
    procs
}

fn ssh_run(target: &str, remote_cmd: &str) -> anyhow::Result<String> {
    let out = crate::hosts::ssh_command(target, remote_cmd).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "ssh {}: {}",
            target,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Build [`detect_remote`]'s batch probe: one shell loop over the pi pids
/// emitting `PID:`/`CWD:`/`PANE:` blocks. Linux reads /proc; macOS remotes
/// (no /proc) use lsof + `ps eww`, same as local detect. Pure for tests.
pub(crate) fn remote_probe_script(pids: &[u32]) -> String {
    let mut script = String::from("OS=$(uname -s); for pid in");
    for pid in pids {
        script.push_str(&format!(" {} ", pid));
    }
    script.push_str(
        "; do echo PID:$pid; \
         if [ \"$OS\" = Darwin ]; then \
           lsof -a -p $pid -d cwd -Fn 2>/dev/null | grep ^n | sed 's/^n/CWD:/'; \
           ps eww -p $pid 2>/dev/null | tr ' ' '\\n' | grep ^TMUX_PANE= | sed 's/^TMUX_PANE=/PANE:/'; \
         else \
           readlink /proc/$pid/cwd 2>/dev/null | sed 's/^/CWD:/'; \
           tr '\\0' '\\n' < /proc/$pid/environ 2>/dev/null | grep ^TMUX_PANE= | sed 's/^TMUX_PANE=/PANE:/'; \
         fi; done",
    );
    script
}

/// Parse `PID:<pid>`, `CWD:<path>`, `PANE:<%N>` blocks from [`detect_remote`]'s
/// batch probe. Pure for tests.
pub(crate) fn parse_remote_probe(out: &str) -> HashMap<u32, (Option<PathBuf>, Option<String>)> {
    let mut map: HashMap<u32, (Option<PathBuf>, Option<String>)> = HashMap::new();
    let mut cur: Option<u32> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("PID:") {
            cur = rest.trim().parse::<u32>().ok();
            if let Some(pid) = cur {
                map.entry(pid).or_insert((None, None));
            }
        } else if let Some(pid) = cur {
            if let Some(cwd) = line.strip_prefix("CWD:") {
                let cwd = cwd.trim();
                if !cwd.is_empty() {
                    if let Some(e) = map.get_mut(&pid) {
                        e.0 = Some(PathBuf::from(cwd));
                    }
                }
            } else if let Some(pane) = line.strip_prefix("PANE:") {
                let pane = pane.trim();
                if !pane.is_empty() {
                    if let Some(e) = map.get_mut(&pid) {
                        e.1 = Some(pane.to_string());
                    }
                }
            }
        }
    }
    map
}

/// Split multi-section script output on `===PSST-<NAME>===` marker lines
/// into name → body. Section bodies keep their trailing newline. Pure.
pub(crate) fn split_sections(out: &str) -> HashMap<String, String> {
    let mut sections: HashMap<String, String> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in out.lines() {
        if let Some(name) = line
            .strip_prefix("===PSST-")
            .and_then(|s| s.strip_suffix("==="))
        {
            // Per-pid markers (`===PSST-NAME:123===`) belong to the
            // enrichment parser, not bare sections — still, never let body
            // lines leak across a marker.
            cur = Some(name.to_string());
            sections.entry(name.to_string()).or_default();
        } else if let Some(name) = &cur {
            if let Some(body) = sections.get_mut(name) {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    sections
}

/// Snapshot pi processes on a remote host over ssh in two roundtrips: `ps`
/// first (to learn the pi pids), then one combined tmux-panes + cwd/environ
/// probe + cpu script whose `===PSST-*===` sections parse separately.
pub fn detect_remote(ssh_target: &str, host_name: &str) -> anyhow::Result<Vec<PiProc>> {
    let ps_out = ssh_run(ssh_target, "ps -axo pid=,ppid=,etime=,command=")?;
    let now = chrono::Utc::now().timestamp();
    let table = parse_ps_table(&ps_out, now);
    let pi_pids: Vec<u32> = table
        .iter()
        .filter(|(_, (_, cmd, _))| looks_like_pi(cmd))
        .map(|(pid, _)| *pid)
        .collect();
    if pi_pids.is_empty() {
        return Ok(Vec::new());
    }
    let mut script = String::from(
        "echo '===PSST-TMUX==='; tmux list-panes -a -F '#{pane_pid}\t#{pane_id}\t#{session_name}\t#{window_index}\t#{@special_activity}' 2>/dev/null || true; echo '===PSST-PROBE==='; ",
    );
    script.push_str(&remote_probe_script(&pi_pids));
    script.push_str("; echo '===PSST-CPU==='; ps -axo pid=,pcpu=");
    let sections = split_sections(&ssh_run(ssh_target, &script).unwrap_or_default());
    let empty = String::new();
    let (panes, panes_by_id) = parse_tmux_panes(sections.get("TMUX").unwrap_or(&empty));
    let probe = parse_remote_probe(sections.get("PROBE").unwrap_or(&empty));
    // Subtree CPU needs every pid's usage, not just the pi pids'.
    let cpu_map = parse_ps_cpu(sections.get("CPU").unwrap_or(&empty));
    let mut procs = Vec::new();
    for pid in pi_pids {
        let Some((ppid, cmd, started_at)) = table.get(&pid) else {
            continue;
        };
        let (cwd, pane) = probe.get(&pid).cloned().unwrap_or((None, None));
        let tmux = resolve_tmux(pid, *ppid, &table, &panes, &panes_by_id, pane);
        procs.push(PiProc {
            host: host_name.to_string(),
            cpu: subtree_cpu(&table, &cpu_map, pid),
            pid,
            ppid: *ppid,
            cwd,
            cmd: cmd.clone(),
            session_arg: session_arg_from_cmd(cmd),
            continue_flag: continue_flag_from_cmd(cmd),
            started_at: *started_at,
            tmux,
        });
    }
    Ok(procs)
}

/// Fetch both remote enrichment sources in one ssh call: peer names from
/// `~/.claude/sessions/<pid>.json` and agent-link summaries from
/// `~/.cache/agent-link/summaries/<pid>.json`. Missing files are skipped;
/// empty maps mean "mint from cwd" downstream. Hosts without pi-agent-link
/// (gfew, coop) simply yield an empty summaries map.
pub fn fetch_remote_enrichment(
    ssh_target: &str,
    pids: &[u32],
) -> (HashMap<u32, String>, HashMap<u32, CacheFields>) {
    let empty = (HashMap::new(), HashMap::new());
    if pids.is_empty() {
        return empty;
    }
    let mut script = String::from("echo '===PSST-NAMES==='; ");
    for pid in pids {
        script.push_str(&format!(
            "echo PID:{pid}; cat ~/.claude/sessions/{pid}.json 2>/dev/null; echo; ",
            pid = pid
        ));
    }
    script.push_str("echo '===PSST-SUMMARIES==='; ");
    for pid in pids {
        script.push_str(&format!(
            "echo PID:{pid}; cat ~/.cache/agent-link/summaries/{pid}.json 2>/dev/null || cat \"$XDG_CACHE_HOME/agent-link/summaries/{pid}.json\" 2>/dev/null; echo; ",
            pid = pid
        ));
    }
    let Ok(out) = ssh_run(ssh_target, &script) else {
        return empty;
    };
    parse_enrichment(&out)
}

/// Parse [`fetch_remote_enrichment`] output: `===PSST-NAMES===` /
/// `===PSST-SUMMARIES===` sections of `PID:<pid>` + single-line JSON rows.
/// Pure for tests.
pub(crate) fn parse_enrichment(out: &str) -> (HashMap<u32, String>, HashMap<u32, CacheFields>) {
    let mut names = HashMap::new();
    let mut summaries = HashMap::new();
    let mut in_summaries = false;
    let mut cur: Option<u32> = None;
    let non_empty = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    for line in out.lines() {
        if line.starts_with("===PSST-") {
            in_summaries = line.contains("SUMMARIES");
            cur = None;
        } else if let Some(rest) = line.strip_prefix("PID:") {
            cur = rest.trim().parse::<u32>().ok();
        } else if let Some(pid) = cur {
            let line = line.trim();
            if line.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if in_summaries {
                        summaries.insert(
                            pid,
                            CacheFields {
                                name: non_empty(&v, "name"),
                                summary: non_empty(&v, "summary"),
                                status: non_empty(&v, "status"),
                            },
                        );
                    } else if let Some(n) = non_empty(&v, "name") {
                        names.insert(pid, n);
                    }
                }
                cur = None;
            }
        }
    }
    (names, summaries)
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
            return Some(PathBuf::from(x).join("agent-link").join("summaries"));
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
pub struct CacheFields {
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
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
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
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
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
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for f in rd.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        let Some(pid_str) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(fpid) = pid_str.parse::<u32>() else {
            continue;
        };
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

/// Attach remote names/summaries (fetched via ssh) to already-matched hits.
/// Local enrichment lives in [`map_running`]; this keeps ssh out of the
/// pure matcher so tests stay offline.
pub fn attach_remote_enrichment(
    hits: &mut HashMap<SessionKey, RunningHit>,
    names: &HashMap<u32, String>,
    summaries: &HashMap<u32, CacheFields>,
) {
    for h in hits.values_mut() {
        if let Some(c) = summaries.get(&h.pid) {
            h.summary = c.summary.clone();
            h.status = c.status.clone();
            if c.name.is_some() {
                h.peer_name = c.name.clone();
            }
        }
        if h.peer_name.is_none() {
            h.peer_name = names.get(&h.pid).cloned();
        }
    }
}

/// Pure per-host matcher: claim sessions for procs without any enrichment.
/// Keys are host-qualified so identical paths on different hosts never collide.
fn match_one_host(
    procs: &[PiProc],
    sessions: &[SessionMeta],
    active_secs: i64,
    hits: &mut HashMap<SessionKey, RunningHit>,
) {
    if procs.is_empty() || sessions.is_empty() {
        return;
    }
    let mut claimed: HashSet<SessionKey> = HashSet::new();

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

    let claim = |hits: &mut HashMap<SessionKey, RunningHit>,
                 claimed: &mut HashSet<SessionKey>,
                 s: &SessionMeta,
                 p: &PiProc,
                 direct: bool| {
        let key = s.key();
        if claimed.contains(&key) {
            return false;
        }
        claimed.insert(key.clone());
        hits.insert(
            key,
            RunningHit {
                pid: p.pid,
                cpu: p.cpu,
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
                sessions
                    .iter()
                    .find(|s| s.path.to_string_lossy().ends_with(arg))
            } else {
                by_id.get(arg.as_str()).copied()
            };
            if let Some(s) = found {
                claim(&mut *hits, &mut claimed, s, p, true);
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
                    if claimed.contains(&s.key()) {
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
                    claim(&mut *hits, &mut claimed, s, p, false);
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
                if let Some(s) = candidates
                    .iter()
                    .find(|s| !claimed.contains(&s.key()))
                    .copied()
                {
                    claim(&mut *hits, &mut claimed, s, p, p.continue_flag);
                }
            }
        }
    }
}

/// Map detected processes to concrete session files, partitioned by host so
/// identical cwds/paths on different machines never cross-match.
/// Local hits get summary-cache + registry enrichment and prune the cache;
/// remote hits get unique minted names here and ssh enrichment via
/// [`attach_remote_enrichment`] by the caller.
pub fn map_running(
    procs: &[PiProc],
    sessions: &[SessionMeta],
    active_secs: i64,
) -> HashMap<SessionKey, RunningHit> {
    let mut hits: HashMap<SessionKey, RunningHit> = HashMap::new();
    if procs.is_empty() || sessions.is_empty() {
        return hits;
    }
    // Partition both sides by host.
    let mut hosts: HashSet<&str> = HashSet::new();
    for p in procs {
        hosts.insert(p.host.as_str());
    }
    for s in sessions {
        hosts.insert(s.host.as_str());
    }
    // Owned views per host for the pure matcher (proc/session counts are
    // small; cloning keeps lifetimes simple).
    for host in hosts {
        let ps: Vec<PiProc> = procs
            .iter()
            .filter(|p| p.host.as_str() == host)
            .cloned()
            .collect();
        let ss: Vec<SessionMeta> = sessions
            .iter()
            .filter(|s| s.host.as_str() == host)
            .cloned()
            .collect();
        match_one_host(&ps, &ss, active_secs, &mut hits);
    }
    // Enrich local hits from the on-disk caches.
    let local_pids: Vec<u32> = hits
        .iter()
        .filter(|(k, _)| k.host == LOCAL_HOST)
        .map(|(_, h)| h.pid)
        .collect();
    if !local_pids.is_empty() {
        let cache = summary_cache_lookup(&local_pids);
        let registry = registry_peer_names(&local_pids);
        for (k, h) in hits.iter_mut().filter(|(k, _)| k.host == LOCAL_HOST) {
            let _ = k;
            if let Some(c) = cache.get(&h.pid) {
                h.summary = c.summary.clone();
                h.status = c.status.clone();
                h.peer_name = c.name.clone();
            }
            if h.peer_name.is_none() {
                h.peer_name = registry.get(&h.pid).cloned();
            }
        }
    }
    // Mint fallback names (global `taken` so coop can't claim `pi-dotfiles`
    // when local owns it; pid-start ordering may interleave across hosts but
    // stays unique, which is all the suffix numbering needs).
    {
        let start_by_pid: HashMap<(String, u32), i64> = procs
            .iter()
            .filter_map(|p| p.started_at.map(|t| ((p.host.clone(), p.pid), t)))
            .collect();
        let cwd_by_key: HashMap<SessionKey, &str> =
            sessions.iter().map(|s| (s.key(), s.cwd.as_str())).collect();
        let mut taken: HashSet<String> =
            hits.values().filter_map(|h| h.peer_name.clone()).collect();
        let mut unnamed: Vec<SessionKey> = hits
            .iter()
            .filter(|(_, h)| h.peer_name.as_ref().map_or(true, |n| n.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        unnamed.sort_by_key(|k| {
            hits.get(k)
                .and_then(|h| start_by_pid.get(&(k.host.clone(), h.pid)).copied())
                .unwrap_or(0)
        });
        for key in unnamed {
            let cwd = cwd_by_key.get(&key).copied().unwrap_or("");
            // Prefix remote mints with the host so `pi-dotfiles` (local) and
            // `pi-coop-dotfiles` never collide even before the UI badge.
            let base = if key.host == LOCAL_HOST {
                peer_slug(cwd)
            } else {
                format!(
                    "pi-{}-{}",
                    key.host,
                    peer_slug(cwd).trim_start_matches("pi-")
                )
            };
            let name = first_free_name(&base, &taken);
            taken.insert(name.clone());
            if let Some(h) = hits.get_mut(&key) {
                h.peer_name = Some(name);
            }
        }
    }
    // Reap local cache files for sessions that are gone (local only; remote
    // pruning happens on the remote via its own sync).
    let live_local: HashSet<u32> = procs
        .iter()
        .filter(|p| p.host == LOCAL_HOST)
        .map(|p| p.pid)
        .collect();
    prune_summary_cache(&live_local);
    hits
}
#[cfg(test)]
mod tests {
    use super::*;

    // HOME is process-global; serialize the tests that repoint it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn summary_cache_lookup_and_prune() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Point HOME at a throwaway dir so the test never touches real caches.
        let tmp = std::env::temp_dir().join(format!(
            "psst-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
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
        assert_eq!(
            peer_slug("/Users/x/Code/github.com/ericboehs/dotfiles"),
            "pi-dotfiles"
        );
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
            cpu: 0.0,
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
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86400 + 3 * 3600 + 4 * 60 + 5)
        );
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

    #[test]
    fn ps_table_parses() {
        let out = "  123   100 00:05 hello pi --session /tmp/x.jsonl\n  bad line\n  124   100 01:02:03 vim foo\n";
        let now = 1_000_000;
        let t = parse_ps_table(out, now);
        assert_eq!(t.len(), 2);
        assert_eq!(t[&123].1, "hello pi --session /tmp/x.jsonl");
        assert_eq!(t[&123].2, Some(now - 5));
        assert_eq!(t[&124].2, Some(now - 3723));
    }

    #[test]
    fn tmux_panes_parse() {
        let out = "100\t%1\tcode\t2\ton\nbad\n101\t%2\tsess\t0\n";
        let (by_pid, by_id) = parse_tmux_panes(out);
        assert_eq!(by_pid[&100].session, "code");
        assert!(by_pid[&100].attention);
        assert_eq!(by_id["%2"].window, Some(0));
        assert!(!by_id["%2"].attention);
    }

    #[test]
    fn remote_probe_parses() {
        let out = "PID:1\nCWD:/home/agent\nPANE:%3\nPID:2\nCWD:\nPID:bad\nCWD:/x\n";
        let m = parse_remote_probe(out);
        assert_eq!(m[&1].0, Some(PathBuf::from("/home/agent")));
        assert_eq!(m[&1].1, Some("%3".to_string()));
        assert_eq!(m[&2].0, None);
        assert!(!m.contains_key(&0));
    }

    #[test]
    fn sections_split_on_markers() {
        let out = "noise\n===PSST-TMUX===\na\nb\n===PSST-CPU===\nc\n";
        let s = split_sections(out);
        assert_eq!(s.get("TMUX").map(|x| x.as_str()), Some("a\nb\n"));
        assert_eq!(s.get("CPU").map(|x| x.as_str()), Some("c\n"));
        assert!(!s.contains_key("PROBE"));
        // Body lines before any marker are dropped, never misattributed.
        assert!(!s.values().any(|b| b.contains("noise")));
    }

    #[test]
    fn enrichment_routes_names_and_summaries() {
        let out = "===PSST-NAMES===\nPID:11\n{\"name\":\"pi-x\"}\nPID:12\n{}\n\
                       ===PSST-SUMMARIES===\nPID:11\n{\"name\":\"pi-x\",\"status\":\"thinking\",\"summary\":\"s\"}\n";
        let (names, summaries) = parse_enrichment(out);
        assert_eq!(names.get(&11).map(|s| s.as_str()), Some("pi-x"));
        assert!(!names.contains_key(&12));
        assert_eq!(
            summaries.get(&11),
            Some(&CacheFields {
                name: Some("pi-x".to_string()),
                summary: Some("s".to_string()),
                status: Some("thinking".to_string()),
            })
        );
    }

    #[test]
    fn probe_script_covers_both_oses() {
        let s = remote_probe_script(&[11, 22]);
        assert!(s.contains("for pid in 11  22"), "got {}", s);
        assert!(s.contains("uname -s"));
        assert!(s.contains("/proc/$pid/cwd"));
        assert!(s.contains("lsof -a -p $pid"));
        assert!(s.contains("echo PID:$pid"));
    }

    #[test]
    fn ps_cpu_parses() {
        let out = "  123   0.0\n  124  87.5\nbad\n  125\n";
        let m = parse_ps_cpu(out);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&123], 0.0);
        assert_eq!(m[&124], 87.5);
    }

    #[test]
    fn subtree_sums_descendants() {
        // 1(UI shell) -> 2(pi) -> 3(rg); 4 unrelated.
        let table: HashMap<u32, (u32, String, Option<i64>)> = HashMap::from([
            (1, (0, "zsh".into(), None)),
            (2, (1, "pi".into(), None)),
            (3, (2, "rg foo".into(), None)),
            (4, (0, "other".into(), None)),
        ]);
        let cpu: HashMap<u32, f64> = HashMap::from([(2, 5.0), (3, 60.0), (4, 99.0)]);
        assert_eq!(subtree_cpu(&table, &cpu, 2), 65.0);
        assert_eq!(subtree_cpu(&table, &cpu, 1), 65.0);
        assert_eq!(subtree_cpu(&table, &cpu, 999), 0.0);
    }

    #[test]
    fn hot_cpu_reads_working_without_hook_status() {
        let mut h = RunningHit {
            pid: 7,
            cpu: CPU_WORKING_PCT,
            tmux: None,
            direct: false,
            summary: None,
            status: None,
            peer_name: None,
        };
        assert_eq!(live_mark(Some(&h)), LiveMark::Working);
        // Stale "idle" loses to live CPU.
        h.status = Some("idle".into());
        assert_eq!(live_mark(Some(&h)), LiveMark::Working);
        // A blocking prompt still wins: the user must answer.
        h.status = Some("tool:ask".into());
        assert_eq!(live_mark(Some(&h)), LiveMark::Prompt);
        // Cool and statusless stays waiting.
        h.status = None;
        h.cpu = CPU_WORKING_PCT - 0.1;
        assert_eq!(live_mark(Some(&h)), LiveMark::Waiting);
    }

    #[test]
    fn cmd_flags_parse() {
        assert_eq!(
            session_arg_from_cmd("pi --session /tmp/x.jsonl"),
            Some("/tmp/x.jsonl".to_string())
        );
        assert_eq!(session_arg_from_cmd("pi -c"), None);
        assert!(continue_flag_from_cmd("pi -c"));
        assert!(continue_flag_from_cmd("pi --continue"));
        assert!(!continue_flag_from_cmd("pi --session x"));
    }

    fn proc_for(host: &str, pid: u32, cwd: &str) -> PiProc {
        PiProc {
            host: host.to_string(),
            cpu: 0.0,
            pid,
            ppid: 1,
            cwd: Some(PathBuf::from(cwd)),
            cmd: "pi".to_string(),
            session_arg: None,
            continue_flag: false,
            started_at: None,
            tmux: None,
        }
    }

    fn sess_for(host: &str, id: &str, cwd: &str) -> SessionMeta {
        SessionMeta {
            path: PathBuf::from(format!("/tmp/{}-{}.jsonl", host, id)),
            host: host.to_string(),
            id: id.to_string(),
            cwd: cwd.to_string(),
            created: None,
            last_activity: Some(100),
            last_user: Some(100),
            name: None,
            first_prompt: None,
            last_prompt: None,
            model: None,
            cost_total: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            messages: 1,
            branches: 0,
            parent_session: None,
            mtime: 100,
            size: 10,
        }
    }

    #[test]
    fn map_partitions_by_host() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Same cwd on two hosts must not cross-match: each proc claims its
        // own host's session even with identical cwd strings.
        let procs = vec![
            proc_for("local", 11, "/proj/a"),
            proc_for("coop", 11, "/proj/a"),
        ];
        let sessions = vec![
            sess_for("local", "a", "/proj/a"),
            sess_for("coop", "b", "/proj/a"),
        ];
        // Point HOME away so prune never touches the real cache.
        let tmp = std::env::temp_dir().join(format!(
            "psst-part-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp);
        let hits = map_running(&procs, &sessions, 120);
        if let Some(h) = prev {
            std::env::set_var("HOME", h);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(hits.len(), 2);
        let local_key = sessions[0].key();
        let coop_key = sessions[1].key();
        assert_eq!(hits[&local_key].pid, 11);
        assert_eq!(hits[&coop_key].pid, 11);
        // Remote mints carry the host prefix so they can't collide with local.
        let coop_name = hits[&coop_key].peer_name.clone().unwrap();
        assert!(coop_name.starts_with("pi-coop-"), "got {}", coop_name);
    }
}
