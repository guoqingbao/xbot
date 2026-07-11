use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;
use xbot::config::{Config, ExecToolConfig};
use xbot::engine::AgentLoop;
use xbot::providers::{LlmResponse, LlmUsage, QueuedProvider, ToolCallRequest};
use xbot::runtime::AgentRuntime;
use xbot::storage::{InboundMessage, MessageBus};

#[test]
fn load_config_keeps_max_tokens_and_ignores_legacy_memory_window() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_string(&json!({
            "agents": {
                "defaults": {
                    "maxTokens": 1234,
                    "memoryMaxBytes": 4096,
                    "memoryWindow": 42
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let config = Config::load(Some(&config_path)).unwrap();
    assert_eq!(config.agents.defaults.max_tokens, 1234);
    assert_eq!(config.agents.defaults.memory_max_bytes, 4096);
    assert_eq!(config.agents.defaults.context_window_tokens, 65_536);
    let saved_path = config.save(Some(&config_path)).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(saved_path).unwrap()).unwrap();
    let defaults = &saved["agents"]["defaults"];
    assert!(defaults.get("memoryWindow").is_none());
    assert_eq!(defaults["memoryMaxBytes"], json!(4096));
}

#[test]
fn provider_matching_prefers_keyword_and_local_base_detection() {
    let mut openrouter_only = Config::default();
    openrouter_only.providers.insert(
        "openrouter".to_string(),
        xbot::config::ProviderConfig {
            api_key: "sk-or-test".to_string(),
            api_base: None,
            extra_headers: Default::default(),
            reasoning_effort: None,
        },
    );
    openrouter_only.agents.defaults.provider = "auto".to_string();

    assert_eq!(
        openrouter_only
            .provider_name_for_model(Some("anthropic/claude-3-7-sonnet"))
            .as_deref(),
        Some("openrouter")
    );

    let mut config = openrouter_only.clone();
    config.providers.insert(
        "ollama".to_string(),
        xbot::config::ProviderConfig {
            api_key: String::new(),
            api_base: Some("http://localhost:11434/v1".to_string()),
            extra_headers: Default::default(),
            reasoning_effort: None,
        },
    );
    config.agents.defaults.provider = "auto".to_string();

    assert_eq!(
        config.provider_name_for_model(Some("llama3.2")).as_deref(),
        Some("ollama")
    );
    assert_eq!(
        config
            .provider_api_base_for_model(Some("llama3.2"))
            .as_deref(),
        Some("http://localhost:11434/v1")
    );
}

#[test]
fn subagent_config_can_select_separate_model_provider_and_api_base() {
    let mut config = Config::default();
    config.providers.insert(
        "subagent-fast".to_string(),
        xbot::config::ProviderConfig {
            api_key: String::new(),
            api_base: Some("http://127.0.0.1:8001/v1".to_string()),
            extra_headers: Default::default(),
            reasoning_effort: None,
        },
    );
    config.agents.subagents.model = "qwen2.5-coder:7b".to_string();
    config.agents.subagents.provider = "subagent-fast".to_string();

    assert_eq!(config.subagent_model("openai/gpt-4.1"), "qwen2.5-coder:7b");
    let (provider_name, provider_cfg) = config
        .subagent_provider_for_model(Some("qwen2.5-coder:7b"))
        .unwrap();
    assert_eq!(provider_name, "subagent-fast");
    assert_eq!(
        provider_cfg.api_base.as_deref(),
        Some("http://127.0.0.1:8001/v1")
    );
}

#[tokio::test]
async fn runtime_processes_bus_messages_and_message_tool_delivers_outbound() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some(String::new()),
            tool_calls: vec![ToolCallRequest {
                id: "msg_1".to_string(),
                name: "message".to_string(),
                arguments: json!({"content": "pushed via message tool"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            4,
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
            vec![],
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
        content: "send me something".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: None,
    })
    .await
    .unwrap();

    let outbound = loop {
        let outbound =
            tokio::time::timeout(std::time::Duration::from_secs(2), bus.consume_outbound())
                .await
                .unwrap()
                .unwrap();
        if outbound.content == "pushed via message tool" {
            break outbound;
        }
    };
    assert_eq!(outbound.channel, "cli");
    assert_eq!(outbound.chat_id, "direct");
    assert_eq!(outbound.content, "pushed via message tool");

    runtime.stop().await;
}
