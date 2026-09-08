//! Bringing a remote session's terminal tab forward.
//!
//! A session on another box already has a home on this screen: the tab
//! holding an ssh to that host. Opening a second ssh beside it splits one
//! conversation across two windows and leaves the original tab sitting there
//! unread, so psst focuses the existing tab instead.
//!
//! The click is delegated to `tmux-focus` (from agent-notify), which already
//! solves the awkward half — macOS offers no way to focus a Ghostty surface,
//! so tabs have to be driven through the accessibility API — and is tested
//! there. psst only decides *which* tab, and says so out loud: if the tool or
//! the tab is missing, callers fall back to opening a fresh ssh rather than
//! silently doing nothing.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Terminal whose tabs are searched, overridable via `[ui] terminal`.
pub const DEFAULT_TERMINAL: &str = "Ghostty";

/// Tab-title fragments identifying `name`, most specific first.
///
/// Ordering is by how much a match proves. Once a remote tmux attaches it
/// renames the tab to `<host>:<session>:<window> …`, so `<host>:` pins one
/// tab even when a local session shares the remote's session name. Before
/// that, Ghostty titles a bare tab with its command line (`ssh coop`). The
/// bare hostname comes last because it is the loose one: it also matches a
/// tab that merely mentions the box, e.g. an editor open on `coop-notes.md`.
pub fn host_tags(name: &str, ssh_target: &str, configured: Option<&str>) -> Vec<String> {
    if let Some(tag) = configured.map(str::trim).filter(|t| !t.is_empty()) {
        return vec![tag.to_string()];
    }
    let mut tags = Vec::new();
    for tag in [
        format!("{}:", name),
        format!("{}:", ssh_target),
        format!("ssh {}", ssh_target),
        format!("ssh {}", name),
        name.to_string(),
        ssh_target.to_string(),
    ] {
        // `name` and `ssh_target` are equal in the common case.
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// First tag present in any tab title, or `None` when the host has no tab.
pub fn match_tab(titles: &[String], tags: &[String]) -> Option<String> {
    tags.iter()
        .find(|tag| titles.iter().any(|t| t.contains(tag.as_str())))
        .cloned()
}

/// Titles of every tab (or untabbed window) of `app`.
///
/// Doubles as a permission probe. Reading the tab bar needs the same
/// Accessibility grant as clicking it, so an empty result means "the focus
/// would not have landed" — which is exactly when the caller should fall
/// back instead of pretending it did.
pub fn tab_titles(app: &str) -> Vec<String> {
    let Ok(mut child) = Command::new("osascript")
        .args(["-", app])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        // No osascript: not macOS, so there is no tab bar to drive.
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(TAB_QUERY.as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_titles(&String::from_utf8_lossy(&out.stdout))
}

/// Pure half of [`tab_titles`]: one title per line, blanks dropped.
pub(crate) fn parse_titles(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| l.trim_end_matches('\r').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The tag to focus `app` with, if this host is reachable by a click:
/// `tmux-focus` installed *and* a tab bearing one of `tags`.
pub fn focusable_tab(app: &str, tags: &[String]) -> Option<String> {
    if tags.is_empty() || tmux_focus_bin().is_none() {
        return None;
    }
    match_tab(&tab_titles(app), tags)
}

/// `tmux-focus` on PATH, if agent-notify is installed.
pub fn tmux_focus_bin() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("tmux-focus"))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Follows symlinks on purpose: the usual install is a link into a repo.
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Click the tab matching `tag`, first selecting `target` in the remote tmux
/// reached through it. Both hops belong to `tmux-focus`: passing the remote
/// also earns its own title fallbacks, so a tab that has not yet been renamed
/// by the remote tmux still matches.
pub fn focus_tab(app: &str, tag: &str, remote: Option<(&str, &str)>) -> Result<()> {
    let bin = tmux_focus_bin().context("tmux-focus is not on PATH")?;
    let mut cmd = Command::new(bin);
    cmd.arg(format!("tab:{}", tag)).arg(app);
    if let Some((host, target)) = remote {
        cmd.arg(format!("{}:{}", host, target));
    }
    let st = cmd.status().context("running tmux-focus")?;
    if !st.success() {
        bail!("tmux-focus exited with {}", st);
    }
    Ok(())
}

/// Read-only inventory of the tab bar. Windows and tabs are addressed by
/// index because System Events keys element references by name, and those go
/// stale as titles change underneath the walk. macOS models tabs as radio
/// buttons in the window's tab group; a single-tab window has none.
const TAB_QUERY: &str = r#"on run argv
  set appName to item 1 of argv
  tell application "System Events"
    if not (exists process appName) then return ""
    tell process appName
      set out to ""
      repeat with w from 1 to (count of windows)
        if exists tab group 1 of window w then
          repeat with t from 1 to (count of radio buttons of tab group 1 of window w)
            set tt to title of radio button t of tab group 1 of window w
            if tt is not missing value then set out to out & tt & linefeed
          end repeat
        else
          set tt to title of window w
          if tt is not missing value then set out to out & tt & linefeed
        end if
      end repeat
      return out
    end tell
  end tell
end run"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tags_run_specific_to_loose() {
        assert_eq!(
            host_tags("gfe", "gfew", None),
            tags(&["gfe:", "gfew:", "ssh gfew", "ssh gfe", "gfe", "gfew"])
        );
        // Name and ssh target usually match; no duplicate tags from that.
        assert_eq!(
            host_tags("coop", "coop", None),
            tags(&["coop:", "ssh coop", "coop"])
        );
        // A configured tab wins outright: the user has seen their tab bar.
        assert_eq!(host_tags("coop", "coop", Some("work ")), tags(&["work"]));
        assert_eq!(
            host_tags("coop", "coop", Some("  ")),
            host_tags("coop", "coop", None)
        );
    }

    #[test]
    fn match_prefers_the_most_specific_tag() {
        // Eric's real tab bar: a local tmux, plus one ssh tab per host, each
        // renamed by the remote tmux to "<host>:<session>:<window> …".
        let titles = tags(&[
            "code:8 e14.local",
            "gfe:code:1 π - fix-fraud-ml - identity-fraud-engine",
            "coop:code:1 π - getting-started - agent",
        ]);
        assert_eq!(
            match_tab(&titles, &host_tags("coop", "coop", None)),
            Some("coop:".to_string())
        );
        // A loose tag must not beat a precise one that also matches.
        let noisy = tags(&["coop:code:1 π", "notes on coop"]);
        assert_eq!(
            match_tab(&noisy, &host_tags("coop", "coop", None)),
            Some("coop:".to_string())
        );
        // Freshly opened ssh tab, not yet renamed: Ghostty titles it by cmd.
        assert_eq!(
            match_tab(&tags(&["ssh coop"]), &host_tags("coop", "coop", None)),
            Some("ssh coop".to_string())
        );
        // Host has no tab open — the caller must not click something else.
        assert_eq!(match_tab(&titles, &host_tags("nas", "nas", None)), None);
        assert_eq!(match_tab(&[], &host_tags("coop", "coop", None)), None);
    }

    #[test]
    fn titles_parse_one_per_line() {
        assert_eq!(
            parse_titles("code:8 e14.local\n\ncoop:code:1 π \n"),
            tags(&["code:8 e14.local", "coop:code:1 π"])
        );
        assert!(parse_titles("").is_empty());
    }

    #[test]
    fn no_tags_means_no_click() {
        // Guards the fallback path: an unconfigured host must never reach
        // the tab-title search, where a stray tag could match anything.
        assert_eq!(focusable_tab(DEFAULT_TERMINAL, &[]), None);
    }
}
