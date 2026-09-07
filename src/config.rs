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
//! # grouping on launch: "day" (default), "project", or "flat"
//! group = "day"
//! # show the right-hand detail pane on launch (default: false; toggle with Ctrl+D)
//! detail = false
//! ```

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    resume: Option<ResumeCfg>,
    #[serde(default)]
    detection: Option<DetectCfg>,
    #[serde(default)]
    ui: Option<UiCfg>,
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
}

pub struct Config {
    resume_command: String,
    active_secs: i64,
    group: String,
    detail: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            resume_command: "pi --session {file}".to_string(),
            active_secs: 120,
            group: "day".to_string(),
            detail: false,
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
        }
        cfg
    }

    pub fn resume_command(&self) -> &str {
        &self.resume_command
    }

    pub fn active_secs(&self) -> i64 {
        self.active_secs
    }

    /// Launch grouping: "day" (default), "project", or "flat".
    pub fn group_default(&self) -> &str {
        &self.group
    }

    /// Whether the right-hand detail pane is shown on launch (default: false).
    pub fn detail_default(&self) -> bool {
        self.detail
    }
}