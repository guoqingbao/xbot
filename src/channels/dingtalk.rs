//! DingTalk (钉钉) channel via Stream mode (WebSocket) and OpenAPI.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
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
use url::Url;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::split_message;

/// DingTalk single-message text limit for batch send (conservative; official templates vary).
pub const DINGTALK_MAX_MESSAGE_LEN: usize = 2000;

const CONNECTIONS_OPEN_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const GET_TOKEN_URL: &str = "https://oapi.dingtalk.com/gettoken";
const BATCH_SEND_URL: &str = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend";

const METADATA_USER_ID: &str = "dingtalk_user_id";
const METADATA_MSGTYPE: &str = "dingtalk_msgtype";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DingTalkConfig {
    pub enabled: bool,
    #[serde(alias = "appKey")]
    pub app_key: String,
    #[serde(alias = "appSecret")]
    pub app_secret: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "robotCode")]
    pub robot_code: String,
}

impl Default for DingTalkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_key: String::new(),
            app_secret: String::new(),
            allow_from: Vec::new(),
            robot_code: String::new(),
        }
    }
}

struct CachedToken {
    access_token: String,
    expires_at: std::time::Instant,
}

#[derive(Clone)]
pub struct DingTalkChannel {
    base: ChannelBase,
    config: DingTalkConfig,
    client: Client,
    stream_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    stream_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    token_cache: Arc<AsyncMutex<Option<CachedToken>>>,
}

impl DingTalkChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: DingTalkConfig = serde_json::from_value(config)?;
        let client = Client::builder().timeout(Duration::from_secs(60)).build()?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            client,
            stream_task: Arc::new(AsyncMutex::new(None)),
            stream_shutdown: Arc::new(AsyncMutex::new(None)),
            token_cache: Arc::new(AsyncMutex::new(None)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(DingTalkConfig::default()).expect("serializable dingtalk config")
    }

    async fn get_access_token(&self) -> Result<String> {
        if self.config.app_key.trim().is_empty() || self.config.app_secret.trim().is_empty() {
            return Err(anyhow!("dingtalk appKey/appSecret not configured"));
        }
        {
            let guard = self.token_cache.lock().await;
            if let Some(cached) = guard.as_ref() {
                if std::time::Instant::now() < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        let url = Url::parse_with_params(
            GET_TOKEN_URL,
            &[
                ("appkey", self.config.app_key.as_str()),
                ("appsecret", self.config.app_secret.as_str()),
            ],
        )?;
        let response = self.client.get(url).send().await?;
        let payload: Value = response.json().await?;
        let errcode = payload.get("errcode").and_then(Value::as_i64).unwrap_or(-1);
        if errcode != 0 {
            return Err(anyhow!(
                "dingtalk gettoken failed: errcode={} {}",
                errcode,
                payload.get("errmsg").and_then(Value::as_str).unwrap_or("")
            ));
        }
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("dingtalk gettoken response missing access_token"))?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(7200);
        let refresh_in = expires_in.saturating_sub(300).max(60);
        let cached = CachedToken {
            access_token: access_token.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(refresh_in),
        };
        *self.token_cache.lock().await = Some(cached);
        Ok(access_token)
    }

    async fn open_stream_ws_url(&self) -> Result<String> {
        let local_ip = local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let body = json!({
            "clientId": self.config.app_key,
            "clientSecret": self.config.app_secret,
            "localIp": local_ip,
            "subscriptions": [
                { "topic": "*", "type": "EVENT" },
                { "topic": "/v1.0/im/bot/messages/get", "type": "CALLBACK" }
            ],
            "ua": "xbot-dingtalk/0.1"
        });
        let response = self
            .client
            .post(CONNECTIONS_OPEN_URL)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("dingtalk connections/open failed: {text}"));
        }
        let payload: Value = response.json().await?;
        let endpoint = payload
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("dingtalk connections/open missing endpoint"))?;
        let ticket = payload
            .get("ticket")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("dingtalk connections/open missing ticket"))?;
        let mut url = Url::parse(endpoint)
            .map_err(|e| anyhow!("invalid stream endpoint URL {endpoint}: {e}"))?;
        url.query_pairs_mut().append_pair("ticket", ticket);
        Ok(url.to_string())
    }

    async fn run_stream_mode(self, mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            match self.run_stream_session(&mut shutdown_rx).await {
                Ok(()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[dingtalk] stream disconnected; reconnecting");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(err) => {
                    eprintln!("[dingtalk] stream error: {err}");
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn run_stream_session(&self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        let ws_url = self.open_stream_ws_url().await?;
        eprintln!("[dingtalk] connecting stream");
        let (mut socket, _) = connect_async(ws_url.as_str()).await?;
        eprintln!("[dingtalk] stream connected");

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let _ = socket.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
                frame = socket.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_stream_text(&mut socket, &text).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            socket.send(Message::Pong(payload)).await?;
                        }
                        Some(Ok(Message::Close(_))) => return Ok(()),
                        Some(Ok(_)) => {}
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    async fn handle_stream_text<S>(&self, socket: &mut S, text: &str) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let payload: Value = serde_json::from_str(text)?;
        let msg_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let headers = payload.get("headers").cloned().unwrap_or(Value::Null);
        let message_id = headers
            .get("messageId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let topic = headers
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match msg_type {
            "SYSTEM" if topic == "ping" => {
                let data_str = payload.get("data").and_then(Value::as_str).unwrap_or("{}");
                let data: Value = serde_json::from_str(data_str).unwrap_or(json!({}));
                let opaque = data
                    .get("opaque")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let ack_data = serde_json::to_string(&json!({ "opaque": opaque }))?;
                let ack = json!({
                    "code": 200,
                    "message": "OK",
                    "headers": {
                        "messageId": message_id,
                        "contentType": "application/json"
                    },
                    "data": ack_data
                });
                socket.send(Message::Text(ack.to_string().into())).await?;
            }
            "SYSTEM" if topic == "disconnect" => {
                eprintln!("[dingtalk] server requested disconnect; closing");
                let _ = socket.send(Message::Close(None)).await;
                return Ok(());
            }
            "CALLBACK" if topic == "/v1.0/im/bot/messages/get" => {
                self.handle_bot_callback(&payload).await?;
                let ack = json!({
                    "code": 200,
                    "message": "OK",
                    "headers": {
                        "messageId": message_id,
                        "contentType": "application/json"
                    },
                    "data": "{\"response\": null}"
                });
                socket.send(Message::Text(ack.to_string().into())).await?;
            }
            "EVENT" => {
                let ack_data = serde_json::to_string(&json!({
                    "status": "SUCCESS",
                    "message": "ok"
                }))?;
                let ack = json!({
                    "code": 200,
                    "message": "OK",
                    "headers": {
                        "messageId": message_id,
                        "contentType": "application/json"
                    },
                    "data": ack_data
                });
                socket.send(Message::Text(ack.to_string().into())).await?;
            }
            _ => {
                if !message_id.is_empty() {
                    let ack = json!({
                        "code": 404,
                        "message": "topic not handled",
                        "headers": {
                            "messageId": message_id,
                            "contentType": "application/json"
                        },
                        "data": "{}"
                    });
                    socket.send(Message::Text(ack.to_string().into())).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_bot_callback(&self, envelope: &Value) -> Result<()> {
        let data_str = envelope
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("bot callback missing data"))?;
        let data: Value = serde_json::from_str(data_str)?;

        let sender_id = data
            .get("senderId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if sender_id.is_empty() {
            return Ok(());
        }

        let conversation_id = data
            .get("conversationId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let chat_id = if conversation_id.is_empty() {
            sender_id.clone()
        } else {
            conversation_id
        };

        let msgtype = data
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or("text");

        let content = match msgtype {
            "markdown" => data
                .get("markdown")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .or_else(|| {
                    data.get("markdown")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string())
                })
                .unwrap_or_default(),
            _ => data
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };

        let content = content.trim().to_string();
        if content.is_empty() {
            return Ok(());
        }

        let mut metadata = BTreeMap::new();
        metadata.insert(METADATA_USER_ID.to_string(), json!(sender_id));
        metadata.insert(METADATA_MSGTYPE.to_string(), json!(msgtype));
        if let Some(cid) = data.get("conversationId").and_then(Value::as_str) {
            metadata.insert("dingtalk_conversation_id".to_string(), json!(cid));
        }
        if let Some(mid) = data.get("msgId").and_then(Value::as_str) {
            metadata.insert("dingtalk_msg_id".to_string(), json!(mid));
        }

        self.base
            .handle_message(
                self.name(),
                &sender_id,
                &chat_id,
                &content,
                None,
                Some(metadata),
                None,
            )
            .await
    }

    fn outbound_user_id(msg: &OutboundMessage) -> Option<String> {
        msg.metadata
            .get(METADATA_USER_ID)
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or_else(|| {
                msg.metadata
                    .get("dingtalk_user_id")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
    }

    fn outbound_use_markdown(msg: &OutboundMessage) -> bool {
        msg.metadata
            .get(METADATA_MSGTYPE)
            .or_else(|| msg.metadata.get("msgtype"))
            .and_then(Value::as_str)
            .map(|s| s.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false)
    }

    async fn batch_send_chunk(
        &self,
        access_token: &str,
        user_id: &str,
        text: &str,
        markdown: bool,
    ) -> Result<()> {
        let robot_code = self.config.robot_code.trim();
        if robot_code.is_empty() {
            return Err(anyhow!("dingtalk robotCode not configured"));
        }
        let (msg_key, msg_param_obj) = if markdown {
            let title = text
                .lines()
                .next()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or("Message");
            ("sampleMarkdown", json!({ "title": title, "text": text }))
        } else {
            ("sampleText", json!({ "content": text }))
        };
        let msg_param = serde_json::to_string(&msg_param_obj)?;
        let body = json!({
            "robotCode": robot_code,
            "userIds": [user_id],
            "msgKey": msg_key,
            "msgParam": msg_param
        });
        let response = self
            .client
            .post(BATCH_SEND_URL)
            .header("x-acs-dingtalk-access-token", access_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let err_body = response.text().await.unwrap_or_default();
            return Err(anyhow!("dingtalk batchSend failed: {err_body}"));
        }
        let payload: Value = response.json().await?;
        if let Some(success) = payload.get("success").and_then(Value::as_bool) {
            if !success {
                return Err(anyhow!("dingtalk batchSend unsuccessful: {payload}"));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "dingtalk"
    }

    fn display_name(&self) -> &'static str {
        "DingTalk"
    }

    fn setup_instructions(&self) -> &'static str {
        "DingTalk uses the Stream gateway protocol.\n\
         \n\
         1. Go to https://open-dev.dingtalk.com and create a robot application\n\
         2. Under 'Credentials', copy the Client ID (AppKey) and Client Secret (AppSecret)\n\
         3. Under 'Robot', enable the robot and copy the Robot Code\n\
         4. Configure xbot:\n\
         \n\
            \"dingtalk\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"clientId\": \"<AppKey>\",\n\
              \"clientSecret\": \"<AppSecret>\",\n\
              \"robotCode\": \"<RobotCode>\"\n\
            }\n\
         \n\
         5. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if !self.config.app_key.trim().is_empty() && !self.config.app_secret.trim().is_empty() {
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            *self.stream_shutdown.lock().await = Some(shutdown_tx);
            let channel = self.clone();
            let handle = tokio::spawn(async move {
                channel.run_stream_mode(shutdown_rx).await;
            });
            *self.stream_task.lock().await = Some(handle);
            eprintln!("[dingtalk] stream mode started");
        }
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(shutdown) = self.stream_shutdown.lock().await.take() {
            let _ = shutdown.send(true);
        }
        if let Some(task) = self.stream_task.lock().await.take() {
            let _ = task.await;
        }
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let Some(user_id) = Self::outbound_user_id(&msg) else {
            return Err(anyhow!(
                "dingtalk outbound requires metadata '{}' (sender id from inbound)",
                METADATA_USER_ID
            ));
        };
        let access_token = self.get_access_token().await?;
        let markdown = Self::outbound_use_markdown(&msg);
        for chunk in split_message(&msg.content, DINGTALK_MAX_MESSAGE_LEN) {
            self.batch_send_chunk(&access_token, &user_id, &chunk, markdown)
                .await?;
        }
        Ok(())
    }
}
