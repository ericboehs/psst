use std::process::Command;

#[test]
fn version_names_crate_version_and_commit() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_psst"))
            .arg(flag)
            .output()
            .expect("run psst");
        assert!(out.status.success(), "{flag} failed");
        let line = String::from_utf8(out.stdout).unwrap();
        let line = line.trim();
        let prefix = format!("psst {} (", env!("CARGO_PKG_VERSION"));
        assert!(line.starts_with(&prefix), "{flag}: {line}");
        assert!(line.ends_with(')'), "{flag}: {line}");
        let sha = &line[prefix.len()..line.len() - 1];
        let sha = sha.strip_suffix("-dirty").unwrap_or(sha);
        assert!(
            sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())),
            "{flag}: unexpected commit {sha:?}"
        );
    }
}
