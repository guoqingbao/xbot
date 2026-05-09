//! WeCom (Enterprise WeChat) channel: WebSocket (`wss://openws.work.weixin.qq.com`) for inbound
//! events (AI Bot protocol; auth uses `agentId` as `bot_id` and `secret`) and
//! `qyapi.weixin.qq.com` REST for outbound text. Optional `token` / `encoding_aes_key` are kept for
//! config compatibility with HTTP-callback setups (not used by this WebSocket client).

use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::split_message;

/// Conservative text cap for `message/send` (official limit is 2048 bytes for UTF-8).
pub const WECOM_MAX_MESSAGE_LEN: usize = 2048;

const WECOM_DEFAULT_WS: &str = "wss://openws.work.weixin.qq.com";
const QYAPI_BASE: &str = "https://qyapi.weixin.qq.com/cgi-bin";

const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_HEARTBEAT: &str = "ping";
const CMD_MSG_CALLBACK: &str = "aibot_msg_callback";
const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";

const HEARTBEAT_INTERVAL_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WecomConfig {
    pub enabled: bool,
    #[serde(alias = "corpId")]
    pub corp_id: String,
    #[serde(alias = "agentId")]
    pub agent_id: String,
    pub secret: String,
    /// Optional; reserved for HTTP callback mode alongside `encoding_aes_key`.
    pub token: String,
    #[serde(alias = "encodingAesKey")]
    pub encoding_aes_key: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "welcomeMessage")]
    pub welcome_message: Option<String>,
    #[serde(alias = "wsUrl")]
    pub ws_url: String,
}

impl Default for WecomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            corp_id: String::new(),
            agent_id: String::new(),
            secret: String::new(),
            token: String::new(),
            encoding_aes_key: String::new(),
            allow_from: Vec::new(),
            welcome_message: None,
            ws_url: WECOM_DEFAULT_WS.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct WecomChannel {
    base: ChannelBase,
    config: WecomConfig,
    client: Client,
    token_cache: Arc<AsyncMutex<Option<CachedAccessToken>>>,
    shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    gateway_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    manual_close: Arc<Mutex<bool>>,
}

struct CachedAccessToken {
    access_token: String,
    expires_at: std::time::Instant,
}

impl WecomChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: WecomConfig = serde_json::from_value(config)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("xbot/", env!("CARGO_PKG_VERSION"), " (WeCom)"))
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
            token_cache: Arc::new(AsyncMutex::new(None)),
            shutdown: Arc::new(AsyncMutex::new(None)),
            gateway_task: Arc::new(AsyncMutex::new(None)),
            manual_close: Arc::new(Mutex::new(false)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(WecomConfig::default()).expect("serializable wecom config")
    }

    fn generate_req_id(prefix: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let rnd: u64 = rand_u64();
        format!("{prefix}_{ts}_{rnd:x}")
    }

    fn ws_url(&self) -> String {
        let u = self.config.ws_url.trim();
        if u.is_empty() {
            WECOM_DEFAULT_WS.to_string()
        } else {
            u.to_string()
        }
    }

    fn bot_id(&self) -> String {
        self.config.agent_id.trim().to_string()
    }

    async fn get_access_token(&self) -> Result<String> {
        if self.config.corp_id.trim().is_empty() || self.config.secret.trim().is_empty() {
            return Err(anyhow!("wecom corpId/corp secret not configured"));
        }
        {
            let g = self.token_cache.lock().await;
            if let Some(c) = g.as_ref() {
                if std::time::Instant::now() < c.expires_at {
                    return Ok(c.access_token.clone());
                }
            }
        }

        let url = format!(
            "{QYAPI_BASE}/gettoken?corpid={}&corpsecret={}",
            urlencoding::encode(self.config.corp_id.trim()),
            urlencoding::encode(self.config.secret.trim()),
        );
        let response = self.client.get(&url).send().await?;
        let payload: Value = response.json().await?;
        let errcode = payload.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if errcode != 0 {
            return Err(anyhow!(
                "wecom gettoken failed: {}",
                payload.get("errmsg").and_then(Value::as_str).unwrap_or("")
            ));
        }
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("wecom gettoken missing access_token"))?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(7200);
        let refresh_in = expires_in.saturating_sub(300).max(60);
        *self.token_cache.lock().await = Some(CachedAccessToken {
            access_token: access_token.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(refresh_in),
        });
        Ok(access_token)
    }

    async fn send_text_rest(&self, touser: &str, text: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let agentid: u32 = self
            .config
            .agent_id
            .trim()
            .parse()
            .map_err(|_| anyhow!("wecom agentId must be a numeric agent id"))?;
        let url = format!(
            "{QYAPI_BASE}/message/send?access_token={}",
            urlencoding::encode(&token)
        );
        let body = json!({
            "touser": touser,
            "msgtype": "text",
            "agentid": agentid,
            "text": { "content": text }
        });
        let response = self.client.post(&url).json(&body).send().await?;
        let payload: Value = response.json().await?;
        let errcode = payload.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if errcode != 0 {
            return Err(anyhow!(
                "wecom message/send: {}",
                payload.get("errmsg").and_then(Value::as_str).unwrap_or("")
            ));
        }
        Ok(())
    }

    async fn run_gateway_loop(self, mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            *self
                .manual_close
                .lock()
                .expect("wecom manual_close lock poisoned") = false;
            match self.clone().run_gateway_session(&mut shutdown_rx).await {
                Ok(()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    if *self
                        .manual_close
                        .lock()
                        .expect("wecom manual_close lock poisoned")
                    {
                        break;
                    }
                    eprintln!("[wecom] gateway session ended; reconnecting");
                }
                Err(err) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[wecom] gateway error: {err}");
                }
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break; }
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    async fn run_gateway_session(self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        let ws_url = self.ws_url();
        eprintln!("[wecom] connecting WebSocket {ws_url}");
        let (mut socket, _) = connect_async(ws_url.as_str())
            .await
            .map_err(|e| anyhow!("wecom ws connect: {e}"))?;

        let auth = json!({
            "cmd": CMD_SUBSCRIBE,
            "headers": { "req_id": Self::generate_req_id(CMD_SUBSCRIBE) },
            "body": {
                "bot_id": self.bot_id(),
                "secret": self.config.secret,
            }
        });
        socket
            .send(Message::Text(auth.to_string().into()))
            .await
            .map_err(|e| anyhow!("wecom auth send: {e}"))?;

        let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
        heartbeat.tick().await;

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let _ = socket.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
                _ = heartbeat.tick() => {
                    let hb = json!({
                        "cmd": CMD_HEARTBEAT,
                        "headers": { "req_id": Self::generate_req_id(CMD_HEARTBEAT) },
                    });
                    if socket.send(Message::Text(hb.to_string().into())).await.is_err() {
                        return Err(anyhow!("wecom heartbeat send failed"));
                    }
                }
                frame = socket.next() => {
                    match frame {
                        Some(Ok(Message::Text(t))) => {
                            self.handle_ws_text(&t).await?;
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = socket.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            return Err(anyhow!("wecom websocket closed"));
                        }
                        Some(Err(e)) => return Err(anyhow!(e)),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_ws_text(&self, text: &str) -> Result<()> {
        let frame: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let cmd = frame.get("cmd").and_then(Value::as_str).unwrap_or("");
        let req_id = frame
            .get("headers")
            .and_then(|h| h.get("req_id"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if req_id.starts_with(CMD_SUBSCRIBE) {
            let errcode = frame.get("errcode").and_then(Value::as_i64).unwrap_or(-1);
            if errcode != 0 {
                return Err(anyhow!(
                    "wecom auth failed: {}",
                    frame.get("errmsg").and_then(Value::as_str).unwrap_or("")
                ));
            }
            eprintln!("[wecom] WebSocket authenticated");
            return Ok(());
        }
        if req_id.starts_with(CMD_HEARTBEAT) {
            return Ok(());
        }

        if cmd == CMD_MSG_CALLBACK {
            self.handle_msg_callback(&frame).await?;
            return Ok(());
        }
        if cmd == CMD_EVENT_CALLBACK {
            self.handle_event_callback(&frame).await?;
            return Ok(());
        }

        Ok(())
    }

    async fn handle_msg_callback(&self, frame: &Value) -> Result<()> {
        let body = frame.get("body").cloned().unwrap_or(Value::Null);
        let msgtype = body.get("msgtype").and_then(Value::as_str).unwrap_or("");
        if msgtype != "text" {
            return Ok(());
        }
        let content = body
            .get("text")
            .and_then(|t| t.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return Ok(());
        }

        let from_info = body.get("from").cloned().unwrap_or(Value::Null);
        let sender_id = from_info
            .get("userid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if sender_id.is_empty() {
            return Ok(());
        }

        let chat_type = body
            .get("chattype")
            .and_then(Value::as_str)
            .unwrap_or("single");
        let chat_id = body
            .get("chatid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let chat_id = if chat_id.is_empty() {
            sender_id.clone()
        } else {
            chat_id
        };

        if !self.base.is_allowed(&sender_id) {
            return Ok(());
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("wecom".to_string(), body.clone());
        metadata.insert("wecom_chat_type".to_string(), json!(chat_type));

        let session_key = Some(format!("wecom:{chat_id}"));

        self.base
            .handle_message(
                self.name(),
                &sender_id,
                &chat_id,
                content,
                None,
                Some(metadata),
                session_key,
            )
            .await
    }

    async fn handle_event_callback(&self, frame: &Value) -> Result<()> {
        let body = frame.get("body").cloned().unwrap_or(Value::Null);
        let event = body.get("event").cloned().unwrap_or(Value::Null);
        let event_type = event.get("eventtype").and_then(Value::as_str).unwrap_or("");
        if event_type == "disconnected_event" {
            *self
                .manual_close
                .lock()
                .expect("wecom manual_close lock poisoned") = true;
            return Err(anyhow!("wecom server disconnected this connection"));
        }
        if event_type != "enter_chat" {
            return Ok(());
        }
        let welcome = self
            .config
            .welcome_message
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let Some(welcome) = welcome else {
            return Ok(());
        };
        let chat_id = body
            .get("chatid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if chat_id.is_empty() {
            return Ok(());
        }
        if let Err(e) = self.send_text_rest(chat_id, welcome).await {
            eprintln!("[wecom] welcome message failed: {e}");
        }
        Ok(())
    }
}

fn rand_u64() -> u64 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1))
        ^ (uuid::Uuid::new_v4().as_u128() as u64)
}

#[async_trait]
impl Channel for WecomChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "wecom"
    }

    fn display_name(&self) -> &'static str {
        "WeCom"
    }

    fn setup_instructions(&self) -> &'static str {
        "WeCom (Enterprise WeChat) uses the AI Bot WebSocket protocol.\n\
         \n\
         1. Log in to https://work.weixin.qq.com admin console\n\
         2. Create a self-built application under 'App Management'\n\
         3. Note the Corp ID (from 'My Enterprise'), Agent ID, and Secret\n\
         4. For the AI Bot WebSocket mode, the Agent ID and Secret are used for auth\n\
         5. Configure xbot:\n\
         \n\
            \"wecom\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"corpId\": \"<your-corp-id>\",\n\
              \"agentId\": \"<your-agent-id>\",\n\
              \"secret\": \"<your-secret>\"\n\
            }\n\
         \n\
         6. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if self.bot_id().is_empty() || self.config.secret.trim().is_empty() {
            self.base.set_running(true);
            return Ok(());
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.shutdown.lock().await = Some(shutdown_tx);
        let channel = self.clone();
        let handle = tokio::spawn(async move {
            channel.run_gateway_loop(shutdown_rx).await;
        });
        *self.gateway_task.lock().await = Some(handle);
        self.base.set_running(true);
        eprintln!("[wecom] gateway task started");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(true);
        }
        if let Some(task) = self.gateway_task.lock().await.take() {
            let _ = task.await;
        }
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if self.config.corp_id.trim().is_empty() || self.config.agent_id.trim().is_empty() {
            return Err(anyhow!("wecom corpId/agentId not configured"));
        }
        for chunk in split_message(&msg.content, WECOM_MAX_MESSAGE_LEN) {
            let trimmed = chunk.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.send_text_rest(&msg.chat_id, trimmed).await?;
        }
        Ok(())
    }
}
