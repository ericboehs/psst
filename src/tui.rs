//! ratatui TUI: list -> filter -> inspect -> open (resume in tmux).

use crate::config::Config;
use crate::running::{detect, map_running, RunningHit};
use crate::scan::{scan, sessions_root, SessionMeta};
use anyhow::Result;
use chrono::Timelike;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::collections::HashMap;
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewFilter {
    All,
    Named,
    Tmux,
}

impl ViewFilter {
    fn label(self) -> &'static str {
        match self {
            ViewFilter::All => "all",
            ViewFilter::Named => "named",
            ViewFilter::Tmux => "tmux",
        }
    }
    fn next(self) -> ViewFilter {
        match self {
            ViewFilter::All => ViewFilter::Named,
            ViewFilter::Named => ViewFilter::Tmux,
            ViewFilter::Tmux => ViewFilter::All,
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
    /// When true, only sessions with a live pi process are listed.
    pub active_only: bool,
    /// f cycles all → named → tmux.
    pub view_filter: ViewFilter,
}

/// One rendered row: a session (index into `sessions`) or a group header.
struct Item {
    session: Option<usize>,
    lines: Vec<Line<'static>>,
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

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{}", n)
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

impl App {
    pub fn new(sessions: Vec<SessionMeta>, config: Config) -> App {
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
            active_only: false,
            view_filter: ViewFilter::All,
            config,
        };
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
        if let Some(p) = sel_path {
            if let Some(pos) = self.order.iter().position(|&i| self.sessions[i].path == p) {
                self.sel = pos;
            }
        }
        self.clamp_sel();
    }

    fn apply_filter_sort(&mut self) {
        let f = self.filter.to_lowercase();
        let mut idx: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                let s = &self.sessions[i];
                let hit = self.running.get(&s.path);
                if self.active_only && hit.is_none() {
                    return false;
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
                    ViewFilter::Tmux => {
                        if hit.and_then(|h| h.tmux.as_ref()).is_none() {
                            return false;
                        }
                    }
                }
                if f.is_empty() {
                    return true;
                }
                let peer = hit.and_then(|h| h.peer_name.as_deref()).unwrap_or("");
                let hay = format!(
                    "{} {} {} {} {} {}",
                    display_title(s, hit, self.show_prompts),
                    s.first_prompt.as_deref().unwrap_or(""),
                    s.cwd,
                    s.model.as_deref().unwrap_or(""),
                    peer,
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

    fn move_sel(&mut self, delta: i32) {
        if self.order.is_empty() {
            return;
        }
        let cur = self.sel as i32 + delta;
        self.sel = cur.clamp(0, self.order.len() as i32 - 1) as usize;
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
        let dot = if hit.is_some() {
            Span::styled("● ", Style::default().fg(Color::Green))
        } else {
            Span::styled("○ ", Style::default().fg(Color::DarkGray))
        };
        let fork = if s.parent_session.is_some() {
            Span::styled("⎇ ", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("  ")
        };
        let title_w = (w / 2).max(32);
        let label = display_title(s, hit, self.show_prompts);
        let named = s.name.is_some() || hit.and_then(|h| h.peer_name.as_deref()).is_some();
        let title = if named {
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
        vec![self.row_line(i, w)]
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
                    });
                }
            }
            Group::Project => {
                // bucket by cwd; groups ordered by most recent activity.
                // Members keep `order` sequence (i.e. the active sort).
                let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
                for &i in &self.order {
                    buckets
                        .entry(self.sessions[i].cwd.clone())
                        .or_default()
                        .push(i);
                }
                let mut groups: Vec<(String, Vec<usize>)> = buckets.into_iter().collect();
                groups.sort_by_key(|(_, v)| {
                    std::cmp::Reverse(
                        v.iter()
                            .map(|&i| self.sessions[i].last_user_or_activity())
                            .max()
                            .unwrap_or(0),
                    )
                });
                for (label, members) in groups {
                    self.push_group(&mut items, label, members, w);
                }
            }
            Group::Day => {
                // bucket by local date of last activity; newest day first
                let today = chrono::Local::now().date_naive();
                let mut buckets: HashMap<chrono::NaiveDate, Vec<usize>> = HashMap::new();
                for &i in &self.order {
                    let ts = self.sessions[i].last_user_or_activity();
                    let date = chrono::DateTime::from_timestamp(ts, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local)
                        .date_naive();
                    buckets.entry(date).or_default().push(i);
                }
                let mut dates: Vec<chrono::NaiveDate> = buckets.keys().copied().collect();
                dates.sort_by(|a, b| b.cmp(a));
                for date in dates {
                    let members = buckets.remove(&date).unwrap_or_default();
                    let label = day_label(date, today);
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
        let header = format!(
            "▾ {}  {} sessions · last {}",
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
        });
        for i in members {
            items.push(Item {
                session: Some(i),
                lines: self.member_lines(i, w),
            });
        }
    }
}

/// Running sessions show the live peer name (`pi-dotfiles-2`); others use /name or prompt.
fn display_title(s: &SessionMeta, hit: Option<&RunningHit>, show_prompts: bool) -> String {
    if let Some(n) = hit.and_then(|h| h.peer_name.as_deref()).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if hit.is_some() {
        return format!("pi-{}", project_base(&s.cwd));
    }
    if let Some(n) = &s.name {
        return n.clone();
    }
    if show_prompts {
        if let Some(p) = &s.first_prompt {
            return p.clone();
        }
    }
    s.id.chars().take(8).collect()
}

fn hay_contains(hay: &str, needle: &str) -> bool {
    // every whitespace-separated term must appear (substring match)
    needle.split_whitespace().all(|t| hay.contains(t))
}

fn detail_text(s: &SessionMeta, hit: Option<&RunningHit>, cfg: &Config) -> String {
    let mut v = Vec::new();
    v.push(format!("title:    {}", display_title(s, hit, true)));
    if let Some(n) = hit.and_then(|h| h.peer_name.as_deref()) {
        if s.title() != n {
            v.push(format!("prompt:   {}", s.title()));
        }
    }
    v.push(format!("project:  {}", s.cwd));
    v.push(format!("file:     {}", s.path.display()));
    v.push(format!("id:       {}", s.id));
    if let Some(c) = s.created {
        if let Some(t) = chrono::DateTime::from_timestamp(c, 0) {
            v.push(format!("created:  {}", t.format("%Y-%m-%d %H:%M")));
        }
    }
    if let Some(a) = s.last_activity {
        if let Some(t) = chrono::DateTime::from_timestamp(a, 0) {
            v.push(format!(
                "activity: {} ({} ago)",
                t.format("%Y-%m-%d %H:%M"),
                fmt_age(now() - a)
            ));
        }
    }
    v.push(format!(
        "tokens:   {} in / {} out",
        fmt_tokens(s.tokens_in),
        fmt_tokens(s.tokens_out)
    ));
    v.push(format!(
        "messages: {} ({} branch pts)",
        s.messages, s.branches
    ));
    if let Some(p) = &s.parent_session {
        v.push(format!("fork of:  {}", p));
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
            v.push(format!(
                "state:    ● RUNNING pid {} @ {} ({}){}",
                h.pid,
                t,
                if h.direct { "exact" } else { "heuristic" },
                status
            ));
        }
        None => v.push("state:    ○ idle".to_string()),
    }
    let preview = cfg
        .resume_command()
        .replace("{file}", &s.path.to_string_lossy())
        .replace("{id}", &s.id)
        .replace("{cwd}", &s.cwd);
    v.push(String::new());
    v.push(format!("resume:   {}", preview));
    if let Some(p) = &s.first_prompt {
        v.push(String::new());
        v.push("first prompt:".to_string());
        v.push(trunc(p, 200));
    }
    v.join("\n")
}

fn ui(f: &mut Frame, app: &App, items: &[Item], list_state: &mut ListState) {
    let area = f.area();
    let [header, filterbar, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let header_spans = vec![
        Span::styled(
            " psst ",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {} sessions · {} running · sort:{} group:{} filter:{}{}",
            app.sessions.len(),
            app.running.len(),
            app.sort.label(),
            app.group.label(),
            app.view_filter.label(),
            if app.active_only { " · active" } else { "" }
        )),
    ];
    f.render_widget(Paragraph::new(Line::from(header_spans)), header);

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

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|it| ListItem::new(Text::from(it.lines.clone())))
        .collect();
    let list = List::new(list_items)
        .block(if app.detail {
            Block::new().borders(Borders::BOTTOM)
        } else {
            Block::new()
        })
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    if app.detail {
        let [list_area, detail_area] = Layout::vertical([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .areas(body);
        f.render_stateful_widget(list, list_area, list_state);
        if let Some(s) = app.selected() {
            let hit = app.running.get(&s.path);
            let detail = Paragraph::new(detail_text(s, hit, &app.config))
                .wrap(Wrap { trim: false })
                .block(
                    Block::new()
                        .borders(Borders::TOP)
                        .title(" detail ")
                        .title_style(Style::default().fg(Color::Cyan)),
                );
            f.render_widget(detail, detail_area);
        }
    } else {
        f.render_stateful_widget(list, body, list_state);
    }

    let mut footer_spans = vec![Span::styled(
        " /=search  f=filter g=group s=sort a=active d=detail p=prompts  j/k  enter=open r=rescan esc q",
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
    let mut last_refresh = std::time::Instant::now();
    let mut result: Option<OpenAction> = None;

    loop {
        if last_refresh.elapsed() > std::time::Duration::from_secs(10) {
            app.refresh(false);
            last_refresh = std::time::Instant::now();
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
                    (KeyCode::Tab, _) => app.group = app.group.next(),
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
                    (KeyCode::Enter, _) => match app.open_selected() {
                        Some(a) => {
                            result = Some(a);
                            break;
                        }
                        None => app.status = "nothing selected".into(),
                    },
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
                    (KeyCode::Char('a'), _) => {
                        app.active_only = !app.active_only;
                        app.apply_filter_sort();
                        app.status = if app.active_only {
                            "active only".into()
                        } else {
                            "showing all".into()
                        };
                    }
                    (KeyCode::Char('f'), _) => {
                        app.view_filter = app.view_filter.next();
                        app.apply_filter_sort();
                        app.status = format!("filter:{}", app.view_filter.label());
                    }
                    (KeyCode::Char('g'), _) => {
                        app.group = app.group.next();
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
                    (KeyCode::Char('j'), _) => app.move_sel(1),
                    (KeyCode::Char('k'), _) => app.move_sel(-1),
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

    ratatui::restore();
    Ok(result)
}

/// Keep the ListState's selected item aligned with app.sel (which indexes `order`).
fn sync_selection(items: &[Item], app: &App, state: &mut ListState) {
    let target_session = app.order.get(app.sel).copied();
    let mut found = 0usize;
    for (item_pos, item) in items.iter().enumerate() {
        if let Some(si) = item.session {
            if Some(si) == target_session {
                found = item_pos;
                break;
            }
        }
    }
    state.select(Some(found));
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
}