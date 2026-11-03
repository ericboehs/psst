//! Embeds the git commit in the binary as `PSST_GIT_SHA`, so `psst --version`
//! names the exact build that's installed. A build with uncommitted changes to
//! tracked files gets a `-dirty` suffix; outside a git checkout it's "unknown".
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let version = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=PSST_GIT_SHA={version}");

    // Rebuild when the commit or the working tree's state changes. --git-path
    // resolves these inside worktrees too, where .git is a file.
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &r]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
