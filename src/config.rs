//! Configuration: `~/.config/psst/config.toml`
//!
//! ```toml
//! [resume]
//! # {file} = session jsonl path, {id} = session uuid, {cwd} = project dir
//! command = 'pi --session {file}'
//!
//! [detection]
//! # a session counts as "running" if a pi process shares its cwd and the
//! # file was written within this many seconds
//! active_secs = 120
//!
//! [ui]
//! # grouping on launch: "day" (default), "project", "flat", or "host"
//! group = "day"
//! # show the right-hand detail pane on launch (default: false; toggle with Ctrl+D)
//! detail = false
//! # terminal whose tabs are focused when opening a remote session
//! terminal = "Ghostty"
//! # mouse: click to select, double-click to open, wheel to scroll
//! mouse = true
//!
//! [hosts.coop]
//! # ssh target for the remote host (as in `ssh <target>`)
//! ssh = "coop"
//! # enabled = true (default when the table is present; set false to hide)
//! # sessions_dir = ".pi/agent/sessions" (default; relative to remote $HOME
//! #   unless absolute or starting with ~/; $PI_CODING_AGENT_DIR wins when set)
//! # tab = "coop:"  (tab-title fragment to focus; guessed from the name
//! #   and ssh target when unset)
//! ```

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    resume: Option<ResumeCfg>,
    #[serde(default)]
    detection: Option<DetectCfg>,
    #[serde(default)]
    ui: Option<UiCfg>,
    #[serde(default)]
    hosts: HashMap<String, HostFileCfg>,
}

#[derive(Deserialize)]
struct ResumeCfg {
    #[serde(default)]
    command: Option<String>,
}

#[derive(Deserialize)]
struct DetectCfg {
    #[serde(default)]
    active_secs: Option<u64>,
}

#[derive(Deserialize)]
struct UiCfg {
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    detail: Option<bool>,
    #[serde(default)]
    terminal: Option<String>,
    mouse: Option<bool>,
}

#[derive(Deserialize, Default)]
struct HostFileCfg {
    #[serde(default)]
    ssh: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    sessions_dir: Option<String>,
    #[serde(default)]
    tab: Option<String>,
}

/// One remote host psst mirrors over ssh. `name` is the display key
/// (`local` is reserved for this machine); `ssh_target` is the `ssh <target>`
/// destination; `sessions_dir` optionally overrides the remote sessions dir;
/// `tab` optionally pins which terminal tab holds this host's ssh.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub ssh_target: String,
    pub sessions_dir: Option<String>,
    pub tab: Option<String>,
}

#[derive(Clone)]
pub struct Config {
    resume_command: String,
    active_secs: i64,
    group: String,
    detail: bool,
    terminal: String,
    mouse: bool,
    hosts: Vec<HostConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            resume_command: "pi --session {file}".to_string(),
            active_secs: 120,
            group: "day".to_string(),
            detail: false,
            terminal: crate::focus::DEFAULT_TERMINAL.to_string(),
            mouse: true,
            hosts: Vec::new(),
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let mut cfg = Config::default();
        let Some(home) = std::env::var_os("HOME") else {
            return cfg;
        };
        let path = PathBuf::from(home).join(".config/psst/config.toml");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return cfg;
        };
        let Ok(fc) = toml::from_str::<FileConfig>(&raw) else {
            return cfg;
        };
        if let Some(resume) = fc.resume {
            if let Some(command) = resume.command {
                cfg.resume_command = command;
            }
        }
        if let Some(detection) = fc.detection {
            if let Some(secs) = detection.active_secs {
                cfg.active_secs = secs as i64;
            }
        }
        if let Some(ui) = fc.ui {
            if let Some(g) = ui.group {
                cfg.group = g;
            }
            if let Some(d) = ui.detail {
                cfg.detail = d;
            }
            if let Some(t) = ui
                .terminal
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
            {
                cfg.terminal = t;
            }
            if let Some(m) = ui.mouse {
                cfg.mouse = m;
            }
        }
        let mut names: Vec<String> = fc.hosts.keys().cloned().collect();
        names.sort();
        for name in names {
            if name == "local" {
                continue;
            }
            if let Some(h) = fc.hosts.get(&name) {
                if h.enabled == Some(false) {
                    continue;
                }
                let ssh_target = h.ssh.clone().unwrap_or_else(|| name.clone());
                if ssh_target.is_empty() {
                    continue;
                }
                cfg.hosts.push(HostConfig {
                    name,
                    ssh_target,
                    sessions_dir: h.sessions_dir.clone(),
                    tab: h.tab.clone(),
                });
            }
        }
        cfg
    }

    pub fn resume_command(&self) -> &str {
        &self.resume_command
    }

    pub fn active_secs(&self) -> i64 {
        self.active_secs
    }

    /// Launch grouping: "day" (default), "project", "flat", or "host".
    pub fn group_default(&self) -> &str {
        &self.group
    }

    /// Whether the right-hand detail pane is shown on launch (default: false).
    pub fn detail_default(&self) -> bool {
        self.detail
    }

    /// Terminal whose tabs are focused when opening a remote session.
    pub fn terminal(&self) -> &str {
        &self.terminal
    }

    /// Whether to capture mouse events. Capture takes over the terminal's
    /// own drag-to-select, so it is worth being able to turn off.
    pub fn mouse(&self) -> bool {
        self.mouse
    }

    /// Remote hosts from `[hosts.<name>]` tables, sorted by name.
    pub fn hosts(&self) -> &[HostConfig] {
        &self.hosts
    }

    /// Test helper: build a config with the given remote hosts.
    #[cfg(test)]
    pub fn with_hosts_for_test(hosts: Vec<HostConfig>) -> Config {
        let mut cfg = Config::default();
        cfg.hosts = hosts;
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_tables() {
        let raw = r#"
[hosts.coop]
ssh = "coop"

[hosts.nas]
ssh = "nas"
enabled = false

[hosts.gfe]
sessions_dir = "/data/sessions"
tab = "gfe box"
"#;
        let fc: FileConfig = toml::from_str(raw).unwrap();
        assert_eq!(fc.hosts.len(), 3);
        assert_eq!(fc.hosts["coop"].ssh.as_deref(), Some("coop"));
        assert_eq!(fc.hosts["nas"].enabled, Some(false));
        assert_eq!(
            fc.hosts["gfe"].sessions_dir.as_deref(),
            Some("/data/sessions")
        );
        assert_eq!(fc.hosts["gfe"].tab.as_deref(), Some("gfe box"));
    }

    #[test]
    fn terminal_defaults_to_ghostty() {
        assert_eq!(Config::default().terminal(), "Ghostty");
        assert!(Config::default().mouse(), "mouse is on unless disabled");
        let ui: UiCfg = toml::from_str(r#"terminal = "WezTerm""#).unwrap();
        assert_eq!(ui.terminal.as_deref(), Some("WezTerm"));
    }
}
