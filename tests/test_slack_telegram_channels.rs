use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::tempdir;
use xbot::channels::{
    Channel, ReplyParameters, SlackApi, SlackChannel, TelegramApi, TelegramBotIdentity,
    TelegramChannel,
};
use xbot::storage::{MessageBus, OutboundMessage};

#[derive(Default)]
struct FakeSlackApi {
    chat_posts: Mutex<Vec<BTreeMap<String, Value>>>,
    uploads: Mutex<Vec<BTreeMap<String, Value>>>,
    reactions_add: Mutex<Vec<BTreeMap<String, Value>>>,
    reactions_remove: Mutex<Vec<BTreeMap<String, Value>>>,
    downloads: Mutex<BTreeMap<String, Vec<u8>>>,
}

#[async_trait]
impl SlackApi for FakeSlackApi {
    async fn auth_test(&self) -> Result<String> {
        Ok("B123".to_string())
    }

    async fn chat_post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        self.chat_posts.lock().unwrap().push(BTreeMap::from([
            ("channel".to_string(), json!(channel)),
            ("text".to_string(), json!(text)),
            ("thread_ts".to_string(), json!(thread_ts)),
        ]));
        Ok(())
    }

    async fn files_upload(&self, channel: &str, file: &str, thread_ts: Option<&str>) -> Result<()> {
        self.uploads.lock().unwrap().push(BTreeMap::from([
            ("channel".to_string(), json!(channel)),
            ("file".to_string(), json!(file)),
            ("thread_ts".to_string(), json!(thread_ts)),
        ]));
        Ok(())
    }

    async fn download_file(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self
            .downloads
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or_default())
    }

    async fn reactions_add(&self, channel: &str, name: &str, timestamp: &str) -> Result<()> {
        self.reactions_add.lock().unwrap().push(BTreeMap::from([
            ("channel".to_string(), json!(channel)),
            ("name".to_string(), json!(name)),
            ("timestamp".to_string(), json!(timestamp)),
        ]));
        Ok(())
    }

    async fn reactions_remove(&self, channel: &str, name: &str, timestamp: &str) -> Result<()> {
        self.reactions_remove.lock().unwrap().push(BTreeMap::from([
            ("channel".to_string(), json!(channel)),
            ("name".to_string(), json!(name)),
            ("timestamp".to_string(), json!(timestamp)),
        ]));
        Ok(())
    }
}

#[derive(Default)]
struct FakeTelegramApi {
    sent_messages: Mutex<Vec<BTreeMap<String, Value>>>,
    sent_media: Mutex<Vec<BTreeMap<String, Value>>>,
    get_me_calls: Mutex<usize>,
}

#[async_trait]
impl TelegramApi for FakeTelegramApi {
    async fn get_me(&self) -> Result<TelegramBotIdentity> {
        *self.get_me_calls.lock().unwrap() += 1;
        Ok(TelegramBotIdentity {
            id: 999,
            username: "xbot_test".to_string(),
        })
    }

    async fn get_file(&self, _file_id: &str) -> Result<String> {
        Ok("mock/file/path".to_string())
    }

    async fn download_file(&self, _file_path: &str) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.sent_messages.lock().unwrap().push(BTreeMap::from([
            ("chat_id".to_string(), json!(chat_id)),
            ("text".to_string(), json!(text)),
            ("message_thread_id".to_string(), json!(message_thread_id)),
            ("reply_parameters".to_string(), json!(reply_parameters)),
        ]));
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: i64,
        photo: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.sent_media.lock().unwrap().push(BTreeMap::from([
            ("kind".to_string(), json!("photo")),
            ("chat_id".to_string(), json!(chat_id)),
            ("source".to_string(), json!(photo)),
            ("message_thread_id".to_string(), json!(message_thread_id)),
            ("reply_parameters".to_string(), json!(reply_parameters)),
        ]));
        Ok(())
    }

    async fn send_voice(
        &self,
        chat_id: i64,
        voice: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.sent_media.lock().unwrap().push(BTreeMap::from([
            ("kind".to_string(), json!("voice")),
            ("chat_id".to_string(), json!(chat_id)),
            ("source".to_string(), json!(voice)),
            ("message_thread_id".to_string(), json!(message_thread_id)),
            ("reply_parameters".to_string(), json!(reply_parameters)),
        ]));
        Ok(())
    }

    async fn send_audio(
        &self,
        chat_id: i64,
        audio: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.sent_media.lock().unwrap().push(BTreeMap::from([
            ("kind".to_string(), json!("audio")),
            ("chat_id".to_string(), json!(chat_id)),
            ("source".to_string(), json!(audio)),
            ("message_thread_id".to_string(), json!(message_thread_id)),
            ("reply_parameters".to_string(), json!(reply_parameters)),
        ]));
        Ok(())
    }

    async fn send_document(
        &self,
        chat_id: i64,
        document: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.sent_media.lock().unwrap().push(BTreeMap::from([
            ("kind".to_string(), json!("document")),
            ("chat_id".to_string(), json!(chat_id)),
            ("source".to_string(), json!(document)),
            ("message_thread_id".to_string(), json!(message_thread_id)),
            ("reply_parameters".to_string(), json!(reply_parameters)),
        ]));
        Ok(())
    }
}

#[tokio::test]
async fn slack_send_uses_thread_for_channel_messages() {
    let channel = SlackChannel::new(
        json!({"enabled": true, "allowFrom": ["*"], "botToken": "x"}),
        MessageBus::new(8),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeSlackApi::default());
    channel.set_api(api.clone()).await;

    channel
        .send(OutboundMessage {
            channel: "slack".to_string(),
            chat_id: "C123".to_string(),
            content: "hello".to_string(),
            reply_to: None,
            media: vec!["/tmp/demo.txt".to_string()],
            reasoning_content: None,
            metadata: BTreeMap::from([(
                "slack".to_string(),
                json!({"thread_ts": "1700000000.000100", "channel_type": "channel"}),
            )]),
        })
        .await
        .unwrap();

    assert_eq!(
        api.chat_posts.lock().unwrap()[0].get("text"),
        Some(&json!("hello\n"))
    );
    assert_eq!(
        api.chat_posts.lock().unwrap()[0].get("thread_ts"),
        Some(&json!("1700000000.000100"))
    );
    assert_eq!(
        api.uploads.lock().unwrap()[0].get("thread_ts"),
        Some(&json!("1700000000.000100"))
    );
}

#[tokio::test]
async fn slack_send_updates_reactions_for_final_responses() {
    let channel = SlackChannel::new(
        json!({"enabled": true, "allowFrom": ["*"], "botToken": "x", "reactEmoji": "eyes"}),
        MessageBus::new(8),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeSlackApi::default());
    channel.set_api(api.clone()).await;

    channel
        .send(OutboundMessage {
            channel: "slack".to_string(),
            chat_id: "C123".to_string(),
            content: "done".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([(
                "slack".to_string(),
                json!({"event": {"ts": "1700000000.000100"}, "channel_type": "channel"}),
            )]),
        })
        .await
        .unwrap();

    assert_eq!(
        api.reactions_remove.lock().unwrap().as_slice(),
        &[BTreeMap::from([
            ("channel".to_string(), json!("C123")),
            ("name".to_string(), json!("eyes")),
            ("timestamp".to_string(), json!("1700000000.000100")),
        ])]
    );
    assert_eq!(
        api.reactions_add.lock().unwrap().as_slice(),
        &[BTreeMap::from([
            ("channel".to_string(), json!("C123")),
            ("name".to_string(), json!("white_check_mark")),
            ("timestamp".to_string(), json!("1700000000.000100")),
        ])]
    );
}

#[tokio::test]
async fn slack_handle_event_scopes_channel_threads_to_session_key() {
    let bus = MessageBus::new(8);
    let channel = SlackChannel::new(
        json!({"enabled": true, "allowFrom": ["u1"], "botToken": "x"}),
        bus.clone(),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    channel.set_bot_user_id(Some("B123".to_string()));

    channel
        .handle_event(&json!({
            "type": "app_mention",
            "user": "u1",
            "channel": "C123",
            "channel_type": "channel",
            "text": "<@B123> investigate",
            "ts": "1700000000.000100"
        }))
        .await
        .unwrap();

    let inbound = tokio::time::timeout(Duration::from_secs(1), bus.consume_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "investigate");
    assert_eq!(inbound.session_key(), "slack:C123:1700000000.000100");
}

#[tokio::test]
async fn slack_handle_event_skips_unrecognized_image_downloads() {
    let dir = tempdir().unwrap();
    let bus = MessageBus::new(8);
    let channel = SlackChannel::new(
        json!({"enabled": true, "allowFrom": ["u1"], "botToken": "x"}),
        bus.clone(),
        dir.path().to_path_buf(),
        String::new(),
    )
    .unwrap();
    channel.set_bot_user_id(Some("B123".to_string()));
    let api = Arc::new(FakeSlackApi::default());
    api.downloads.lock().unwrap().insert(
        "https://files.slack.com/demo".to_string(),
        b"not really an image".to_vec(),
    );
    channel.set_api(api).await;

    channel
        .handle_event(&json!({
            "type": "app_mention",
            "user": "u1",
            "channel": "C123",
            "channel_type": "channel",
            "text": "<@B123> inspect this image",
            "ts": "1700000000.000100",
            "files": [
                {
                    "url_private": "https://files.slack.com/demo",
                    "name": "photo.png",
                    "mimetype": "image/png"
                }
            ]
        }))
        .await
        .unwrap();

    let inbound = tokio::time::timeout(Duration::from_secs(1), bus.consume_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "inspect this image");
    assert!(inbound.media.is_empty());
}

#[test]
fn telegram_is_allowed_accepts_legacy_id_username_formats() {
    let channel = TelegramChannel::new(
        json!({"allowFrom": ["12345", "alice", "67890|bob"]}),
        MessageBus::new(8),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    assert!(channel.is_allowed("12345|carol"));
    assert!(channel.is_allowed("99999|alice"));
    assert!(channel.is_allowed("67890|bob"));
    assert!(!channel.is_allowed("not-a-number|alice"));
}

#[test]
fn telegram_extract_reply_context_and_topic_key_match_python_behavior() {
    let context = TelegramChannel::extract_reply_context(&json!({
        "reply_to_message": {"text": "Hello world"}
    }));
    assert_eq!(context.as_deref(), Some("[Reply to: Hello world]"));
    assert_eq!(
        TelegramChannel::derive_topic_session_key("-100123", 42),
        "telegram:-100123:topic:42"
    );
}

#[tokio::test]
async fn telegram_send_preserves_topic_and_infers_reply_topic_from_cache() {
    let channel = TelegramChannel::new(
        json!({"enabled": true, "token": "123:abc", "allowFrom": ["*"], "replyToMessage": true}),
        MessageBus::new(8),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeTelegramApi::default());
    channel.set_api(api.clone()).await;
    channel.set_message_thread("123", 10, 42);

    channel
        .send(OutboundMessage {
            channel: "telegram".to_string(),
            chat_id: "123".to_string(),
            content: "hello".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([
                ("_progress".to_string(), json!(true)),
                ("message_thread_id".to_string(), json!(42)),
            ]),
        })
        .await
        .unwrap();
    channel
        .send(OutboundMessage {
            channel: "telegram".to_string(),
            chat_id: "123".to_string(),
            content: "reply".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([("message_id".to_string(), json!(10))]),
        })
        .await
        .unwrap();

    let sent = api.sent_messages.lock().unwrap();
    assert_eq!(sent[0].get("message_thread_id"), Some(&json!(42)));
    assert_eq!(sent[1].get("message_thread_id"), Some(&json!(42)));
    assert_eq!(
        sent[1].get("reply_parameters"),
        Some(&json!({"message_id": 10}))
    );
}

#[tokio::test]
async fn telegram_send_routes_media_and_blocks_unsafe_remote_urls() {
    let dir = tempdir().unwrap();
    let local_photo = dir.path().join("cat.jpg");
    std::fs::write(&local_photo, b"fake").unwrap();

    let channel = TelegramChannel::new(
        json!({"enabled": true, "token": "123:abc", "allowFrom": ["*"]}),
        MessageBus::new(8),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeTelegramApi::default());
    channel.set_api(api.clone()).await;

    channel
        .send(OutboundMessage {
            channel: "telegram".to_string(),
            chat_id: "123".to_string(),
            content: String::new(),
            reply_to: None,
            media: vec![
                local_photo.display().to_string(),
                "http://127.0.0.1/internal.jpg".to_string(),
            ],
            reasoning_content: None,
            metadata: BTreeMap::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        api.sent_media.lock().unwrap()[0].get("kind"),
        Some(&json!("photo"))
    );
    assert_eq!(
        api.sent_messages.lock().unwrap()[0].get("text"),
        Some(&json!("[Failed to send: internal.jpg]"))
    );
}

#[tokio::test]
async fn telegram_group_policy_mention_gates_group_messages() {
    let bus = MessageBus::new(8);
    let channel = TelegramChannel::new(
        json!({"enabled": true, "token": "123:abc", "allowFrom": ["*"], "groupPolicy": "mention"}),
        bus.clone(),
        std::env::temp_dir(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeTelegramApi::default());
    channel.set_api(api.clone()).await;

    channel
        .handle_update(&json!({
            "message": {
                "chat": {"id": -100123, "type": "supergroup"},
                "chat_id": -100123,
                "from": {"id": 12345, "username": "alice"},
                "text": "hello everyone",
                "message_id": 1
            }
        }))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), bus.consume_inbound())
            .await
            .is_err()
    );

    channel
        .handle_update(&json!({
            "message": {
                "chat": {"id": -100123, "type": "supergroup"},
                "chat_id": -100123,
                "from": {"id": 12345, "username": "alice"},
                "text": "@xbot_test hi",
                "message_id": 2
            }
        }))
        .await
        .unwrap();
    let inbound = tokio::time::timeout(Duration::from_secs(1), bus.consume_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "@xbot_test hi");
    assert_eq!(*api.get_me_calls.lock().unwrap(), 1);
}
