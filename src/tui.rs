//! ratatui TUI: list -> filter -> inspect -> open (resume in tmux).

use crate::config::Config;
use crate::hosts::{self, local_path_for};
use crate::running::{
    attach_remote_enrichment, detect, detect_remote, fetch_remote_enrichment, live_mark,
    map_running, LiveMark, RunningHit,
};
use crate::scan::{
    in_scope, recent_messages_from_str, scan, sessions_root, SessionKey, SessionMeta, LOCAL_HOST,
};
use anyhow::Result;
use chrono::Timelike;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sort {
    Activity,
    Tmux,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Activity => "activity",
            Sort::Tmux => "tmux",
        }
    }
    fn next(self) -> Sort {
        match self {
            Sort::Activity => Sort::Tmux,
            Sort::Tmux => Sort::Activity,
        }
    }
    fn from_label(s: &str) -> Option<Sort> {
        match s {
            "activity" => Some(Sort::Activity),
            "tmux" => Some(Sort::Tmux),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Group {
    Flat,
    Project,
    Day,
    Host,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Flat => "flat",
            Group::Project => "project",
            Group::Day => "day",
            Group::Host => "host",
        }
    }
    fn next(self) -> Group {
        match self {
            Group::Flat => Group::Project,
            Group::Project => Group::Day,
            Group::Day => Group::Host,
            Group::Host => Group::Flat,
        }
    }
    fn from_label(s: &str) -> Option<Group> {
        match s {
            "flat" => Some(Group::Flat),
            "project" => Some(Group::Project),
            "day" => Some(Group::Day),
            "host" => Some(Group::Host),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewFilter {
    All,
    Named,
    Active,
}

impl ViewFilter {
    fn label(self) -> &'static str {
        match self {
            ViewFilter::All => "all",
            ViewFilter::Named => "named",
            ViewFilter::Active => "active",
        }
    }
    fn next(self) -> ViewFilter {
        match self {
            ViewFilter::All => ViewFilter::Named,
            ViewFilter::Named => ViewFilter::Active,
            ViewFilter::Active => ViewFilter::All,
        }
    }
    fn from_label(s: &str) -> Option<ViewFilter> {
        match s {
            "all" => Some(ViewFilter::All),
            "named" => Some(ViewFilter::Named),
            "active" => Some(ViewFilter::Active),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenAction {
    /// Switch/attach to an existing local tmux session (`session[:window]`).
    TmuxAttach { target: String, inside: bool },
    /// Create a local tmux window/session running the resume command.
    TmuxNew {
        cwd: String,
        name: String,
        command: String,
        inside: bool,
    },
    /// Attach to a remote tmux target via `ssh -t <ssh_target> ...`.
    /// `target` is `session[:window]` on the remote.
    SshAttach {
        ssh_target: String,
        target: String,
        inside: bool,
    },
    /// Resume a remote session via `ssh -t <ssh_target> 'cd <cwd> && <command>'`.
    /// `command` is the remote resume command; `cwd` is the remote cwd.
    SshNew {
        ssh_target: String,
        cwd: String,
        name: String,
        command: String,
        inside: bool,
    },
    /// Bring forward the terminal tab already holding an ssh to this host,
    /// with `target` (`session[:window]` on the remote) selected there.
    /// Degrades to [`OpenAction::SshAttach`] when no tab can be focused.
    SshFocus {
        focus: TabFocus,
        target: String,
        inside: bool,
    },
    /// Open a window for an idle remote session in the host's own tmux, then
    /// focus its tab. Degrades to [`OpenAction::SshNew`].
    SshFocusNew {
        focus: TabFocus,
        cwd: String,
        name: String,
        command: String,
        inside: bool,
    },
}

/// Everything needed to find and click one host's terminal tab.
#[derive(Debug, Clone, PartialEq)]
pub struct TabFocus {
    /// Terminal application owning the tab bar.
    pub app: String,
    pub ssh_target: String,
    /// Tab-title fragments identifying the host, most specific first.
    pub tags: Vec<String>,
}

pub struct App {
    pub sessions: Vec<SessionMeta>,
    pub running: HashMap<SessionKey, RunningHit>,
    /// Mirror/sync errors per host (e.g. `coop: ssh failed`); shown in the
    /// header until the next successful refresh.
    pub host_errors: Vec<String>,
    /// A background full-refresh worker is in flight. Set by the event loop
    /// in [`run`]; renders a `sync…` badge so a slow host reads as loading,
    /// not frozen.
    pub syncing: bool,
    /// Indices into `sessions` after filter+sort; `sel` indexes this.
    pub order: Vec<usize>,
    pub sel: usize,
    pub filter: String,
    pub group: Group,
    pub sort: Sort,
    pub config: Config,
    pub status: String,
    /// Right-hand detail pane. Off by default so the list can use full width.
    pub detail: bool,
    /// When false, list titles skip first-prompt text and the ▸ last-message line.
    pub show_prompts: bool,
    /// `/` focuses the filter so j/k type into it instead of moving the selection.
    pub search_focus: bool,
    /// f cycles all → named → active.
    pub view_filter: ViewFilter,
    /// `psst .` — only sessions whose cwd is this project.
    pub cwd_scope: Option<PathBuf>,
    /// Restored once after the first refresh from ui-state.json.
    /// Host-qualified (`host:path`); legacy path-only values read as local.
    pending_sel: Option<SessionKey>,
    /// Collapsed group keys (project cwd, or day label). h/l toggles.
    pub collapsed: HashSet<String>,
    /// Parked-on header key: j/k stop on headers, Enter toggles the fold.
    /// Not persisted; cleared whenever the selection lands on a session.
    header_park: Option<String>,
    /// ? help overlay.
    pub show_help: bool,
    /// Detail pane messages: (host-qualified key, mtime, turns). Recomputed
    /// when the selection moves or the file grows.
    recent_cache: Option<(SessionKey, i64, Vec<(String, String)>)>,
}

/// One rendered row: a session (index into `sessions`) or a group header.
pub(crate) struct Item {
    session: Option<usize>,
    lines: Vec<Line<'static>>,
    /// Group key for headers (project cwd / day label); lets a parked
    /// header highlight.
    group_key: Option<String>,
}

/// One navigable row: a header, or a member (position in `order`).
enum Row {
    Header { key: String, first: usize },
    Member(usize),
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn fmt_clock(ts: i64) -> String {
    let Some(t) = chrono::DateTime::from_timestamp(ts, 0) else {
        return "?".into();
    };
    let t = t.with_timezone(&chrono::Local);
    let (is_pm, hour) = t.hour12();
    format!(
        "{}:{:02}{}",
        hour,
        t.minute(),
        if is_pm { "p" } else { "a" }
    )
}

fn fmt_age(secs: i64) -> String {
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

fn trunc(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w {
        return s.to_string();
    }
    let mut out: String = chars[..w.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = t.chars().count();
    format!("{}{}", t, " ".repeat(w.saturating_sub(n)))
}

fn project_base(cwd: &str) -> String {
    let c = cwd.trim_end_matches('/');
    c.rsplit('/').next().unwrap_or(c).to_string()
}

/// Status-friendly group name: project basename (`coop:dotfiles` for remote
/// projects), host/day labels as-is.
fn short_group(key: &str) -> String {
    match key.split_once(':') {
        Some((host, cwd)) if cwd.starts_with('/') => {
            format!("{}:{}", host, project_base(cwd))
        }
        _ => project_base(key),
    }
}

/// Parse a persisted selection (`host:path`); legacy bare paths read as local.
fn parse_selected(s: &str) -> SessionKey {
    match s.split_once(':') {
        Some((host, path)) if !host.contains('/') && path.starts_with('/') => SessionKey {
            host: host.to_string(),
            path: PathBuf::from(path),
        },
        _ => SessionKey {
            host: LOCAL_HOST.to_string(),
            path: PathBuf::from(s),
        },
    }
}

impl App {
    pub fn new(config: Config, cwd_scope: Option<PathBuf>) -> App {
        let mut app = App {
            sessions: Vec::new(),
            running: HashMap::new(),
            order: Vec::new(),
            sel: 0,
            filter: String::new(),
            host_errors: Vec::new(),
            syncing: false,
            group: match config.group_default() {
                "project" => Group::Project,
                "flat" => Group::Flat,
                "host" => Group::Host,
                _ => Group::Day,
            },
            sort: Sort::Activity,
            status: String::new(),
            detail: config.detail_default(),
            show_prompts: false,
            search_focus: false,
            view_filter: ViewFilter::All,
            cwd_scope,
            pending_sel: None,
            recent_cache: None,
            collapsed: HashSet::new(),
            header_park: None,
            show_help: false,
            config,
        };
        app.load_ui_state();
        app.refresh_local();
        app
    }

    /// Lookup helper so call sites never forget the host qualifier.
    fn hit_for(&self, s: &SessionMeta) -> Option<&RunningHit> {
        self.running.get(&s.key())
    }

    /// The `[hosts.<name>]` entry backing a session, or `None` when local.
    fn host_config_for(&self, host: &str) -> Option<&crate::config::HostConfig> {
        if host == LOCAL_HOST {
            return None;
        }
        self.config.hosts().iter().find(|h| h.name == host)
    }

    /// How to reach this host's terminal tab (see [`crate::focus`]).
    fn tab_focus_for(&self, h: &crate::config::HostConfig) -> TabFocus {
        TabFocus {
            app: self.config.terminal().to_string(),
            ssh_target: h.ssh_target.clone(),
            tags: crate::focus::host_tags(&h.name, &h.ssh_target, h.tab.as_deref()),
        }
    }

    /// Swap in a background [`full_refresh`] result, keeping the selection
    /// pinned to its session (or the restored ui-state row) across the swap.
    fn apply_refresh(&mut self, res: RefreshResult, sel_key: Option<SessionKey>) {
        self.sessions = res.sessions;
        self.running = res.hits;
        self.host_errors = res.errors;
        self.apply_filter_sort();
        self.restore_selection(sel_key);
    }

    /// Re-pin the selection to a session key after the list changes.
    fn restore_selection(&mut self, sel_key: Option<SessionKey>) {
        let restore = sel_key.or_else(|| self.pending_sel.take());
        if let Some(k) = restore {
            if let Some(pos) = self.order.iter().position(|&i| self.sessions[i].key() == k) {
                self.sel = pos;
            }
        }
        self.clamp_sel();
    }

    /// First-frame refresh: local sessions from the cache plus whatever the
    /// host mirrors already hold, and local `ps` for running state. No ssh,
    /// so the frame is not waiting on the network — [`run`] queues the full
    /// pass on a worker thread and swaps it in when it lands.
    pub fn refresh_local(&mut self) {
        let sel_key = self.order.get(self.sel).map(|&i| self.sessions[i].key());
        let mut sessions = scan(&sessions_root(), false).unwrap_or_default();
        sessions.extend(hosts::cached_sessions(self.config.hosts()));
        self.sessions = sessions;
        let procs = detect();
        self.running = map_running(&procs, &self.sessions, self.config.active_secs());
        self.apply_filter_sort();
        self.restore_selection(sel_key);
    }
}

/// Output of one full rescan, computed off the UI thread.
struct RefreshResult {
    sessions: Vec<SessionMeta>,
    hits: HashMap<SessionKey, RunningHit>,
    errors: Vec<String>,
}

/// Rescan everything: local sessions, host mirrors, local + remote running
/// processes with remote ssh enrichment. Pure I/O + compute, no `&mut App` —
/// safe to run on a background thread while the TUI keeps handling keys.
fn full_refresh(config: &Config, force: bool) -> RefreshResult {
    let root = sessions_root();
    let mut sessions = scan(&root, force).unwrap_or_default();
    let mut errors = Vec::new();
    let mut synced_ok: HashSet<String> = HashSet::new();
    for sync in hosts::sync_all(config.hosts(), force) {
        if sync.ok() {
            synced_ok.insert(sync.name.clone());
        }
        if let Some(e) = sync.error {
            errors.push(e);
        }
        sessions.extend(sync.sessions);
    }
    let mut procs = detect();
    for h in config.hosts() {
        // A failed sync already waited out one ssh timeout; don't spend
        // two more on detection for a host that's down.
        if !synced_ok.contains(&h.name) {
            continue;
        }
        match detect_remote(&h.ssh_target, &h.name) {
            Ok(rp) => procs.extend(rp),
            Err(e) => errors.push(format!("{}: {}", h.name, e)),
        }
    }
    let mut hits = map_running(&procs, &sessions, config.active_secs());
    // ssh enrichment for remote hits (names/status from the remote).
    for h in config.hosts() {
        let remote_pids: Vec<u32> = hits
            .iter()
            .filter(|(k, _)| k.host == h.name)
            .map(|(_, hit)| hit.pid)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if remote_pids.is_empty() {
            continue;
        }
        let (names, summaries) = fetch_remote_enrichment(&h.ssh_target, &remote_pids);
        let mut sub: HashMap<SessionKey, RunningHit> = hits
            .iter()
            .filter(|(k, _)| k.host == h.name)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        attach_remote_enrichment(&mut sub, &names, &summaries);
        for (k, v) in sub {
            hits.insert(k, v);
        }
    }
    RefreshResult {
        sessions,
        hits,
        errors,
    }
}

impl App {
    /// Turns for the detail pane, cached by host-qualified key+mtime so live
    /// sessions update. Remote sessions read from their ssh mirror.
    fn recent_for_selected(&mut self) -> Vec<(String, String)> {
        let (key, local_path, mtime) = match self.selected() {
            Some(s) => {
                let lp = local_path_for(s);
                let mt = std::fs::metadata(&lp)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (s.key(), lp, mt)
            }
            None => return Vec::new(),
        };
        if let Some((k, m, msgs)) = &self.recent_cache {
            if k == &key && *m == mtime {
                return msgs.clone();
            }
        }
        let Ok(raw) = std::fs::read_to_string(&local_path) else {
            return Vec::new();
        };
        let msgs = recent_messages_from_str(&raw, 6);
        self.recent_cache = Some((key, mtime, msgs.clone()));
        msgs
    }

    /// Cheap 1s tick: re-detect *local* running processes without reparsing
    /// session files or touching ssh. Remote running state refreshes on the
    /// 10s `refresh` (and `r`/`R`) so a slow host can't stall the list.
    pub fn refresh_running(&mut self) {
        let sel_key = self.order.get(self.sel).map(|&i| self.sessions[i].key());
        // Preserve remote hits across local ticks: re-map local procs against
        // local sessions, then merge over the previous remote entries.
        let local_procs = detect();
        let local_sessions: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| s.is_local())
            .cloned()
            .collect();
        let local_hits = map_running(&local_procs, &local_sessions, self.config.active_secs());
        self.running.retain(|k, _| k.host != LOCAL_HOST);
        for (k, v) in local_hits {
            self.running.insert(k, v);
        }
        self.apply_filter_sort();
        if let Some(k) = sel_key {
            if let Some(pos) = self.order.iter().position(|&i| self.sessions[i].key() == k) {
                self.sel = pos;
            }
        }
        self.clamp_sel();
    }

    fn load_ui_state(&mut self) {
        let raw = std::fs::read_to_string(ui_state_path()).ok();
        let Some(st) = raw.and_then(|s| serde_json::from_str::<UiState>(&s).ok()) else {
            return;
        };
        if let Some(v) = ViewFilter::from_label(&st.view_filter) {
            self.view_filter = v;
        }
        if let Some(g) = Group::from_label(&st.group) {
            self.group = g;
        }
        if let Some(s) = Sort::from_label(&st.sort) {
            self.sort = s;
        }
        self.show_prompts = st.show_prompts;
        self.detail = st.detail;
        self.pending_sel = st.selected.as_deref().map(parse_selected);
        self.collapsed = st.collapsed.into_iter().collect();
    }

    pub fn save_ui_state(&self) {
        let selected = self.order.get(self.sel).map(|&i| {
            let s = &self.sessions[i];
            format!("{}:{}", s.host, s.path.to_string_lossy())
        });
        let st = UiState {
            view_filter: self.view_filter.label().to_string(),
            group: self.group.label().to_string(),
            sort: self.sort.label().to_string(),
            show_prompts: self.show_prompts,
            detail: self.detail,
            selected,
            collapsed: self.collapsed.iter().cloned().collect(),
        };
        let p = ui_state_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&st) {
            let _ = std::fs::write(p, json);
        }
    }

    fn apply_filter_sort(&mut self) {
        let f = self.filter.to_lowercase();
        let mut idx: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                let s = &self.sessions[i];
                let hit = self.hit_for(s);
                if let Some(scope) = &self.cwd_scope {
                    if !in_scope(&s.cwd, scope) {
                        return false;
                    }
                }
                match self.view_filter {
                    ViewFilter::All => {}
                    ViewFilter::Named => {
                        let named =
                            s.name.is_some() || hit.and_then(|h| h.peer_name.as_deref()).is_some();
                        if !named {
                            return false;
                        }
                    }
                    ViewFilter::Active => {
                        if hit.is_none() {
                            return false;
                        }
                    }
                }
                if f.is_empty() {
                    return true;
                }
                let peer = hit.and_then(|h| h.peer_name.as_deref()).unwrap_or("");
                let mark = live_mark(hit);
                let hay = format!(
                    "{} {} {} {} {} {} {} {} {}",
                    display_title(s, hit),
                    s.host,
                    s.first_prompt.as_deref().unwrap_or(""),
                    s.last_prompt.as_deref().unwrap_or(""),
                    s.cwd,
                    s.model.as_deref().unwrap_or(""),
                    peer,
                    mark.as_str(),
                    if hit.is_some() { "active running" } else { "" }
                )
                .to_lowercase();
                hay_contains(&hay, &f)
            })
            .collect();

        match self.sort {
            Sort::Activity => {
                idx.sort_by_key(|&i| std::cmp::Reverse(self.sessions[i].last_user_or_activity()));
            }
            Sort::Tmux => {
                idx.sort_by(|&a, &b| {
                    let key = |i: usize| -> (u8, String, u32, std::cmp::Reverse<i64>) {
                        match self.running.get(&self.sessions[i].key()) {
                            Some(h) => (
                                0u8,
                                h.tmux
                                    .as_ref()
                                    .map(|t| t.session.clone())
                                    .unwrap_or_default(),
                                h.tmux.as_ref().and_then(|t| t.window).unwrap_or(0),
                                std::cmp::Reverse(self.sessions[i].last_user_or_activity()),
                            ),
                            None => (
                                1u8,
                                String::new(),
                                0,
                                std::cmp::Reverse(self.sessions[i].last_user_or_activity()),
                            ),
                        }
                    };
                    key(a).cmp(&key(b))
                });
            }
        }
        self.order = idx;
        self.clamp_sel();
    }

    fn clamp_sel(&mut self) {
        if self.order.is_empty() {
            self.sel = 0;
        } else {
            self.sel = self.sel.min(self.order.len() - 1);
        }
    }

    fn selected(&self) -> Option<&SessionMeta> {
        self.order.get(self.sel).map(|&i| &self.sessions[i])
    }

    /// j/k walk visual rows: every header is a stop, folded groups show
    /// header only. Parking on a header sets header_park (Enter toggles).
    fn move_sel(&mut self, delta: i32) {
        let rows = self.visual_rows();
        if rows.is_empty() {
            return;
        }
        let cur = rows.iter().position(|r| match (r, &self.header_park) {
            (Row::Header { key, .. }, Some(park)) => key == park,
            (Row::Member(pos), None) => *pos == self.sel,
            _ => false,
        });
        // Stale park (group filtered out) or hidden sel: resync from sel.
        let cur = cur.or_else(|| {
            self.header_park = None;
            rows.iter()
                .position(|r| matches!(r, Row::Member(pos) if *pos == self.sel))
        });
        let next = match cur {
            Some(c) => (c as i32 + delta).clamp(0, rows.len() as i32 - 1) as usize,
            None => {
                if delta > 0 {
                    0
                } else {
                    rows.len() - 1
                }
            }
        };
        match &rows[next] {
            Row::Header { key, first } => {
                self.header_park = Some(key.clone());
                self.sel = *first;
            }
            Row::Member(pos) => {
                self.header_park = None;
                self.sel = *pos;
            }
        }
    }

    /// Bucket key of session `i` under the current grouping (empty when flat).
    /// Project keys are host-qualified for remotes (`coop:/home/...`) so two
    /// hosts never share a fold; local keys stay bare cwds.
    fn group_label(&self, i: usize) -> String {
        match self.group {
            Group::Flat => String::new(),
            Group::Host => self.sessions[i].host.clone(),
            Group::Project => {
                let s = &self.sessions[i];
                if s.is_local() {
                    s.cwd.clone()
                } else {
                    format!("{}:{}", s.host, s.cwd)
                }
            }
            Group::Day => {
                let today = chrono::Local::now().date_naive();
                let ts = self.sessions[i].last_user_or_activity();
                let date = chrono::DateTime::from_timestamp(ts, 0)
                    .unwrap_or_default()
                    .with_timezone(&chrono::Local)
                    .date_naive();
                day_label(date, today)
            }
        }
    }

    /// h: fold the selected group and park on its header.
    /// Ok(key) when something folded.
    fn collapse_current(&mut self) -> Option<String> {
        if self.group == Group::Flat || self.order.is_empty() {
            return None;
        }
        let key = self.group_label(self.order[self.sel]);
        if self.collapsed.insert(key.clone()) {
            self.header_park = Some(key.clone());
            Some(key)
        } else {
            None
        }
    }

    /// l: unfold the selected group. True when something unfolded.
    fn expand_current(&mut self) -> bool {
        if self.group == Group::Flat || self.order.is_empty() {
            return false;
        }
        let key = self.group_label(self.order[self.sel]);
        self.collapsed.remove(&key)
    }

    /// H: fold every group and park on the selected one.
    /// Returns group count (0 when flat).
    fn collapse_all(&mut self) -> usize {
        if self.group == Group::Flat || self.order.is_empty() {
            return 0;
        }
        let mut keys = HashSet::new();
        for &i in &self.order {
            keys.insert(self.group_label(i));
        }
        let n = keys.len();
        self.collapsed = keys;
        self.header_park = Some(self.group_label(self.order[self.sel]));
        n
    }

    /// L: unfold everything. Returns how many were folded.
    fn expand_all(&mut self) -> usize {
        let n = self.collapsed.len();
        self.collapsed.clear();
        n
    }

    /// Positions in `order` of the first session in each visual group.
    /// Groups in display order: (key, order-positions). Members keep the
    /// active sort's sequence. Shared by rendering and keyboard navigation.
    fn groups_in_order(&self) -> Vec<(String, Vec<usize>)> {
        if self.order.is_empty() {
            return Vec::new();
        }
        match self.group {
            Group::Flat => vec![(String::new(), (0..self.order.len()).collect())],
            Group::Host => {
                // bucket by host; local first, then by most recent activity.
                let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
                for (pos, &i) in self.order.iter().enumerate() {
                    buckets
                        .entry(self.sessions[i].host.clone())
                        .or_default()
                        .push(pos);
                }
                let mut groups: Vec<(String, Vec<usize>)> = buckets.into_iter().collect();
                groups.sort_by_key(|(host, v)| {
                    let newest = v
                        .iter()
                        .map(|&pos| self.sessions[self.order[pos]].last_user_or_activity())
                        .max()
                        .unwrap_or(0);
                    (
                        if host == LOCAL_HOST { 0u8 } else { 1u8 },
                        std::cmp::Reverse(newest),
                        host.clone(),
                    )
                });
                groups
            }
            Group::Project => {
                // bucket by (host-qualified) cwd; groups ordered by most recent activity.
                let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
                for (pos, &i) in self.order.iter().enumerate() {
                    buckets.entry(self.group_label(i)).or_default().push(pos);
                }
                let mut groups: Vec<(String, Vec<usize>)> = buckets.into_iter().collect();
                groups.sort_by_key(|(_, v)| {
                    std::cmp::Reverse(
                        v.iter()
                            .map(|&pos| self.sessions[self.order[pos]].last_user_or_activity())
                            .max()
                            .unwrap_or(0),
                    )
                });
                groups
            }
            Group::Day => {
                // bucket by local date of last activity; newest day first
                let today = chrono::Local::now().date_naive();
                let mut buckets: HashMap<chrono::NaiveDate, Vec<usize>> = HashMap::new();
                for (pos, &i) in self.order.iter().enumerate() {
                    let ts = self.sessions[i].last_user_or_activity();
                    let date = chrono::DateTime::from_timestamp(ts, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local)
                        .date_naive();
                    buckets.entry(date).or_default().push(pos);
                }
                let mut dates: Vec<chrono::NaiveDate> = buckets.keys().copied().collect();
                dates.sort_by(|a, b| b.cmp(a));
                dates
                    .into_iter()
                    .map(|d| (day_label(d, today), buckets.remove(&d).unwrap_or_default()))
                    .collect()
            }
        }
    }

    /// Visual rows for j/k: every header is a stop, folded groups show
    /// header only. Positions index `order`.
    fn visual_rows(&self) -> Vec<Row> {
        if self.group == Group::Flat {
            return (0..self.order.len()).map(Row::Member).collect();
        }
        let mut rows = Vec::new();
        for (key, members) in self.groups_in_order() {
            let first = members.first().copied().unwrap_or(0);
            let folded = self.collapsed.contains(&key);
            rows.push(Row::Header { key, first });
            if !folded {
                rows.extend(members.into_iter().map(Row::Member));
            }
        }
        rows
    }

    /// J / K jump between headers. K from a member snaps to its own
    /// header first (row kept); K from a header goes to the previous one.
    fn move_group(&mut self, dir: i32) {
        if self.group == Group::Flat || self.order.is_empty() {
            return;
        }
        let groups = self.groups_in_order();
        if groups.is_empty() {
            return;
        }
        let cur = match self.header_park.clone() {
            Some(k) => groups.iter().position(|(key, _)| key == &k),
            None => groups.iter().position(|(_, v)| v.contains(&self.sel)),
        };
        let next = match (cur, dir) {
            (Some(g), d) if d > 0 => g as i32 + 1,
            (Some(g), _) if self.header_park.is_some() => g as i32 - 1,
            (Some(g), _) => g as i32,
            (None, d) if d > 0 => 0,
            (None, _) => return,
        };
        if next < 0 || (next as usize) >= groups.len() {
            return;
        }
        let (key, members) = &groups[next as usize];
        self.header_park = Some(key.clone());
        // K-snapping to the current group keeps the row; jumps take the head.
        if Some(next as usize) != cur {
            if let Some(&first) = members.first() {
                self.sel = first;
            }
        }
    }

    /// Build the open action for the selected session.
    pub fn open_selected(&self) -> Option<OpenAction> {
        let s = self.selected()?;
        let inside = std::env::var("TMUX").is_ok();
        // Remote sessions live in their host's terminal tab; local ones use
        // tmux directly.
        let host_cfg = self.host_config_for(&s.host);
        if let Some(hit) = self.hit_for(s) {
            if let Some(t) = &hit.tmux {
                let target = match t.window {
                    Some(w) => format!("{}:{}", t.session, w),
                    None => t.session.clone(),
                };
                return match host_cfg {
                    Some(h) => Some(OpenAction::SshFocus {
                        focus: self.tab_focus_for(h),
                        target,
                        inside,
                    }),
                    None => Some(OpenAction::TmuxAttach { target, inside }),
                };
            }
            // running without a tmux target: fall through to a fresh resume window
        }
        let command = self
            .config
            .resume_command()
            .replace("{file}", &s.path.to_string_lossy())
            .replace("{id}", &s.id)
            .replace("{cwd}", &s.cwd);
        match host_cfg {
            Some(h) => Some(OpenAction::SshFocusNew {
                focus: self.tab_focus_for(h),
                cwd: s.cwd.clone(),
                name: project_base(&s.cwd),
                command,
                inside,
            }),
            None => Some(OpenAction::TmuxNew {
                cwd: s.cwd.clone(),
                name: project_base(&s.cwd),
                command,
                inside,
            }),
        }
    }

    fn row_line(&self, i: usize, w: usize) -> Line<'static> {
        let s = &self.sessions[i];
        let hit = self.hit_for(s);
        let mark = live_mark(hit);
        let dot = match mark {
            LiveMark::Working => Span::styled("● ", Style::default().fg(Color::Yellow)),
            LiveMark::Prompt => Span::styled(
                "● ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            LiveMark::Unread => Span::styled(
                "● ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            LiveMark::Waiting => Span::styled("◎ ", Style::default().fg(Color::Cyan)),
            LiveMark::Exited => Span::styled("○ ", Style::default().fg(Color::DarkGray)),
        };
        let fork = if s.parent_session.is_some() {
            Span::styled("⎇ ", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("  ")
        };
        let title_w = (w / 2).max(32);
        let label = display_title(s, hit);
        let named = s.name.is_some() || hit.and_then(|h| h.peer_name.as_deref()).is_some();
        let attention = matches!(mark, LiveMark::Prompt | LiveMark::Unread);
        let title = if named || attention {
            Span::styled(
                pad(&label, title_w),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(pad(&label, title_w))
        };
        let proj = Span::raw(pad(&project_base(&s.cwd), (w / 6).max(10)));
        let host_badge = if s.is_local() {
            Span::raw("")
        } else {
            Span::styled(
                format!(" [{}]", s.host),
                Style::default().fg(Color::Magenta),
            )
        };
        // Don't paint age DarkGray: highlight_style keeps span fg, so DarkGray-on-DarkGray
        // made last-activity invisible on the selected row.
        let ts = s.last_user_or_activity();
        let age = Span::raw(if self.group == Group::Day {
            pad(&fmt_clock(ts), 7)
        } else {
            pad(&fmt_age(now() - ts), 5)
        });
        let tmux = match hit.and_then(|h| h.tmux.as_ref()) {
            Some(t) => Span::styled(
                format!(
                    " [{}{}]",
                    t.session,
                    t.window.map(|w| format!(":{}", w)).unwrap_or_default()
                ),
                Style::default().fg(Color::Green),
            ),
            None => Span::raw(""),
        };
        Line::from(vec![
            dot,
            fork,
            title,
            Span::raw(" "),
            proj,
            Span::raw(" "),
            age,
            tmux,
            host_badge,
        ])
    }

    fn member_lines(&self, i: usize, w: usize) -> Vec<Line<'static>> {
        let mut lines = vec![self.row_line(i, w)];
        if self.show_prompts {
            if let Some(p) = self.sessions[i]
                .last_prompt
                .as_deref()
                .or(self.sessions[i].first_prompt.as_deref())
            {
                let one_line = p.split_whitespace().collect::<Vec<_>>().join(" ");
                if !one_line.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("    {}", trunc(&one_line, w.saturating_sub(6))),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }
        lines
    }

    fn build_items(&self, w: usize) -> Vec<Item> {
        let w = w.max(80);
        let mut items = Vec::new();

        match self.group {
            Group::Flat => {
                for &i in &self.order {
                    items.push(Item {
                        session: Some(i),
                        lines: self.member_lines(i, w),
                        group_key: None,
                    });
                }
            }
            Group::Project | Group::Day | Group::Host => {
                for (label, positions) in self.groups_in_order() {
                    let members: Vec<usize> =
                        positions.iter().map(|&pos| self.order[pos]).collect();
                    self.push_group(&mut items, label, members, w);
                }
            }
        }
        items
    }

    /// Header row + member rows for one group.
    fn push_group(&self, items: &mut Vec<Item>, label: String, members: Vec<usize>, w: usize) {
        let newest = members
            .iter()
            .map(|&i| self.sessions[i].last_user_or_activity())
            .max()
            .unwrap_or(0);
        let folded = self.collapsed.contains(&label);
        let header = format!(
            "{} {}  {} sessions · last {}",
            if folded { "▸" } else { "▾" },
            trunc(&label, (w / 2).max(30)),
            members.len(),
            fmt_age(now() - newest)
        );
        items.push(Item {
            session: None,
            lines: vec![Line::from(Span::styled(
                header,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))],
            group_key: Some(label),
        });
        if folded {
            return;
        }
        for i in members {
            items.push(Item {
                session: Some(i),
                lines: self.member_lines(i, w),
                group_key: None,
            });
        }
    }
}

/// Running sessions show the live peer name (`pi-dotfiles-2`); others use /name or prompt.
fn display_title(s: &SessionMeta, hit: Option<&RunningHit>) -> String {
    if let Some(n) = hit
        .and_then(|h| h.peer_name.as_deref())
        .filter(|n| !n.is_empty())
    {
        return n.to_string();
    }
    if hit.is_some() {
        return format!("pi-{}", project_base(&s.cwd));
    }
    if let Some(n) = &s.name {
        return n.clone();
    }
    s.id.chars().take(8).collect()
}

fn hay_contains(hay: &str, needle: &str) -> bool {
    // every whitespace-separated term must appear (substring match)
    needle.split_whitespace().all(|t| hay.contains(t))
}

fn detail_lines(
    s: &SessionMeta,
    hit: Option<&RunningHit>,
    recent: &[(String, String)],
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut v = vec![
        Line::from(vec![
            Span::styled("title:   ", dim),
            Span::styled(display_title(s, hit), bold),
        ]),
        Line::from(vec![
            Span::styled("host:    ", dim),
            Span::styled(
                s.host.clone(),
                if s.is_local() {
                    Style::default()
                } else {
                    Style::default().fg(Color::Magenta)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("project: ", dim),
            Span::raw(s.cwd.clone()),
        ]),
        Line::from(vec![
            Span::styled("path:    ", dim),
            Span::raw(s.path.to_string_lossy().into_owned()),
        ]),
    ];
    if let Some(p) = &s.parent_session {
        v.push(Line::from(vec![
            Span::styled("fork of: ", dim),
            Span::styled(p.clone(), Style::default().fg(Color::Yellow)),
        ]));
    }
    match hit {
        Some(h) => {
            let t = h
                .tmux
                .as_ref()
                .map(|t| {
                    format!(
                        "{}{}",
                        t.session,
                        t.window.map(|w| format!(":{}", w)).unwrap_or_default()
                    )
                })
                .unwrap_or_else(|| "no tmux".into());
            let status = h
                .status
                .as_deref()
                .map(|s| format!(" · {}", s))
                .unwrap_or_default();
            // Surface inferred load: on hosts without the status hook this
            // is the only evidence behind a yellow dot.
            let cpu = if h.cpu >= 1.0 {
                format!(" · cpu {:.0}%", h.cpu)
            } else {
                String::new()
            };
            let mark = live_mark(Some(h));
            let (color, emphasis) = match mark {
                LiveMark::Working => (Color::Yellow, true),
                LiveMark::Prompt => (Color::Red, true),
                LiveMark::Unread => (Color::Cyan, true),
                LiveMark::Waiting => (Color::Cyan, false),
                LiveMark::Exited => (Color::DarkGray, false),
            };
            let mut glyph = Style::default().fg(color);
            if emphasis {
                glyph = glyph.add_modifier(Modifier::BOLD);
            }
            v.push(Line::from(vec![
                Span::styled("state:   ", dim),
                Span::styled(mark.glyph(), glyph),
                Span::raw(format!(
                    " {} pid {} @ {} ({}){}{}",
                    mark.as_str(),
                    h.pid,
                    t,
                    if h.direct { "exact" } else { "heuristic" },
                    status,
                    cpu
                )),
            ]));
        }
        None => v.push(Line::from(vec![
            Span::styled("state:   ", dim),
            Span::styled("○ exited", dim),
        ])),
    }
    if !recent.is_empty() {
        v.push(Line::from(""));
        for (role, text) in recent {
            let (who, color) = if role == "user" {
                ("you", Color::Green)
            } else {
                ("agent", Color::Magenta)
            };
            v.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", who),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(text.clone()),
            ]));
        }
    }
    v
}

fn ui(f: &mut Frame, app: &mut App, items: &[Item], list_state: &mut ListState) {
    let area = f.area();
    let [header, filterbar, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let dim = Style::default().fg(Color::DarkGray);
    let mut working = 0usize;
    let mut attention = 0usize;
    let mut idle = 0usize;
    let mut has_prompt = false;
    for &i in &app.order {
        match live_mark(app.running.get(&app.sessions[i].key())) {
            LiveMark::Working => working += 1,
            LiveMark::Prompt => {
                attention += 1;
                has_prompt = true;
            }
            LiveMark::Unread => attention += 1,
            LiveMark::Waiting => idle += 1,
            LiveMark::Exited => {}
        }
    }
    let mut header_spans = vec![
        Span::styled(
            " psst ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {} sessions", app.order.len())),
    ];
    if working > 0 {
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(
            format!("{} running", working),
            Style::default().fg(Color::Yellow),
        ));
    }
    if attention > 0 {
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(
            format!("{} attention", attention),
            Style::default()
                .fg(if has_prompt { Color::Red } else { Color::Cyan })
                .add_modifier(Modifier::BOLD),
        ));
    }
    if idle > 0 {
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(
            format!("{} idle", idle),
            Style::default().fg(Color::Cyan),
        ));
    }
    header_spans.extend([
        Span::styled(" · ", dim),
        Span::styled("sort:", dim),
        Span::raw(app.sort.label()),
        Span::styled(" · ", dim),
        Span::styled("group:", dim),
        Span::raw(app.group.label()),
        Span::styled(" · ", dim),
        Span::styled("filter:", dim),
        Span::styled(
            app.view_filter.label(),
            if app.view_filter == ViewFilter::All {
                dim
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
    ]);
    if let Some(p) = &app.cwd_scope {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(name, Style::default().fg(Color::Cyan)));
    }
    // Per-host session counts when remotes are configured (e.g. `coop:10`).
    if !app.config.hosts().is_empty() {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for &i in &app.order {
            *counts.entry(app.sessions[i].host.as_str()).or_default() += 1;
        }
        let mut names: Vec<&str> = counts.keys().copied().collect();
        names.sort();
        for name in names {
            if name == LOCAL_HOST {
                continue;
            }
            header_spans.push(Span::styled(" · ", dim));
            header_spans.push(Span::styled(
                format!("{}:{}", name, counts[name]),
                Style::default().fg(Color::Magenta),
            ));
        }
    }
    if app.syncing {
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled("sync…", dim));
    }
    if !app.host_errors.is_empty() {
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(
            app.host_errors.join("; "),
            Style::default().fg(Color::Red),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(header_spans)), header);

    if !app.search_focus && app.filter.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "/ to search",
                Style::default().fg(Color::DarkGray),
            ))),
            filterbar,
        );
    } else {
        let slash = if app.search_focus {
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("/", Style::default().fg(Color::DarkGray))
        };
        let query = if app.search_focus {
            Span::styled(
                format!("{}▏", app.filter),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(app.filter.clone(), Style::default().fg(Color::DarkGray))
        };
        let filter_spans = vec![
            slash,
            query,
            Span::styled(
                format!("  ({} shown)", app.order.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(filter_spans)), filterbar);
    }

    let target = app.order.get(app.sel).copied();
    let parked = app.header_park.clone();
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|it| {
            let selected = match it.session {
                Some(_) => it.session == target && parked.is_none(),
                None => parked
                    .as_deref()
                    .is_some_and(|k| it.group_key.as_deref() == Some(k)),
            };
            ListItem::new(Text::from(if selected {
                highlight_row(&it.lines, it.session.is_none())
            } else {
                it.lines.clone()
            }))
        })
        .collect();
    let list = List::new(list_items).block(Block::new());

    if app.detail {
        let [list_area, detail_area] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        f.render_stateful_widget(list, list_area, list_state);
        let recent = app.recent_for_selected();
        if let Some(s) = app.selected() {
            let hit = app.running.get(&s.key());
            let detail = Paragraph::new(detail_lines(s, hit, &recent))
                .wrap(Wrap { trim: false })
                .block(
                    Block::new()
                        .borders(Borders::TOP)
                        .title(" details ")
                        .title_alignment(Alignment::Center)
                        .title_style(Style::default().fg(Color::Cyan)),
                );
            f.render_widget(detail, detail_area);
        }
    } else {
        f.render_stateful_widget(list, body, list_state);
    }

    if app.show_help {
        let lines = help_lines();
        let area = centered(body, 46, lines.len() as u16 + 2);
        let help = Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" keys ")
                .title_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(Clear, area);
        f.render_widget(help, area);
    }

    let mut footer_spans = vec![Span::styled(
        " / to search · ? keys · enter=open · q quit",
        Style::default().fg(Color::DarkGray),
    )];
    if !app.status.is_empty() {
        footer_spans.push(Span::styled(
            format!("  {}", app.status),
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(footer_spans)), footer);
}

/// What the event loop should do after one keypress.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// Keep running.
    Continue,
    /// Rescan (`r`) or rebuild the cache (`R`). Performed by the event
    /// loop so the reducer itself stays free of I/O.
    Refresh(bool),
    /// Exit and open the selected session.
    Open(OpenAction),
    /// Exit with nothing to do.
    Quit,
}

/// Single-key reducer: all key handling lives here, mutating only `App`
/// state. Render inputs (the built items + highlighted position) come in
/// as arguments; side effects leave as `KeyOutcome`. Callers must only
/// pass `Press` events.
pub(crate) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    items: &[Item],
    selected: Option<usize>,
) -> KeyOutcome {
    if app.show_help {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyOutcome::Quit,
            (KeyCode::Char('?'), _) | (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
                app.show_help = false
            }
            _ => {}
        }
        return KeyOutcome::Continue;
    }
    // Global: Esc, arrows, tab, ctrl chords work in both modes.
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if app.search_focus {
                app.search_focus = false;
            } else if !app.filter.is_empty() {
                app.filter.clear();
                app.apply_filter_sort();
            } else {
                return KeyOutcome::Quit;
            }
        }
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyOutcome::Quit,
        (KeyCode::Down, _) => app.move_sel(1),
        (KeyCode::Up, _) => app.move_sel(-1),
        (KeyCode::PageDown, _) => app.move_sel(10),
        (KeyCode::PageUp, _) => app.move_sel(-10),
        (KeyCode::Home, _) => app.sel = 0,
        (KeyCode::End, _) => app.sel = app.order.len().saturating_sub(1),
        (KeyCode::Tab, _) => {
            app.group = app.group.next();
            app.header_park = None;
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            app.sort = app.sort.next();
            app.apply_filter_sort();
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            app.detail = !app.detail;
        }
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            app.show_prompts = !app.show_prompts;
            app.status = if app.show_prompts {
                "prompts shown".into()
            } else {
                "prompts hidden".into()
            };
        }
        (KeyCode::Enter, _) => {
            let header_key = selected
                .and_then(|pos| items.get(pos))
                .filter(|it| it.session.is_none())
                .and_then(|it| it.group_key.clone());
            match header_key {
                Some(k) => {
                    if app.collapsed.remove(&k) {
                        app.status = "unfolded".into();
                    } else {
                        app.collapsed.insert(k.clone());
                        app.status = format!("folded {}", short_group(&k));
                    }
                }
                None => match app.open_selected() {
                    Some(a) => return KeyOutcome::Open(a),
                    None => app.status = "nothing selected".into(),
                },
            }
        }
        (KeyCode::Char('/'), m) if m == KeyModifiers::NONE && !app.search_focus => {
            app.search_focus = true;
        }
        _ if app.search_focus => match key.code {
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                app.filter.push(c);
                app.apply_filter_sort();
                app.sel = 0;
            }
            KeyCode::Backspace => {
                app.filter.pop();
                app.apply_filter_sort();
            }
            _ => {}
        },
        (KeyCode::Char('q'), _) => return KeyOutcome::Quit,
        (KeyCode::Char('?'), _) => app.show_help = !app.show_help,
        (KeyCode::Char('f'), _) => {
            app.view_filter = app.view_filter.next();
            app.apply_filter_sort();
            app.status = format!("filter:{}", app.view_filter.label());
        }
        (KeyCode::Char('g'), _) => {
            app.group = app.group.next();
            // parked headers belong to the old grouping
            app.header_park = None;
            app.status = format!("group:{}", app.group.label());
        }
        (KeyCode::Char('s'), _) => {
            app.sort = app.sort.next();
            app.apply_filter_sort();
            app.status = format!("sort:{}", app.sort.label());
        }
        (KeyCode::Char('d'), _) => {
            app.detail = !app.detail;
        }
        (KeyCode::Char('p'), _) => {
            app.show_prompts = !app.show_prompts;
            app.status = if app.show_prompts {
                "prompts shown".into()
            } else {
                "prompts hidden".into()
            };
        }
        (KeyCode::Char('J'), _) => app.move_group(1),
        (KeyCode::Char('K'), _) => app.move_group(-1),
        (KeyCode::Char('j'), KeyModifiers::SHIFT) => app.move_group(1),
        (KeyCode::Char('k'), KeyModifiers::SHIFT) => app.move_group(-1),
        (KeyCode::Char('j'), _) => app.move_sel(1),
        (KeyCode::Char('k'), _) => app.move_sel(-1),
        (KeyCode::Char('h'), _) => match app.collapse_current() {
            Some(key) => app.status = format!("folded {}", short_group(&key)),
            None => {
                app.status = if app.group == Group::Flat {
                    "nothing to fold (flat)".into()
                } else {
                    "already folded".into()
                }
            }
        },
        (KeyCode::Char('l'), _) => {
            app.status = if app.group == Group::Flat {
                "nothing to fold (flat)".into()
            } else if app.expand_current() {
                "unfolded".into()
            } else {
                "already unfolded".into()
            }
        }
        (KeyCode::Char('H'), _) => {
            let n = app.collapse_all();
            app.status = if app.group == Group::Flat {
                "nothing to fold (flat)".into()
            } else {
                format!("folded {} groups", n)
            }
        }
        (KeyCode::Char('L'), _) => {
            let n = app.expand_all();
            app.status = if n > 0 {
                format!("unfolded {} groups", n)
            } else {
                "nothing folded".into()
            }
        }
        (KeyCode::Char('r'), _) => {
            app.status = "rescanned".into();
            return KeyOutcome::Refresh(false);
        }
        (KeyCode::Char('R'), _) => {
            app.status = "full rescan (cache rebuilt)".into();
            return KeyOutcome::Refresh(true);
        }
        (KeyCode::Backspace, _) => {
            app.filter.pop();
            app.apply_filter_sort();
        }
        _ => {}
    }
    KeyOutcome::Continue
}

/// Run the TUI. `force_first` makes the initial background rescan a full
/// rebuild (`--refresh`), which still runs off the UI thread: the first
/// frame comes from the cache either way.
pub fn run(app: &mut App, force_first: bool) -> Result<Option<OpenAction>> {
    let mut terminal = ratatui::init();
    let mut list_state = ListState::default();
    let mut last_full = std::time::Instant::now();
    let mut last_running = std::time::Instant::now();
    let mut result: Option<OpenAction> = None;
    // Full rescans (local scan + ssh mirrors + remote detection) run on a
    // worker thread: with two hosts that's a dozen ssh roundtrips, and doing
    // them here froze key handling for seconds. Only one worker at a time;
    // a refresh requested mid-flight (10s tick or r/R) queues behind it.
    // A panicking worker must wedge neither the list nor future refreshes:
    // failures arrive as `None` and keep the current data on screen.
    let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<(Option<RefreshResult>, bool)>();
    let mut refresh_busy = false;
    // Startup painted from the cache (`App::new` → `refresh_local`); queue the
    // ssh pass at once so remotes correct themselves behind the first frame
    // instead of blocking it on handshakes.
    let mut refresh_queued: Option<bool> = Some(force_first);
    let spawn_refresh =
        |app: &App, tx: &std::sync::mpsc::Sender<(Option<RefreshResult>, bool)>, force: bool| {
            let config = app.config.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let res = std::panic::catch_unwind(|| full_refresh(&config, force)).ok();
                let _ = tx.send((res, force));
            });
        };

    loop {
        // Collect a finished background refresh without blocking. The
        // selection key is read now (post-keypress) so the swap doesn't yank
        // a row the user just moved to while ssh was in flight.
        let mut drained: Option<(Option<RefreshResult>, bool)> = None;
        while let Ok(msg) = refresh_rx.try_recv() {
            drained = Some(msg);
        }
        if let Some((res, _force)) = drained {
            refresh_busy = false;
            app.syncing = false;
            if let Some(res) = res {
                let sel_key = app.order.get(app.sel).map(|&i| app.sessions[i].key());
                app.apply_refresh(res, sel_key);
            }
            last_full = std::time::Instant::now();
            last_running = last_full;
        }
        if !refresh_busy {
            if let Some(force) = refresh_queued.take() {
                app.syncing = true;
                spawn_refresh(app, &refresh_tx, force);
                refresh_busy = true;
            } else if last_full.elapsed() > std::time::Duration::from_secs(10) {
                app.syncing = true;
                spawn_refresh(app, &refresh_tx, false);
                refresh_busy = true;
            } else if last_running.elapsed() > std::time::Duration::from_secs(1) {
                // Cheap synchronous tick: local `ps` only, no ssh.
                app.refresh_running();
                last_running = std::time::Instant::now();
            }
        } else if last_running.elapsed() > std::time::Duration::from_secs(1) {
            // Even mid-sync, local dots stay live: re-map local procs over
            // the current (pre-swap) session list.
            app.refresh_running();
            last_running = std::time::Instant::now();
        }
        let width = terminal.size()?.width as usize;
        let items = app.build_items(width);
        sync_selection(&items, app, &mut list_state);

        terminal.draw(|f| ui(f, app, &items, &mut list_state))?;

        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match handle_key(app, key, &items, list_state.selected()) {
                    KeyOutcome::Continue => {}
                    KeyOutcome::Refresh(full) => {
                        // Queue behind any in-flight worker; the loop spawns
                        // it (a force refresh wins over a queued routine one).
                        refresh_queued = Some(refresh_queued.unwrap_or(false) || full);
                        app.status = if full {
                            "full rescan queued".into()
                        } else {
                            "rescan queued".into()
                        };
                    }
                    KeyOutcome::Open(a) => {
                        result = Some(a);
                        break;
                    }
                    KeyOutcome::Quit => break,
                }
            }
        }
    }

    app.save_ui_state();
    ratatui::restore();
    Ok(result)
}

#[derive(Serialize, Deserialize, Default)]
struct UiState {
    #[serde(default)]
    view_filter: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    sort: String,
    #[serde(default)]
    show_prompts: bool,
    #[serde(default)]
    detail: bool,
    selected: Option<String>,
    #[serde(default)]
    collapsed: Vec<String>,
}

fn ui_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/psst/ui-state.json")
}

/// Reverse-video the row except the status glyph, which keeps its own color.
/// Headers (no glyph) reverse fully so a parked selection still reads.
fn highlight_row(lines: &[Line<'static>], full: bool) -> Vec<Line<'static>> {
    lines
        .iter()
        .enumerate()
        .map(|(li, line)| {
            Line::from(
                line.spans
                    .iter()
                    .enumerate()
                    .map(|(si, sp)| {
                        if !full && li == 0 && si == 0 {
                            sp.clone()
                        } else {
                            sp.clone()
                                .patch_style(Style::default().add_modifier(Modifier::REVERSED))
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Keep the ListState's selected item aligned with app.sel (which indexes `order`).
fn sync_selection(items: &[Item], app: &mut App, state: &mut ListState) {
    // Parked on a header: highlight it, clearing a stale park.
    if let Some(park) = app.header_park.clone() {
        for (item_pos, item) in items.iter().enumerate() {
            if item.group_key.as_deref() == Some(park.as_str()) {
                state.select(Some(item_pos));
                return;
            }
        }
        app.header_park = None;
    }
    let target_session = app.order.get(app.sel).copied();
    for (item_pos, item) in items.iter().enumerate() {
        if item.session.is_some() && item.session == target_session {
            state.select(Some(item_pos));
            return;
        }
    }
    // Selection landed inside a folded group (filter narrowed the list):
    // park on its header instead of jumping to the top.
    if let Some(t) = target_session {
        if t < app.sessions.len() {
            let key = app.group_label(t);
            if app.collapsed.contains(&key) {
                app.header_park = Some(key.clone());
                for (item_pos, item) in items.iter().enumerate() {
                    if item.group_key.as_deref() == Some(key.as_str()) {
                        state.select(Some(item_pos));
                        return;
                    }
                }
                app.header_park = None;
            }
        }
    }
    state.select(Some(0));
}

/// Centered rect of (w, h) inside `r` for the help overlay.
fn centered(r: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(r.width);
    let h = h.min(r.height);
    Rect::new(
        r.x + r.width.saturating_sub(w) / 2,
        r.y + r.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

fn help_lines() -> Vec<Line<'static>> {
    let key = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let row = |k: &str, d: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<12}", k), key),
            Span::raw(d.to_string()),
        ])
    };
    vec![
        row("/", "search (type anything, Esc unfocuses)"),
        row("f", "filter: all → named → active"),
        row("g / Tab", "group: flat → project → day → host"),
        row("s", "sort: activity → tmux"),
        row("j / k", "move"),
        row("J / K", "next / previous group"),
        row("h / l", "fold / unfold group"),
        row("H / L", "fold / unfold all groups"),
        row("d", "detail pane"),
        row("p", "last-prompt subtitles"),
        row("r / R", "rescan / full rescan"),
        row("enter", "open in tmux"),
        row("esc", "clear filter / close / quit"),
        row("q", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  ? / esc closes",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

/// Human label for a day bucket, relative to `today`.
fn day_label(date: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    if date == today {
        "Today".to_string()
    } else if date == today - chrono::Duration::days(1) {
        "Yesterday".to_string()
    } else {
        date.format("%a %b %-d %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_labels() {
        let today = chrono::NaiveDate::from_ymd_opt(2025, 6, 2).unwrap();
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(
            day_label(today - chrono::Duration::days(1), today),
            "Yesterday"
        );
        assert_eq!(
            day_label(today - chrono::Duration::days(2), today),
            "Sat May 31 2025"
        );
        assert_eq!(
            day_label(chrono::NaiveDate::from_ymd_opt(2025, 1, 5).unwrap(), today),
            "Sun Jan 5 2025"
        );
    }

    fn fake_session(name: &str, cwd: &str, ts: i64) -> SessionMeta {
        SessionMeta {
            path: PathBuf::from(format!("/tmp/{}.jsonl", name)),
            host: LOCAL_HOST.to_string(),
            id: name.to_string(),
            cwd: cwd.to_string(),
            created: Some(ts),
            last_activity: Some(ts),
            last_user: Some(ts),
            name: Some(name.to_string()),
            first_prompt: None,
            last_prompt: None,
            model: None,
            cost_total: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            messages: 1,
            branches: 0,
            parent_session: None,
            mtime: ts,
            size: 100,
        }
    }

    fn fake_remote(name: &str, cwd: &str, ts: i64) -> SessionMeta {
        let mut s = fake_session(name, cwd, ts);
        s.host = "coop".to_string();
        s.path = PathBuf::from(format!("/home/agent/{}.jsonl", name));
        s
    }

    fn fold_app() -> App {
        App {
            sessions: vec![
                fake_session("a1", "/proj/a", 1000),
                fake_session("a2", "/proj/a", 2000),
                fake_session("b1", "/proj/b", 1500),
                fake_session("c1", "/proj/c", 1200),
            ],
            running: HashMap::new(),
            host_errors: Vec::new(),
            syncing: false,
            order: Vec::new(),
            sel: 0,
            filter: String::new(),
            group: Group::Project,
            sort: Sort::Activity,
            config: Config::default(),
            status: String::new(),
            detail: false,
            show_prompts: false,
            search_focus: false,
            view_filter: ViewFilter::All,
            cwd_scope: None,
            pending_sel: None,
            recent_cache: None,
            collapsed: HashSet::new(),
            header_park: None,
            show_help: false,
        }
    }

    fn visible_members(app: &App) -> usize {
        app.build_items(100)
            .iter()
            .filter(|it| it.session.is_some())
            .count()
    }

    #[test]
    fn filter_cycles_all_named_active() {
        let mut f = ViewFilter::All;
        f = f.next();
        assert_eq!(f, ViewFilter::Named);
        f = f.next();
        assert_eq!(f, ViewFilter::Active);
        f = f.next();
        assert_eq!(f, ViewFilter::All);
        // the dropped tmux filter reads back as nothing (old state -> All)
        assert_eq!(ViewFilter::from_label("tmux"), None);
    }

    #[test]
    fn fold_current_hides_members_and_moves_selection() {
        let mut app = fold_app();
        app.apply_filter_sort();
        // activity desc: a2(2000), b1(1500), c1(1200), a1(1000)
        assert_eq!(visible_members(&app), 4);
        assert_eq!(app.collapse_current().as_deref(), Some("/proj/a"));
        // folding parks on the header
        assert_eq!(app.header_park.as_deref(), Some("/proj/a"));
        assert_eq!(visible_members(&app), 2);
        // header flips to the folded arrow
        let items = app.build_items(100);
        assert!(items[0].lines[0]
            .spans
            .iter()
            .any(|sp| sp.content.contains('\u{25b8}')));
        // j walks rows: next header, member, header, parked tail
        app.move_sel(1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(app.order[app.sel], 2);
        app.move_sel(1);
        assert_eq!(app.header_park, None);
        assert_eq!(app.order[app.sel], 2);
        app.move_sel(1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/c"));
        // k walks back; headers stop both ways
        app.move_sel(-1);
        assert_eq!(app.header_park, None);
        assert_eq!(app.order[app.sel], 2);
        app.move_sel(-1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        // unfold from the parked spot, twice is a no-op
        app.sel = 0;
        assert!(app.expand_current());
        assert!(!app.expand_current());
        assert_eq!(visible_members(&app), 4);
    }

    #[test]
    fn fold_all_and_unfold_all() {
        let mut app = fold_app();
        app.apply_filter_sort();
        assert_eq!(app.collapse_all(), 3);
        assert_eq!(visible_members(&app), 0);
        // headers still render, one per group
        assert_eq!(app.build_items(100).len(), 3);
        assert_eq!(app.expand_all(), 3);
        assert_eq!(app.expand_all(), 0);
        assert_eq!(visible_members(&app), 4);
    }

    #[test]
    fn group_jump_lands_on_headers() {
        let mut app = fold_app();
        app.apply_filter_sort();
        // order a2(0), b1(1), c1(2), a1(3); display groups A, B, C
        app.move_group(1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(app.sel, 1);
        app.move_group(1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/c"));
        assert_eq!(app.sel, 2);
        // K steps back a header at a time
        app.move_group(-1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(app.sel, 1);
        // K from a member snaps to its own header, keeping the row
        app.header_park = None;
        app.sel = 3;
        app.move_group(-1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/a"));
        assert_eq!(app.sel, 3);
        // K from the first group's header goes nowhere
        app.sel = 0;
        app.move_group(-1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/a"));
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn folded_header_highlights_and_carries_key() {
        let mut app = fold_app();
        app.apply_filter_sort();
        app.collapse_all();
        let items = app.build_items(100);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].group_key.as_deref(), Some("/proj/a"));
        // sel=0 sits on a2 inside folded /proj/a: first header highlights
        let mut state = ListState::default();
        sync_selection(&items, &mut app, &mut state);
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn open_headers_are_stops() {
        let mut app = fold_app();
        app.apply_filter_sort();
        // rows: H_a, a2, a1, H_b, b1, H_c, c1 — nothing folded
        app.move_sel(1);
        assert_eq!(app.order[app.sel], 0);
        assert_eq!(app.header_park, None);
        app.move_sel(1);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(app.order[app.sel], 2);
        app.move_sel(1);
        assert_eq!(app.header_park, None);
        assert_eq!(app.order[app.sel], 2);
    }

    fn reversed(sp: &Span) -> bool {
        sp.style.add_modifier.contains(Modifier::REVERSED)
    }

    #[test]
    fn highlight_keeps_glyph_but_reverses_headers() {
        let row = Line::from(vec![Span::raw("● "), Span::raw("title")]);
        let out = highlight_row(&[row], false);
        assert!(!reversed(&out[0].spans[0]));
        assert!(reversed(&out[0].spans[1]));
        let head = Line::from(Span::raw("▾ Today"));
        let out = highlight_row(&[head], true);
        assert!(reversed(&out[0].spans[0]));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn keyed_app() -> App {
        let mut app = fold_app();
        app.apply_filter_sort();
        app
    }

    fn press(app: &mut App, ke: KeyEvent) -> KeyOutcome {
        let items = app.build_items(100);
        handle_key(app, ke, &items, None)
    }

    #[test]
    fn keys_quit_and_escape() {
        let mut app = keyed_app();
        assert_eq!(press(&mut app, key(KeyCode::Char('q'))), KeyOutcome::Quit);
        assert_eq!(press(&mut app, ctrl(KeyCode::Char('c'))), KeyOutcome::Quit);
        assert_eq!(press(&mut app, key(KeyCode::Esc)), KeyOutcome::Quit);
        // Esc clears a filter first, and drops search focus before that
        app.filter = "a".into();
        app.search_focus = true;
        assert_eq!(press(&mut app, key(KeyCode::Esc)), KeyOutcome::Continue);
        assert!(!app.search_focus);
        assert_eq!(app.filter, "a");
        assert_eq!(press(&mut app, key(KeyCode::Esc)), KeyOutcome::Continue);
        assert!(app.filter.is_empty());
    }

    #[test]
    fn keys_help_overlay() {
        let mut app = keyed_app();
        assert_eq!(
            press(&mut app, key(KeyCode::Char('?'))),
            KeyOutcome::Continue
        );
        assert!(app.show_help);
        // ordinary keys are swallowed while help is up
        assert_eq!(
            press(&mut app, key(KeyCode::Char('j'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.sel, 0);
        for closer in [KeyCode::Char('?'), KeyCode::Esc, KeyCode::Char('q')] {
            app.show_help = true;
            assert_eq!(press(&mut app, key(closer)), KeyOutcome::Continue);
            assert!(!app.show_help);
        }
        app.show_help = true;
        assert_eq!(press(&mut app, ctrl(KeyCode::Char('c'))), KeyOutcome::Quit);
    }

    #[test]
    fn keys_search_typing() {
        let mut app = keyed_app();
        assert_eq!(
            press(&mut app, key(KeyCode::Char('/'))),
            KeyOutcome::Continue
        );
        assert!(app.search_focus);
        assert_eq!(
            press(&mut app, key(KeyCode::Char('b'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.filter, "b");
        assert_eq!(
            press(&mut app, key(KeyCode::Backspace)),
            KeyOutcome::Continue
        );
        assert!(app.filter.is_empty());
    }

    #[test]
    fn keys_filter_sort_group_toggles() {
        let mut app = keyed_app();
        assert_eq!(
            press(&mut app, key(KeyCode::Char('f'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.view_filter, ViewFilter::Named);
        assert_eq!(app.status, "filter:named");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('g'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.group, Group::Day);
        assert_eq!(app.header_park, None);
        assert_eq!(app.status, "group:day");
        assert_eq!(press(&mut app, key(KeyCode::Tab)), KeyOutcome::Continue);
        assert_eq!(app.group, Group::Host);
        assert_eq!(press(&mut app, key(KeyCode::Tab)), KeyOutcome::Continue);
        assert_eq!(app.group, Group::Flat);
        assert_eq!(
            press(&mut app, key(KeyCode::Char('s'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "sort:tmux");
        assert_eq!(
            press(&mut app, ctrl(KeyCode::Char('s'))),
            KeyOutcome::Continue
        );
        assert_eq!(
            press(&mut app, key(KeyCode::Char('d'))),
            KeyOutcome::Continue
        );
        assert!(app.detail);
        assert_eq!(
            press(&mut app, ctrl(KeyCode::Char('d'))),
            KeyOutcome::Continue
        );
        assert!(!app.detail);
        assert_eq!(
            press(&mut app, key(KeyCode::Char('p'))),
            KeyOutcome::Continue
        );
        assert!(app.show_prompts);
        assert_eq!(app.status, "prompts shown");
        assert_eq!(
            press(&mut app, ctrl(KeyCode::Char('p'))),
            KeyOutcome::Continue
        );
        assert!(!app.show_prompts);
    }

    #[test]
    fn keys_movement_and_group_jump() {
        let mut app = keyed_app();
        assert_eq!(press(&mut app, key(KeyCode::End)), KeyOutcome::Continue);
        assert_eq!(app.sel, 3);
        assert_eq!(press(&mut app, key(KeyCode::Home)), KeyOutcome::Continue);
        assert_eq!(app.sel, 0);
        assert_eq!(press(&mut app, key(KeyCode::Down)), KeyOutcome::Continue);
        assert_eq!(app.order[app.sel], 0);
        assert_eq!(press(&mut app, key(KeyCode::Up)), KeyOutcome::Continue);
        assert_eq!(app.order[app.sel], 1);
        assert_eq!(
            press(&mut app, key(KeyCode::Char('J'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(
            press(&mut app, key(KeyCode::Char('K'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.header_park.as_deref(), Some("/proj/a"));
        // explicit shift modifiers take the same path
        let shift_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SHIFT);
        assert_eq!(press(&mut app, shift_j), KeyOutcome::Continue);
        assert_eq!(app.header_park.as_deref(), Some("/proj/b"));
        assert_eq!(press(&mut app, key(KeyCode::F(1))), KeyOutcome::Continue);
    }

    #[test]
    fn keys_fold_unfold() {
        let mut app = keyed_app();
        assert_eq!(
            press(&mut app, key(KeyCode::Char('h'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "folded a");
        assert_eq!(app.header_park.as_deref(), Some("/proj/a"));
        assert_eq!(
            press(&mut app, key(KeyCode::Char('h'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "already folded");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('l'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "unfolded");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('l'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "already unfolded");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('H'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "folded 3 groups");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('L'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "unfolded 3 groups");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('L'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "nothing folded");
        // flat has nothing to fold
        app.group = Group::Flat;
        assert_eq!(
            press(&mut app, key(KeyCode::Char('h'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "nothing to fold (flat)");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('l'))),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "nothing to fold (flat)");
    }

    #[test]
    fn key_enter_opens_session() {
        let mut app = keyed_app();
        let items = app.build_items(100);
        let pos = items.iter().position(|it| it.session.is_some()).unwrap();
        match handle_key(&mut app, key(KeyCode::Enter), &items, Some(pos)) {
            KeyOutcome::Open(OpenAction::TmuxNew { cwd, .. }) => {
                assert_eq!(cwd, "/proj/a")
            }
            other => panic!("expected Open, got {:?}", other),
        }
    }

    #[test]
    fn key_enter_toggles_header() {
        let mut app = keyed_app();
        app.collapse_current();
        let items = app.build_items(100);
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter), &items, Some(0)),
            KeyOutcome::Continue
        );
        assert!(app.collapsed.is_empty());
        assert_eq!(app.status, "unfolded");
        let items = app.build_items(100);
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter), &items, Some(0)),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "folded a");
    }

    #[test]
    fn key_enter_with_empty_list() {
        let mut app = keyed_app();
        app.filter = "zzz-no-match".into();
        app.apply_filter_sort();
        let items = app.build_items(100);
        assert!(items.is_empty());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter), &items, None),
            KeyOutcome::Continue
        );
        assert_eq!(app.status, "nothing selected");
    }

    #[test]
    fn keys_refresh_is_an_effect() {
        let mut app = keyed_app();
        assert_eq!(
            press(&mut app, key(KeyCode::Char('r'))),
            KeyOutcome::Refresh(false)
        );
        assert_eq!(app.status, "rescanned");
        assert_eq!(
            press(&mut app, key(KeyCode::Char('R'))),
            KeyOutcome::Refresh(true)
        );
        assert_eq!(app.status, "full rescan (cache rebuilt)");
    }

    #[test]
    fn uistate_round_trips_collapsed() {
        let st = UiState {
            view_filter: "named".to_string(),
            group: "project".to_string(),
            sort: "activity".to_string(),
            show_prompts: true,
            detail: false,
            selected: None,
            collapsed: vec!["/proj/a".to_string()],
        };
        let raw = serde_json::to_string(&st).unwrap();
        let back: UiState = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.collapsed, vec!["/proj/a".to_string()]);
        assert_eq!(back.view_filter, "named");
    }

    #[test]
    fn selected_keys_round_trip_with_host() {
        let k = parse_selected("coop:/home/agent/x.jsonl");
        assert_eq!(k.host, "coop");
        assert_eq!(k.path, PathBuf::from("/home/agent/x.jsonl"));
        // legacy bare paths read as local
        let k = parse_selected("/tmp/x.jsonl");
        assert_eq!(k.host, LOCAL_HOST);
    }

    #[test]
    fn short_group_qualifies_remote_projects() {
        assert_eq!(short_group("/proj/a"), "a");
        assert_eq!(short_group("coop:/home/agent/dotfiles"), "coop:dotfiles");
        assert_eq!(short_group("Today"), "Today");
    }

    #[test]
    fn host_grouping_buckets_by_host() {
        let mut app = fold_app();
        app.sessions.push(fake_remote("r1", "/home/agent", 5000));
        app.group = Group::Host;
        app.apply_filter_sort();
        let groups = app.groups_in_order();
        assert_eq!(groups.len(), 2);
        // local first even though the remote is newest
        assert_eq!(groups[0].0, LOCAL_HOST);
        assert_eq!(groups[1].0, "coop");
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn project_grouping_qualifies_remote_cwds() {
        let mut app = fold_app();
        // Same cwd string on both hosts must form two groups, not one.
        let mut dup = fake_remote("r1", "/proj/a", 5000);
        dup.cwd = "/proj/a".to_string();
        app.sessions.push(dup);
        app.group = Group::Project;
        app.apply_filter_sort();
        let groups = app.groups_in_order();
        let keys: Vec<&str> = groups.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"/proj/a"), "got {:?}", keys);
        assert!(keys.contains(&"coop:/proj/a"), "got {:?}", keys);
    }

    #[test]
    fn remote_rows_carry_badge_and_detail_host() {
        let mut app = fold_app();
        app.sessions.push(fake_remote("r1", "/home/agent", 5000));
        app.apply_filter_sort();
        let items = app.build_items(120);
        let badge = items.iter().any(|it| {
            it.lines
                .iter()
                .any(|l| l.spans.iter().any(|sp| sp.content.contains("[coop]")))
        });
        assert!(badge, "remote row should carry a [coop] badge");
        let idx = app.sessions.iter().position(|s| s.host == "coop").unwrap();
        let hit_lines = detail_lines(&app.sessions[idx], None, &[]);
        assert!(hit_lines
            .iter()
            .any(|l| l.spans.iter().any(|sp| sp.content == "coop")));
    }

    #[test]
    fn apply_refresh_swaps_data_and_keeps_selection() {
        let mut app = fold_app();
        app.apply_filter_sort();
        // Select b1 (order is activity-desc: a2, b1, c1, a1).
        app.sel = 1;
        let key = app.sessions[app.order[app.sel]].key();
        // Background result: same sessions plus a newcomer on top.
        let mut sessions = app.sessions.clone();
        sessions.push(fake_session("z9", "/proj/z", 9999));
        let res = RefreshResult {
            sessions,
            hits: HashMap::new(),
            errors: vec!["coop: boom".to_string()],
        };
        app.apply_refresh(res, Some(key.clone()));
        assert_eq!(app.sessions.len(), 5);
        assert_eq!(app.sessions[app.order[app.sel]].key(), key);
        assert_eq!(app.host_errors, vec!["coop: boom".to_string()]);
        // Unknown keys clamp instead of panicking.
        app.apply_refresh(
            RefreshResult {
                sessions: vec![],
                hits: HashMap::new(),
                errors: vec![],
            },
            Some(key),
        );
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn open_remote_session_targets_its_host_tab() {
        use crate::config::HostConfig;
        let cfg = Config::with_hosts_for_test(vec![HostConfig {
            name: "coop".to_string(),
            ssh_target: "coop".to_string(),
            sessions_dir: None,
            tab: None,
        }]);
        assert_eq!(cfg.hosts().len(), 1);
        let mut app = fold_app();
        app.config = cfg;
        app.sessions.push(fake_remote("r1", "/home/agent", 5000));
        app.apply_filter_sort();
        // Select the remote session (newest activity sorts first).
        let pos = app
            .order
            .iter()
            .position(|&i| app.sessions[i].host == "coop")
            .unwrap();
        app.sel = pos;
        app.header_park = None;
        // Idle remote session: window goes to the host's tmux, tab gets focus.
        match app.open_selected() {
            Some(OpenAction::SshFocusNew { focus, cwd, .. }) => {
                assert_eq!(focus.ssh_target, "coop");
                assert_eq!(focus.app, "Ghostty");
                assert_eq!(focus.tags.first().map(String::as_str), Some("coop:"));
                assert_eq!(cwd, "/home/agent");
            }
            other => panic!("expected SshFocusNew, got {:?}", other),
        }
        // Running remote session: focus the tab showing its tmux window.
        let key = app.sessions[app.order[app.sel]].key();
        app.running.insert(
            key,
            RunningHit {
                pid: 99,
                cpu: 0.0,
                tmux: Some(crate::running::TmuxTarget {
                    session: "code".to_string(),
                    window: Some(3),
                    attention: false,
                }),
                direct: true,
                summary: None,
                status: None,
                peer_name: None,
            },
        );
        match app.open_selected() {
            Some(OpenAction::SshFocus { focus, target, .. }) => {
                assert_eq!(focus.ssh_target, "coop");
                assert_eq!(target, "code:3");
            }
            other => panic!("expected SshFocus, got {:?}", other),
        }
    }

    #[test]
    fn local_sessions_never_focus_a_tab() {
        // psst already runs in the tab holding the local tmux, so a click
        // would be a no-op at best and a jump away at worst.
        let mut app = fold_app();
        app.apply_filter_sort();
        app.sel = 0;
        app.header_park = None;
        assert!(matches!(
            app.open_selected(),
            Some(OpenAction::TmuxNew { .. }) | Some(OpenAction::TmuxAttach { .. })
        ));
    }
}
