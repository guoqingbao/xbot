use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use reqwest::header::CONTENT_DISPOSITION;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::{safe_filename, workspace_state_dir};

const FEISHU_BASE_URL: &str = "https://open.feishu.cn";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeishuConfig {
    pub enabled: bool,
    #[serde(alias = "appId")]
    pub app_id: String,
    #[serde(alias = "appSecret")]
    pub app_secret: String,
    #[serde(alias = "encryptKey")]
    pub encrypt_key: String,
    #[serde(alias = "verificationToken")]
    pub verification_token: String,
    #[serde(alias = "webhookPath")]
    pub webhook_path: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "reactEmoji")]
    pub react_emoji: String,
    #[serde(alias = "groupPolicy")]
    pub group_policy: String,
    #[serde(alias = "replyToMessage")]
    pub reply_to_message: bool,
}

impl Default for FeishuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: String::new(),
            encrypt_key: String::new(),
            verification_token: String::new(),
            webhook_path: "/feishu/events".to_string(),
            allow_from: Vec::new(),
            react_emoji: "THUMBSUP".to_string(),
            group_policy: "mention".to_string(),
            reply_to_message: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuMessageDetails {
    pub msg_type: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuResource {
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

#[async_trait]
pub trait FeishuApi: Send + Sync {
    async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()>;
    async fn reply_message(
        &self,
        parent_message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()>;
    async fn get_message(&self, message_id: &str) -> Result<Option<FeishuMessageDetails>>;
    async fn upload_image(&self, file_path: &str) -> Result<Option<String>>;
    async fn upload_file(&self, file_path: &str) -> Result<Option<String>>;
    async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<()>;
    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Option<FeishuResource>>;
}

struct TokenState {
    token: String,
    expires_at: Instant,
}

pub struct ReqwestFeishuApi {
    client: Client,
    app_id: String,
    app_secret: String,
    token_state: AsyncMutex<Option<TokenState>>,
}

impl ReqwestFeishuApi {
    pub fn new(app_id: String, app_secret: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(60)).build()?,
            app_id,
            app_secret,
            token_state: AsyncMutex::new(None),
        })
    }

    async fn tenant_access_token(&self) -> Result<String> {
        if let Some(state) = self.token_state.lock().await.as_ref() {
            if Instant::now() < state.expires_at {
                return Ok(state.token.clone());
            }
        }
        let payload: Value = self
            .client
            .post(format!(
                "{FEISHU_BASE_URL}/open-apis/auth/v3/tenant_access_token/internal"
            ))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?
            .json()
            .await?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "feishu auth error: {}",
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ));
        }
        let token = payload
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing tenant_access_token in feishu auth response"))?
            .to_string();
        let expires_in = payload
            .get("expire")
            .and_then(Value::as_u64)
            .unwrap_or(7200)
            .saturating_sub(60);
        *self.token_state.lock().await = Some(TokenState {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        });
        Ok(token)
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let token = self.tenant_access_token().await?;
        let payload: Value = self
            .client
            .post(format!("{FEISHU_BASE_URL}{path}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            Ok(payload)
        } else {
            Err(anyhow!(
                "feishu api error: {}",
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }

    async fn post_multipart(&self, path: &str, form: reqwest::multipart::Form) -> Result<Value> {
        let token = self.tenant_access_token().await?;
        let payload: Value = self
            .client
            .post(format!("{FEISHU_BASE_URL}{path}"))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            Ok(payload)
        } else {
            Err(anyhow!(
                "feishu api error: {}",
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }
}

#[async_trait]
impl FeishuApi for ReqwestFeishuApi {
    async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        self.post_json(
            &format!("/open-apis/im/v1/messages?receive_id_type={receive_id_type}"),
            json!({
                "receive_id": receive_id,
                "msg_type": msg_type,
                "content": content,
            }),
        )
        .await?;
        Ok(())
    }

    async fn reply_message(
        &self,
        parent_message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        self.post_json(
            &format!("/open-apis/im/v1/messages/{parent_message_id}/reply"),
            json!({
                "msg_type": msg_type,
                "content": content,
            }),
        )
        .await?;
        Ok(())
    }

    async fn get_message(&self, message_id: &str) -> Result<Option<FeishuMessageDetails>> {
        let token = self.tenant_access_token().await?;
        let payload: Value = self
            .client
            .get(format!(
                "{FEISHU_BASE_URL}/open-apis/im/v1/messages/{message_id}"
            ))
            .bearer_auth(token)
            .send()
            .await?
            .json()
            .await?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            return Ok(None);
        }
        let item = payload
            .get("data")
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .or_else(|| payload.get("data"));
        let Some(item) = item else {
            return Ok(None);
        };
        let msg_type = item
            .get("msg_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let content = item
            .get("body")
            .and_then(|body| body.get("content"))
            .and_then(Value::as_str)
            .or_else(|| item.get("content").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        Ok(Some(FeishuMessageDetails { msg_type, content }))
    }

    async fn upload_image(&self, file_path: &str) -> Result<Option<String>> {
        let bytes = std::fs::read(file_path)?;
        let filename = Path::new(file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_string();
        let payload = self
            .post_multipart(
                "/open-apis/im/v1/images",
                reqwest::multipart::Form::new()
                    .text("image_type", "message")
                    .part(
                        "image",
                        reqwest::multipart::Part::bytes(bytes).file_name(filename),
                    ),
            )
            .await?;
        Ok(payload
            .get("data")
            .and_then(|data| data.get("image_key"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    async fn upload_file(&self, file_path: &str) -> Result<Option<String>> {
        let ext = Path::new(file_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
            .unwrap_or_default();
        let file_type = match ext.as_str() {
            ".opus" => "opus",
            ".mp4" => "mp4",
            ".pdf" => "pdf",
            ".doc" | ".docx" => "doc",
            ".xls" | ".xlsx" => "xls",
            ".ppt" | ".pptx" => "ppt",
            _ => "stream",
        };
        let bytes = std::fs::read(file_path)?;
        let filename = Path::new(file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        let payload = self
            .post_multipart(
                "/open-apis/im/v1/files",
                reqwest::multipart::Form::new()
                    .text("file_type", file_type.to_string())
                    .text("file_name", filename.clone())
                    .part(
                        "file",
                        reqwest::multipart::Part::bytes(bytes).file_name(filename),
                    ),
            )
            .await?;
        Ok(payload
            .get("data")
            .and_then(|data| data.get("file_key"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<()> {
        self.post_json(
            &format!("/open-apis/im/v1/messages/{message_id}/reactions"),
            json!({
                "reaction_type": {"emoji_type": emoji_type}
            }),
        )
        .await?;
        Ok(())
    }

    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Option<FeishuResource>> {
        let token = self.tenant_access_token().await?;
        let api_type = if resource_type == "image" {
            "image"
        } else {
            "file"
        };
        let response = self
            .client
            .get(format!(
                "{FEISHU_BASE_URL}/open-apis/im/v1/messages/{message_id}/resources/{file_key}"
            ))
            .query(&[("type", api_type)])
            .bearer_auth(token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let file_name = response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_disposition_filename);
        let bytes = response.bytes().await?.to_vec();
        Ok(Some(FeishuResource { bytes, file_name }))
    }
}

pub fn extract_post_content(content_json: &Value) -> (String, Vec<String>) {
    fn parse_block(block: &Value) -> (Option<String>, Vec<String>) {
        let Some(rows) = block.get("content").and_then(Value::as_array) else {
            return (None, Vec::new());
        };
        let mut texts = Vec::new();
        let mut images = Vec::new();
        if let Some(title) = block.get("title").and_then(Value::as_str) {
            if !title.is_empty() {
                texts.push(title.to_string());
            }
        }
        for row in rows {
            let Some(row) = row.as_array() else {
                continue;
            };
            for el in row {
                match el.get("tag").and_then(Value::as_str).unwrap_or_default() {
                    "text" | "a" => {
                        if let Some(text) = el.get("text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        }
                    }
                    "at" => {
                        let name = el
                            .get("user_name")
                            .and_then(Value::as_str)
                            .unwrap_or("user");
                        texts.push(format!("@{name}"));
                    }
                    "code_block" => {
                        let lang = el.get("language").and_then(Value::as_str).unwrap_or("");
                        let code = el.get("text").and_then(Value::as_str).unwrap_or("");
                        texts.push(format!("\n```{lang}\n{code}\n```\n"));
                    }
                    "img" => {
                        if let Some(key) = el.get("image_key").and_then(Value::as_str) {
                            images.push(key.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        let text = texts.join(" ").trim().to_string();
        ((!text.is_empty()).then_some(text), images)
    }

    let root = content_json.get("post").unwrap_or(content_json);
    if root.get("content").is_some() {
        let (text, images) = parse_block(root);
        if text.is_some() || !images.is_empty() {
            return (text.unwrap_or_default(), images);
        }
    }
    for locale in ["zh_cn", "en_us", "ja_jp"] {
        if let Some(block) = root.get(locale) {
            let (text, images) = parse_block(block);
            if text.is_some() || !images.is_empty() {
                return (text.unwrap_or_default(), images);
            }
        }
    }
    if let Some(map) = root.as_object() {
        for value in map.values() {
            if value.is_object() {
                let (text, images) = parse_block(value);
                if text.is_some() || !images.is_empty() {
                    return (text.unwrap_or_default(), images);
                }
            }
        }
    }
    (String::new(), Vec::new())
}

fn parse_content_disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|part| {
        let part = part.trim();
        let file_name = part
            .strip_prefix("filename=")
            .or_else(|| part.strip_prefix("filename*="))?;
        let file_name = file_name
            .trim_matches('"')
            .rsplit("''")
            .next()
            .unwrap_or(file_name);
        (!file_name.is_empty()).then(|| file_name.to_string())
    })
}

fn extract_interactive_content(content: &Value) -> Vec<String> {
    if let Some(text) = content.as_str() {
        return if text.trim().is_empty() {
            Vec::new()
        } else if let Ok(json) = serde_json::from_str::<Value>(text) {
            extract_interactive_content(&json)
        } else {
            vec![text.to_string()]
        };
    }
    let Some(content) = content.as_object() else {
        return Vec::new();
    };

    let mut parts = Vec::new();
    if let Some(title) = content.get("title") {
        match title {
            Value::Object(map) => {
                let text = map
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| map.get("text").and_then(Value::as_str))
                    .unwrap_or_default();
                if !text.is_empty() {
                    parts.push(format!("title: {text}"));
                }
            }
            Value::String(text) if !text.is_empty() => parts.push(format!("title: {text}")),
            _ => {}
        }
    }

    if let Some(elements) = content.get("elements").and_then(Value::as_array) {
        for item in elements {
            if let Some(group) = item.as_array() {
                for element in group {
                    parts.extend(extract_element_content(element));
                }
            } else {
                parts.extend(extract_element_content(item));
            }
        }
    }

    if let Some(card) = content.get("card") {
        parts.extend(extract_interactive_content(card));
    }

    if let Some(header) = content.get("header").and_then(Value::as_object) {
        if let Some(title) = header.get("title").and_then(Value::as_object) {
            let text = title
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| title.get("text").and_then(Value::as_str))
                .unwrap_or_default();
            if !text.is_empty() {
                parts.push(format!("title: {text}"));
            }
        }
    }

    parts
}

fn extract_element_content(element: &Value) -> Vec<String> {
    let Some(element) = element.as_object() else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    match element
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "markdown" | "lark_md" => {
            if let Some(content) = element.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    parts.push(content.to_string());
                }
            }
        }
        "div" => {
            if let Some(text) = element.get("text") {
                match text {
                    Value::Object(map) => {
                        let content = map
                            .get("content")
                            .and_then(Value::as_str)
                            .or_else(|| map.get("text").and_then(Value::as_str))
                            .unwrap_or_default();
                        if !content.is_empty() {
                            parts.push(content.to_string());
                        }
                    }
                    Value::String(text) if !text.is_empty() => parts.push(text.to_string()),
                    _ => {}
                }
            }
            if let Some(fields) = element.get("fields").and_then(Value::as_array) {
                for field in fields {
                    if let Some(text) = field.get("text").and_then(Value::as_object) {
                        if let Some(content) = text.get("content").and_then(Value::as_str) {
                            if !content.is_empty() {
                                parts.push(content.to_string());
                            }
                        }
                    }
                }
            }
        }
        "a" => {
            if let Some(href) = element.get("href").and_then(Value::as_str) {
                if !href.is_empty() {
                    parts.push(format!("link: {href}"));
                }
            }
            if let Some(text) = element.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
        "button" => {
            if let Some(text) = element.get("text").and_then(Value::as_object) {
                if let Some(content) = text.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        parts.push(content.to_string());
                    }
                }
            }
            let url = element
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| {
                    element
                        .get("multi_url")
                        .and_then(Value::as_object)
                        .and_then(|url| url.get("url"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            if !url.is_empty() {
                parts.push(format!("link: {url}"));
            }
        }
        "img" => {
            let alt = element
                .get("alt")
                .and_then(Value::as_object)
                .and_then(|alt| alt.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("[image]");
            parts.push(alt.to_string());
        }
        "note" => {
            if let Some(elements) = element.get("elements").and_then(Value::as_array) {
                for item in elements {
                    parts.extend(extract_element_content(item));
                }
            }
        }
        "column_set" => {
            if let Some(columns) = element.get("columns").and_then(Value::as_array) {
                for column in columns {
                    if let Some(elements) = column.get("elements").and_then(Value::as_array) {
                        for item in elements {
                            parts.extend(extract_element_content(item));
                        }
                    }
                }
            }
        }
        "plain_text" => {
            if let Some(content) = element.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    parts.push(content.to_string());
                }
            }
        }
        _ => {
            if let Some(elements) = element.get("elements").and_then(Value::as_array) {
                for item in elements {
                    parts.extend(extract_element_content(item));
                }
            }
        }
    }
    parts
}

fn extract_share_card_content(content_json: &Value, msg_type: &str) -> String {
    let mut parts = Vec::new();
    match msg_type {
        "share_chat" => {
            if let Some(chat_id) = content_json.get("chat_id").and_then(Value::as_str) {
                parts.push(format!("[shared chat: {chat_id}]"));
            }
        }
        "share_user" => {
            if let Some(user_id) = content_json.get("user_id").and_then(Value::as_str) {
                parts.push(format!("[shared user: {user_id}]"));
            }
        }
        "interactive" => parts.extend(extract_interactive_content(content_json)),
        "share_calendar_event" => {
            if let Some(event_key) = content_json.get("event_key").and_then(Value::as_str) {
                parts.push(format!("[shared calendar event: {event_key}]"));
            }
        }
        "system" => parts.push("[system message]".to_string()),
        "merge_forward" => parts.push("[merged forward messages]".to_string()),
        _ => {}
    }
    if parts.is_empty() {
        format!("[{msg_type}]")
    } else {
        parts.join("\n")
    }
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^(#{1,6})\s+(.+)$").expect("valid heading regex"))
}

fn code_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?ms)(```[\s\S]*?```)").expect("valid code block regex"))
}

fn complex_md_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)```|^\|.+\|.*\n\s*\|[-:\s|]+\||^#{1,6}\s+")
            .expect("valid complex markdown regex")
    })
}

fn simple_md_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)\*\*.+?\*\*|__.+?__|\*[^*\n]+\*|~~.+?~~")
            .expect("valid simple markdown regex")
    })
}

fn list_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[\s]*[-*+]\s+").expect("valid list regex"))
}

fn ordered_list_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[\s]*\d+\.\s+").expect("valid ordered list regex"))
}

fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[([^\]]+)\]\((https?://[^\)]+)\)").expect("valid md link regex")
    })
}

fn table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)((?:^[ \t]*\|.+\|[ \t]*\n)(?:^[ \t]*\|[-:\s|]+\|[ \t]*\n)(?:^[ \t]*\|.+\|[ \t]*\n?)+)",
        )
        .expect("valid table regex")
    })
}

fn bold_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").expect("valid bold regex"))
}

fn bold_underscore_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"__(.+?)__").expect("valid bold underscore regex"))
}

fn italic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*([^*\n]+)\*").expect("valid italic regex"))
}

fn strike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"~~(.+?)~~").expect("valid strike regex"))
}

pub struct FeishuChannel {
    base: ChannelBase,
    config: FeishuConfig,
    api: AsyncMutex<Option<Arc<dyn FeishuApi>>>,
    _processed_message_ids: Mutex<VecDeque<String>>,
}

impl FeishuChannel {
    const TEXT_MAX_LEN: usize = 200;
    const POST_MAX_LEN: usize = 2000;
    pub const REPLY_CONTEXT_MAX_LEN: usize = 200;
    const MAX_PROCESSED_MESSAGE_IDS: usize = 1000;
    const IMAGE_EXTS: &'static [&'static str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".ico", ".tiff", ".tif",
    ];
    const AUDIO_EXTS: &'static [&'static str] = &[".opus"];
    const VIDEO_EXTS: &'static [&'static str] = &[".mp4", ".mov", ".avi"];

    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: FeishuConfig = serde_json::from_value(config)?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            api: AsyncMutex::new(None),
            _processed_message_ids: Mutex::new(VecDeque::new()),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(FeishuConfig::default()).expect("serializable feishu config")
    }

    pub async fn set_api(&self, api: Arc<dyn FeishuApi>) {
        *self.api.lock().await = Some(api);
    }

    async fn api(&self) -> Result<Arc<dyn FeishuApi>> {
        if let Some(api) = self.api.lock().await.clone() {
            return Ok(api);
        }
        if self.config.app_id.trim().is_empty() || self.config.app_secret.trim().is_empty() {
            return Err(anyhow!("feishu app_id and app_secret are not configured"));
        }
        let api: Arc<dyn FeishuApi> = Arc::new(ReqwestFeishuApi::new(
            self.config.app_id.clone(),
            self.config.app_secret.clone(),
        )?);
        *self.api.lock().await = Some(api.clone());
        Ok(api)
    }

    pub fn strip_md_formatting(text: &str) -> String {
        let text = bold_re().replace_all(text, "$1");
        let text = bold_underscore_re().replace_all(&text, "$1");
        let text = italic_re().replace_all(&text, "$1");
        strike_re().replace_all(&text, "$1").to_string()
    }

    pub fn parse_md_table(table_text: &str) -> Option<Value> {
        let lines = table_text
            .trim()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if lines.len() < 3 {
            return None;
        }
        let split = |line: &str| -> Vec<String> {
            line.trim_matches('|')
                .split('|')
                .map(|cell| Self::strip_md_formatting(cell.trim()))
                .collect()
        };
        let headers = split(lines[0]);
        let rows = lines[2..]
            .iter()
            .map(|line| split(line))
            .collect::<Vec<_>>();
        Some(json!({
            "tag": "table",
            "page_size": rows.len() + 1,
            "columns": headers.iter().enumerate().map(|(idx, header)| {
                json!({"tag": "column", "name": format!("c{idx}"), "display_name": header, "width": "auto"})
            }).collect::<Vec<_>>(),
            "rows": rows.iter().map(|row| {
                let mut out = serde_json::Map::new();
                for (idx, header) in headers.iter().enumerate() {
                    let _ = header;
                    out.insert(format!("c{idx}"), Value::String(row.get(idx).cloned().unwrap_or_default()));
                }
                Value::Object(out)
            }).collect::<Vec<_>>(),
        }))
    }

    pub fn split_elements_by_table_limit(
        elements: Vec<Value>,
        max_tables: usize,
    ) -> Vec<Vec<Value>> {
        if elements.is_empty() {
            return vec![Vec::new()];
        }
        let mut groups = Vec::new();
        let mut current = Vec::new();
        let mut table_count = 0;
        for el in elements {
            if el.get("tag").and_then(Value::as_str) == Some("table") {
                if table_count >= max_tables {
                    if !current.is_empty() {
                        groups.push(current);
                    }
                    current = Vec::new();
                    table_count = 0;
                }
                current.push(el);
                table_count += 1;
            } else {
                current.push(el);
            }
        }
        if !current.is_empty() {
            groups.push(current);
        }
        if groups.is_empty() {
            vec![Vec::new()]
        } else {
            groups
        }
    }

    pub fn split_headings(&self, content: &str) -> Vec<Value> {
        let mut protected = content.to_string();
        let code_blocks = code_block_re()
            .find_iter(content)
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>();
        for (idx, block) in code_blocks.iter().enumerate() {
            protected = protected.replacen(block, &format!("\0CODE{idx}\0"), 1);
        }

        let mut elements = Vec::new();
        let mut last_end = 0;
        for m in heading_re().captures_iter(&protected) {
            let full = m.get(0).expect("full heading match");
            let before = protected[last_end..full.start()].trim();
            if !before.is_empty() {
                elements.push(json!({"tag": "markdown", "content": before}));
            }
            let text = Self::strip_md_formatting(m.get(2).map(|g| g.as_str()).unwrap_or("").trim());
            elements.push(json!({
                "tag": "div",
                "text": {"tag": "lark_md", "content": if text.is_empty() { "".to_string() } else { format!("**{text}**") }},
            }));
            last_end = full.end();
        }
        let remaining = protected[last_end..].trim();
        if !remaining.is_empty() {
            elements.push(json!({"tag": "markdown", "content": remaining}));
        }
        if elements.is_empty() {
            elements.push(json!({"tag": "markdown", "content": content}));
        }
        for (idx, block) in code_blocks.iter().enumerate() {
            for el in &mut elements {
                if el.get("tag").and_then(Value::as_str) == Some("markdown") {
                    if let Some(text) = el.get("content").and_then(Value::as_str) {
                        *el = json!({"tag": "markdown", "content": text.replace(&format!("\0CODE{idx}\0"), block)});
                    }
                }
            }
        }
        elements
    }

    pub fn build_card_elements(&self, content: &str) -> Vec<Value> {
        let mut elements = Vec::new();
        let mut last_end = 0;
        for m in table_re().find_iter(content) {
            let before = &content[last_end..m.start()];
            if !before.trim().is_empty() {
                elements.extend(self.split_headings(before));
            }
            elements.push(
                Self::parse_md_table(m.as_str())
                    .unwrap_or_else(|| json!({"tag": "markdown", "content": m.as_str()})),
            );
            last_end = m.end();
        }
        let remaining = &content[last_end..];
        if !remaining.trim().is_empty() {
            elements.extend(self.split_headings(remaining));
        }
        if elements.is_empty() {
            vec![json!({"tag": "markdown", "content": content})]
        } else {
            elements
        }
    }

    pub fn detect_msg_format(content: &str) -> &'static str {
        let stripped = content.trim();
        if complex_md_re().is_match(stripped) {
            return "interactive";
        }
        if stripped.len() > Self::POST_MAX_LEN {
            return "interactive";
        }
        if simple_md_re().is_match(stripped) {
            return "interactive";
        }
        if list_re().is_match(stripped) || ordered_list_re().is_match(stripped) {
            return "interactive";
        }
        if md_link_re().is_match(stripped) {
            return "post";
        }
        if stripped.len() <= Self::TEXT_MAX_LEN {
            return "text";
        }
        "post"
    }

    pub fn markdown_to_post(content: &str) -> String {
        let mut paragraphs = Vec::new();
        for line in content.trim().split('\n') {
            let mut elements = Vec::new();
            let mut last_end = 0;
            for m in md_link_re().captures_iter(line) {
                let full = m.get(0).expect("full md link");
                let before = &line[last_end..full.start()];
                if !before.is_empty() {
                    elements.push(json!({"tag": "text", "text": before}));
                }
                elements.push(json!({
                    "tag": "a",
                    "text": m.get(1).map(|g| g.as_str()).unwrap_or_default(),
                    "href": m.get(2).map(|g| g.as_str()).unwrap_or_default(),
                }));
                last_end = full.end();
            }
            let remaining = &line[last_end..];
            if !remaining.is_empty() {
                elements.push(json!({"tag": "text", "text": remaining}));
            }
            if elements.is_empty() {
                elements.push(json!({"tag": "text", "text": ""}));
            }
            paragraphs.push(Value::Array(elements));
        }
        json!({"zh_cn": {"content": paragraphs}}).to_string()
    }

    pub async fn get_message_content(&self, message_id: &str) -> Option<String> {
        let Ok(api) = self.api().await else {
            return None;
        };
        let Ok(Some(details)) = api.get_message(message_id).await else {
            return None;
        };
        let content_json = serde_json::from_str::<Value>(&details.content).ok()?;
        let text = match details.msg_type.as_str() {
            "text" => content_json
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            "post" => extract_post_content(&content_json).0.trim().to_string(),
            _ => String::new(),
        };
        if text.is_empty() {
            return None;
        }
        let text = if text.chars().count() > Self::REPLY_CONTEXT_MAX_LEN {
            format!(
                "{}...",
                text.chars()
                    .take(Self::REPLY_CONTEXT_MAX_LEN)
                    .collect::<String>()
            )
        } else {
            text
        };
        Some(format!("[Reply to: {text}]"))
    }

    fn media_dir(&self) -> Result<std::path::PathBuf> {
        let dir = workspace_state_dir(&self.base.workspace).join("downloads");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    async fn add_reaction(&self, message_id: &str, emoji_type: &str) {
        if emoji_type.trim().is_empty() {
            return;
        }
        let Ok(api) = self.api().await else {
            return;
        };
        let _ = api.add_reaction(message_id, emoji_type).await;
    }

    async fn download_and_save_media(
        &self,
        msg_type: &str,
        content_json: &Value,
        message_id: &str,
    ) -> (Option<String>, String) {
        let (file_key, resource_type, fallback_suffix) = match msg_type {
            "image" => (
                content_json.get("image_key").and_then(Value::as_str),
                "image",
                ".jpg",
            ),
            "audio" => (
                content_json.get("file_key").and_then(Value::as_str),
                "file",
                ".opus",
            ),
            "file" | "media" => (
                content_json.get("file_key").and_then(Value::as_str),
                "file",
                "",
            ),
            _ => (None, "file", ""),
        };
        let Some(file_key) = file_key else {
            return (None, format!("[{msg_type}: download failed]"));
        };
        let Ok(api) = self.api().await else {
            return (None, format!("[{msg_type}: download failed]"));
        };
        let Ok(Some(resource)) = api
            .download_resource(message_id, file_key, resource_type)
            .await
        else {
            return (None, format!("[{msg_type}: download failed]"));
        };
        let Ok(media_dir) = self.media_dir() else {
            return (None, format!("[{msg_type}: download failed]"));
        };
        let stem = file_key.chars().take(16).collect::<String>();
        let mut file_name = resource
            .file_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(safe_filename)
            .unwrap_or_else(|| format!("{stem}{fallback_suffix}"));
        if msg_type == "audio" && !file_name.ends_with(".opus") {
            file_name.push_str(".opus");
        }
        let path = media_dir.join(file_name);
        if std::fs::write(&path, &resource.bytes).is_err() {
            return (None, format!("[{msg_type}: download failed]"));
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .to_string();
        (
            Some(path.display().to_string()),
            format!("[{msg_type}: {file_name}]"),
        )
    }

    pub async fn reply_message(
        &self,
        parent_message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> bool {
        let Ok(api) = self.api().await else {
            return false;
        };
        api.reply_message(parent_message_id, msg_type, content)
            .await
            .is_ok()
    }

    pub async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &str,
    ) -> bool {
        let Ok(api) = self.api().await else {
            return false;
        };
        api.send_message(receive_id_type, receive_id, msg_type, content)
            .await
            .is_ok()
    }

    async fn send_or_reply(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        reply_message_id: Option<&str>,
        first_send: &mut bool,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        if let Some(reply_message_id) = reply_message_id {
            if *first_send {
                *first_send = false;
                if self
                    .reply_message(reply_message_id, msg_type, content)
                    .await
                {
                    return Ok(());
                }
            }
        }
        if self
            .send_message(receive_id_type, receive_id, msg_type, content)
            .await
        {
            Ok(())
        } else {
            Err(anyhow!("failed to send feishu {msg_type} message"))
        }
    }

    pub fn format_tool_hint_lines(tool_hint: &str) -> String {
        let mut parts = Vec::new();
        let mut buf = String::new();
        let mut depth = 0;
        let mut in_string = false;
        let mut quote_char = '\0';
        let mut escaped = false;

        let chars = tool_hint.chars().collect::<Vec<_>>();
        for (idx, ch) in chars.iter().enumerate() {
            buf.push(*ch);
            if in_string {
                if escaped {
                    escaped = false;
                } else if *ch == '\\' {
                    escaped = true;
                } else if *ch == quote_char {
                    in_string = false;
                }
                continue;
            }
            if *ch == '"' || *ch == '\'' {
                in_string = true;
                quote_char = *ch;
                continue;
            }
            if *ch == '(' {
                depth += 1;
                continue;
            }
            if *ch == ')' && depth > 0 {
                depth -= 1;
                continue;
            }
            if *ch == ',' && depth == 0 {
                let next = chars.get(idx + 1).copied().unwrap_or('\0');
                if next == ' ' {
                    parts.push(buf.trim_end().to_string());
                    buf.clear();
                }
            }
        }
        if !buf.trim().is_empty() {
            parts.push(buf.trim().to_string());
        }
        parts.join("\n")
    }

    async fn send_tool_hint_card(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        tool_hint: &str,
    ) -> Result<()> {
        let clean_hint = tool_hint
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        let clean_hint = if let Some((_, remainder)) = clean_hint.split_once(' ') {
            remainder.trim()
        } else {
            clean_hint
        };
        let formatted = Self::format_tool_hint_lines(clean_hint);
        let card = json!({
            "config": {"wide_screen_mode": true},
            "elements": [
                {"tag": "markdown", "content": format!("**Tool Calls**\n\n```text\n{formatted}\n```")}
            ]
        });
        if self
            .send_message(
                receive_id_type,
                receive_id,
                "interactive",
                &card.to_string(),
            )
            .await
        {
            Ok(())
        } else {
            Err(anyhow!("failed to send feishu tool hint card"))
        }
    }

    pub async fn handle_event(&self, payload: &Value) -> Result<()> {
        let Some(event) = payload.get("event") else {
            return Ok(());
        };
        let Some(message) = event.get("message") else {
            return Ok(());
        };
        let message_id = message
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if message_id.is_empty() {
            return Ok(());
        }
        {
            let mut processed = self
                ._processed_message_ids
                .lock()
                .expect("feishu processed ids lock poisoned");
            if processed.iter().any(|id| id == &message_id) {
                return Ok(());
            }
            processed.push_back(message_id.clone());
            while processed.len() > Self::MAX_PROCESSED_MESSAGE_IDS {
                processed.pop_front();
            }
        }

        if event
            .get("sender")
            .and_then(|sender| sender.get("sender_type"))
            .and_then(Value::as_str)
            == Some("bot")
        {
            return Ok(());
        }

        let sender_id = event
            .get("sender")
            .and_then(|sender| sender.get("sender_id"))
            .and_then(|sender_id| sender_id.get("open_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let chat_id = message
            .get("chat_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let chat_type = message
            .get("chat_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let msg_type = message
            .get("message_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if chat_id.is_empty() {
            return Ok(());
        }
        if chat_type == "group"
            && self.config.group_policy != "open"
            && message
                .get("mentions")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
        {
            return Ok(());
        }
        self.add_reaction(&message_id, &self.config.react_emoji)
            .await;

        let content_json = message
            .get("content")
            .and_then(Value::as_str)
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .unwrap_or(Value::Null);

        let mut content_parts = Vec::new();
        let mut media_paths = Vec::new();
        match msg_type {
            "text" => {
                if let Some(text) = content_json.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        content_parts.push(text.trim().to_string());
                    }
                }
            }
            "post" => {
                let (text, image_keys) = extract_post_content(&content_json);
                if !text.trim().is_empty() {
                    content_parts.push(text);
                }
                for image_key in image_keys {
                    let (file_path, content_text) = self
                        .download_and_save_media(
                            "image",
                            &json!({"image_key": image_key}),
                            &message_id,
                        )
                        .await;
                    if let Some(file_path) = file_path {
                        media_paths.push(file_path);
                    }
                    content_parts.push(content_text);
                }
            }
            "image" | "audio" | "file" | "media" => {
                let (file_path, content_text) = self
                    .download_and_save_media(msg_type, &content_json, &message_id)
                    .await;
                if let Some(file_path) = file_path {
                    media_paths.push(file_path);
                }
                content_parts.push(content_text);
            }
            "interactive"
            | "share_chat"
            | "share_user"
            | "share_calendar_event"
            | "system"
            | "merge_forward" => {
                content_parts.push(extract_share_card_content(&content_json, msg_type));
            }
            _ => {
                content_parts.push(format!("[{msg_type}]"));
            }
        }

        let parent_id = message.get("parent_id").and_then(Value::as_str);
        let root_id = message.get("root_id").and_then(Value::as_str);
        if let Some(parent_id) = parent_id {
            if let Some(reply_ctx) = self.get_message_content(parent_id).await {
                content_parts.insert(0, reply_ctx);
            }
        }

        let content = content_parts.join("\n");
        if content.trim().is_empty() && media_paths.is_empty() {
            return Ok(());
        }
        let reply_to = if chat_type == "group" {
            chat_id.to_string()
        } else {
            sender_id.to_string()
        };
        self.base
            .handle_message(
                self.name(),
                sender_id,
                &reply_to,
                &content,
                Some(media_paths),
                Some(BTreeMap::from([
                    ("message_id".to_string(), json!(message_id)),
                    ("chat_type".to_string(), json!(chat_type)),
                    ("msg_type".to_string(), json!(msg_type)),
                    ("parent_id".to_string(), json!(parent_id)),
                    ("root_id".to_string(), json!(root_id)),
                ])),
                None,
            )
            .await
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "feishu"
    }

    fn display_name(&self) -> &'static str {
        "Feishu"
    }

    fn setup_instructions(&self) -> &'static str {
        "Feishu (Lark) uses the Event Subscription v2 with WebSocket.\n\
         \n\
         1. Go to https://open.feishu.cn/app and create a custom app\n\
         2. Under 'Credentials', copy the App ID and App Secret\n\
         3. Under 'Event Subscriptions', enable WebSocket mode\n\
         4. Add event subscriptions: im.message.receive_v1\n\
         5. Under 'Permissions', add: im:message, im:message:send, im:resource\n\
         6. Publish the app version and have an admin approve it\n\
         7. Configure xbot:\n\
         \n\
            \"feishu\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"appId\": \"<your-app-id>\",\n\
              \"appSecret\": \"<your-app-secret>\"\n\
            }\n\
         \n\
         8. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if !self.config.app_id.trim().is_empty() && !self.config.app_secret.trim().is_empty() {
            let _ = self.api().await?;
        }
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let receive_id_type = if msg.chat_id.starts_with("oc_") {
            "chat_id"
        } else {
            "open_id"
        };
        if msg
            .metadata
            .get("_tool_hint")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if !msg.content.trim().is_empty() {
                self.send_tool_hint_card(receive_id_type, &msg.chat_id, msg.content.trim())
                    .await?;
            }
            return Ok(());
        }

        let reply_message_id = if self.config.reply_to_message
            && !msg
                .metadata
                .get("_progress")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            msg.metadata
                .get("message_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            None
        };

        let mut first_send = true;

        for file_path in &msg.media {
            if !Path::new(file_path).is_file() {
                continue;
            }
            let ext = Path::new(file_path)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
                .unwrap_or_default();
            if Self::IMAGE_EXTS.iter().any(|candidate| *candidate == ext) {
                if let Some(key) = self.api().await?.upload_image(file_path).await? {
                    self.send_or_reply(
                        receive_id_type,
                        &msg.chat_id,
                        reply_message_id.as_deref(),
                        &mut first_send,
                        "image",
                        &json!({"image_key": key}).to_string(),
                    )
                    .await?;
                }
            } else if let Some(key) = self.api().await?.upload_file(file_path).await? {
                let media_type = if Self::AUDIO_EXTS.iter().any(|candidate| *candidate == ext) {
                    "audio"
                } else if Self::VIDEO_EXTS.iter().any(|candidate| *candidate == ext) {
                    "video"
                } else {
                    "file"
                };
                self.send_or_reply(
                    receive_id_type,
                    &msg.chat_id,
                    reply_message_id.as_deref(),
                    &mut first_send,
                    media_type,
                    &json!({"file_key": key}).to_string(),
                )
                .await?;
            }
        }

        if !msg.content.trim().is_empty() {
            match Self::detect_msg_format(&msg.content) {
                "text" => {
                    self.send_or_reply(
                        receive_id_type,
                        &msg.chat_id,
                        reply_message_id.as_deref(),
                        &mut first_send,
                        "text",
                        &json!({"text": msg.content.trim()}).to_string(),
                    )
                    .await?;
                }
                "post" => {
                    self.send_or_reply(
                        receive_id_type,
                        &msg.chat_id,
                        reply_message_id.as_deref(),
                        &mut first_send,
                        "post",
                        &Self::markdown_to_post(&msg.content),
                    )
                    .await?;
                }
                _ => {
                    let elements = self.build_card_elements(&msg.content);
                    for chunk in Self::split_elements_by_table_limit(elements, 1) {
                        self.send_or_reply(
                            receive_id_type,
                            &msg.chat_id,
                            reply_message_id.as_deref(),
                            &mut first_send,
                            "interactive",
                            &json!({"config": {"wide_screen_mode": true}, "elements": chunk})
                                .to_string(),
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }
}
