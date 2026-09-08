//! Session discovery and parsing.
//!
//! Sessions live under `~/.pi/agent/sessions/<encoded-path>/<timestamp>_<uuid>.jsonl`
//! (or `$PI_CODING_AGENT_DIR/sessions/...`). Each file is JSONL: a `session`
//! header line followed by tree-linked entries (`id`/`parentId`).
//! Format reference: pi docs `session-format.md` (v3).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Host key for the local machine. Remote hosts use their config name (e.g. "coop").
pub const LOCAL_HOST: &str = "local";

fn default_host() -> String {
    LOCAL_HOST.to_string()
}

/// Identity of a session across hosts: (host, path-on-that-host).
/// Two hosts can share the same path string (e.g. `/home/agent/...`),
/// so the path alone is not a unique key.
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize)]
pub struct SessionKey {
    pub host: String,
    pub path: PathBuf,
}

/// Metadata extracted from one session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub path: PathBuf,
    /// Host that owns the session: "local" or a `[hosts.<name>]` key.
    #[serde(default = "default_host")]
    pub host: String,
    /// UUID from the session header.
    pub id: String,
    /// Working directory recorded in the header (authoritative project dir).
    pub cwd: String,
    /// Epoch seconds; header timestamp, falling back to file mtime.
    pub created: Option<i64>,
    /// Latest entry timestamp (last write to the file).
    pub last_activity: Option<i64>,
    /// Latest user-message timestamp (when you last sent something).
    #[serde(default)]
    pub last_user: Option<i64>,
    /// Latest `session_info` display name (`/name`).
    pub name: Option<String>,
    /// Text of the first user message.
    pub first_prompt: Option<String>,
    /// Text of the latest user message (not a slash command).
    #[serde(default)]
    pub last_prompt: Option<String>,
    /// Latest model seen (assistant message or `model_change`).
    pub model: Option<String>,
    /// Sum of `usage.cost.total` across assistant messages and toolResult nested usage.
    pub cost_total: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Count of `message` entries.
    pub messages: u64,
    /// Number of branch points (entries with more than one child).
    pub branches: u64,
    /// `parentSession` from the header (set by `/fork`, `/clone`).
    pub parent_session: Option<String>,
    /// File mtime in epoch seconds (also the "last written" signal).
    pub mtime: i64,
    pub size: u64,
}

impl SessionMeta {
    /// Host-qualified identity for running-hit maps and UI restore.
    pub fn key(&self) -> SessionKey {
        SessionKey {
            host: self.host.clone(),
            path: self.path.clone(),
        }
    }

    pub fn is_local(&self) -> bool {
        self.host == LOCAL_HOST
    }

    /// When you last sent a message, falling back to any activity.
    pub fn last_user_or_activity(&self) -> i64 {
        self.last_user.or(self.last_activity).unwrap_or(0)
    }

    /// Best human title: session name, else first prompt, else file name.
    pub fn title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        if let Some(p) = &self.first_prompt {
            return p.clone();
        }
        self.path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CacheEntry {
    pub(crate) mtime: i64,
    pub(crate) size: u64,
    pub(crate) meta: SessionMeta,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Cache(pub(crate) HashMap<String, CacheEntry>);

pub(crate) fn cache_path() -> PathBuf {
    if let Ok(x) = std::env::var("PSST_CACHE") {
        return PathBuf::from(x);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/psst/cache.json")
}

/// Cache key namespaces entries by host: "<host>\0<path>".
/// Pre-host cache files used the bare path; those read back as local.
pub(crate) fn cache_key(host: &str, path: &Path) -> String {
    format!("{}\0{}", host, path.to_string_lossy())
}

pub(crate) fn cache_lookup<'a>(cache: &'a Cache, host: &str, path: &Path) -> Option<&'a CacheEntry> {
    if let Some(e) = cache.0.get(&cache_key(host, path)) {
        return Some(e);
    }
    if host == LOCAL_HOST {
        return cache.0.get(&path.to_string_lossy().to_string());
    }
    None
}

pub(crate) fn load_cache() -> Cache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_cache(c: &Cache) {
    let p = cache_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(c) {
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// Sessions root: `$PI_CODING_AGENT_DIR/sessions` or `~/.pi/agent/sessions`.
pub fn sessions_root() -> PathBuf {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        return PathBuf::from(dir).join("sessions");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pi/agent/sessions")
}

fn parse_epoch(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp())
}

/// Extract readable text from a message `content` field (string or block array).
fn content_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        return Some(t.to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Parse a single session JSONL file into metadata (local host).
pub fn parse_file(path: &Path) -> Result<SessionMeta> {
    parse_file_with_host(path, LOCAL_HOST)
}

/// Parse a session file and tag it with `host` (`local` or a remote name).
pub fn parse_file_with_host(path: &Path, host: &str) -> Result<SessionMeta> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let stat = std::fs::metadata(path)?;
    let mtime = stat
        .modified()
        .ok()
        .and_then(|m: SystemTime| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(parse_full(&raw, path, mtime, stat.len(), host))
}

/// Fold one JSONL line into `meta` (shared by fresh and incremental parses).
fn fold_line(
    meta: &mut SessionMeta,
    child_counts: &mut HashMap<String, u32>,
    line: &str,
) {
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "session" => {
                meta.id = v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                meta.cwd = v
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string();
                meta.parent_session = v
                    .get("parentSession")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string());
                meta.created = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(parse_epoch);
            }
            "message" => {
                meta.messages += 1;
                let Some(msg) = v.get("message") else {
                    return;
                };
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
                match role {
                    "user" => {
                        let text =
                            content_text(msg.get("content").unwrap_or(&serde_json::Value::Null));
                        if meta.first_prompt.is_none() {
                            meta.first_prompt = text.clone();
                        }
                        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_epoch) {
                            if meta.last_user.is_none() || ts >= meta.last_user.unwrap_or(0) {
                                meta.last_user = Some(ts);
                                if let Some(t) = text.as_deref() {
                                    let t = t.trim();
                                    if !t.is_empty() && !t.starts_with('/') {
                                        meta.last_prompt = Some(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                    "assistant" => {
                        if let Some(u) = msg.get("usage") {
                            if let Some(c) = u.get("cost").and_then(|c| c.get("total")).and_then(|c| c.as_f64()) {
                                meta.cost_total += c;
                            }
                            meta.tokens_in += u.get("input").and_then(|t| t.as_u64()).unwrap_or(0);
                            meta.tokens_out += u.get("output").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                        if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                            meta.model = Some(m.to_string());
                        }
                    }
                    "toolResult" => {
                        // nested usage = LLM work performed inside tools
                        if let Some(u) = msg.get("usage") {
                            if let Some(c) = u.get("cost").and_then(|c| c.get("total")).and_then(|c| c.as_f64()) {
                                meta.cost_total += c;
                            }
                            meta.tokens_in += u.get("input").and_then(|t| t.as_u64()).unwrap_or(0);
                            meta.tokens_out += u.get("output").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                    }
                    _ => {}
                }
            }
            "session_info" => {
                if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                    meta.name = Some(n.to_string());
                }
            }
            "model_change" => {
                if let Some(m) = v.get("modelId").and_then(|m| m.as_str()) {
                    meta.model = Some(m.to_string());
                }
            }
            _ => {}
        }
        if let Some(pid) = v.get("parentId").and_then(|p| p.as_str()) {
            *child_counts.entry(pid.to_string()).or_insert(0) += 1;
        }
        // track max timestamp for last_activity
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_epoch) {
            if meta.last_activity.is_none() || ts > meta.last_activity.unwrap_or(0) {
                meta.last_activity = Some(ts);
            }
        }
}

/// Finish a fold: branch points from the child counts; a file with no
/// timestamps falls back to its mtime for activity sorting.
fn finalize(meta: &mut SessionMeta, child_counts: &HashMap<String, u32>, mtime: i64) {
    meta.branches = child_counts.values().filter(|&&c| c > 1).count() as u64;
    if meta.last_activity.is_none() {
        meta.last_activity = Some(mtime);
    }
}

fn fresh_meta(path: &Path, mtime: i64, size: u64, host: &str) -> SessionMeta {
    SessionMeta {
        path: path.to_path_buf(),
        host: host.to_string(),
        id: String::new(),
        cwd: String::new(),
        created: None,
        last_activity: None,
        last_user: None,
        name: None,
        first_prompt: None,
        last_prompt: None,
        model: None,
        cost_total: 0.0,
        tokens_in: 0,
        tokens_out: 0,
        messages: 0,
        branches: 0,
        parent_session: None,
        mtime,
        size,
    }
}

pub(crate) fn parse_full(
    raw: &str,
    path: &Path,
    mtime: i64,
    size: u64,
    host: &str,
) -> SessionMeta {
    let mut meta = fresh_meta(path, mtime, size, host);
    let mut child_counts: HashMap<String, u32> = HashMap::new();
    for line in raw.lines() {
        fold_line(&mut meta, &mut child_counts, line);
    }
    finalize(&mut meta, &child_counts, mtime);
    meta
}

/// Parse session JSONL already in memory (e.g. fetched over ssh).
/// `path` is the canonical path *on its host*; `mtime`/`size` are the
/// remote file's attributes so the shared cache stays valid.
pub fn parse_content(
    raw: &str,
    path: &Path,
    mtime: i64,
    size: u64,
    host: &str,
) -> Result<SessionMeta> {
    Ok(parse_full(raw, path, mtime, size, host))
}

/// Discover and parse all local sessions under `root`. `force` bypasses the cache.
pub fn scan(root: &Path, force: bool) -> anyhow::Result<Vec<SessionMeta>> {
    scan_with_host(root, LOCAL_HOST, force)
}

/// Same as [`scan`], tagging every session with `host`.
/// Used for remote mirrors (the mirror dir is scanned locally, then each
/// meta is rewritten to its remote path by the caller when needed).
pub fn scan_with_host(root: &Path, host: &str, force: bool) -> anyhow::Result<Vec<SessionMeta>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut cache = if force { Cache::default() } else { load_cache() };
    let mut changed = false;

    // structure: <root>/<encoded-project-dir>/*.jsonl
    let project_dirs = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    for pd in project_dirs.flatten() {
        let pd_path = pd.path();
        if !pd_path.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&pd_path) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(md) = f.metadata() else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let size = md.len();
            let key = cache_key(host, &path);

            if let Some(entry) = cache_lookup(&cache, host, &path) {
                if entry.mtime == mtime && entry.size == size {
                    // Reparse once after last_user was added so activity sort
                    // isn't stuck on None until the file next changes.
                    let missing_last = entry.meta.last_prompt.is_none()
                        && entry.meta.first_prompt.is_some();
                    if (entry.meta.last_user.is_some() || entry.meta.messages == 0)
                        && !missing_last
                    {
                        let mut meta = entry.meta.clone();
                        if meta.host.is_empty() {
                            meta.host = host.to_string();
                        }
                        out.push(meta);
                        continue;
                    }
                }
            }
            match parse_file_with_host(&path, host) {
                Ok(mut meta) => {
                    // Old cache entries predate `host`; normalize on rewrite
                    // and drop the legacy bare-path key.
                    meta.host = host.to_string();
                    if host == LOCAL_HOST {
                        cache.0.remove(&path.to_string_lossy().to_string());
                    }
                    cache.0.insert(
                        key,
                        CacheEntry {
                            mtime,
                            size,
                            meta: meta.clone(),
                        },
                    );
                    changed = true;
                    out.push(meta);
                }
                Err(_) => continue,
            }
        }
    }
    if changed {
        save_cache(&cache);
    }
    Ok(out)
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Directory `psst .` (or `psst <dir>`) should list: git root if any, else the dir.
pub fn resolve_scope(dir: &Path) -> PathBuf {
    let start = if dir == Path::new(".") {
        std::env::current_dir().unwrap_or_else(|_| dir.to_path_buf())
    } else {
        dir.to_path_buf()
    };
    let mut p = canon(&start);
    let orig = p.clone();
    loop {
        if p.join(".git").exists() {
            return p;
        }
        if !p.pop() {
            return orig;
        }
    }
}

/// True when the session belongs to `scope` (exact cwd or a subdirectory of it).
pub fn in_scope(session_cwd: &str, scope: &Path) -> bool {
    let s = canon(Path::new(session_cwd));
    let scope = canon(scope);
    s == scope || s.starts_with(&scope)
}

fn is_framed_delivery(t: &str) -> bool {
    t.starts_with("[cross-agent") || t.starts_with("[reply from")
}

fn join_text_blocks(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => None,
    }
}

fn tool_names(content: &serde_json::Value) -> Vec<&str> {
    let mut names = Vec::new();
    if let Some(blocks) = content.as_array() {
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                continue;
            }
            if let Some(n) = b.get("name").and_then(|x| x.as_str()) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
    }
    names
}

/// Latest `n` turns as (role, single-line text), oldest first, for the detail
/// pane. Skips slash commands, empty content, and framed cross-agent
/// deliveries. A textless assistant turn (pure tool calls) collapses to
/// `used tool:<names>` instead of vanishing; consecutive duplicates from
/// parallel batches collapse (`used tool:edit ×2`).
pub fn recent_messages(path: &Path, n: usize) -> Vec<(String, String)> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    recent_messages_from_str(&raw, n)
}

/// Same as [`recent_messages`] over already-loaded JSONL (e.g. a remote
/// mirror read through [`crate::hosts::local_path_for`]).
pub fn recent_messages_from_str(raw: &str, n: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |role: &str, text: String| {
        let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let one = one.trim().to_string();
        if one.is_empty() || one.starts_with('/') || is_framed_delivery(&one) {
            return;
        }
        let one = if one.chars().count() > 300 {
            let mut s: String = one.chars().take(299).collect();
            s.push('…');
            s
        } else {
            one
        };
        if role == "assistant" && one.starts_with("used tool:") {
            if let Some((lr, lt)) = out.last_mut() {
                let tag = format!("{} \u{d7}", one);
                if lr == "assistant" && (*lt == one || lt.starts_with(&tag)) {
                    let n = lt
                        .strip_prefix(&tag)
                        .and_then(|s| s.parse::<usize>().ok())
                        .map(|x| x + 1)
                        .unwrap_or(2);
                    *lt = format!("{} \u{d7}{}", one, n);
                    return;
                }
            }
        }
        out.push((role.to_string(), one));
        while out.len() > n {
            out.remove(0);
        }
    };
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let content = msg.get("content").unwrap_or(&serde_json::Value::Null);
        match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                if let Some(t) = join_text_blocks(content) {
                    push("user", t);
                }
            }
            "assistant" => {
                let text = join_text_blocks(content)
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| {
                        let names = tool_names(content);
                        (!names.is_empty()).then(|| format!("used tool:{}", names.join(", ")))
                    });
                if let Some(t) = text {
                    push("assistant", t);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_tags_host() {
        let raw = r#"{"type":"session","version":3,"id":"abc","timestamp":"2025-06-01T10:00:00.000Z","cwd":"/home/agent"}"#;
        let m = parse_content(raw, Path::new("/home/agent/x.jsonl"), 123, 45, "coop").unwrap();
        assert_eq!(m.host, "coop");
        assert_eq!(m.id, "abc");
        assert_eq!(m.mtime, 123);
        assert_eq!(m.size, 45);
        assert_eq!(
            m.key(),
            SessionKey {
                host: "coop".to_string(),
                path: PathBuf::from("/home/agent/x.jsonl"),
            }
        );
        assert!(!m.is_local());
    }

    #[test]
    fn cache_keys_namespace_hosts() {
        let p = Path::new("/same/path.jsonl");
        assert_ne!(cache_key("local", p), cache_key("coop", p));
    }

    #[test]
    fn in_scope_matches_exact_and_children() {
        let tmp = std::env::temp_dir().join(format!("psst-scope-{}", std::process::id()));
        let child = tmp.join("src");
        std::fs::create_dir_all(&child).unwrap();
        assert!(in_scope(&tmp.to_string_lossy(), &tmp));
        assert!(in_scope(&child.to_string_lossy(), &tmp));
        assert!(!in_scope("/tmp/other-project", &tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}