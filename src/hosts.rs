//! Remote hosts over ssh.
//!
//! Each `[hosts.<name>]` entry mirrors its pi sessions to
//! `~/.cache/psst/hosts/<name>/<absolute-remote-path>` (so `coop` +
//! `/home/agent/.pi/agent/sessions/...` becomes
//! `~/.cache/psst/hosts/coop/home/agent/.pi/agent/sessions/...`).
//! [`sync_host`] lists the remote files, fetches only what changed
//! (checked against the shared metadata cache), parses via
//! [`crate::scan::parse_content`], and writes mirrors for the detail pane.
//! Unreachable hosts return an error string instead of failing the whole load,
//! so local sessions always render.

use crate::config::HostConfig;
use crate::scan::{self, SessionMeta};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Base dir for all host mirrors. Sibling of `cache.json`, so `PSST_CACHE`
/// test overrides stay isolated.
pub fn hosts_dir() -> PathBuf {
    let cache = scan::cache_path();
    match cache.parent() {
        Some(d) => d.join("hosts"),
        None => PathBuf::from("hosts"),
    }
}

/// Local mirror for `remote_path` on `host`. Preserves the full absolute
/// path so no remote-root bookkeeping is needed for reads.
pub fn mirror_path(host: &str, remote_path: &Path) -> PathBuf {
    let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
    hosts_dir().join(host).join(rel)
}

/// File to read for a session's detail pane: the real path locally,
/// the mirror for remotes.
pub fn local_path_for(meta: &SessionMeta) -> PathBuf {
    if meta.is_local() {
        meta.path.clone()
    } else {
        mirror_path(&meta.host, &meta.path)
    }
}

/// One remote `find` row: absolute path + mtime + size.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: u64,
}

/// Parse `find -printf '%p\t%T@\t%s\n'` output. Paths never contain tabs;
/// mtimes are floats (`1787357221.12`) — the integer part is enough for the cache.
pub fn parse_find_output(out: &str) -> Vec<RemoteFile> {
    let mut files = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.rsplit('\t');
        let size = parts.next().unwrap_or("");
        let mtime = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        // rsplit on 2 tabs yields exactly 3 parts; extra tabs in path would
        // break this, but session paths never contain tabs.
        if path.is_empty() {
            continue;
        }
        let mtime = mtime
            .split('.')
            .next()
            .unwrap_or("")
            .parse::<i64>()
            .unwrap_or(0);
        let size = size.trim().parse::<u64>().unwrap_or(0);
        files.push(RemoteFile {
            path: PathBuf::from(path),
            mtime,
            size,
        });
    }
    files
}

/// Directory for ssh multiplex sockets. Sibling of `cache.json`, so
/// `PSST_CACHE` test overrides stay isolated.
pub(crate) fn ssh_socket_dir() -> PathBuf {
    let cache = scan::cache_path();
    match cache.parent() {
        Some(d) => d.join("ssh"),
        None => PathBuf::from("ssh"),
    }
}

/// Build an ssh invocation with connection multiplexing: the first call to a
/// host pays the handshake, follow-ups reuse the master in milliseconds.
/// Masters linger 10min (`ControlPersist`) so separate psst runs share them.
/// With `ControlMaster=auto`, a dead master is replaced transparently.
pub fn ssh_command(target: &str, remote_cmd: &str) -> Command {
    let dir = ssh_socket_dir();
    let _ = std::fs::create_dir_all(&dir);
    let socket = dir.join("%r@%h:%p");
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "ConnectTimeout=5",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "ControlMaster=auto",
        "-o",
        &format!("ControlPath={}", socket.to_string_lossy()),
        "-o",
        "ControlPersist=600",
        target,
        remote_cmd,
    ]);
    cmd
}

fn ssh_output(target: &str, remote_cmd: &str) -> anyhow::Result<String> {
    let out = ssh_command(target, remote_cmd).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!(
            "ssh {} failed{}",
            target,
            if err.is_empty() {
                String::new()
            } else {
                format!(": {}", err.lines().next().unwrap_or(&err))
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Resolve the remote sessions root: explicit `sessions_dir` wins, else
/// `$PI_CODING_AGENT_DIR/sessions` when set, else `~/.pi/agent/sessions`.
fn resolve_remote_root(host: &HostConfig) -> anyhow::Result<String> {
    if let Some(dir) = &host.sessions_dir {
        if dir.starts_with('/') {
            return Ok(dir.clone());
        }
        // Let the remote shell expand ~/$HOME so `~/x` and `x` both work.
        let expr = if let Some(rest) = dir.strip_prefix("~/") {
            format!("$HOME/{}", rest)
        } else if dir.starts_with("$") || dir.starts_with('~') {
            dir.clone()
        } else {
            format!("$HOME/{}", dir.trim_start_matches("./"))
        };
        let out = ssh_output(&host.ssh_target, &format!("echo {}", expr))?;
        let root = out.trim().to_string();
        if root.is_empty() {
            anyhow::bail!("empty sessions dir for host {}", host.name);
        }
        return Ok(root);
    }
    let out = ssh_output(
        &host.ssh_target,
        r#"if [ -n "$PI_CODING_AGENT_DIR" ]; then echo "$PI_CODING_AGENT_DIR/sessions"; else echo "$HOME/.pi/agent/sessions"; fi"#,
    )?;
    let root = out.trim().to_string();
    if root.is_empty() {
        anyhow::bail!("empty sessions dir for host {}", host.name);
    }
    Ok(root)
}

/// List remote session files with mtimes/sizes. GNU find (Linux) first,
/// BSD `stat` (macOS remotes like the GFE box) as fallback — both emit
/// tab-separated `path\tmtime\tsize` rows that [`parse_find_output`] reads.
fn list_remote(host: &HostConfig, root: &str) -> anyhow::Result<Vec<RemoteFile>> {
    // Session filenames never contain tabs/newlines, so tab-separated rows
    // parse reliably on either flavor.
    let gnu = format!(
        "find {} -name '*.jsonl' -printf '%p\\t%T@\\t%s\\n' 2>/dev/null",
        sh_quote(root)
    );
    // One roundtrip, not two: GNU `-printf` where supported, else BSD `stat`
    // (macOS remotes fail the first branch and run the second). Every ssh
    // call costs ~200ms even multiplexed, so branches compose with `||`.
    // NB: `\t` in the BSD format is a literal TAB (Rust escape): BSD stat
    // prints format backslashes through untouched, so an escaped `\\t`
    // would yield no separators for `parse_find_output` to split on.
    let bsd = format!(
        "find {} -name '*.jsonl' -exec stat -f '%N\t%m\t%z' {{}} + 2>/dev/null",
        sh_quote(root)
    );
    let out = ssh_output(&host.ssh_target, &format!("{} || {}", gnu, bsd))?;
    Ok(parse_find_output(&out))
}

const FETCH_DELIM: &str = "===PSST-FILE:";

/// Fetch several remote files in one ssh call. Returns path -> content for
/// the files that could be read; missing files are skipped.
fn fetch_remote_contents(
    target: &str,
    paths: &[PathBuf],
) -> anyhow::Result<HashMap<PathBuf, String>> {
    let mut out_map = HashMap::new();
    if paths.is_empty() {
        return Ok(out_map);
    }
    // One `printf` delimiter + `cat` per file; every content line starts
    // with `{` so a column-0 delimiter can't collide with session JSONL.
    let mut script = String::new();
    for p in paths {
        let q = sh_quote(&p.to_string_lossy());
        script.push_str(&format!(
            "printf '\\n{delim}{path}\\n' {q}; cat {q} 2>/dev/null; ",
            delim = FETCH_DELIM,
            path = p.to_string_lossy(),
            q = q,
        ));
    }
    let out = ssh_output(target, &script)?;
    out_map.extend(parse_fetch_output(&out));
    // `printf '\n...'` emits a leading blank line before the first delimiter;
    // files with no content still get an entry (empty string).
    Ok(out_map)
}

/// Parse batched `cat` output delimited by `===PSST-FILE:<path>` lines.
/// Pure for tests; every content line starts with `{` so the delimiter can't
/// collide with session JSONL.
pub(crate) fn parse_fetch_output(out: &str) -> HashMap<PathBuf, String> {
    let mut out_map = HashMap::new();
    let mut cur: Option<PathBuf> = None;
    let mut buf = String::new();
    let mut flush = |cur: &mut Option<PathBuf>, buf: &mut String| {
        if let Some(p) = cur.take() {
            // strip the leading newline the printf adds
            let content = buf.strip_prefix('\n').unwrap_or(buf).to_string();
            out_map.insert(p, content);
        }
        buf.clear();
    };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix(FETCH_DELIM) {
            flush(&mut cur, &mut buf);
            cur = Some(PathBuf::from(rest.trim()));
        } else if cur.is_some() {
            buf.push('\n');
            buf.push_str(line);
        }
    }
    flush(&mut cur, &mut buf);
    out_map
}

/// Result of syncing one host: sessions plus a displayable error when the
/// host was unreachable (sessions then holds whatever the cache had, if anything).
pub struct HostSync {
    pub name: String,
    pub sessions: Vec<SessionMeta>,
    pub error: Option<String>,
}

impl HostSync {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Mirror one host's sessions and return parsed metadata.
pub fn sync_host(host: &HostConfig, force: bool) -> HostSync {
    match sync_host_inner(host, force) {
        Ok(s) => HostSync {
            name: host.name.clone(),
            sessions: s,
            error: None,
        },
        Err(e) => HostSync {
            name: host.name.clone(),
            sessions: cached_sessions_for_host(&host.name),
            error: Some(format!("{}: {}", host.name, e)),
        },
    }
}

/// Sessions for `hosts` straight from the cache, with no ssh at all.
///
/// Backs the first TUI frame: the mirrors and their metadata are already on
/// disk, so remote rows can be drawn immediately and corrected by the
/// background sync, instead of popping in seconds later.
pub fn cached_sessions(hosts: &[HostConfig]) -> Vec<SessionMeta> {
    if hosts.is_empty() {
        return Vec::new();
    }
    // One cache read for every host: it is a single multi-MB file.
    let cache = scan::load_cache();
    let names: HashSet<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
    let mut out: HashMap<(String, PathBuf), SessionMeta> = HashMap::new();
    for (key, entry) in &cache.0 {
        let host = match key.split_once('\0') {
            Some((h, _)) => h,
            // Legacy bare-path keys are local by definition.
            None => continue,
        };
        if !names.contains(host) {
            continue;
        }
        let mut meta = entry.meta.clone();
        if meta.host.is_empty() {
            meta.host = host.to_string();
        }
        out.insert((meta.host.clone(), meta.path.clone()), meta);
    }
    out.into_values().collect()
}

fn cached_sessions_for_host(host_name: &str) -> Vec<SessionMeta> {
    // Offline view: whatever the shared cache already holds for this host.
    // Mirrors on disk still back the detail pane while the host is down.
    let cache = scan::load_cache();
    let prefix = format!("{}\0", host_name);
    cache
        .0
        .values()
        .filter(|e| e.meta.host == host_name)
        .map(|e| e.meta.clone())
        .chain(cache.0.iter().filter_map(|(k, e)| {
            if k.starts_with(&prefix) {
                let mut m = e.meta.clone();
                if m.host.is_empty() {
                    m.host = host_name.to_string();
                }
                Some(m)
            } else {
                None
            }
        }))
        .collect::<Vec<_>>()
        // dedupe by path (belt and suspenders across the two filters)
        .into_iter()
        .fold(
            HashMap::new(),
            |mut acc: HashMap<PathBuf, SessionMeta>, m| {
                acc.insert(m.path.clone(), m);
                acc
            },
        )
        .into_values()
        .collect()
}

fn sync_host_inner(host: &HostConfig, force: bool) -> anyhow::Result<Vec<SessionMeta>> {
    let root = resolve_remote_root(host)?;
    let remote_files = list_remote(host, &root)?;
    let mut cache = scan::load_cache();
    let mut changed = false;
    let mut out = Vec::new();

    let live: HashSet<String> = remote_files
        .iter()
        .map(|f| f.path.to_string_lossy().to_string())
        .collect();

    // Decide what to fetch: cache misses, changed mtime/size, or missing mirrors.
    let mut need: Vec<&RemoteFile> = Vec::new();
    for f in &remote_files {
        if force {
            need.push(f);
            continue;
        }
        match scan::cache_lookup(&cache, &host.name, &f.path) {
            Some(e) if e.mtime == f.mtime && e.size == f.size => {
                if mirror_path(&host.name, &f.path).exists() {
                    let mut meta = e.meta.clone();
                    if meta.host.is_empty() {
                        meta.host = host.name.clone();
                    }
                    out.push(meta);
                } else {
                    need.push(f);
                }
            }
            _ => need.push(f),
        }
    }

    // Fetch in chunks to stay under ARG_MAX (paths are short; 100 is plenty).
    let mut fetched: HashMap<PathBuf, String> = HashMap::new();
    for chunk in need.chunks(100) {
        let paths: Vec<PathBuf> = chunk.iter().map(|f| f.path.clone()).collect();
        let got = fetch_remote_contents(&host.ssh_target, &paths)?;
        fetched.extend(got);
    }

    for f in &remote_files {
        if out.iter().any(|s: &SessionMeta| s.path == f.path) {
            continue;
        }
        let Some(raw) = fetched.get(&f.path) else {
            // Vanished between list and fetch, or unreadable: fall back to
            // the cached row if the bytes match, else skip.
            if let Some(e) = scan::cache_lookup(&cache, &host.name, &f.path) {
                if e.mtime == f.mtime && e.size == f.size {
                    out.push(e.meta.clone());
                }
            }
            continue;
        };
        // Persist the mirror first so the detail pane works offline.
        let mirror = mirror_path(&host.name, &f.path);
        if let Some(dir) = mirror.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&mirror, raw);
        match scan::parse_content(raw, &f.path, f.mtime, f.size, &host.name) {
            Ok(meta) => {
                cache.0.insert(
                    scan::cache_key(&host.name, &f.path),
                    scan::CacheEntry {
                        mtime: f.mtime,
                        size: f.size,
                        meta: meta.clone(),
                        // Remote files are KBs; full reparse on change.
                        lines: 0,
                        child_counts: HashMap::new(),
                    },
                );
                changed = true;
                out.push(meta);
            }
            Err(_) => continue,
        }
    }

    // Drop cache rows + mirrors for sessions deleted on the remote.
    let prefix = format!("{}\0", host.name);
    let stale: Vec<String> = cache
        .0
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .filter(|k| {
            k.strip_prefix(&prefix)
                .map(|p| !live.contains(p))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for k in &stale {
        cache.0.remove(k);
        changed = true;
        if let Some(rel) = k.strip_prefix(&prefix) {
            let mirror = mirror_path(&host.name, Path::new(rel));
            let _ = std::fs::remove_file(&mirror);
        }
    }

    if changed {
        scan::save_cache(&cache);
    }
    Ok(out)
}

/// Sync every configured host. Callers merge `sessions`, show `error`s, and
/// skip ssh detection for hosts that failed to sync (one timeout, not three).
pub fn sync_all(hosts: &[HostConfig], force: bool) -> Vec<HostSync> {
    hosts.iter().map(|h| sync_host(h, force)).collect()
}

/// Open a window in the *host's own* tmux, returning its `%pane_id`.
///
/// The point is to land where the user is already looking: the terminal tab
/// for this host shows one remote session, so the new window goes to the
/// session its attached client is on rather than whichever one tmux would
/// pick by default. Falling back to the most recent session covers a host
/// whose tmux is running but currently unattached.
///
/// A pane id rather than `session:window` because hosts commonly set
/// `renumber-windows`: one window closing between here and the focus hop
/// shifts every index after it, so the target could name a different window
/// by the time it is used. A pane id belongs to that pane for life.
pub fn new_remote_window(
    ssh_target: &str,
    cwd: &str,
    name: &str,
    command: &str,
) -> anyhow::Result<String> {
    let out = ssh_command(ssh_target, &new_window_script(cwd, name, command)).output()?;
    if !out.status.success() {
        anyhow::bail!("no tmux on {}", ssh_target);
    }
    let target = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if target.is_empty() {
        anyhow::bail!("tmux on {} reported no window", ssh_target);
    }
    Ok(target)
}

/// Script for [`new_remote_window`]; separated so the quoting is testable.
/// Exits nonzero when the host has no tmux server, which is the caller's
/// signal to open a local ssh window the old way instead.
pub(crate) fn new_window_script(cwd: &str, name: &str, command: &str) -> String {
    format!(
        "sess=$(tmux list-clients -F '#{{client_activity}} #{{client_session}}' 2>/dev/null \
         | sort -rn | awk 'NR==1{{print $2}}'); \
         [ -n \"$sess\" ] || sess=$(tmux list-sessions -F '#{{session_activity}} #{{session_name}}' \
         2>/dev/null | sort -rn | awk 'NR==1{{print $2}}'); \
         [ -n \"$sess\" ] || exit 1; \
         tmux new-window -t \"$sess:\" -c {cwd} -n {name} \
         -P -F '#{{pane_id}}' {command}",
        cwd = sh_quote(cwd),
        name = sh_quote(name),
        command = sh_quote(command),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_output_parses() {
        let out = "/home/agent/.pi/agent/sessions/--x--/a.jsonl\t1787357221.123456789\t4111\n\
                   /home/agent/.pi/agent/sessions/--x--/b.jsonl\t1787357222.5\t100\n\
                   \n";
        let got = parse_find_output(out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].mtime, 1787357221);
        assert_eq!(got[0].size, 4111);
        assert_eq!(
            got[1].path,
            PathBuf::from("/home/agent/.pi/agent/sessions/--x--/b.jsonl")
        );
    }

    #[test]
    fn mirror_preserves_absolute_path() {
        let m = mirror_path(
            "coop",
            Path::new("/home/agent/.pi/agent/sessions/--x--/a.jsonl"),
        );
        let s = m.to_string_lossy();
        assert!(
            s.contains("hosts/coop/home/agent/.pi/agent/sessions/--x--/a.jsonl"),
            "got {}",
            s
        );
    }

    #[test]
    fn ssh_uses_connection_multiplexing() {
        // Drop the Bourne-sensitive parts; assert the multiplex flags that
        // make hosts 2-12 cheap after the first handshake.
        let cmd = format!("{:?}", ssh_command("coop", "true"));
        assert!(cmd.contains("ControlMaster=auto"), "got {}", cmd);
        assert!(cmd.contains("ControlPersist=600"), "got {}", cmd);
        assert!(cmd.contains("ControlPath="), "got {}", cmd);
        assert!(cmd.contains("BatchMode=yes"), "got {}", cmd);
    }

    #[test]
    fn new_window_script_lands_in_the_attached_session() {
        let s = new_window_script("/home/agent/proj", "proj", "pi --continue");
        // The tab shows whatever session its client is attached to, so that
        // is the session the window has to join — not tmux's default pick.
        assert!(s.contains("tmux list-clients"), "got {}", s);
        assert!(s.contains("tmux new-window -t \"$sess:\""), "got {}", s);
        // Unattached-but-running tmux still works; no server at all exits 1
        // so the caller can fall back to a local ssh window.
        assert!(s.contains("tmux list-sessions"), "got {}", s);
        assert!(s.contains("|| exit 1"), "got {}", s);
        // Prints the new pane id for the focus hop that follows: window
        // indices shift under `renumber-windows`, pane ids do not.
        assert!(s.contains("-P -F '#{pane_id}'"), "got {}", s);
        // Every interpolated value is quoted: a project dir with a space
        // must not turn into two tmux arguments.
        let spaced = new_window_script("/home/agent/my proj", "my proj", "pi -c");
        assert!(
            spaced.contains("-c '/home/agent/my proj'"),
            "got {}",
            spaced
        );
        assert!(spaced.contains("-n 'my proj'"), "got {}", spaced);
        assert!(spaced.ends_with("'pi -c'"), "got {}", spaced);
    }

    #[test]
    fn sh_quote_escapes() {
        assert_eq!(sh_quote("/a/b"), "'/a/b'");
        assert_eq!(sh_quote("/a'b"), "'/a'\\''b'");
    }

    #[test]
    fn fetch_output_splits_on_delimiters() {
        let out = "\n===PSST-FILE:/a/x.jsonl\n{\"type\":\"session\"}\n{\"type\":\"message\"}\n===PSST-FILE:/a/empty.jsonl\n===PSST-FILE:/a/y.jsonl\n{}\n";
        let got = parse_fetch_output(out);
        assert_eq!(got.len(), 3);
        assert!(got[&PathBuf::from("/a/x.jsonl")].contains("message"));
        assert_eq!(got[&PathBuf::from("/a/empty.jsonl")], "");
        assert_eq!(got[&PathBuf::from("/a/y.jsonl")].trim(), "{}");
    }

    #[test]
    fn find_output_skips_blanks() {
        assert!(parse_find_output("").is_empty());
        assert!(parse_find_output("\n  \n").is_empty());
    }

    #[test]
    fn find_output_parses_bsd_integer_mtimes() {
        // BSD `stat -f '%N<mTAB>%m<mTAB>%z'` rows (macOS remotes): integer
        // mtime, no fractional part.
        let out = "/Users/x/.pi/agent/sessions/--a--/f.jsonl\t1787413259\t1159\n";
        let got = parse_find_output(out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].mtime, 1787413259);
        assert_eq!(got[0].size, 1159);
    }
}
