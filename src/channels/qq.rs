//! QQ channel: Tencent QQ Open Platform — WebSocket gateway for inbound events, REST for outbound.

use std::any::Any;
use std::collections::HashMap;
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

/// Conservative text chunk size for QQ bot messages (plain text).
pub const QQ_MAX_MESSAGE_LEN: usize = 2000;

const QQ_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
/// `GROUP_AND_C2C_EVENT` — C2C and group @ messages.
const QQ_INTENTS: u32 = 1 << 25;

struct CachedToken {
    access_token: String,
    expires_at: std::time::Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QqConfig {
    pub enabled: bool,
    #[serde(alias = "appId")]
    pub app_id: String,
    #[serde(alias = "clientSecret")]
    pub secret: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "apiBase")]
    pub api_base: String,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            secret: String::new(),
            allow_from: Vec::new(),
            api_base: "https://api.sgroup.qq.com".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct QqChannel {
    base: ChannelBase,
    config: QqConfig,
    client: Client,
    token_cache: Arc<AsyncMutex<Option<CachedToken>>>,
    gateway_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    gateway_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    last_seq: Arc<Mutex<Option<u64>>>,
    session_id: Arc<Mutex<Option<String>>>,
    heartbeat_seq: Arc<Mutex<Option<u64>>>,
    chat_type_cache: Arc<Mutex<HashMap<String, String>>>,
    msg_seq: Arc<Mutex<u64>>,
}

impl QqChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: QqConfig = serde_json::from_value(config)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("xbot/", env!("CARGO_PKG_VERSION"), " (QQ bot)"))
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
            gateway_shutdown: Arc::new(AsyncMutex::new(None)),
            gateway_task: Arc::new(AsyncMutex::new(None)),
            last_seq: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
            heartbeat_seq: Arc::new(Mutex::new(None)),
            chat_type_cache: Arc::new(Mutex::new(HashMap::new())),
            msg_seq: Arc::new(Mutex::new(0)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(QqConfig::default()).expect("serializable qq config")
    }

    fn api_base(&self) -> String {
        self.config
            .api_base
            .trim()
            .trim_end_matches('/')
            .to_string()
    }

    async fn get_access_token(&self) -> Result<String> {
        if self.config.app_id.trim().is_empty() || self.config.secret.trim().is_empty() {
            return Err(anyhow!("qq app_id/secret not configured"));
        }
        {
            let guard = self.token_cache.lock().await;
            if let Some(cached) = guard.as_ref() {
                if std::time::Instant::now() < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        let body = json!({
            "appId": self.config.app_id,
            "clientSecret": self.config.secret,
        });
        let response = self
            .client
            .post(QQ_TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("qq getAppAccessToken missing access_token"))?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(7200);
        let refresh_in = expires_in.saturating_sub(300).max(60);
        *self.token_cache.lock().await = Some(CachedToken {
            access_token: access_token.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(refresh_in),
        });
        Ok(access_token)
    }

    async fn fetch_gateway_url(&self, access_token: &str) -> Result<String> {
        let url = format!("{}/gateway", self.api_base());
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("QQBot {}", access_token))
            .send()
            .await?;
        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("qq GET /gateway failed: {text}"));
        }
        let payload: Value = response.json().await?;
        payload
            .get("url")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("qq gateway response missing url"))
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
                    eprintln!("[qq] gateway session ended; reconnecting");
                }
                Err(err) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[qq] gateway error: {err}");
                }
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
        }
    }

    async fn run_gateway_session(self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        let access_token = self.get_access_token().await?;
        let ws_url = self.fetch_gateway_url(&access_token).await?;
        eprintln!("[qq] connecting gateway");
        let (ws, _) = connect_async(ws_url.as_str())
            .await
            .map_err(|e| anyhow!("qq gateway connect: {e}"))?;
        let (write, mut read) = ws.split();

        let hello_text = match read.next().await {
            Some(Ok(Message::Text(t))) => t,
            other => return Err(anyhow!("qq expected Hello, got {other:?}")),
        };
        let hello: Value = serde_json::from_str(&hello_text)?;
        if hello.get("op").and_then(|v| v.as_u64()) != Some(10) {
            return Err(anyhow!("qq expected opcode 10 Hello"));
        }
        let heartbeat_interval_ms = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(45_000);

        let write = Arc::new(tokio::sync::Mutex::new(write));
        let hb_seq = self.heartbeat_seq.clone();
        let hb_write = write.clone();
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % heartbeat_interval_ms.max(1) as u128) as u64;

        let hb_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
            loop {
                let d = {
                    let g = hb_seq.lock().expect("qq heartbeat_seq lock poisoned");
                    match *g {
                        Some(s) => json!(s),
                        None => Value::Null,
                    }
                };
                let payload = json!({ "op": 1, "d": d });
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

        let token_header = format!("QQBot {}", access_token);
        let identify_or_resume = {
            let sid = self
                .session_id
                .lock()
                .expect("qq session_id lock poisoned")
                .clone();
            let seq = *self.last_seq.lock().expect("qq last_seq lock poisoned");
            match (sid, seq) {
                (Some(session_id), Some(seq)) => json!({
                    "op": 6,
                    "d": {
                        "token": token_header,
                        "session_id": session_id,
                        "seq": seq,
                    }
                }),
                _ => json!({
                    "op": 2,
                    "d": {
                        "token": token_header,
                        "intents": QQ_INTENTS,
                        "shard": [0, 1],
                        "properties": {
                            "$os": "linux",
                            "$browser": "xbot",
                            "$device": "xbot"
                        }
                    }
                }),
            }
        };

        {
            let mut w = write.lock().await;
            w.send(Message::Text(identify_or_resume.to_string().into()))
                .await
                .map_err(|e| anyhow!("qq identify/resume: {e}"))?;
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        hb_task.abort();
                        let mut w = write.lock().await;
                        let _ = w.send(Message::Close(None)).await;
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
                            return Err(anyhow!("qq gateway closed"));
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
                    *self.last_seq.lock().expect("qq last_seq lock poisoned") = Some(s);
                    *self
                        .heartbeat_seq
                        .lock()
                        .expect("qq heartbeat_seq lock poisoned") = Some(s);
                }
                let t = v.get("t").and_then(|x| x.as_str());
                let d = v.get("d").cloned().unwrap_or(Value::Null);
                match t {
                    Some("READY") => {
                        if let Some(sid) = d.get("session_id").and_then(Value::as_str) {
                            *self.session_id.lock().expect("qq session_id lock poisoned") =
                                Some(sid.to_string());
                            eprintln!("[qq] ready session_id={sid}");
                        }
                    }
                    Some("RESUMED") => {
                        eprintln!("[qq] gateway resumed");
                    }
                    Some("C2C_MESSAGE_CREATE") | Some("DIRECT_MESSAGE_CREATE") => {
                        if let Err(e) = self.handle_c2c_event(&d).await {
                            eprintln!("[qq] C2C handler: {e}");
                        }
                    }
                    Some("GROUP_AT_MESSAGE_CREATE") => {
                        if let Err(e) = self.handle_group_event(&d).await {
                            eprintln!("[qq] group handler: {e}");
                        }
                    }
                    _ => {}
                }
            }
            Some(7) => {
                eprintln!("[qq] gateway opcode 7 RECONNECT");
                hb_task.abort();
                return Err(anyhow!("reconnect"));
            }
            Some(9) => {
                if v.get("d").and_then(|x| x.as_bool()) == Some(false) {
                    *self.session_id.lock().expect("qq session_id lock poisoned") = None;
                    *self.last_seq.lock().expect("qq last_seq lock poisoned") = None;
                }
                hb_task.abort();
                return Err(anyhow!("invalid session"));
            }
            Some(11) => {}
            _ => {}
        }
        Ok(())
    }

    fn author_user_openid(d: &Value) -> Option<String> {
        d.get("author")
            .and_then(|a| {
                a.get("user_openid")
                    .or_else(|| a.get("id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .or_else(|| {
                d.get("user_openid")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    }

    fn author_member_openid(d: &Value) -> Option<String> {
        d.get("author")
            .and_then(|a| a.get("member_openid").and_then(Value::as_str))
            .map(str::to_string)
    }

    async fn handle_c2c_event(&self, d: &Value) -> Result<()> {
        let content = d
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return Ok(());
        }
        let Some(user_id) = Self::author_user_openid(d) else {
            return Ok(());
        };
        let chat_id = Self::author_user_openid(d)
            .or_else(|| d.get("openid").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| user_id.clone());

        self.chat_type_cache
            .lock()
            .expect("qq chat_type_cache lock poisoned")
            .insert(chat_id.clone(), "c2c".to_string());

        let mut metadata = std::collections::BTreeMap::new();
        if let Some(id) = d.get("id").and_then(Value::as_str) {
            metadata.insert("message_id".to_string(), json!(id));
        }
        metadata.insert("qq_chat_type".to_string(), json!("c2c"));
        metadata.insert("qq".to_string(), d.clone());

        self.base
            .handle_message(
                self.name(),
                &user_id,
                &chat_id,
                content,
                None,
                Some(metadata),
                None,
            )
            .await
    }

    async fn handle_group_event(&self, d: &Value) -> Result<()> {
        let content = d
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return Ok(());
        }
        let Some(group_openid) = d.get("group_openid").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(user_id) = Self::author_member_openid(d) else {
            return Ok(());
        };

        self.chat_type_cache
            .lock()
            .expect("qq chat_type_cache lock poisoned")
            .insert(group_openid.to_string(), "group".to_string());

        let mut metadata = std::collections::BTreeMap::new();
        if let Some(id) = d.get("id").and_then(Value::as_str) {
            metadata.insert("message_id".to_string(), json!(id));
        }
        metadata.insert("qq_chat_type".to_string(), json!("group"));
        metadata.insert("qq".to_string(), d.clone());

        self.base
            .handle_message(
                self.name(),
                &user_id,
                group_openid,
                content,
                None,
                Some(metadata),
                None,
            )
            .await
    }

    fn is_group_chat(&self, chat_id: &str) -> bool {
        self.chat_type_cache
            .lock()
            .expect("qq chat_type_cache lock poisoned")
            .get(chat_id)
            .map(|s| s == "group")
            .unwrap_or(false)
    }

    async fn post_text_message(
        &self,
        access_token: &str,
        chat_id: &str,
        content: &str,
        msg_id: Option<&str>,
    ) -> Result<()> {
        let is_group = self.is_group_chat(chat_id);
        let base = self.api_base();
        let url = if is_group {
            format!("{base}/v2/groups/{chat_id}/messages")
        } else {
            format!("{base}/v2/users/{chat_id}/messages")
        };
        let seq = {
            let mut g = self.msg_seq.lock().expect("qq msg_seq lock poisoned");
            *g += 1;
            *g
        };
        let mut body = json!({
            "msg_type": 0,
            "msg_seq": seq,
            "content": content,
        });
        if let Some(mid) = msg_id.filter(|s| !s.is_empty()) {
            body["msg_id"] = json!(mid);
        }
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("QQBot {}", access_token))
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("qq send message failed: {text}"));
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for QqChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "qq"
    }

    fn display_name(&self) -> &'static str {
        "QQ"
    }

    fn setup_instructions(&self) -> &'static str {
        "QQ uses the QQ Bot API with WebSocket gateway.\n\
         \n\
         1. Go to https://q.qq.com and register as a QQ Bot developer\n\
         2. Create a bot application and obtain the App ID and Secret\n\
         3. Configure the bot's intents and permissions in the developer console\n\
         4. Configure xbot:\n\
         \n\
            \"qq\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"appId\": \"<your-app-id>\",\n\
              \"secret\": \"<your-secret>\"\n\
            }\n\
         \n\
         5. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if self.config.app_id.trim().is_empty() || self.config.secret.trim().is_empty() {
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
        eprintln!("[qq] gateway task started");
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
        let access_token = self.get_access_token().await?;
        let msg_id = msg.metadata.get("message_id").and_then(Value::as_str);
        let mid = msg_id.or_else(|| {
            msg.metadata
                .get("qq")
                .and_then(|q| q.get("id"))
                .and_then(Value::as_str)
        });
        for chunk in split_message(&msg.content, QQ_MAX_MESSAGE_LEN) {
            self.post_text_message(&access_token, &msg.chat_id, &chunk, mid)
                .await?;
        }
        Ok(())
    }
}
