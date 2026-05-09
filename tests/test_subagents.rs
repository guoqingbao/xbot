use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::tempdir;
use xbot::config::ExecToolConfig;
use xbot::engine::AgentLoop;
use xbot::providers::{LlmProvider, LlmResponse, LlmUsage, ToolCallRequest};
use xbot::runtime::AgentRuntime;
use xbot::storage::{ChatMessage, InboundMessage, MessageBus};

#[derive(Clone, Copy)]
enum SubagentMode {
    Complete,
    Slow,
}

struct DeterministicSubagentProvider {
    mode: SubagentMode,
}

impl DeterministicSubagentProvider {
    fn new(mode: SubagentMode) -> Self {
        Self { mode }
    }
}

#[async_trait]
impl LlmProvider for DeterministicSubagentProvider {
    fn default_model(&self) -> &str {
        "test-model"
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        let last_text = messages
            .iter()
            .rev()
            .filter_map(ChatMessage::content_as_text)
            .next()
            .unwrap_or_default();

        if last_text.contains("check subagent context") {
            let has_subagent_result = messages
                .iter()
                .filter_map(ChatMessage::content_as_text)
                .any(|text| {
                    text.contains("[Subagent 'delegate' completed]")
                        && text.contains("Subagent finished the delegated work.")
                });
            return Ok(stop_response(if has_subagent_result {
                "Subagent context present."
            } else {
                "Subagent context missing."
            }));
        }

        if last_text.contains("Subagent results for the current session")
            && last_text.contains("Subagent finished the delegated work.")
        {
            return Ok(stop_response("Main task continued with subagent result."));
        }

        if last_text.contains("[Subagent 'delegate' completed]") {
            return Ok(stop_response("Background summary."));
        }

        let should_wait_for_subagent = messages
            .iter()
            .filter_map(ChatMessage::content_as_text)
            .any(|text| text.contains("delegate and wait"));
        let is_budget_subagent = messages
            .iter()
            .filter_map(ChatMessage::content_as_text)
            .any(|text| text.contains("loop until budget"));

        if last_text.contains("Budget final synthesized.") {
            return Ok(stop_response("Main saw synthesized budget result."));
        }

        if last_text.contains("delegate budget test") {
            return Ok(tool_response(
                "spawn_budget_1",
                "loop until budget",
                "budget",
            ));
        }

        if last_text.contains("delegate work") {
            return Ok(tool_response("spawn_1", "collect report", "delegate"));
        }

        if last_text.contains("delegate and wait") {
            return Ok(tool_response("spawn_wait_1", "collect report", "delegate"));
        }

        if last_text.contains("delegate slow work") {
            return Ok(tool_response("spawn_2", "slow task", "slow"));
        }

        if last_text.starts_with("Subagent [") {
            if should_wait_for_subagent {
                return Ok(wait_subagents_response("wait_1"));
            }
            return Ok(stop_response("Background task started."));
        }

        if is_budget_subagent {
            if tools.is_none() || last_text.contains("reached the subagent tool-iteration budget") {
                return Ok(stop_response(
                    "Budget final synthesized. Bugs found: none. Reviewed repeated directory observations and stopped with a final report instead of raw truncated tool output.",
                ));
            }
            return Ok(list_dir_response(format!("budget_{}", messages.len())));
        }

        if last_text.contains("collect report") {
            return Ok(stop_response("Subagent finished the delegated work."));
        }

        if last_text.contains("slow task") {
            if matches!(self.mode, SubagentMode::Slow) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            return Ok(stop_response("Slow subagent completed."));
        }

        Ok(stop_response("Unhandled test prompt."))
    }
}

fn stop_response(content: &str) -> LlmResponse {
    LlmResponse {
        content: Some(content.to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

#[allow(dead_code)]
fn outbound_metadata(base: &BTreeMap<String, Value>, session_key: &str) -> BTreeMap<String, Value> {
    let mut m = base.clone();
    m.insert("_session_key".to_string(), json!(session_key));
    m
}

use xbot::storage::OutboundMessage;

async fn consume_non_progress(bus: &MessageBus, timeout_secs: u64) -> OutboundMessage {
    let deadline = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline
            .checked_sub(start.elapsed())
            .unwrap_or(Duration::ZERO);
        let msg = tokio::time::timeout(remaining, bus.consume_outbound())
            .await
            .expect("timeout waiting for non-progress outbound")
            .expect("bus error");
        let is_progress = msg
            .metadata
            .get("_progress")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_tool_hint = msg
            .metadata
            .get("_tool_hint")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_progress && !is_tool_hint && !msg.content.is_empty() {
            return msg;
        }
    }
}

async fn consume_content(bus: &MessageBus, expected: &str, timeout_secs: u64) -> OutboundMessage {
    let deadline = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline
            .checked_sub(start.elapsed())
            .unwrap_or(Duration::ZERO);
        let msg = tokio::time::timeout(remaining, bus.consume_outbound())
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for message containing '{expected}'"))
            .expect("bus error");
        if msg.content.contains(expected) {
            return msg;
        }
    }
}

fn tool_response(id: &str, task: &str, label: &str) -> LlmResponse {
    LlmResponse {
        content: Some("Delegating work.".to_string()),
        tool_calls: vec![ToolCallRequest {
            id: id.to_string(),
            name: "spawn".to_string(),
            arguments: json!({
                "task": task,
                "label": label,
            }),
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

fn wait_subagents_response(id: &str) -> LlmResponse {
    LlmResponse {
        content: Some("Waiting for subagent results.".to_string()),
        tool_calls: vec![ToolCallRequest {
            id: id.to_string(),
            name: "wait_subagents".to_string(),
            arguments: json!({
                "timeout_seconds": 5,
            }),
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

fn list_dir_response(id: String) -> LlmResponse {
    LlmResponse {
        content: Some("Inspecting another slice.".to_string()),
        tool_calls: vec![ToolCallRequest {
            id,
            name: "list_dir".to_string(),
            arguments: json!({
                "path": ".",
            }),
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

#[tokio::test]
async fn runtime_routes_completed_subagent_back_to_origin_chat() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Complete));
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            6,
            5,
            8_000,
            32 * 1024,
            Default::default(),
            None,
            ExecToolConfig {
                enable: false,
                timeout: 60,
                path_append: String::new(),
            },
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap(),
    );
    let bus = MessageBus::new(8);
    let runtime = AgentRuntime::new(agent, bus.clone(), 3);
    runtime.start().await.unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "user".to_string(),
        chat_id: "direct".to_string(),
        content: "delegate work".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: None,
    })
    .await
    .unwrap();

    let completed = consume_non_progress(&bus, 2).await;
    assert_eq!(completed.channel, "cli");
    assert_eq!(completed.chat_id, "direct");
    assert_eq!(completed.content, "Background summary.");

    runtime.stop().await;
}

#[tokio::test]
async fn runtime_saves_completed_subagent_result_to_origin_session_history() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Complete));
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            6,
            5,
            8_000,
            32 * 1024,
            Default::default(),
            None,
            ExecToolConfig {
                enable: false,
                timeout: 60,
                path_append: String::new(),
            },
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap(),
    );
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 3);
    runtime.start().await.unwrap();

    let metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);
    let session_key = "slack:C123:1700000000.000100".to_string();

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "delegate work".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: metadata.clone(),
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    let session_notice = consume_content(&bus, "Session: started new session", 2).await;
    assert_eq!(session_notice.channel, "slack");

    let started = consume_content(&bus, "Background task started.", 3).await;
    assert_eq!(started.channel, "slack");

    let completed = consume_content(&bus, "Background summary.", 3).await;
    assert_eq!(completed.channel, "slack");
    assert_eq!(
        completed
            .metadata
            .get("_session_key")
            .and_then(Value::as_str),
        Some(session_key.as_str())
    );

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "check subagent context".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata,
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    consume_content(&bus, "Subagent context present.", 2).await;

    runtime.stop().await;
}

#[tokio::test]
async fn main_turn_can_wait_for_subagent_results_in_same_context() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Complete));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        8,
        5,
        8_000,
        32 * 1024,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();

    let response = agent
        .process_direct("delegate and wait", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        response.content,
        "Main task continued with subagent result."
    );
}

#[tokio::test]
async fn subagent_iteration_budget_gets_final_synthesis_not_tool_snippets() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Complete));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        64,
        5,
        8_000,
        32 * 1024,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();

    let response = agent
        .process_direct("delegate budget test", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.content, "Main saw synthesized budget result.");
}

#[tokio::test]
async fn stop_command_cancels_active_subagent_tasks() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Slow));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        6,
        5,
        8_000,
        32 * 1024,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();

    let agent = Arc::new(agent);
    let active = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .process_direct("delegate slow work", "cli:direct", "cli", "direct")
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;

    let stopped = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped.content, "Stopping current turn and 1 task(s)...");
    assert!(active.await.unwrap().unwrap().is_none());

    let stopped_again = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped_again.content, "No active task to stop.");
}

#[tokio::test]
async fn clear_command_resets_active_subagent_tasks() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Slow));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        6,
        5,
        8_000,
        32 * 1024,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();

    let agent = Arc::new(agent);
    let active = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .process_direct("delegate slow work", "cli:direct", "cli", "direct")
                .await
        })
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while agent.snapshot().unwrap().running_subagents == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "subagent did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let cleared = agent
        .process_direct("/clear", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cleared.content,
        "New session started. Previous messages were cleared."
    );
    assert_eq!(agent.snapshot().unwrap().running_subagents, 0);

    let active_response = tokio::time::timeout(Duration::from_secs(3), active)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(active_response.content, "Background task started.");
}

#[tokio::test]
async fn runtime_stop_command_acknowledges_and_confirms_subagent_cancellation() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(DeterministicSubagentProvider::new(SubagentMode::Slow));
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            6,
            5,
            8_000,
            32 * 1024,
            Default::default(),
            None,
            ExecToolConfig {
                enable: false,
                timeout: 60,
                path_append: String::new(),
            },
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap(),
    );
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 3);
    runtime.start().await.unwrap();

    let metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);
    let session_key = "slack:C123:1700000000.000100".to_string();

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "delegate slow work".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: metadata.clone(),
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    // Non-cli channels get a one-time backend session notice before the turn reply.
    let session_notice = consume_content(&bus, "Session: started new session", 2).await;
    assert_eq!(session_notice.channel, "slack");

    let started = consume_content(&bus, "Background task started.", 3).await;
    assert_eq!(started.channel, "slack");

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "/stop".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: metadata.clone(),
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    let ack = consume_content(&bus, "Stopping", 2).await;
    assert_eq!(ack.channel, "slack");

    let completion = consume_content(&bus, "Stopped", 2).await;
    assert_eq!(completion.channel, "slack");

    runtime.stop().await;
}
