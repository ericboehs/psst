//! ratatui TUI: list -> filter -> inspect -> open (resume in tmux).

use crate::config::Config;
use crate::running::{detect, live_mark, map_running, LiveMark, RunningHit};
use crate::scan::{in_scope, recent_messages, scan, sessions_root, SessionMeta};
use anyhow::Result;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
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
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Flat => "flat",
            Group::Project => "project",
            Group::Day => "day",
        }
    }
    fn next(self) -> Group {
        match self {
            Group::Flat => Group::Project,
            Group::Project => Group::Day,
            Group::Day => Group::Flat,
        }
    }
    fn from_label(s: &str) -> Option<Group> {
        match s {
            "flat" => Some(Group::Flat),
            "project" => Some(Group::Project),
            "day" => Some(Group::Day),
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

#[derive(Debug, Clone)]
pub enum OpenAction {
    /// Switch/attach to an existing tmux session (`session[:window]`).
    TmuxAttach { target: String, inside: bool },
    /// Create a tmux window/session running the resume command.
    TmuxNew {
        cwd: String,
        name: String,
        command: String,
        inside: bool,
    },
}

pub struct App {
    pub sessions: Vec<SessionMeta>,
    pub running: HashMap<PathBuf, RunningHit>,
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
    pending_sel: Option<PathBuf>,
    /// Collapsed group keys (project cwd, or day label). h/l toggles.
    pub collapsed: HashSet<String>,
    /// Parked-on header key: j/k stop on headers, Enter toggles the fold.
    /// Not persisted; cleared whenever the selection lands on a session.
    header_park: Option<String>,
    /// ? help overlay.
    pub show_help: bool,
    /// Detail pane messages: (path, mtime, turns). Recomputed when the
    /// selection moves or the file grows.
    recent_cache: Option<(PathBuf, i64, Vec<(String, String)>)>,
}

/// One rendered row: a session (index into `sessions`) or a group header.
struct Item {
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
    format!("{}:{:02}{}", hour, t.minute(), if is_pm { "p" } else { "a" })
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

/// Status-friendly group name: project basename, day label as-is.
fn short_group(key: &str) -> String {
    project_base(key)
}

impl App {
    pub fn new(sessions: Vec<SessionMeta>, config: Config, cwd_scope: Option<PathBuf>) -> App {
        let mut app = App {
            sessions,
            running: HashMap::new(),
            order: Vec::new(),
            sel: 0,
            filter: String::new(),
            group: match config.group_default() {
                "project" => Group::Project,
                "flat" => Group::Flat,
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
        app.refresh(false);
        app
    }

    /// Rescan sessions (cache-aware) and re-detect running processes.
    pub fn refresh(&mut self, force: bool) {
        let root = sessions_root();
        let sel_path = self
            .order
            .get(self.sel)
            .map(|&i| self.sessions[i].path.clone());
        if let Ok(s) = scan(&root, force) {
            self.sessions = s;
        }
        let procs = detect();
        self.running = map_running(&procs, &self.sessions, self.config.active_secs());
        self.apply_filter_sort();
        let restore = sel_path.or_else(|| self.pending_sel.take());
        if let Some(p) = restore {
            if let Some(pos) = self.order.iter().position(|&i| self.sessions[i].path == p) {
                self.sel = pos;
            }
        }
        self.clamp_sel();
    }

    /// Turns for the detail pane, cached by path+mtime so live sessions update.
    fn recent_for_selected(&mut self) -> Vec<(String, String)> {
        let (path, mtime) = match self.selected() {
            Some(s) => {
                let mt = std::fs::metadata(&s.path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (s.path.clone(), mt)
            }
            None => return Vec::new(),
        };
        if let Some((p, m, msgs)) = &self.recent_cache {
            if p == &path && *m == mtime {
                return msgs.clone();
            }
        }
        let msgs = recent_messages(&path, 6);
        self.recent_cache = Some((path, mtime, msgs.clone()));
        msgs
    }

    /// Cheap tick: re-detect running processes without reparsing session files.
    pub fn refresh_running(&mut self) {
        let sel_path = self
            .order
            .get(self.sel)
            .map(|&i| self.sessions[i].path.clone());
        let procs = detect();
        self.running = map_running(&procs, &self.sessions, self.config.active_secs());
        self.apply_filter_sort();
        if let Some(p) = sel_path {
            if let Some(pos) = self.order.iter().position(|&i| self.sessions[i].path == p) {
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
        self.pending_sel = st.selected.map(PathBuf::from);
        self.collapsed = st.collapsed.into_iter().collect();
    }

    pub fn save_ui_state(&self) {
        let selected = self.order.get(self.sel).map(|&i| {
            self.sessions[i]
                .path
                .to_string_lossy()
                .into_owned()
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
                let hit = self.running.get(&s.path);
                if let Some(scope) = &self.cwd_scope {
                    if !in_scope(&s.cwd, scope) {
                        return false;
                    }
                }
                match self.view_filter {
                    ViewFilter::All => {}
                    ViewFilter::Named => {
                        let named = s.name.is_some()
                            || hit.and_then(|h| h.peer_name.as_deref()).is_some();
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
                    "{} {} {} {} {} {} {} {}",
                    display_title(s, hit),
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
                idx.sort_by_key(|&i| {
                    std::cmp::Reverse(self.sessions[i].last_user_or_activity())
                });
            }
            Sort::Tmux => {
                idx.sort_by(|&a, &b| {
                    let key =
                        |i: usize| -> (u8, String, u32, std::cmp::Reverse<i64>) {
                            match self.running.get(&self.sessions[i].path) {
                                Some(h) => (
                                    0u8,
                                    h.tmux
                                        .as_ref()
                                        .map(|t| t.session.clone())
                                        .unwrap_or_default(),
                                    h.tmux.as_ref().and_then(|t| t.window).unwrap_or(0),
                                    std::cmp::Reverse(
                                        self.sessions[i].last_user_or_activity(),
                                    ),
                                ),
                                None => (
                                    1u8,
                                    String::new(),
                                    0,
                                    std::cmp::Reverse(
                                        self.sessions[i].last_user_or_activity(),
                                    ),
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
            rows.iter().position(|r| matches!(r, Row::Member(pos) if *pos == self.sel))
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
    fn group_label(&self, i: usize) -> String {
        match self.group {
            Group::Flat => String::new(),
            Group::Project => self.sessions[i].cwd.clone(),
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
            Group::Project => {
                // bucket by cwd; groups ordered by most recent activity.
                let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
                for (pos, &i) in self.order.iter().enumerate() {
                    buckets
                        .entry(self.sessions[i].cwd.clone())
                        .or_default()
                        .push(pos);
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
                    .map(|d| {
                        (
                            day_label(d, today),
                            buckets.remove(&d).unwrap_or_default(),
                        )
                    })
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
        if let Some(hit) = self.running.get(&s.path) {
            if let Some(t) = &hit.tmux {
                let target = match t.window {
                    Some(w) => format!("{}:{}", t.session, w),
                    None => t.session.clone(),
                };
                return Some(OpenAction::TmuxAttach { target, inside });
            }
            // running without a tmux target: fall through to a fresh resume window
        }
        let command = self
            .config
            .resume_command()
            .replace("{file}", &s.path.to_string_lossy())
            .replace("{id}", &s.id)
            .replace("{cwd}", &s.cwd);
        Some(OpenAction::TmuxNew {
            cwd: s.cwd.clone(),
            name: project_base(&s.cwd),
            command,
            inside,
        })
    }

    fn row_line(&self, i: usize, w: usize) -> Line<'static> {
        let s = &self.sessions[i];
        let hit = self.running.get(&s.path);
        let mark = live_mark(hit);
        let dot = match mark {
            LiveMark::Working => Span::styled("● ", Style::default().fg(Color::Yellow)),
            LiveMark::Prompt => {
                Span::styled("● ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            }
            LiveMark::Unread => {
                Span::styled("● ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            }
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
            dot, fork, title, Span::raw(" "), proj, Span::raw(" "), age, tmux,
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
            Group::Project | Group::Day => {
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
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
    if let Some(n) = hit.and_then(|h| h.peer_name.as_deref()).filter(|n| !n.is_empty()) {
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
            Span::styled("project: ", dim),
            Span::raw(s.cwd.clone()),
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
                    " {} pid {} @ {} ({}){}",
                    mark.as_str(),
                    h.pid,
                    t,
                    if h.direct { "exact" } else { "heuristic" },
                    status
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
        match live_mark(app.running.get(&app.sessions[i].path)) {
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
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
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
            Span::styled("/", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("/", Style::default().fg(Color::DarkGray))
        };
    let query = if app.search_focus {
        Span::styled(
            format!("{}▏", app.filter),
            Style::default().add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            app.filter.clone(),
            Style::default().fg(Color::DarkGray),
        )
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
        let [list_area, detail_area] = Layout::vertical([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .areas(body);
        f.render_stateful_widget(list, list_area, list_state);
        let recent = app.recent_for_selected();
        if let Some(s) = app.selected() {
            let hit = app.running.get(&s.path);
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

pub fn run(app: &mut App) -> Result<Option<OpenAction>> {
    let mut terminal = ratatui::init();
    let mut list_state = ListState::default();
    let mut last_full = std::time::Instant::now();
    let mut last_running = std::time::Instant::now();
    let mut result: Option<OpenAction> = None;

    loop {
        if last_full.elapsed() > std::time::Duration::from_secs(10) {
            app.refresh(false);
            last_full = std::time::Instant::now();
            last_running = last_full;
        } else if last_running.elapsed() > std::time::Duration::from_secs(1) {
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
                if app.show_help {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                        (KeyCode::Char('?'), _)
                        | (KeyCode::Esc, _)
                        | (KeyCode::Char('q'), _) => app.show_help = false,
                        _ => {}
                    }
                    continue;
                }
                // Global: Esc, arrows, tab, ctrl chords work in both modes.
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {
                        if app.search_focus {
                            app.search_focus = false;
                            continue;
                        }
                        if !app.filter.is_empty() {
                            app.filter.clear();
                            app.apply_filter_sort();
                        } else {
                            break;
                        }
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
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
                        let header_key = list_state
                            .selected()
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
                                Some(a) => {
                                    result = Some(a);
                                    break;
                                }
                                None => app.status = "nothing selected".into(),
                            },
                        }
                    }
                    (KeyCode::Char('/'), m) if m == KeyModifiers::NONE && !app.search_focus => {
                        app.search_focus = true;
                    }
                    _ if app.search_focus => match key.code {
                        KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE
                            || key.modifiers == KeyModifiers::SHIFT =>
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
                    (KeyCode::Char('q'), _) => break,
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
                        app.refresh(false);
                        app.status = "rescanned".into();
                    }
                    (KeyCode::Char('R'), _) => {
                        app.refresh(true);
                        app.status = "full rescan (cache rebuilt)".into();
                    }
                    (KeyCode::Backspace, _) => {
                        app.filter.pop();
                        app.apply_filter_sort();
                    }
                    _ => {}
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
                            sp.clone().patch_style(
                                Style::default().add_modifier(Modifier::REVERSED),
                            )
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
        row("g / Tab", "group: flat → project → day"),
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

    fn fold_app() -> App {
        App {
            sessions: vec![
                fake_session("a1", "/proj/a", 1000),
                fake_session("a2", "/proj/a", 2000),
                fake_session("b1", "/proj/b", 1500),
                fake_session("c1", "/proj/c", 1200),
            ],
            running: HashMap::new(),
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
}