//! Memory consolidation parsing and LLM fallback behavior.

use tempfile::tempdir;
use xbot::engine::memory::{MemoryConsolidator, parse_consolidation_json};
use xbot::providers::{LlmResponse, LlmUsage, QueuedProvider};
use xbot::storage::Session;

fn bad_llm_body() -> LlmResponse {
    LlmResponse {
        content: Some("{not json".to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

fn good_llm_body() -> LlmResponse {
    LlmResponse {
        content: Some(r#"{"history_entry":"ok","memory_update":null}"#.to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

fn fat_session() -> Session {
    let mut session = Session::new("t");
    let filler = "tok ".repeat(120);
    for i in 0..24 {
        session.add_message(if i % 2 == 0 { "user" } else { "assistant" }, &filler);
    }
    session
}

#[test]
fn parse_consolidation_json_accepts_plain_json() {
    let (h, m) = parse_consolidation_json(
        r#"{"history_entry":"did a thing","memory_update":"user likes rust"}"#,
    )
    .expect("valid JSON");
    assert_eq!(h, "did a thing");
    assert_eq!(m.as_deref(), Some("user likes rust"));
}

#[test]
fn parse_consolidation_json_strips_markdown_fences() {
    let (h, m) =
        parse_consolidation_json("```json\n{\"history_entry\":\"x\",\"memory_update\":\"y\"}\n```")
            .expect("markdown wrapped");
    assert_eq!(h, "x");
    assert_eq!(m.as_deref(), Some("y"));
}

#[test]
fn parse_consolidation_json_null_memory_update_becomes_none() {
    let (h, m) =
        parse_consolidation_json(r#"{"history_entry":"only history","memory_update":null}"#)
            .expect("null memory_update");
    assert_eq!(h, "only history");
    assert!(
        m.is_none(),
        "expected None for null memory_update, got {m:?}"
    );
}

#[test]
fn parse_consolidation_json_rejects_invalid_json() {
    let err = parse_consolidation_json("not json").unwrap_err();
    assert!(
        err.to_string().contains("invalid consolidation JSON"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn consecutive_consolidation_failures_increment_and_reset_on_success() {
    let dir = tempdir().unwrap();
    let c = MemoryConsolidator::new(dir.path(), 400, 32 * 1024).expect("consolidator");

    let mut session = fat_session();
    let bad = QueuedProvider::new("m", vec![bad_llm_body()]);
    c.maybe_consolidate_by_tokens_with_provider(&mut session, 400, &bad, "m")
        .await
        .expect("first run");
    assert!(
        c.consecutive_consolidation_failures() >= 1,
        "invalid consolidation output should increment the failure counter"
    );

    c.reset_failure_count();
    assert_eq!(c.consecutive_consolidation_failures(), 0);

    let mut session2 = fat_session();
    let good_responses: Vec<LlmResponse> = (0..20).map(|_| good_llm_body()).collect();
    let good = QueuedProvider::new("m", good_responses);
    c.maybe_consolidate_by_tokens_with_provider(&mut session2, 400, &good, "m")
        .await
        .expect("good run");
    assert_eq!(
        c.consecutive_consolidation_failures(),
        0,
        "successful LLM consolidation must reset the failure counter"
    );
}

#[tokio::test]
async fn raw_archive_fallback_after_max_consolidation_failures() {
    let dir = tempdir().unwrap();
    let c = MemoryConsolidator::new(dir.path(), 400, 32 * 1024).expect("consolidator");

    let mut session = fat_session();

    let three_bad: Vec<LlmResponse> = (0..3).map(|_| bad_llm_body()).collect();
    let provider = QueuedProvider::new("m", three_bad);

    c.maybe_consolidate_by_tokens_with_provider(&mut session, 400, &provider, "m")
        .await
        .expect("consolidation run");

    let history = std::fs::read_to_string(c.store().memory_dir().join("HISTORY.md"))
        .expect("read HISTORY.md");
    assert!(
        history.contains("[RAW]"),
        "expected raw archive fallback in HISTORY after repeated failures; got:\n{history}"
    );
}
