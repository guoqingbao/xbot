use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::tempdir;
use xbot::channels::{
    Channel, FeishuApi, FeishuChannel, FeishuMessageDetails, FeishuResource, extract_post_content,
};
use xbot::storage::{MessageBus, OutboundMessage};

#[derive(Default)]
struct FakeFeishuApi {
    send_calls: Mutex<Vec<(String, String, String, String)>>,
    reply_calls: Mutex<Vec<(String, String, String)>>,
    next_reply_ok: Mutex<Vec<bool>>,
    messages: Mutex<BTreeMap<String, FeishuMessageDetails>>,
    image_key: Mutex<Option<String>>,
    file_key: Mutex<Option<String>>,
    reactions: Mutex<Vec<(String, String)>>,
    resources: Mutex<BTreeMap<(String, String, String), FeishuResource>>,
}

#[async_trait]
impl FeishuApi for FakeFeishuApi {
    async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        self.send_calls.lock().unwrap().push((
            receive_id_type.to_string(),
            receive_id.to_string(),
            msg_type.to_string(),
            content.to_string(),
        ));
        Ok(())
    }

    async fn reply_message(
        &self,
        parent_message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        self.reply_calls.lock().unwrap().push((
            parent_message_id.to_string(),
            msg_type.to_string(),
            content.to_string(),
        ));
        let ok = self.next_reply_ok.lock().unwrap().pop().unwrap_or(true);
        if ok {
            Ok(())
        } else {
            Err(anyhow!("reply failed"))
        }
    }

    async fn get_message(&self, message_id: &str) -> Result<Option<FeishuMessageDetails>> {
        Ok(self.messages.lock().unwrap().get(message_id).cloned())
    }

    async fn upload_image(&self, _file_path: &str) -> Result<Option<String>> {
        Ok(self.image_key.lock().unwrap().clone())
    }

    async fn upload_file(&self, _file_path: &str) -> Result<Option<String>> {
        Ok(self.file_key.lock().unwrap().clone())
    }

    async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<()> {
        self.reactions
            .lock()
            .unwrap()
            .push((message_id.to_string(), emoji_type.to_string()));
        Ok(())
    }

    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Option<FeishuResource>> {
        Ok(self
            .resources
            .lock()
            .unwrap()
            .get(&(
                message_id.to_string(),
                file_key.to_string(),
                resource_type.to_string(),
            ))
            .cloned())
    }
}

#[test]
fn parse_md_table_strips_markdown_formatting_in_headers_and_cells() {
    let table = FeishuChannel::parse_md_table(
        r#"
| **Name** | __Status__ | *Notes* | ~~State~~ |
| --- | --- | --- | --- |
| **Alice** | __Ready__ | *Fast* | ~~Old~~ |
"#,
    )
    .unwrap();

    assert_eq!(
        table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|col| col["display_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Name", "Status", "Notes", "State"]
    );
    assert_eq!(
        table["rows"],
        json!([{"c0": "Alice", "c1": "Ready", "c2": "Fast", "c3": "Old"}])
    );
}

#[test]
fn split_headings_strips_embedded_markdown_before_bolding() {
    let channel = FeishuChannel::new(
        FeishuChannel::default_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap();
    let elements = channel.split_headings("# **Important** *status* ~~update~~");
    assert_eq!(
        elements,
        vec![json!({
            "tag": "div",
            "text": {"tag": "lark_md", "content": "**Important status update**"}
        })]
    );
}

#[test]
fn split_headings_keeps_markdown_body_and_code_blocks_intact() {
    let channel = FeishuChannel::new(
        FeishuChannel::default_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap();
    let elements = channel
        .split_headings("# **Heading**\n\nBody with **bold** text.\n\n```python\nprint('hi')\n```");
    assert_eq!(
        elements[0],
        json!({"tag": "div", "text": {"tag": "lark_md", "content": "**Heading**"}})
    );
    assert_eq!(elements[1]["tag"], json!("markdown"));
    let body = elements[1]["content"].as_str().unwrap();
    assert!(body.contains("Body with **bold** text."));
    assert!(body.contains("```python\nprint('hi')\n```"));
}

#[test]
fn extract_post_content_supports_wrapped_and_direct_shapes() {
    let wrapped = json!({
        "post": {"zh_cn": {"title": "日报", "content": [[{"tag":"text","text":"完成"},{"tag":"img","image_key":"img_1"}]]}}
    });
    let direct = json!({
        "title": "Daily",
        "content": [[{"tag":"text","text":"report"},{"tag":"img","image_key":"img_a"},{"tag":"img","image_key":"img_b"}]]
    });
    assert_eq!(
        extract_post_content(&wrapped),
        ("日报 完成".to_string(), vec!["img_1".to_string()])
    );
    assert_eq!(
        extract_post_content(&direct),
        (
            "Daily report".to_string(),
            vec!["img_a".to_string(), "img_b".to_string()]
        )
    );
}

#[test]
fn split_elements_by_table_limit_matches_python_behavior() {
    let md = |text: &str| json!({"tag":"markdown","content": text});
    let t1 = json!({"tag":"table","columns":[],"rows":[{"c0":"one"}],"page_size":1});
    let t2 = json!({"tag":"table","columns":[],"rows":[{"c0":"two"}],"page_size":1});
    let result = FeishuChannel::split_elements_by_table_limit(
        vec![
            md("before"),
            t1.clone(),
            md("between"),
            t2.clone(),
            md("after"),
        ],
        1,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].contains(&t1));
    assert!(!result[0].contains(&t2));
    assert!(result[1].contains(&t2));
}

#[test]
fn format_tool_hint_lines_keeps_commas_inside_arguments() {
    let formatted = FeishuChannel::format_tool_hint_lines(
        r#"web_search("foo, bar"), read_file("/path/to/file")"#,
    );
    assert_eq!(
        formatted,
        "web_search(\"foo, bar\"),\nread_file(\"/path/to/file\")"
    );
}

#[tokio::test]
async fn tool_hint_sends_interactive_code_card() {
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"]}),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    channel.set_api(api.clone()).await;

    channel
        .send(OutboundMessage {
            channel: "feishu".to_string(),
            chat_id: "oc_123456".to_string(),
            content: r#"[ 🔍 web_search  query="test query" ]"#.to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([("_tool_hint".to_string(), json!(true))]),
        })
        .await
        .unwrap();

    let calls = api.send_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "chat_id");
    assert_eq!(calls[0].1, "oc_123456");
    assert_eq!(calls[0].2, "interactive");
    let card: Value = serde_json::from_str(&calls[0].3).unwrap();
    assert_eq!(
        card["elements"][0]["content"],
        json!("**Tool Calls**\n\n```text\nweb_search  query=\"test query\"\n```")
    );
}

#[tokio::test]
async fn send_uses_reply_api_when_configured_and_falls_back_to_create() {
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"], "replyToMessage": true}),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    api.next_reply_ok.lock().unwrap().push(false);
    channel.set_api(api.clone()).await;

    channel
        .send(OutboundMessage {
            channel: "feishu".to_string(),
            chat_id: "oc_abc".to_string(),
            content: "hello".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([("message_id".to_string(), json!("om_001"))]),
        })
        .await
        .unwrap();

    assert_eq!(api.reply_calls.lock().unwrap().len(), 1);
    assert_eq!(api.send_calls.lock().unwrap().len(), 1);
    assert_eq!(api.send_calls.lock().unwrap()[0].2, "text");
}

#[tokio::test]
async fn send_uses_expected_feishu_msg_type_for_uploaded_files() {
    let dir = tempdir().unwrap();
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"]}),
        MessageBus::new(8),
        dir.path().to_path_buf(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    *api.file_key.lock().unwrap() = Some("file-key".to_string());
    channel.set_api(api.clone()).await;

    for (name, expected) in [
        ("voice.opus", "audio"),
        ("clip.mp4", "video"),
        ("report.pdf", "file"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"demo").unwrap();
        channel
            .send(OutboundMessage {
                channel: "feishu".to_string(),
                chat_id: "oc_test".to_string(),
                content: String::new(),
                reply_to: None,
                media: vec![path.display().to_string()],
                reasoning_content: None,
                metadata: BTreeMap::new(),
            })
            .await
            .unwrap();
        let last = api.send_calls.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.2, expected);
        assert_eq!(
            serde_json::from_str::<Value>(&last.3).unwrap(),
            json!({"file_key":"file-key"})
        );
    }
}

#[tokio::test]
async fn get_message_content_returns_reply_prefix_and_truncates() {
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"]}),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    api.messages.lock().unwrap().insert(
        "om_parent".to_string(),
        FeishuMessageDetails {
            msg_type: "text".to_string(),
            content: json!({"text": "what time is it?"}).to_string(),
        },
    );
    api.messages.lock().unwrap().insert(
        "om_long".to_string(),
        FeishuMessageDetails {
            msg_type: "text".to_string(),
            content: json!({"text": "x".repeat(FeishuChannel::REPLY_CONTEXT_MAX_LEN + 50)})
                .to_string(),
        },
    );
    channel.set_api(api).await;

    assert_eq!(
        channel.get_message_content("om_parent").await.as_deref(),
        Some("[Reply to: what time is it?]")
    );
    let long = channel.get_message_content("om_long").await.unwrap();
    assert!(long.ends_with("...]"));
}

#[tokio::test]
async fn handle_event_downloads_inbound_image_and_adds_reaction() {
    let bus = MessageBus::new(8);
    let dir = tempdir().unwrap();
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"], "reactEmoji": "THUMBSUP"}),
        bus.clone(),
        dir.path().to_path_buf(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    api.resources.lock().unwrap().insert(
        (
            "om_img".to_string(),
            "img_key".to_string(),
            "image".to_string(),
        ),
        FeishuResource {
            bytes: b"image-bytes".to_vec(),
            file_name: Some("photo.jpg".to_string()),
        },
    );
    channel.set_api(api.clone()).await;

    channel
        .handle_event(&json!({
            "event": {
                "message": {
                    "message_id": "om_img",
                    "chat_id": "ou_chat",
                    "chat_type": "p2p",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_key\"}"
                },
                "sender": {
                    "sender_type": "user",
                    "sender_id": {"open_id": "ou_alice"}
                }
            }
        }))
        .await
        .unwrap();

    let inbound = bus.consume_inbound().await.unwrap();
    assert_eq!(inbound.channel, "feishu");
    assert_eq!(inbound.media.len(), 1);
    assert!(std::path::Path::new(&inbound.media[0]).is_file());
    assert!(inbound.content.contains("[image: photo.jpg]"));
    assert_eq!(
        api.reactions.lock().unwrap().as_slice(),
        &[("om_img".to_string(), "THUMBSUP".to_string())]
    );
}

#[tokio::test]
async fn handle_event_extracts_interactive_card_text_and_downloads_post_images() {
    let bus = MessageBus::new(8);
    let dir = tempdir().unwrap();
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"]}),
        bus.clone(),
        dir.path().to_path_buf(),
        String::new(),
    )
    .unwrap();
    let api = Arc::new(FakeFeishuApi::default());
    api.resources.lock().unwrap().insert(
        (
            "om_post".to_string(),
            "img_1".to_string(),
            "image".to_string(),
        ),
        FeishuResource {
            bytes: b"post-image".to_vec(),
            file_name: Some("post.png".to_string()),
        },
    );
    channel.set_api(api).await;

    channel
        .handle_event(&json!({
            "event": {
                "message": {
                    "message_id": "om_post",
                    "chat_id": "oc_group",
                    "chat_type": "group",
                    "message_type": "post",
                    "mentions": [{"name": "bot"}],
                    "content": serde_json::to_string(&json!({
                        "post": {
                            "zh_cn": {
                                "title": "日报",
                                "content": [[
                                    {"tag":"text","text":"完成"},
                                    {"tag":"img","image_key":"img_1"}
                                ]]
                            }
                        }
                    })).unwrap()
                },
                "sender": {
                    "sender_type": "user",
                    "sender_id": {"open_id": "ou_alice"}
                }
            }
        }))
        .await
        .unwrap();

    let inbound = bus.consume_inbound().await.unwrap();
    assert_eq!(inbound.chat_id, "oc_group");
    assert!(inbound.content.contains("日报 完成"));
    assert!(inbound.content.contains("[image: post.png]"));
    assert_eq!(inbound.media.len(), 1);
}

#[tokio::test]
async fn handle_event_extracts_interactive_share_card_text() {
    let bus = MessageBus::new(8);
    let dir = tempdir().unwrap();
    let channel = FeishuChannel::new(
        json!({"enabled": true, "appId": "id", "appSecret": "secret", "allowFrom": ["*"]}),
        bus.clone(),
        dir.path().to_path_buf(),
        String::new(),
    )
    .unwrap();
    channel.set_api(Arc::new(FakeFeishuApi::default())).await;

    channel
        .handle_event(&json!({
            "event": {
                "message": {
                    "message_id": "om_card",
                    "chat_id": "ou_chat",
                    "chat_type": "p2p",
                    "message_type": "interactive",
                    "content": serde_json::to_string(&json!({
                        "header": {"title": {"content": "Deploy Status"}},
                        "elements": [
                            {"tag":"markdown","content":"**Build** ok"},
                            {"tag":"button","text":{"content":"Open"},"url":"https://example.com"}
                        ]
                    })).unwrap()
                },
                "sender": {
                    "sender_type": "user",
                    "sender_id": {"open_id": "ou_alice"}
                }
            }
        }))
        .await
        .unwrap();

    let inbound = bus.consume_inbound().await.unwrap();
    assert!(inbound.content.contains("title: Deploy Status"));
    assert!(inbound.content.contains("**Build** ok"));
    assert!(inbound.content.contains("Open"));
    assert!(inbound.content.contains("link: https://example.com"));
}
