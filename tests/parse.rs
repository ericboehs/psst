use psst::scan::parse_file;
use std::io::Write;

/// Synthetic session file built from the documented v3 format
/// (pi docs session-format.md). Never touches real session data.
fn fixture(name: &str, lines: &[&str]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("psst-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    for l in lines {
        writeln!(f, "{}", l).unwrap();
    }
    path
}

#[test]
fn parses_header_name_model_cost_and_branches() {
    let path = fixture(
        "basic.jsonl",
        &[
            r#"{"type":"session","version":3,"id":"11111111-2222-3333-4444-555555555555","timestamp":"2025-06-01T10:00:00.000Z","cwd":"/tmp/proj-a"}"#,
            r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2025-06-01T10:00:01.000Z","message":{"role":"user","content":"Hello, can you help me refactor the auth module?"}}"#,
            r#"{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2025-06-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Sure!"}],"provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"totalTokens":150,"cost":{"input":0.001,"output":0.002,"cacheRead":0,"cacheWrite":0,"total":0.003}},"stopReason":"stop","timestamp":0}}"#,
            r#"{"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2025-06-01T10:00:03.000Z","message":{"role":"user","content":"try approach A"}}"#,
            r#"{"type":"message","id":"d4e5f6a7","parentId":"c3d4e5f6","timestamp":"2025-06-01T10:00:04.000Z","message":{"role":"user","content":"actually approach B"}}"#,
            r#"{"type":"model_change","id":"e5f6g7h8","parentId":"c3d4e5f6","timestamp":"2025-06-01T10:05:00.000Z","provider":"openai","modelId":"gpt-4o"}"#,
            r#"{"type":"session_info","id":"f6g7h8i9","parentId":"e5f6g7h8","timestamp":"2025-06-01T10:06:00.000Z","name":"Refactor auth module"}"#,
        ],
    );

    let meta = parse_file(&path).unwrap();
    assert_eq!(meta.id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(meta.cwd, "/tmp/proj-a");
    assert_eq!(meta.name.as_deref(), Some("Refactor auth module"));
    assert_eq!(
        meta.first_prompt.as_deref(),
        Some("Hello, can you help me refactor the auth module?")
    );
    assert_eq!(meta.last_prompt.as_deref(), Some("actually approach B"));
    // model_change is the latest model
    assert_eq!(meta.model.as_deref(), Some("gpt-4o"));
    assert!((meta.cost_total - 0.003).abs() < 1e-9);
    assert_eq!(meta.tokens_in, 100);
    assert_eq!(meta.tokens_out, 50);
    assert_eq!(meta.messages, 4);
    // c3d4e5f6 has two children (d4e5f6a7 and e5f6g7h8) -> one branch point
    assert_eq!(meta.branches, 1);
    assert_eq!(meta.parent_session, None);
    assert_eq!(meta.created, Some(1_748_772_000));
    assert_eq!(meta.last_activity, Some(1_748_772_360));
    std::fs::remove_file(&path).ok();
}

#[test]
fn parses_fork_parent_and_tool_result_nested_usage() {
    let path = fixture(
        "forked.jsonl",
        &[
            r#"{"type":"session","version":3,"id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","timestamp":"2025-06-02T09:00:00.000Z","cwd":"/tmp/proj-b","parentSession":"/tmp/cache/other.jsonl"}"#,
            r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2025-06-02T09:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"run the tests"}]}}"#,
            r#"{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2025-06-02T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"cargo test"}}],"provider":"anthropic","model":"claude-opus-4","usage":{"input":10,"output":5,"totalTokens":15,"cost":{"total":0.01}},"stopReason":"toolUse","timestamp":0}}"#,
            r#"{"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2025-06-02T09:00:03.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"usage":{"input":200,"output":20,"cost":{"total":0.02}},"timestamp":0}}"#,
        ],
    );

    let meta = parse_file(&path).unwrap();
    assert_eq!(meta.parent_session.as_deref(), Some("/tmp/cache/other.jsonl"));
    // text extracted from block-array user content
    assert_eq!(meta.first_prompt.as_deref(), Some("run the tests"));
    // assistant + nested toolResult usage both counted
    assert!((meta.cost_total - 0.03).abs() < 1e-9);
    assert_eq!(meta.tokens_in, 210);
    assert_eq!(meta.tokens_out, 25);
    assert_eq!(meta.model.as_deref(), Some("claude-opus-4"));
    assert_eq!(meta.branches, 0);
    std::fs::remove_file(&path).ok();
}

#[test]
fn skips_malformed_lines() {
    let path = fixture(
        "broken.jsonl",
        &[
            r#"{"type":"session","version":3,"id":"99999999-8888-7777-6666-555555555555","timestamp":"2025-06-03T08:00:00.000Z","cwd":"/tmp/proj-c"}"#,
            "not json at all",
            r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2025-06-03T08:00:01.000Z","message":{"role":"user","content":"still works"}}"#,
        ],
    );
    let meta = parse_file(&path).unwrap();
    assert_eq!(meta.cwd, "/tmp/proj-c");
    assert_eq!(meta.first_prompt.as_deref(), Some("still works"));
    assert_eq!(meta.messages, 1);
    std::fs::remove_file(&path).ok();
}