//! Fake commands and a fake HOME: never touch a real agent, terminal, or host.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("psst-cli-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join(".config/psst")).unwrap();
        fs::create_dir_all(dir.join(".pi/agent/sessions/fixture")).unwrap();
        fs::write(dir.join(".config/psst/config.toml"), "[hosts.forbidden]\nssh = 'never-connect'\n").unwrap();
        let fixture = Self(dir);
        fixture.script("ssh", "echo unexpected-ssh >> \"$HOME/actions\"; exit 99");
        fixture.script("rsync", "echo unexpected-rsync >> \"$HOME/actions\"; exit 99");
        fixture.script("ps", "exit 0");
        fixture.script("tmux", "case \"$1\" in\nlist-clients) echo '100|$1';;\nnew-window) printf '%s\\n' \"$@\" >> \"$HOME/actions\"; echo '%42';;\nesac");
        fixture.script("tmux-focus", "printf 'focus %s %s\\n' \"$1\" \"$2\" >> \"$HOME/actions\"");
        let header = serde_json::json!({"type":"session", "version":3, "id":"fixture-uuid", "cwd":"/fixture/project with spaces", "timestamp":"2026-01-01T00:00:00Z"});
        fs::write(fixture.file(), format!("{}\n", header)).unwrap();
        fixture
    }
    fn script(&self, name: &str, body: &str) {
        let path = self.0.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn file(&self) -> PathBuf { self.0.join(".pi/agent/sessions/fixture/a 'quoted' session.jsonl") }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_psst")).env_clear()
            .env("HOME", &self.0).env("PATH", self.0.join("bin"))
            .env("PSST_CACHE", self.0.join("cache.json"))
            .args(args).output().unwrap()
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

#[test]
fn json_local_skips_configured_hosts_and_never_opens() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.file()).unwrap();
    for args in [&["--local", "--json"][..], &["--local", "--json", "--refresh"][..]] {
        let out = fixture.run(args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["host"], "local");
        assert_eq!(rows[0]["mark"], "exited");
        assert!(!fixture.0.join("actions").exists());
    }
    assert_eq!(fs::read(fixture.file()).unwrap(), before);
}

#[test]
fn explicit_open_uses_existing_planner_and_focus_helper() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.file()).unwrap();
    let out = fixture.run(&["--local", "--open", "fixture-uuid"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let actions = fs::read_to_string(fixture.0.join("actions")).unwrap();
    assert!(actions.contains("new-window\n-d\n-P\n-F\n#{pane_id}"));
    assert!(actions.contains("pi --session '"));
    assert!(actions.contains("'\\''quoted'\\''"), "shell-quoted session path");
    assert!(actions.contains("focus %42 Ghostty"));
    assert!(!actions.contains("unexpected"));
    assert_eq!(fs::read(fixture.file()).unwrap(), before);
}

#[test]
fn missing_session_or_client_never_creates_a_window() {
    let fixture = Fixture::new();
    assert!(!fixture.run(&["--local", "--open", "unknown"]).status.success());
    fixture.script("tmux", "exit 0");
    assert!(!fixture.run(&["--local", "--open", "fixture-uuid"]).status.success());
    assert!(!fixture.0.join("actions").exists());
}
