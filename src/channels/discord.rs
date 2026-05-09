//! Discord channel: Gateway WebSocket (v10) + REST for outbound messages.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{Channel, ChannelBase};
use crate::security::validate_url_target;
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::{split_message, workspace_state_dir};

pub const DISCORD_MAX_MESSAGE_LEN: usize = 2000;

const DISCORD_API: &str = "https://discord.com/api/v10";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
/// GUILDS (1) + GUILD_MESSAGES (512) + MESSAGE_CONTENT (32768) = 33281
const GATEWAY_INTENTS: u32 = 33281;

const SEND_MAX_RETRIES: usize = 3;
const SEND_RETRY_BASE_DELAY_MS: u64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub enabled: bool,
    #[serde(alias = "botToken")]
    pub bot_token: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "groupPolicy")]
    pub group_policy: String,
    pub streaming: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            allow_from: Vec::new(),
            group_policy: "mention".to_string(),
            streaming: false,
        }
    }
}

#[derive(Clone)]
pub struct DiscordChannel {
    base: ChannelBase,
    config: DiscordConfig,
    client: Client,
    bot_user_id: Arc<Mutex<Option<String>>>,
    session_id: Arc<Mutex<Option<String>>>,
    last_seq: Arc<Mutex<Option<u64>>>,
    gateway_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    gateway_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
}

impl DiscordChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: DiscordConfig = serde_json::from_value(config)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!(
                "xbot/",
                env!("CARGO_PKG_VERSION"),
                " (Discord bot)"
            ))
            .build()?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            client,
            bot_user_id: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
            last_seq: Arc::new(Mutex::new(None)),
            gateway_shutdown: Arc::new(AsyncMutex::new(None)),
            gateway_task: Arc::new(AsyncMutex::new(None)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(DiscordConfig::default()).expect("serializable discord config")
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.config.bot_token)
    }

    async fn run_gateway_loop(self, mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            match self.clone().run_gateway_session(&mut shutdown_rx).await {
                Ok(()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[discord] gateway session ended; reconnecting");
                }
                Err(err) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[discord] gateway error: {err}");
                }
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    async fn run_gateway_session(self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        eprintln!("[discord] connecting gateway");
        let (ws, _) = connect_async(GATEWAY_URL)
            .await
            .map_err(|e| anyhow!("gateway connect: {e}"))?;
        let (mut write, mut read) = ws.split();

        let hello_text = match read.next().await {
            Some(Ok(Message::Text(t))) => t,
            other => return Err(anyhow!("expected Hello, got {other:?}")),
        };
        let hello: Value = serde_json::from_str(&hello_text)?;
        if hello.get("op").and_then(|v| v.as_u64()) != Some(10) {
            return Err(anyhow!("expected opcode 10 Hello"));
        }
        let heartbeat_interval_ms = hello["d"]["heartbeat_interval"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing heartbeat_interval"))?;

        let resume_or_identify = {
            let sid = self
                .session_id
                .lock()
                .expect("discord session_id lock poisoned")
                .clone();
            let seq = *self
                .last_seq
                .lock()
                .expect("discord last_seq lock poisoned");
            match (sid, seq) {
                (Some(session_id), Some(seq)) => json!({
                    "op": 6,
                    "d": {
                        "token": self.config.bot_token,
                        "session_id": session_id,
                        "seq": seq,
                    }
                }),
                _ => json!({
                    "op": 2,
                    "d": {
                        "token": self.config.bot_token,
                        "intents": GATEWAY_INTENTS,
                        "properties": {
                            "os": "linux",
                            "browser": "xbot",
                            "device": "xbot"
                        }
                    }
                }),
            }
        };

        write
            .send(Message::Text(resume_or_identify.to_string().into()))
            .await
            .map_err(|e| anyhow!("gateway identify/resume: {e}"))?;

        let write = Arc::new(AsyncMutex::new(write));
        let seq_for_hb = self.last_seq.clone();
        let hb_write = write.clone();
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % heartbeat_interval_ms.max(1) as u128) as u64;

        let hb_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
            loop {
                let s = seq_for_hb
                    .lock()
                    .expect("discord last_seq lock poisoned")
                    .clone();
                let payload = json!({ "op": 1, "d": s });
                let mut w = hb_write.lock().await;
                if w.send(Message::Text(payload.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                drop(w);
                tokio::time::sleep(Duration::from_millis(heartbeat_interval_ms)).await;
            }
        });

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        hb_task.abort();
                        let _ = write.lock().await.close().await;
                        return Ok(());
                    }
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(t))) => {
                            if let Err(e) = self.handle_gateway_payload(&t, &hb_task).await {
                                hb_task.abort();
                                return Err(e);
                            }
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let mut w = write.lock().await;
                            let _ = w.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            hb_task.abort();
                            return Err(anyhow!("gateway closed"));
                        }
                        Some(Err(e)) => {
                            hb_task.abort();
                            return Err(anyhow!(e));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_gateway_payload(&self, text: &str, hb_task: &JoinHandle<()>) -> Result<()> {
        let v: Value = serde_json::from_str(text)?;
        match v.get("op").and_then(|x| x.as_u64()) {
            Some(0) => {
                if let Some(s) = v.get("s").and_then(|x| x.as_u64()) {
                    *self
                        .last_seq
                        .lock()
                        .expect("discord last_seq lock poisoned") = Some(s);
                }
                match v.get("t").and_then(|t| t.as_str()) {
                    Some("READY") => {
                        if let Some(session) = v["d"]["session_id"].as_str() {
                            *self
                                .session_id
                                .lock()
                                .expect("discord session_id lock poisoned") =
                                Some(session.to_string());
                        }
                        if let Some(uid) = v["d"]["user"]["id"].as_str() {
                            *self
                                .bot_user_id
                                .lock()
                                .expect("discord bot_user_id lock poisoned") =
                                Some(uid.to_string());
                            eprintln!("[discord] ready as user {uid}");
                        }
                    }
                    Some("MESSAGE_CREATE") => {
                        if let Err(e) = self.handle_message_create(&v["d"]).await {
                            eprintln!("[discord] MESSAGE_CREATE handler: {e}");
                        }
                    }
                    Some("RESUMED") => {
                        eprintln!("[discord] gateway resumed");
                    }
                    _ => {}
                }
            }
            Some(7) => {
                eprintln!("[discord] gateway opcode 7 RECONNECT");
                hb_task.abort();
                return Err(anyhow!("reconnect"));
            }
            Some(9) => {
                if v.get("d").and_then(|x| x.as_bool()) == Some(false) {
                    *self
                        .session_id
                        .lock()
                        .expect("discord session_id lock poisoned") = None;
                    *self
                        .last_seq
                        .lock()
                        .expect("discord last_seq lock poisoned") = None;
                }
                hb_task.abort();
                return Err(anyhow!("invalid session"));
            }
            Some(11) => {}
            _ => {}
        }
        Ok(())
    }

    async fn handle_message_create(&self, data: &Value) -> Result<()> {
        if data
            .get("author")
            .and_then(|a| a.get("bot"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Ok(());
        }
        let author_id = data
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if author_id.is_empty() {
            return Ok(());
        }
        if Some(author_id)
            == self
                .bot_user_id
                .lock()
                .expect("discord bot_user_id lock poisoned")
                .as_deref()
        {
            return Ok(());
        }

        if data.get("guild_id").is_some()
            && self.config.group_policy.eq_ignore_ascii_case("mention")
            && !self.message_mentions_bot(data)?
        {
            return Ok(());
        }

        if !self.base.is_allowed(author_id) {
            return Ok(());
        }

        let channel_id = data
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if channel_id.is_empty() {
            return Ok(());
        }

        let mut content = data
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        content = strip_discord_mentions(&content);

        let mut media_paths = Vec::new();
        if let Some(attachments) = data.get("attachments").and_then(Value::as_array) {
            for att in attachments {
                let Some(url) = att.get("url").and_then(Value::as_str) else {
                    continue;
                };
                let filename = att
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("attachment");
                match self.download_attachment(url, filename).await {
                    Ok(path) => media_paths.push(path),
                    Err(e) => eprintln!("[discord] attachment download failed: {e}"),
                }
            }
        }

        if content.trim().is_empty() && media_paths.is_empty() {
            return Ok(());
        }

        let mut metadata = BTreeMap::new();
        if let Some(mid) = data.get("id").and_then(Value::as_str) {
            metadata.insert("message_id".to_string(), json!(mid));
        }
        metadata.insert("discord".to_string(), data.clone());

        let session_key = data
            .get("guild_id")
            .and_then(Value::as_str)
            .map(|gid| format!("discord:{channel_id}:{gid}"))
            .or_else(|| Some(format!("discord:{channel_id}")));

        self.base
            .handle_message(
                self.name(),
                author_id,
                channel_id,
                content.trim(),
                Some(media_paths),
                Some(metadata),
                session_key,
            )
            .await
    }

    fn message_mentions_bot(&self, data: &Value) -> Result<bool> {
        let bot_id = self
            .bot_user_id
            .lock()
            .expect("discord bot_user_id lock poisoned")
            .clone()
            .ok_or_else(|| anyhow!("bot user id not ready"))?;

        if data
            .get("mentions")
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter().any(|m| {
                    m.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == bot_id)
                })
            })
        {
            return Ok(true);
        }

        if let Some(ref_msg) = data.get("referenced_message") {
            if ref_msg
                .get("author")
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                == Some(bot_id.as_str())
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn download_attachment(&self, url: &str, filename: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "attachment GET {} failed: {}",
                url,
                response.status()
            ));
        }
        let bytes = response.bytes().await?.to_vec();

        let downloads_dir = workspace_state_dir(&self.base.workspace).join("downloads");
        std::fs::create_dir_all(&downloads_dir)?;

        let safe = crate::util::safe_filename(filename);
        let local_path = downloads_dir.join(format!(
            "{}_{}",
            uuid::Uuid::new_v4(),
            if safe.is_empty() {
                "file".to_string()
            } else {
                safe
            }
        ));
        std::fs::write(&local_path, bytes)?;
        Ok(local_path.display().to_string())
    }

    async fn trigger_typing(&self, channel_id: &str) -> Result<()> {
        let response = self
            .client
            .post(format!("{DISCORD_API}/channels/{channel_id}/typing"))
            .header("Authorization", self.auth_header())
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("typing failed: {}", response.status()))
        }
    }

    async fn post_message_json(&self, channel_id: &str, body: &Value) -> Result<()> {
        let mut last_error = None;
        for attempt in 0..SEND_MAX_RETRIES {
            let response = self
                .client
                .post(format!("{DISCORD_API}/channels/{channel_id}/messages"))
                .header("Authorization", self.auth_header())
                .json(body)
                .send()
                .await?;
            if response.status().is_success() {
                return Ok(());
            }
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("discord API {status}: {text}"));
            if attempt + 1 < SEND_MAX_RETRIES {
                tokio::time::sleep(Duration::from_millis(
                    SEND_RETRY_BASE_DELAY_MS * 2_u64.pow(attempt as u32),
                ))
                .await;
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("send failed")))
    }

    async fn post_message_multipart(
        &self,
        channel_id: &str,
        payload: Value,
        file_path: &str,
        field_index: usize,
    ) -> Result<()> {
        let bytes = if file_path.starts_with("http://") || file_path.starts_with("https://") {
            let (ok, err) = validate_url_target(file_path);
            if !ok {
                return Err(anyhow!("blocked url: {err}"));
            }
            let response = self.client.get(file_path).send().await?;
            if !response.status().is_success() {
                return Err(anyhow!("fetch media failed: {}", response.status()));
            }
            response.bytes().await?.to_vec()
        } else {
            tokio::fs::read(file_path).await?
        };

        let filename = Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        let attachment_meta = json!([{
            "id": field_index,
            "filename": filename.clone(),
        }]);
        let mut payload_obj = match payload {
            Value::Object(m) => m,
            _ => return Err(anyhow!("payload must be object")),
        };
        payload_obj.insert("attachments".to_string(), attachment_meta);
        let payload_json = Value::Object(payload_obj);
        let payload_json_str = payload_json.to_string();

        let mut last_error = None;
        for attempt in 0..SEND_MAX_RETRIES {
            let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone());
            let form = reqwest::multipart::Form::new()
                .text("payload_json", payload_json_str.clone())
                .part(format!("files[{field_index}]"), part);

            let response = self
                .client
                .post(format!("{DISCORD_API}/channels/{channel_id}/messages"))
                .header("Authorization", self.auth_header())
                .multipart(form)
                .send()
                .await?;
            if response.status().is_success() {
                return Ok(());
            }
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("discord multipart {status}: {text}"));
            if attempt + 1 < SEND_MAX_RETRIES {
                tokio::time::sleep(Duration::from_millis(
                    SEND_RETRY_BASE_DELAY_MS * 2_u64.pow(attempt as u32),
                ))
                .await;
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("multipart send failed")))
    }
}

fn strip_discord_mentions(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<@!?\d+>").expect("discord mention regex"));
    re.replace_all(s, "").trim().to_string()
}

#[async_trait]
impl Channel for DiscordChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "discord"
    }

    fn display_name(&self) -> &'static str {
        "Discord"
    }

    fn setup_instructions(&self) -> &'static str {
        "Discord uses the Gateway v10 WebSocket protocol.\n\
         \n\
         1. Go to https://discord.com/developers/applications and create an application\n\
         2. Under 'Bot', click 'Add Bot' and copy the bot token\n\
         3. Under 'Bot', enable 'Message Content Intent' in Privileged Gateway Intents\n\
         4. Under 'OAuth2 > URL Generator', select 'bot' scope with 'Send Messages' permission\n\
         5. Use the generated URL to invite the bot to your server\n\
         6. Configure xbot:\n\
         \n\
            \"discord\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"botToken\": \"<your-bot-token>\",\n\
              \"groupPolicy\": \"mention\"\n\
            }\n\
         \n\
         7. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if self.config.bot_token.trim().is_empty() {
            self.base.set_running(true);
            return Ok(());
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.gateway_shutdown.lock().await = Some(shutdown_tx);
        let channel = self.clone();
        let handle = tokio::spawn(async move {
            channel.run_gateway_loop(shutdown_rx).await;
        });
        *self.gateway_task.lock().await = Some(handle);
        self.base.set_running(true);
        eprintln!(
            "[discord] gateway task started; group_policy={}",
            self.config.group_policy
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(shutdown) = self.gateway_shutdown.lock().await.take() {
            let _ = shutdown.send(true);
        }
        if let Some(task) = self.gateway_task.lock().await.take() {
            let _ = task.await;
        }
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if self.config.bot_token.trim().is_empty() {
            return Err(anyhow!("discord bot token not configured"));
        }

        if self.config.streaming && msg.metadata.get("_progress").is_none() {
            let _ = self.trigger_typing(&msg.chat_id).await;
        }

        let reply_ref = msg.reply_to.as_ref().map(|id| {
            json!({
                "message_id": id,
                "fail_if_not_exists": false,
            })
        });

        let discord_meta = msg.metadata.get("discord").cloned().unwrap_or(Value::Null);
        let reply_from_inbound = discord_meta.get("id").and_then(Value::as_str).map(|id| {
            json!({
                "message_id": id,
                "fail_if_not_exists": false,
            })
        });

        let message_reference = reply_ref.or(reply_from_inbound);

        let mut first_reply = message_reference.clone();

        for chunk in split_message(&msg.content, DISCORD_MAX_MESSAGE_LEN) {
            let mut body = json!({ "content": chunk });
            if let Some(ref r) = first_reply {
                body["message_reference"] = r.clone();
            }
            if !chunk.is_empty() || msg.media.is_empty() {
                self.post_message_json(&msg.chat_id, &body).await?;
            }
            first_reply = None;
        }

        for (i, media) in msg.media.iter().enumerate() {
            let mut payload = json!({ "content": "" });
            if let Some(ref r) = first_reply {
                payload["message_reference"] = r.clone();
            }
            first_reply = None;

            if media.starts_with("http://") || media.starts_with("https://") {
                let (ok, err) = validate_url_target(media);
                if !ok {
                    let label = media.rsplit('/').next().unwrap_or("attachment");
                    self.post_message_json(
                        &msg.chat_id,
                        &json!({ "content": format!("[Failed to send: {label}] ({err})") }),
                    )
                    .await?;
                    continue;
                }
            }

            self.post_message_multipart(&msg.chat_id, payload, media, i)
                .await?;
        }

        Ok(())
    }
}
