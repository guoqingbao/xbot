//! AgentRuntime: per-session serialization and global concurrency limit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Barrier;
use xbot::config::ExecToolConfig;
use xbot::engine::AgentLoop;
use xbot::providers::{LlmProvider, LlmResponse, LlmUsage, QueuedProvider};
use xbot::runtime::AgentRuntime;
use xbot::storage::{InboundMessage, MessageBus};

fn terminal_response(text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

/// First `chat()` waits until `release` is completed; later calls return immediately.
struct StallFirstChatProvider {
    model: String,
    response: LlmResponse,
    release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    chat_calls: Arc<AtomicUsize>,
}

impl StallFirstChatProvider {
    fn new(
        model: impl Into<String>,
        response: LlmResponse,
        release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        chat_calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            model: model.into(),
            response,
            release,
            chat_calls,
        }
    }
}

#[async_trait]
impl LlmProvider for StallFirstChatProvider {
    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        _messages: &[xbot::storage::ChatMessage],
        _tools: Option<&[serde_json::Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        let wait = {
            let mut g = self.release.lock().expect("release lock");
            g.take()
        };
        if let Some(rx) = wait {
            let _ = rx.await;
        }
        Ok(self.response.clone())
    }
}

/// Every `chat()` synchronizes with a 2-party barrier so two sessions prove overlap.
struct BarrierChatProvider {
    model: String,
    response: LlmResponse,
    barrier: Arc<Barrier>,
    entered: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for BarrierChatProvider {
    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        _messages: &[xbot::storage::ChatMessage],
        _tools: Option<&[serde_json::Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.barrier.wait().await;
        Ok(self.response.clone())
    }
}

async fn new_test_agent(
    provider: Arc<dyn LlmProvider>,
    workspace: &std::path::Path,
) -> Arc<AgentLoop> {
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            workspace,
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
        .expect("agent"),
    );
    agent.set_auto_task_summary_enabled(false);
    agent
}

#[tokio::test]
async fn same_session_messages_run_serially_second_chat_starts_after_first_finishes() {
    let dir = tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Mutex::new(None));
    let (tx, rx) = tokio::sync::oneshot::channel();
    *release.lock().unwrap() = Some(rx);

    let provider = Arc::new(StallFirstChatProvider::new(
        "test-model",
        terminal_response("ok"),
        release.clone(),
        chat_calls.clone(),
    ));
    let agent = new_test_agent(provider, dir.path()).await;
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 4);
    runtime.start().await.unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "c1".to_string(),
        content: "first".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("shared".to_string()),
    })
    .await
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while chat_calls.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for first chat to start");

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "c1".to_string(),
        content: "second".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("shared".to_string()),
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(
        chat_calls.load(Ordering::SeqCst),
        1,
        "expected second message to wait on session lock while first is still in chat(); got {} concurrent chats",
        chat_calls.load(Ordering::SeqCst)
    );

    tx.send(()).ok();

    for _ in 0..2 {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), bus.consume_outbound())
            .await
            .expect("timeout waiting for outbound")
            .expect("outbound channel closed");
    }

    assert!(
        chat_calls.load(Ordering::SeqCst) >= 2,
        "expected at least one chat per inbound turn (extra calls may come from memory tooling)"
    );

    runtime.stop().await;
}

#[tokio::test]
async fn different_sessions_can_enter_chat_concurrently() {
    let dir = tempdir().unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let provider = Arc::new(BarrierChatProvider {
        model: "test-model".to_string(),
        response: terminal_response("ok"),
        barrier: barrier.clone(),
        entered: entered.clone(),
    });
    let agent = new_test_agent(provider, dir.path()).await;
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 4);
    runtime.start().await.unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "a".to_string(),
        content: "a".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("sess-a".to_string()),
    })
    .await
    .unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "b".to_string(),
        content: "b".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("sess-b".to_string()),
    })
    .await
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while entered.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("both sessions should reach chat() concurrently");

    for _ in 0..2 {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), bus.consume_outbound())
            .await
            .expect("timeout outbound")
            .expect("outbound closed");
    }

    runtime.stop().await;
}

#[tokio::test]
async fn global_semaphore_limits_concurrent_processing_across_sessions() {
    let dir = tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Mutex::new(None));
    let (tx, rx) = tokio::sync::oneshot::channel();
    *release.lock().unwrap() = Some(rx);

    let provider = Arc::new(StallFirstChatProvider::new(
        "test-model",
        terminal_response("ok"),
        release.clone(),
        chat_calls.clone(),
    ));
    let agent = new_test_agent(provider, dir.path()).await;
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone(), 1);
    runtime.start().await.unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "a".to_string(),
        content: "a".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("sem-a".to_string()),
    })
    .await
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while chat_calls.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "b".to_string(),
        content: "b".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: Some("sem-b".to_string()),
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(
        chat_calls.load(Ordering::SeqCst),
        1,
        "with max_concurrent_requests=1 the second session should not start chat() until the first finishes"
    );

    tx.send(()).ok();

    for _ in 0..2 {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), bus.consume_outbound())
            .await
            .expect("timeout outbound")
            .expect("outbound closed");
    }

    assert!(
        chat_calls.load(Ordering::SeqCst) >= 2,
        "expected both sessions to run at least one provider chat each"
    );

    runtime.stop().await;
}

#[tokio::test]
async fn queued_provider_pattern_still_drives_runtime_end_to_end() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![terminal_response("hello from queue")],
    ));
    let agent = new_test_agent(provider, dir.path()).await;
    let bus = MessageBus::new(8);
    let runtime = AgentRuntime::new(agent, bus.clone(), 2);
    runtime.start().await.unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "cli".to_string(),
        sender_id: "u".to_string(),
        chat_id: "d".to_string(),
        content: "ping".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: Default::default(),
        session_key_override: None,
    })
    .await
    .unwrap();

    let out = tokio::time::timeout(std::time::Duration::from_secs(3), bus.consume_outbound())
        .await
        .expect("timeout")
        .expect("closed");

    assert!(
        out.content.contains("hello from queue"),
        "unexpected outbound: {:?}",
        out.content
    );

    runtime.stop().await;
}
