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
//! ```

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    resume: Option<ResumeCfg>,
    #[serde(default)]
    detection: Option<DetectCfg>,
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

pub struct Config {
    resume_command: String,
    active_secs: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            resume_command: "pi --session {file}".to_string(),
            active_secs: 120,
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
        cfg
    }

    pub fn resume_command(&self) -> &str {
        &self.resume_command
    }

    pub fn active_secs(&self) -> i64 {
        self.active_secs
    }
}