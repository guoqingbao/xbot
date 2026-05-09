use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use tower::ServiceExt;
use xbot::channels::{ChannelManager, TelegramApi, TelegramBotIdentity};
use xbot::config::{ChannelsConfig, Config, ExecToolConfig};
use xbot::cron::CronService;
use xbot::engine::AgentLoop;
use xbot::observability::RuntimeTelemetry;
use xbot::providers::QueuedProvider;
use xbot::runtime::{build_gateway_router, build_webhook_router};
use xbot::storage::MessageBus;

struct DummyTelegramApi;

#[async_trait]
impl TelegramApi for DummyTelegramApi {
    async fn get_me(&self) -> Result<TelegramBotIdentity> {
        Ok(TelegramBotIdentity {
            id: 999,
            username: "xbot_test".to_string(),
        })
    }

    async fn send_message(
        &self,
        _chat_id: i64,
        _text: &str,
        _message_thread_id: Option<i64>,
        _reply_parameters: Option<xbot::channels::ReplyParameters>,
    ) -> Result<()> {
        Ok(())
    }

    async fn send_photo(
        &self,
        _chat_id: i64,
        _photo: &str,
        _message_thread_id: Option<i64>,
        _reply_parameters: Option<xbot::channels::ReplyParameters>,
    ) -> Result<()> {
        Ok(())
    }

    async fn send_voice(
        &self,
        _chat_id: i64,
        _voice: &str,
        _message_thread_id: Option<i64>,
        _reply_parameters: Option<xbot::channels::ReplyParameters>,
    ) -> Result<()> {
        Ok(())
    }

    async fn send_audio(
        &self,
        _chat_id: i64,
        _audio: &str,
        _message_thread_id: Option<i64>,
        _reply_parameters: Option<xbot::channels::ReplyParameters>,
    ) -> Result<()> {
        Ok(())
    }

    async fn send_document(
        &self,
        _chat_id: i64,
        _document: &str,
        _message_thread_id: Option<i64>,
        _reply_parameters: Option<xbot::channels::ReplyParameters>,
    ) -> Result<()> {
        Ok(())
    }

    async fn get_file(&self, _file_id: &str) -> Result<String> {
        Ok(String::new())
    }

    async fn download_file(&self, _file_path: &str) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn slack_gateway_handles_url_verification() {
    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "slack": {"enabled": true, "allowFrom": ["*"], "botToken": "xoxb-test", "signingSecret": "secret", "webhookPath": "/slack/events"}
    }))
    .unwrap();
    let manager = Arc::new(ChannelManager::new(cfg.clone(), bus, PathBuf::new()).unwrap());
    let slack_channel = manager.get_channel("slack").unwrap();
    let slack = slack_channel
        .as_any()
        .downcast_ref::<xbot::channels::SlackChannel>()
        .unwrap();

    struct FakeSlackApi;
    #[async_trait]
    impl xbot::channels::SlackApi for FakeSlackApi {
        async fn auth_test(&self) -> Result<String> {
            Ok("B123".to_string())
        }
        async fn chat_post_message(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn files_upload(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn reactions_add(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn reactions_remove(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn download_file(&self, _: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }
    slack.set_api(Arc::new(FakeSlackApi)).await;

    let router = build_webhook_router(&manager, &cfg).unwrap().unwrap();
    manager.start_all().await.unwrap();

    let body = json!({"type":"url_verification","challenge":"abc123"}).to_string();
    let timestamp = Utc::now().timestamp().to_string();
    let signature = slack_signature("secret", &timestamp, &body);

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/slack/events")
                .header("content-type", "application/json")
                .header("x-slack-request-timestamp", &timestamp)
                .header("x-slack-signature", signature)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "abc123");
}

#[tokio::test]
async fn telegram_gateway_validates_secret_and_publishes_inbound() {
    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "telegram": {"enabled": true, "allowFrom": ["*"], "token": "123:abc", "webhookPath": "/telegram/webhook", "webhookSecret": "secret"}
    }))
    .unwrap();
    let manager = Arc::new(ChannelManager::new(cfg.clone(), bus.clone(), PathBuf::new()).unwrap());
    let telegram_channel = manager.get_channel("telegram").unwrap();
    let telegram = telegram_channel
        .as_any()
        .downcast_ref::<xbot::channels::TelegramChannel>()
        .unwrap();
    telegram.set_api(Arc::new(DummyTelegramApi)).await;
    let router = build_webhook_router(&manager, &cfg).unwrap().unwrap();
    manager.start_all().await.unwrap();

    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/telegram/webhook")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/telegram/webhook")
                .header("content-type", "application/json")
                .header("x-telegram-bot-api-secret-token", "secret")
                .body(Body::from(
                    json!({
                        "message": {
                            "chat": {"id": 123, "type": "private"},
                            "chat_id": 123,
                            "from": {"id": 1, "username": "alice"},
                            "text": "hello",
                            "message_id": 7
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let inbound = tokio::time::timeout(Duration::from_secs(1), bus.consume_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.channel, "telegram");
    assert_eq!(inbound.content, "hello");
}

#[tokio::test]
async fn feishu_gateway_handles_challenge_and_event_delivery() {
    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "feishu": {"enabled": true, "allowFrom": ["*"], "appId": "id", "appSecret": "secret", "webhookPath": "/feishu/events", "verificationToken": "vt"}
    }))
    .unwrap();
    let manager = Arc::new(ChannelManager::new(cfg.clone(), bus.clone(), PathBuf::new()).unwrap());
    let router = build_webhook_router(&manager, &cfg).unwrap().unwrap();
    manager.start_all().await.unwrap();

    let challenge = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/feishu/events")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"challenge":"abc","token":"vt"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(challenge.status(), StatusCode::OK);
    let challenge_body = to_bytes(challenge.into_body(), 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&challenge_body).unwrap(),
        json!({"challenge":"abc"})
    );

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/feishu/events")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "token": "vt",
                        "event": {
                            "message": {
                                "message_id": "om_001",
                                "chat_id": "ou_chat",
                                "chat_type": "p2p",
                                "message_type": "text",
                                "content": "{\"text\":\"hello from feishu\"}"
                            },
                            "sender": {
                                "sender_type": "user",
                                "sender_id": {"open_id": "ou_alice"}
                            }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let inbound = tokio::time::timeout(Duration::from_secs(1), bus.consume_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.channel, "feishu");
    assert_eq!(inbound.content, "hello from feishu");
}

#[tokio::test]
async fn slack_gateway_rejects_invalid_signature() {
    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "slack": {"enabled": true, "allowFrom": ["*"], "botToken": "xoxb-test", "signingSecret": "secret", "webhookPath": "/slack/events"}
    }))
    .unwrap();
    let manager = Arc::new(ChannelManager::new(cfg.clone(), bus, PathBuf::new()).unwrap());
    let slack_channel = manager.get_channel("slack").unwrap();
    let slack = slack_channel
        .as_any()
        .downcast_ref::<xbot::channels::SlackChannel>()
        .unwrap();

    struct FakeSlackApi;
    #[async_trait]
    impl xbot::channels::SlackApi for FakeSlackApi {
        async fn auth_test(&self) -> Result<String> {
            Ok("B123".to_string())
        }
        async fn chat_post_message(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn files_upload(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn reactions_add(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn reactions_remove(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn download_file(&self, _: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }
    slack.set_api(Arc::new(FakeSlackApi)).await;

    let router = build_webhook_router(&manager, &cfg).unwrap().unwrap();

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/slack/events")
                .header("content-type", "application/json")
                .header(
                    "x-slack-request-timestamp",
                    Utc::now().timestamp().to_string(),
                )
                .header("x-slack-signature", "v0=invalid")
                .body(Body::from(
                    json!({"type":"event_callback","event":{}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_gateway_exposes_overview_and_metrics() {
    let workspace = tempfile::tempdir().unwrap();
    let cron_dir = tempfile::tempdir().unwrap();
    let bus = MessageBus::new(8);
    let config: Config = serde_json::from_value(json!({
        "agents": {
            "defaults": {
                "workspace": workspace.path(),
                "model": "ollama/qwen2.5-coder:7b",
                "provider": "ollama"
            }
        },
        "providers": {
            "ollama": {
                "apiBase": "http://localhost:11434/v1"
            }
        },
        "channels": {
            "local": {
                "enabled": true,
                "allowFrom": ["*"]
            }
        }
    }))
    .unwrap();
    let manager = Arc::new(
        ChannelManager::new(config.channels.clone(), bus, workspace.path().to_path_buf()).unwrap(),
    );
    manager.start_all().await.unwrap();
    let agent = Arc::new(
        AgentLoop::new(
            Arc::new(QueuedProvider::new("test-model", vec![])),
            workspace.path(),
            Some("ollama/qwen2.5-coder:7b".to_string()),
            4,
            5,
            8_000,
            32 * 1024,
            Default::default(),
            None,
            ExecToolConfig::default(),
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap(),
    );
    let telemetry = RuntimeTelemetry::new(
        "ollama",
        "ollama/qwen2.5-coder:7b",
        Some("http://localhost:11434/v1".to_string()),
    );
    let cron = CronService::new(cron_dir.path().join("jobs.json"));
    let router = build_gateway_router(
        &manager,
        &config,
        Some(agent),
        Some(cron),
        None,
        Some(telemetry),
    )
    .unwrap()
    .unwrap();

    let overview = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/admin/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(overview.status(), StatusCode::OK);

    let metrics = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
    let body = to_bytes(metrics.into_body(), 64 * 1024).await.unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("xbot_provider_requests_total"));
}

#[tokio::test]
async fn gateway_exposes_health_and_status_endpoints() {
    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "telegram": {"enabled": true, "allowFrom": ["*"], "token": "123:abc", "webhookPath": "/telegram/webhook"}
    }))
    .unwrap();
    let manager = Arc::new(ChannelManager::new(cfg.clone(), bus, PathBuf::new()).unwrap());
    let telegram_channel = manager.get_channel("telegram").unwrap();
    let telegram = telegram_channel
        .as_any()
        .downcast_ref::<xbot::channels::TelegramChannel>()
        .unwrap();
    telegram.set_api(Arc::new(DummyTelegramApi)).await;
    manager.start_all().await.unwrap();
    let router = build_webhook_router(&manager, &cfg).unwrap().unwrap();

    let health = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let ready = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);

    let status = router
        .oneshot(
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let body = to_bytes(status.into_body(), 4096).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["channels"]["telegram"]["running"], json!(true));
    assert_eq!(payload["webhooks"], json!(["/telegram/webhook"]));
}

fn slack_signature(secret: &str, timestamp: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("v0:{timestamp}:{body}").as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}
