use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rbot::config::ExecToolConfig;
use rbot::engine::AgentLoop;
use rbot::providers::{
    LlmProvider, LlmResponse, LlmUsage, ProviderModelInfo, QueuedProvider, ToolCallRequest,
};
use rbot::runtime::AgentRuntime;
use rbot::storage::{ChatMessage, InboundMessage, MessageBus, OutboundMessage, SessionManager};
use rbot::tools::MessageSendCallback;
use rbot::util::workspace_state_dir;
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn agent_loop_executes_tool_then_returns_final_answer() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hello from rust").unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Inspecting file".to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": file.display().to_string()}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("File inspection complete.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
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
        .process_direct("please inspect note.txt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "File inspection complete.");
    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert!(
        session
            .messages
            .iter()
            .any(|message| message.role == "tool" && message.name.as_deref() == Some("read_file"))
    );
}

#[tokio::test]
async fn zero_max_tool_iterations_means_unbounded_until_completion() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hello from rust").unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Inspecting file".to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": file.display().to_string()}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("File inspection complete.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        0,
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
        .process_direct("please inspect note.txt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "File inspection complete.");
}

struct AlwaysFailProvider;

#[async_trait]
impl LlmProvider for AlwaysFailProvider {
    fn default_model(&self) -> &str {
        "test-model"
    }

    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        Err(anyhow!("error decoding response body"))
    }
}

struct CatalogProvider {
    model: String,
    models: Vec<ProviderModelInfo>,
    responses: std::sync::Mutex<VecDeque<LlmResponse>>,
}

#[async_trait]
impl LlmProvider for CatalogProvider {
    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("catalog provider exhausted"))
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        Ok(self.models.clone())
    }
}

#[tokio::test]
async fn provider_errors_still_persist_the_user_turn() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(AlwaysFailProvider),
        dir.path(),
        Some("test-model".to_string()),
        0,
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

    let error = agent
        .process_direct("continue investigating", "cli:direct", "cli", "direct")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("error decoding response body"));

    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert!(session.messages.iter().any(|message| {
        message.role == "user"
            && message
                .content_as_text()
                .as_deref()
                .is_some_and(|text| text.contains("continue investigating"))
    }));
}

#[tokio::test]
async fn agent_loop_stops_on_repeated_tool_calls() {
    let dir = tempdir().unwrap();
    let repeated_response = LlmResponse {
        content: None,
        tool_calls: vec![ToolCallRequest {
            id: "call_1".to_string(),
            name: "list_dir".to_string(),
            arguments: json!({"path": "."}),
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    };
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![repeated_response; 30],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
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
        .process_direct("list files", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("pattern repeated"));
}

#[tokio::test]
async fn agent_loop_stops_on_repeated_tool_calls_even_with_new_ids() {
    let dir = tempdir().unwrap();
    let responses = (1..=30)
        .map(|idx| LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{idx}"),
                name: "exec".to_string(),
                arguments: json!({"command": "find /root -maxdepth 3 -name Cargo.toml"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        })
        .collect();
    let provider = Arc::new(QueuedProvider::new("test-model", responses));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
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
        .process_direct("find cargo manifests", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("pattern repeated"));
    assert!(response.content.contains("30 times"));
}

#[tokio::test]
async fn agent_loop_does_not_stop_when_tool_arguments_change() {
    let dir = tempdir().unwrap();
    let mut responses = (1..=12)
        .map(|idx| LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{idx}"),
                name: "exec".to_string(),
                arguments: json!({"command": format!("find /root -maxdepth {idx} -name Cargo.toml")}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        })
        .collect::<Vec<_>>();
    responses.push(LlmResponse {
        content: Some("Finished reviewing varying search attempts.".to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    });
    let provider = Arc::new(QueuedProvider::new("test-model", responses));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
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
        .process_direct("find cargo manifests", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.content,
        "Finished reviewing varying search attempts."
    );
}

struct SlowQueuedProvider {
    inner: QueuedProvider,
    delay: std::time::Duration,
}

#[async_trait]
impl LlmProvider for SlowQueuedProvider {
    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        tokio::time::sleep(self.delay).await;
        self.inner
            .chat(messages, tools, model, max_tokens, temperature)
            .await
    }
}

#[tokio::test]
async fn agent_loop_honors_stop_command() {
    let dir = tempdir().unwrap();
    // A provider that keeps requesting tools with UNIQUE arguments to avoid repetition detection
    let mut responses = Vec::new();
    for i in 0..100 {
        responses.push(LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{i}"),
                name: "list_dir".to_string(),
                arguments: json!({"path": format!("./{i}")}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        });
    }

    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new("test-model", responses),
        delay: std::time::Duration::from_millis(10),
    });
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            200, // Many iterations
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

    let agent_clone = agent.clone();
    let handle = tokio::spawn(async move {
        agent_clone
            .process_direct("loop forever", "cli:direct", "cli", "direct")
            .await
    });

    // Wait a bit then send stop
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let stop_response = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop_response.content, "Stopping current turn...");

    let result = handle.await.unwrap().unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn stop_command_does_not_block_the_next_prompt() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new(
            "test-model",
            vec![
                LlmResponse {
                    content: Some("first response".to_string()),
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: LlmUsage::default(),
                    reasoning_content: None,
                    thinking_blocks: None,
                },
                LlmResponse {
                    content: Some("second response".to_string()),
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: LlmUsage::default(),
                    reasoning_content: None,
                    thinking_blocks: None,
                },
            ],
        ),
        delay: Duration::from_millis(50),
    });
    let agent = Arc::new(
        AgentLoop::new(
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
        .unwrap(),
    );

    let agent_clone = agent.clone();
    let handle = tokio::spawn(async move {
        agent_clone
            .process_direct("first prompt", "cli:direct", "cli", "direct")
            .await
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    let stop_response = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop_response.content, "Stopping current turn...");
    assert!(handle.await.unwrap().unwrap().is_none());

    let next = agent
        .process_direct("second prompt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.content, "second response");
}

#[tokio::test]
async fn runtime_stop_command_sends_threaded_ack_and_completion() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new(
            "test-model",
            vec![LlmResponse {
                content: Some("should not be delivered".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            }],
        ),
        delay: Duration::from_millis(75),
    });
    let agent = Arc::new(
        AgentLoop::new(
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
        .unwrap(),
    );
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 3);
    runtime.start().await.unwrap();

    let session_key = "slack:C123:1700000000.000100".to_string();
    let slack_metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "work on this".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: slack_metadata.clone(),
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "/stop".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: slack_metadata.clone(),
        session_key_override: Some(session_key),
    })
    .await
    .unwrap();

    let mut ack = tokio::time::timeout(Duration::from_secs(1), bus.consume_outbound())
        .await
        .unwrap()
        .unwrap();
    while ack.content != "Stopping current turn..." {
        ack = tokio::time::timeout(Duration::from_secs(1), bus.consume_outbound())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(ack.content, "Stopping current turn...");
    let mut expected_meta = slack_metadata.clone();
    expected_meta.insert(
        "_session_key".to_string(),
        json!("slack:C123:1700000000.000100"),
    );
    assert_eq!(ack.metadata, expected_meta);

    let completion = tokio::time::timeout(Duration::from_secs(1), bus.consume_outbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completion.content, "Task stopped by user.");
    assert_eq!(completion.metadata, ack.metadata);

    runtime.stop().await;
}

#[tokio::test]
async fn clear_command_clears_session_and_preserves_history_file() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("Task finished.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
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

    agent
        .process_direct("finish something", "cli:direct", "cli", "direct")
        .await
        .unwrap();

    let history_path = workspace_state_dir(dir.path())
        .join("memory")
        .join("HISTORY.md");
    std::fs::write(&history_path, "junk history").unwrap();

    let response = agent
        .process_direct("/clear", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.content,
        "New session started. Previous messages were cleared."
    );
    assert_eq!(
        std::fs::read_to_string(history_path).unwrap(),
        "junk history"
    );
}

#[tokio::test]
async fn completed_tasks_append_task_summary_to_memory() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some(
                    "Implemented the stop acknowledgement flow. Added runtime replies, wired session cancellation cleanup, and covered the path with regression tests.".to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(
                    r#"{"title":"Stop acknowledgement flow","summary":"Added immediate stop acknowledgement and completion replies, with cancellation cleanup and regression coverage.","attention_points":["Keep threaded channel metadata on both replies."]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
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

    agent
        .process_direct(
            "Implement stop acknowledgement flow",
            "cli:direct",
            "cli",
            "direct",
        )
        .await
        .unwrap();

    let memory =
        std::fs::read_to_string(workspace_state_dir(dir.path()).join("memory/MEMORY.md")).unwrap();
    assert!(memory.contains("Task Summary"));
    assert!(memory.contains("Stop acknowledgement flow"));
    assert!(memory.contains("Added immediate stop acknowledgement and completion replies"));
    assert!(memory.contains("Keep threaded channel metadata on both replies."));
}

#[tokio::test]
async fn disabled_auto_task_summary_skips_memory_write_and_tool_hint() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("Finished without automatic memory summary.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
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
    agent.set_auto_task_summary_enabled(false);
    let progress_messages = Arc::new(Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("progress lock poisoned").push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    let response = agent
        .process_direct("finish requested work", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.content,
        "Finished without automatic memory summary."
    );

    let memory = std::fs::read_to_string(workspace_state_dir(dir.path()).join("memory/MEMORY.md"))
        .unwrap_or_default();
    assert!(!memory.contains("Task Summary"));
    let progress = progress_messages.lock().expect("progress lock poisoned");
    assert!(!progress.iter().any(|msg| {
        msg.metadata.get("_tool_name").and_then(Value::as_str) == Some("memory_summary")
    }));
}

#[tokio::test]
async fn disabled_memory_skips_memorize_and_memory_files() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new("test-model", vec![]));
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
    agent.set_memory_enabled(false);

    let response = agent
        .process_direct("/memorize remember this", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("Memory is disabled in this mode"));
    assert!(
        !workspace_state_dir(dir.path())
            .join("memory/MEMORY.md")
            .exists()
    );
    assert!(
        !workspace_state_dir(dir.path())
            .join("memory/HISTORY.md")
            .exists()
    );
}

#[tokio::test]
async fn near_context_limit_compresses_context_before_next_request() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "important file finding").unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Inspecting file.".to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": file.display().to_string()}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage {
                    prompt_tokens: 90,
                    completion_tokens: 8,
                },
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("Kept the file inspection result and current request.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("Done after compression.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        8,
        5,
        100,
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
    agent.set_auto_task_summary_enabled(false);
    let progress_messages = Arc::new(Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("progress lock poisoned").push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    let response = agent
        .process_direct("inspect the file", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "Done after compression.");

    let progress = progress_messages.lock().expect("progress lock poisoned");
    assert!(progress.iter().any(|msg| {
        msg.metadata.get("_tool_name").and_then(Value::as_str) == Some("context_compression")
    }));
    assert!(progress.iter().any(|msg| {
        msg.metadata.get("_tool_name").and_then(Value::as_str) == Some("context_compression_done")
    }));
    drop(progress);

    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    let combined = session
        .messages
        .iter()
        .filter_map(ChatMessage::content_as_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(combined.contains("[Compressed Context]"));
    assert!(combined.contains("Kept the file inspection result"));
    assert!(combined.contains("inspect the file"));
    assert!(combined.contains("Done after compression."));
    assert!(!combined.contains("important file finding"));
}

#[tokio::test]
async fn provider_usage_emits_realtime_context_update() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("Done.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage {
                prompt_tokens: 42,
                completion_tokens: 7,
            },
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        8,
        5,
        100,
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
    agent.set_auto_task_summary_enabled(false);
    let progress_messages = Arc::new(Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("progress lock poisoned").push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    agent
        .process_direct("finish", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();

    let progress = progress_messages.lock().expect("progress lock poisoned");
    let update = progress
        .iter()
        .find(|msg| msg.metadata.get("_context_update").and_then(Value::as_bool) == Some(true))
        .expect("context update should be emitted");
    assert_eq!(
        update.metadata.get("_context").and_then(Value::as_str),
        Some("42/100 (42%)")
    );
    drop(progress);

    let status = agent
        .process_direct("/status", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(status.content.contains("Context: 42/100 (42%)"));
}

#[tokio::test]
async fn completed_task_memory_emits_backend_tool_hint_without_context_entry() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Finished the requested work.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(
                    r#"{"title":"Requested work finished","summary":"Finished the requested work.","attention_points":[]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
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
    let progress_messages = Arc::new(Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("progress lock poisoned").push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    agent
        .process_direct("finish requested work", "cli:direct", "cli", "direct")
        .await
        .unwrap();

    let progress = progress_messages.lock().expect("progress lock poisoned");
    let hint = progress
        .iter()
        .find(|msg| {
            msg.metadata.get("_tool_name").and_then(Value::as_str) == Some("memory_summary")
        })
        .expect("memory summary tool hint should be emitted");
    assert_eq!(
        hint.metadata.get("_progress").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        hint.metadata.get("_tool_hint").and_then(Value::as_bool),
        Some(true)
    );
    assert!(hint.content.contains("memory_summary"));
    drop(progress);

    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert!(
        !session
            .messages
            .iter()
            .filter_map(ChatMessage::content_as_text)
            .any(|content| content.contains("memory_summary"))
    );
}

#[tokio::test]
async fn memorize_command_appends_user_memory_entry() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(QueuedProvider::new(
            "test-model",
            vec![LlmResponse {
                content: Some(
                    r#"{"title":"User prefers concise delivery","summary":"Keep responses concise and include documentation updates when feature work changes behavior.","attention_points":["Docs should stay in sync with shipped changes."]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            }],
        )),
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
        .process_direct(
            "/memorize User prefers concise delivery and wants docs updated with feature work.",
            "cli:direct",
            "cli",
            "direct",
        )
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("Memorized into permanent memory"));

    let memory =
        std::fs::read_to_string(workspace_state_dir(dir.path()).join("memory/MEMORY.md")).unwrap();
    assert!(memory.contains("User Instructed Memory"));
    assert!(memory.contains("User prefers concise delivery"));
    assert!(memory.contains("Docs should stay in sync with shipped changes."));
}

#[tokio::test]
async fn help_command_preserves_inbound_metadata() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(QueuedProvider::new("test-model", Vec::new())),
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
    let metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);

    let response = agent
        .process_inbound(InboundMessage {
            channel: "slack".to_string(),
            sender_id: "u1".to_string(),
            chat_id: "C123".to_string(),
            content: "/help".to_string(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: metadata.clone(),
            session_key_override: Some("slack:C123:1700000000.000100".to_string()),
        })
        .await
        .unwrap()
        .unwrap();

    let mut expected_meta = metadata.clone();
    expected_meta.insert(
        "_session_key".to_string(),
        json!("slack:C123:1700000000.000100"),
    );
    assert_eq!(response.metadata, expected_meta);
    assert_eq!(
        response.content,
        "Available commands:\n\
  /help     - Show this help message\n\
  /status   - Show current session status\n\
  /new      - Clear current session and start fresh\n\
  /stop     - Cancel current processing\n\
  /model    - Switch model (e.g. /model gpt-4.1)\n\
  /memorize - Save important facts to long-term memory"
    );
}

#[tokio::test]
async fn model_command_lists_available_models() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(CatalogProvider {
            model: "base-model".to_string(),
            models: vec![
                ProviderModelInfo {
                    id: "base-model".to_string(),
                    context_window_tokens: Some(4096),
                },
                ProviderModelInfo {
                    id: "alt-model".to_string(),
                    context_window_tokens: Some(256000),
                },
            ],
            responses: std::sync::Mutex::new(VecDeque::new()),
        }),
        dir.path(),
        Some("base-model".to_string()),
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
        .process_direct("/model", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("Current model: base-model"));
    assert!(response.content.contains("Context window: 4096"));
    assert!(response.content.contains("- base-model"));
    assert!(response.content.contains("- alt-model"));
}

#[tokio::test]
async fn model_command_switches_session_model_and_status_uses_provider_context() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(CatalogProvider {
            model: "base-model".to_string(),
            models: vec![
                ProviderModelInfo {
                    id: "base-model".to_string(),
                    context_window_tokens: Some(4096),
                },
                ProviderModelInfo {
                    id: "/models/alt-model".to_string(),
                    context_window_tokens: Some(256000),
                },
            ],
            responses: std::sync::Mutex::new(VecDeque::new()),
        }),
        dir.path(),
        Some("base-model".to_string()),
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

    let switched = agent
        .process_direct("/model alt-model", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(
        switched
            .content
            .contains("Model switched to /models/alt-model")
    );

    let status = agent
        .process_direct("/status", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(status.content.contains("Model: /models/alt-model"));
    assert!(status.content.contains("Workspace:"));
    assert!(status.content.contains("Context: 0/256000 (0%)"));
}

#[tokio::test]
async fn model_command_persists_selected_model_and_context() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(CatalogProvider {
            model: "base-model".to_string(),
            models: vec![
                ProviderModelInfo {
                    id: "base-model".to_string(),
                    context_window_tokens: Some(4096),
                },
                ProviderModelInfo {
                    id: "alt-model".to_string(),
                    context_window_tokens: Some(256000),
                },
            ],
            responses: std::sync::Mutex::new(VecDeque::new()),
        }),
        dir.path(),
        Some("base-model".to_string()),
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

    let persisted = Arc::new(std::sync::Mutex::new(None::<(String, Option<usize>)>));
    let persisted_clone = persisted.clone();
    agent.set_model_switch_callback(Some(Arc::new(move |model, context_window_tokens| {
        *persisted_clone.lock().unwrap() = Some((model, context_window_tokens));
        Ok(())
    })));

    agent
        .process_direct("/model alt-model", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        *persisted.lock().unwrap(),
        Some(("alt-model".to_string(), Some(256000)))
    );
}

#[tokio::test]
async fn status_refreshes_resumed_session_context_from_provider_models() {
    let dir = tempdir().unwrap();
    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let mut session = sessions.get_or_create("cli:direct").unwrap();
    session.metadata.insert(
        "model".to_string(),
        Value::String("Qwen3.5-35B-A3B-FP8".to_string()),
    );
    session
        .metadata
        .insert("contextWindowTokens".to_string(), Value::from(262144_u64));
    sessions.save(&session).unwrap();

    let agent = AgentLoop::new(
        Arc::new(CatalogProvider {
            model: "Qwen3.5 27B".to_string(),
            models: vec![ProviderModelInfo {
                id: "Qwen3.5-35B-A3B-FP8".to_string(),
                context_window_tokens: Some(262144),
            }],
            responses: std::sync::Mutex::new(VecDeque::new()),
        }),
        dir.path(),
        Some("Qwen3.5 27B".to_string()),
        8,
        5,
        262144,
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

    let status = agent
        .process_direct("/status", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(status.content.contains("Model: Qwen3.5-35B-A3B-FP8"));
    assert!(status.content.contains("Workspace:"));
    assert!(status.content.contains("Context: 0/262144 (0%)"));

    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert_eq!(
        session
            .metadata
            .get("contextWindowTokens")
            .and_then(Value::as_u64),
        Some(262144)
    );
}

#[tokio::test]
async fn backend_announces_new_session_once_per_runtime_session() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("First reply.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(
                    r#"{"title":"First turn","summary":"Completed the first backend turn.","attention_points":[]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("Second reply.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(
                    r#"{"title":"Second turn","summary":"Completed the second backend turn.","attention_points":[]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
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
    let progress_messages = Arc::new(std::sync::Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().unwrap().push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    let first = agent
        .process_inbound(InboundMessage {
            channel: "slack".to_string(),
            sender_id: "u1".to_string(),
            chat_id: "C123".to_string(),
            content: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: BTreeMap::new(),
            session_key_override: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.content, "First reply.");

    let second = agent
        .process_inbound(InboundMessage {
            channel: "slack".to_string(),
            sender_id: "u1".to_string(),
            chat_id: "C123".to_string(),
            content: "again".to_string(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: BTreeMap::new(),
            session_key_override: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.content, "Second reply.");

    let messages = progress_messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0].content.starts_with(
            "Session: started new session for this conversation.\n\n_Model: test-model_"
        )
    );
    assert!(messages[0].content.contains("Workspace:"));
    assert!(messages[0].content.contains("Session messages: 0"));
}

#[tokio::test]
async fn backend_announces_when_resuming_existing_session() {
    let dir = tempdir().unwrap();
    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let mut session = sessions.get_or_create("slack:C123").unwrap();
    session.add_message("user", "Earlier message");
    sessions.save(&session).unwrap();

    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("Reply after resume.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        },
        LlmResponse {
            content: Some(
                r#"{"title":"Resumed turn","summary":"Completed a resumed backend turn.","attention_points":[]}"#
                    .to_string(),
            ),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
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
    let progress_messages = Arc::new(std::sync::Mutex::new(Vec::<OutboundMessage>::new()));
    let captured = progress_messages.clone();
    let callback: MessageSendCallback = Arc::new(move |msg| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().unwrap().push(msg);
            Ok(())
        })
    });
    agent.set_progress_sender(Some(callback));

    let response = agent
        .process_inbound(InboundMessage {
            channel: "slack".to_string(),
            sender_id: "u1".to_string(),
            chat_id: "C123".to_string(),
            content: "resume".to_string(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: BTreeMap::new(),
            session_key_override: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "Reply after resume.");

    let messages = progress_messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].content.starts_with(
        "Session: resuming 1 previous message; /new to start fresh.\n\n_Model: test-model_"
    ));
    assert!(messages[0].content.contains("Workspace:"));
    assert!(messages[0].content.contains("Session messages: 1"));
}
