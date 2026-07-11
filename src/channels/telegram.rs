use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use super::{Channel, ChannelBase};
use crate::security::validate_url_target;
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::{split_message, workspace_state_dir};

pub const TELEGRAM_MAX_MESSAGE_LEN: usize = 4000;
pub const TELEGRAM_REPLY_CONTEXT_MAX_LEN: usize = TELEGRAM_MAX_MESSAGE_LEN;
const SEND_MAX_RETRIES: usize = 3;
const SEND_RETRY_BASE_DELAY_MS: u64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub token: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    pub proxy: Option<String>,
    #[serde(alias = "webhookPath")]
    pub webhook_path: String,
    #[serde(alias = "webhookSecret")]
    pub webhook_secret: String,
    #[serde(alias = "replyToMessage")]
    pub reply_to_message: bool,
    #[serde(alias = "groupPolicy")]
    pub group_policy: String,
    #[serde(alias = "connectionPoolSize")]
    pub connection_pool_size: usize,
    #[serde(alias = "poolTimeout")]
    pub pool_timeout: f32,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token: String::new(),
            allow_from: Vec::new(),
            proxy: None,
            webhook_path: "/telegram/webhook".to_string(),
            webhook_secret: String::new(),
            reply_to_message: false,
            group_policy: "mention".to_string(),
            connection_pool_size: 32,
            pool_timeout: 5.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramBotIdentity {
    pub id: i64,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyParameters {
    pub message_id: i64,
}

#[async_trait]
pub trait TelegramApi: Send + Sync {
    async fn get_me(&self) -> Result<TelegramBotIdentity>;
    async fn get_file(&self, file_id: &str) -> Result<String>;
    async fn download_file(&self, file_path: &str) -> Result<Vec<u8>>;
    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()>;
    async fn send_photo(
        &self,
        chat_id: i64,
        photo: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()>;
    async fn send_voice(
        &self,
        chat_id: i64,
        voice: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()>;
    async fn send_audio(
        &self,
        chat_id: i64,
        audio: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()>;
    async fn send_document(
        &self,
        chat_id: i64,
        document: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()>;
}

pub struct ReqwestTelegramApi {
    client: Client,
    token: String,
    base_url: String,
}

impl ReqwestTelegramApi {
    pub fn new(token: &str, proxy: Option<&str>) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(anyhow!("telegram token not configured"));
        }
        let mut builder = Client::builder().timeout(Duration::from_secs(60));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        Ok(Self {
            client: builder.build()?,
            token: token.to_string(),
            base_url: format!("https://api.telegram.org/bot{token}"),
        })
    }

    async fn post_json(&self, method: &str, body: Value) -> Result<()> {
        let response = self
            .client
            .post(format!("{}/{}", self.base_url, method))
            .json(&body)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow!(
                "telegram api error: {}",
                payload
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }

    async fn send_media_request(
        &self,
        method: &str,
        field: &str,
        chat_id: i64,
        source: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        if source.starts_with("http://") || source.starts_with("https://") {
            return self
                .post_json(
                    method,
                    json!({
                        "chat_id": chat_id,
                        field: source,
                        "message_thread_id": message_thread_id,
                        "reply_parameters": reply_parameters,
                    }),
                )
                .await;
        }
        let bytes = std::fs::read(source)?;
        let filename = Path::new(source)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                field.to_string(),
                reqwest::multipart::Part::bytes(bytes).file_name(filename),
            );
        if let Some(message_thread_id) = message_thread_id {
            form = form.text("message_thread_id", message_thread_id.to_string());
        }
        if let Some(reply_parameters) = reply_parameters {
            form = form.text(
                "reply_parameters",
                serde_json::to_string(&reply_parameters)?,
            );
        }
        let response = self
            .client
            .post(format!("{}/{}", self.base_url, method))
            .multipart(form)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow!(
                "telegram media api error: {}",
                payload
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }
}

#[async_trait]
impl TelegramApi for ReqwestTelegramApi {
    async fn get_me(&self) -> Result<TelegramBotIdentity> {
        let response = self
            .client
            .post(format!("{}/getMe", self.base_url))
            .send()
            .await?;
        let payload: Value = response.json().await?;
        Ok(TelegramBotIdentity {
            id: payload["result"]["id"].as_i64().unwrap_or_default(),
            username: payload["result"]["username"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn get_file(&self, file_id: &str) -> Result<String> {
        let response = self
            .client
            .post(format!("{}/getFile", self.base_url))
            .json(&json!({ "file_id": file_id }))
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool) == Some(true) {
            payload["result"]["file_path"]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("telegram getFile result missing file_path"))
        } else {
            Err(anyhow!(
                "telegram getFile error: {}",
                payload["description"].as_str().unwrap_or("unknown")
            ))
        }
    }

    async fn download_file(&self, file_path: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.token, file_path
        );
        let response = self.client.get(url).send().await?;
        if response.status().is_success() {
            Ok(response.bytes().await?.to_vec())
        } else {
            Err(anyhow!(
                "telegram file download failed: {}",
                response.status()
            ))
        }
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.post_json(
            "sendMessage",
            json!({
                "chat_id": chat_id,
                "text": text,
                "message_thread_id": message_thread_id,
                "reply_parameters": reply_parameters,
            }),
        )
        .await
    }

    async fn send_photo(
        &self,
        chat_id: i64,
        photo: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.send_media_request(
            "sendPhoto",
            "photo",
            chat_id,
            photo,
            message_thread_id,
            reply_parameters,
        )
        .await
    }

    async fn send_voice(
        &self,
        chat_id: i64,
        voice: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.send_media_request(
            "sendVoice",
            "voice",
            chat_id,
            voice,
            message_thread_id,
            reply_parameters,
        )
        .await
    }

    async fn send_audio(
        &self,
        chat_id: i64,
        audio: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.send_media_request(
            "sendAudio",
            "audio",
            chat_id,
            audio,
            message_thread_id,
            reply_parameters,
        )
        .await
    }

    async fn send_document(
        &self,
        chat_id: i64,
        document: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        self.send_media_request(
            "sendDocument",
            "document",
            chat_id,
            document,
            message_thread_id,
            reply_parameters,
        )
        .await
    }
}

pub struct TelegramChannel {
    base: ChannelBase,
    config: TelegramConfig,
    api: AsyncMutex<Option<Arc<dyn TelegramApi>>>,
    bot_identity: Mutex<Option<TelegramBotIdentity>>,
    message_threads: Mutex<BTreeMap<(String, i64), i64>>,
}

impl TelegramChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: TelegramConfig = serde_json::from_value(config)?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            api: AsyncMutex::new(None),
            bot_identity: Mutex::new(None),
            message_threads: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(TelegramConfig::default()).expect("serializable telegram config")
    }

    pub async fn set_api(&self, api: Arc<dyn TelegramApi>) {
        *self.api.lock().await = Some(api);
    }

    async fn api(&self) -> Result<Arc<dyn TelegramApi>> {
        if let Some(api) = self.api.lock().await.clone() {
            return Ok(api);
        }
        let api: Arc<dyn TelegramApi> = Arc::new(ReqwestTelegramApi::new(
            &self.config.token,
            self.config.proxy.as_deref(),
        )?);
        *self.api.lock().await = Some(api.clone());
        Ok(api)
    }

    async fn ensure_bot_identity(&self) -> Result<TelegramBotIdentity> {
        if let Some(identity) = self
            .bot_identity
            .lock()
            .expect("telegram bot identity lock poisoned")
            .clone()
        {
            return Ok(identity);
        }
        let identity = self.api().await?.get_me().await?;
        *self
            .bot_identity
            .lock()
            .expect("telegram bot identity lock poisoned") = Some(identity.clone());
        Ok(identity)
    }

    pub fn is_allowed(&self, sender_id: &str) -> bool {
        if self.base.is_allowed(sender_id) {
            return true;
        }
        if self.config.allow_from.iter().any(|item| item == "*") {
            return true;
        }
        let Some((sid, username)) = sender_id.split_once('|') else {
            return false;
        };
        if sid.parse::<i64>().is_err() || username.is_empty() {
            return false;
        }
        self.config
            .allow_from
            .iter()
            .any(|item| item == sid || item == username)
    }

    pub fn derive_topic_session_key(chat_id: &str, message_thread_id: i64) -> String {
        format!("telegram:{chat_id}:topic:{message_thread_id}")
    }

    pub fn extract_reply_context(message: &Value) -> Option<String> {
        let reply = message.get("reply_to_message")?;
        let text = reply
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| reply.get("caption").and_then(Value::as_str))?;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let truncated = if text.chars().count() > TELEGRAM_REPLY_CONTEXT_MAX_LEN {
            format!(
                "{}...",
                text.chars()
                    .take(TELEGRAM_REPLY_CONTEXT_MAX_LEN)
                    .collect::<String>()
            )
        } else {
            text.to_string()
        };
        Some(format!("[Reply to: {truncated}]"))
    }

    pub async fn handle_update(&self, update: &Value) -> Result<()> {
        let Some(message) = update.get("message") else {
            return Ok(());
        };
        let sender_id_num = message
            .get("from")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let username = message
            .get("from")
            .and_then(|value| value.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sender_id = format!("{sender_id_num}|{username}");
        if !self.is_allowed(&sender_id) {
            return Ok(());
        }

        let chat_type = message
            .get("chat")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("private");
        if matches!(chat_type, "group" | "supergroup")
            && self.config.group_policy == "mention"
            && !self.group_message_targets_bot(message).await?
        {
            return Ok(());
        }

        let chat_id = message
            .get("chat_id")
            .or_else(|| message.get("chat").and_then(|value| value.get("id")))
            .and_then(Value::as_i64)
            .unwrap_or_default()
            .to_string();
        let mut content = message
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| message.get("caption").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        if let Some(reply_context) = Self::extract_reply_context(message) {
            content = if content.is_empty() {
                reply_context
            } else {
                format!("{reply_context}\n\n{content}")
            };
        }

        let mut media_paths = Vec::new();
        if let Some(photo) = message.get("photo").and_then(Value::as_array) {
            if let Some(best_photo) = photo.last() {
                if let Some(file_id) = best_photo.get("file_id").and_then(Value::as_str) {
                    if let Ok(file_path) = self.download_telegram_file(file_id).await {
                        media_paths.push(file_path);
                    }
                }
            }
        }

        if content.trim().is_empty() && media_paths.is_empty() {
            return Ok(());
        }
        let message_id = message.get("message_id").and_then(Value::as_i64);
        let message_thread_id = message.get("message_thread_id").and_then(Value::as_i64);
        if let (Some(message_id), Some(message_thread_id)) = (message_id, message_thread_id) {
            self.message_threads
                .lock()
                .expect("telegram message threads lock poisoned")
                .insert((chat_id.clone(), message_id), message_thread_id);
        }
        let session_key = message_thread_id
            .map(|message_thread_id| Self::derive_topic_session_key(&chat_id, message_thread_id));
        let mut metadata = BTreeMap::new();
        if let Some(message_id) = message_id {
            metadata.insert("message_id".to_string(), json!(message_id));
        }
        if let Some(message_thread_id) = message_thread_id {
            metadata.insert("message_thread_id".to_string(), json!(message_thread_id));
        }
        self.base
            .handle_message(
                self.name(),
                &sender_id,
                &chat_id,
                &content,
                Some(media_paths),
                Some(metadata),
                session_key,
            )
            .await
    }

    async fn download_telegram_file(&self, file_id: &str) -> Result<String> {
        let api = self.api().await?;
        let file_path = api.get_file(file_id).await?;
        let bytes = api.download_file(&file_path).await?;

        let downloads_dir = workspace_state_dir(&self.base.workspace).join("downloads");
        std::fs::create_dir_all(&downloads_dir)?;

        let filename = Path::new(&file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let local_path = downloads_dir.join(format!("{}_{}", file_id, filename));
        std::fs::write(&local_path, bytes)?;

        Ok(local_path.display().to_string())
    }

    async fn group_message_targets_bot(&self, message: &Value) -> Result<bool> {
        let identity = self.ensure_bot_identity().await?;
        if message
            .get("reply_to_message")
            .and_then(|reply| reply.get("from"))
            .and_then(|from| from.get("id"))
            .and_then(Value::as_i64)
            == Some(identity.id)
        {
            return Ok(true);
        }
        let text = message
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| message.get("caption").and_then(Value::as_str))
            .unwrap_or_default();
        Ok(text.contains(&format!("@{}", identity.username)))
    }

    pub fn set_message_thread(&self, chat_id: &str, message_id: i64, message_thread_id: i64) {
        self.message_threads
            .lock()
            .expect("telegram message threads lock poisoned")
            .insert((chat_id.to_string(), message_id), message_thread_id);
    }

    async fn send_text(
        &self,
        chat_id: i64,
        text: &str,
        message_thread_id: Option<i64>,
        reply_parameters: Option<ReplyParameters>,
    ) -> Result<()> {
        let api = self.api().await?;
        let mut last_error = None;
        for attempt in 0..SEND_MAX_RETRIES {
            match api
                .send_message(chat_id, text, message_thread_id, reply_parameters.clone())
                .await
            {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_error = Some(err);
                    if attempt + 1 < SEND_MAX_RETRIES {
                        tokio::time::sleep(Duration::from_millis(
                            SEND_RETRY_BASE_DELAY_MS * 2_u64.pow(attempt as u32),
                        ))
                        .await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("telegram send failed")))
    }

    fn infer_media_kind(path: &str) -> &'static str {
        let lower = path.to_ascii_lowercase();
        if [".jpg", ".jpeg", ".png", ".gif", ".webp"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            "photo"
        } else if [".ogg", ".opus"].iter().any(|ext| lower.ends_with(ext)) {
            "voice"
        } else if [".mp3", ".m4a", ".wav"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            "audio"
        } else {
            "document"
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "telegram"
    }

    fn display_name(&self) -> &'static str {
        "Telegram"
    }

    fn setup_instructions(&self) -> &'static str {
        "Telegram uses the Bot API with long-polling.\n\
         \n\
         1. Message @BotFather on Telegram and send /newbot\n\
         2. Follow the prompts to name your bot and get a token\n\
         3. (Optional) Send /setprivacy to @BotFather and disable privacy mode\n\
            if you want the bot to see all group messages\n\
         4. Configure xbot:\n\
         \n\
            \"telegram\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"token\": \"<your-bot-token>\"\n\
            }\n\
         \n\
         5. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if !self.config.token.trim().is_empty() {
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
        let chat_id = msg.chat_id.parse::<i64>()?;
        let mut reply_parameters = self
            .config
            .reply_to_message
            .then(|| {
                msg.metadata
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .map(|message_id| ReplyParameters { message_id })
            })
            .flatten();
        let mut message_thread_id = msg
            .metadata
            .get("message_thread_id")
            .and_then(Value::as_i64);
        if message_thread_id.is_none() {
            if let Some(message_id) = msg.metadata.get("message_id").and_then(Value::as_i64) {
                message_thread_id = self
                    .message_threads
                    .lock()
                    .expect("telegram message threads lock poisoned")
                    .get(&(msg.chat_id.clone(), message_id))
                    .copied();
            }
        }

        for chunk in split_message(&msg.content, TELEGRAM_MAX_MESSAGE_LEN) {
            self.send_text(chat_id, &chunk, message_thread_id, reply_parameters.clone())
                .await?;
            reply_parameters = None;
        }

        let api = self.api().await?;
        for media in &msg.media {
            if media.starts_with("http://") || media.starts_with("https://") {
                let (ok, err) = validate_url_target(media, &[]);
                if !ok {
                    let label = media.rsplit('/').next().unwrap_or("attachment");
                    self.send_text(
                        chat_id,
                        &format!("[Failed to send: {label}]"),
                        message_thread_id,
                        None,
                    )
                    .await?;
                    let _ = err;
                    continue;
                }
            }
            match Self::infer_media_kind(media) {
                "photo" => {
                    api.send_photo(chat_id, media, message_thread_id, None)
                        .await?;
                }
                "voice" => {
                    api.send_voice(chat_id, media, message_thread_id, None)
                        .await?;
                }
                "audio" => {
                    api.send_audio(chat_id, media, message_thread_id, None)
                        .await?;
                }
                _ => {
                    api.send_document(chat_id, media, message_thread_id, None)
                        .await?;
                }
            }
        }
        Ok(())
    }
}
