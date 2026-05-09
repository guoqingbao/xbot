use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use xbot::providers::{LlmResponse, QueuedProvider, SharedProvider, ToolCallRequest};
use xbot::runtime::HeartbeatService;

#[tokio::test]
async fn start_is_idempotent() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new("test-model", vec![]));
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        None,
        None,
        None,
        9999,
        true,
    );
    service.start().await.unwrap();
    service.start().await.unwrap();
    service.stop().await;
}

#[tokio::test]
async fn decide_returns_skip_when_no_tool_call() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("no tool call".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: Default::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        None,
        None,
        None,
        1,
        true,
    );
    let (action, tasks) = service.decide("heartbeat content").await.unwrap();
    assert_eq!(action, "skip");
    assert_eq!(tasks, "");
}

#[tokio::test]
async fn trigger_now_executes_when_decision_is_run() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some(String::new()),
            tool_calls: vec![ToolCallRequest {
                id: "hb_1".to_string(),
                name: "heartbeat".to_string(),
                arguments: json!({"action": "run", "tasks": "check open tasks"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: Default::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let called_with = Arc::new(Mutex::new(Vec::<String>::new()));
    let called_ref = called_with.clone();
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        Some(Arc::new(move |tasks| {
            let called = called_ref.clone();
            Box::pin(async move {
                called.lock().unwrap().push(tasks.clone());
                Ok("done".to_string())
            })
        })),
        None,
        None,
        1,
        true,
    );
    std::fs::create_dir_all(service.heartbeat_file().parent().unwrap()).unwrap();
    std::fs::write(service.heartbeat_file(), "- [ ] do thing").unwrap();
    let result = service.trigger_now().await.unwrap();
    assert_eq!(result.as_deref(), Some("done"));
    assert_eq!(
        called_with.lock().unwrap().as_slice(),
        &["check open tasks"]
    );
}

#[tokio::test]
async fn tick_notifies_when_evaluator_says_yes() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some(String::new()),
            tool_calls: vec![ToolCallRequest {
                id: "hb_1".to_string(),
                name: "heartbeat".to_string(),
                arguments: json!({"action": "run", "tasks": "check deployments"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: Default::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let notified = Arc::new(Mutex::new(Vec::<String>::new()));
    let notified_ref = notified.clone();
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        Some(Arc::new(move |tasks| {
            Box::pin(async move { Ok(format!("executed: {tasks}")) })
        })),
        Some(Arc::new(move |response| {
            let notified = notified_ref.clone();
            Box::pin(async move {
                notified.lock().unwrap().push(response);
                Ok(())
            })
        })),
        Some(Arc::new(move |_response, _tasks| {
            Box::pin(async move { Ok(true) })
        })),
        1,
        true,
    );
    std::fs::create_dir_all(service.heartbeat_file().parent().unwrap()).unwrap();
    std::fs::write(service.heartbeat_file(), "- [ ] check deployments").unwrap();
    service.tick().await.unwrap();
    assert_eq!(
        notified.lock().unwrap().as_slice(),
        &["executed: check deployments"]
    );
}

#[tokio::test]
async fn tick_suppresses_when_evaluator_says_no() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some(String::new()),
            tool_calls: vec![ToolCallRequest {
                id: "hb_1".to_string(),
                name: "heartbeat".to_string(),
                arguments: json!({"action": "run", "tasks": "check status"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: Default::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let notified = Arc::new(Mutex::new(Vec::<String>::new()));
    let notified_ref = notified.clone();
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        Some(Arc::new(move |tasks| {
            Box::pin(async move { Ok(format!("executed: {tasks}")) })
        })),
        Some(Arc::new(move |response| {
            let notified = notified_ref.clone();
            Box::pin(async move {
                notified.lock().unwrap().push(response);
                Ok(())
            })
        })),
        Some(Arc::new(move |_response, _tasks| {
            Box::pin(async move { Ok(false) })
        })),
        1,
        true,
    );
    std::fs::create_dir_all(service.heartbeat_file().parent().unwrap()).unwrap();
    std::fs::write(service.heartbeat_file(), "- [ ] check status").unwrap();
    service.tick().await.unwrap();
    assert!(notified.lock().unwrap().is_empty());
}

#[tokio::test]
async fn decide_retries_transient_error_then_succeeds() {
    let dir = tempdir().unwrap();
    let provider: SharedProvider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("429 rate limit".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "error".to_string(),
                usage: Default::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(String::new()),
                tool_calls: vec![ToolCallRequest {
                    id: "hb_1".to_string(),
                    name: "heartbeat".to_string(),
                    arguments: json!({"action": "run", "tasks": "check open tasks"}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: Default::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
    let service = HeartbeatService::new(
        dir.path(),
        provider,
        "test-model",
        None,
        None,
        None,
        1,
        true,
    );
    let (action, tasks) = service.decide("heartbeat content").await.unwrap();
    assert_eq!(action, "run");
    assert_eq!(tasks, "check open tasks");
}
