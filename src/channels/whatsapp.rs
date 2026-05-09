//! WhatsApp channel: WebSocket client to a Node.js Baileys bridge.

use std::any::Any;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::split_message;

/// Conservative chunk size for bridge text payloads.
pub const WHATSAPP_MAX_MESSAGE_LEN: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WhatsAppConfig {
    pub enabled: bool,
    #[serde(alias = "bridgeUrl")]
    pub bridge_url: String,
    #[serde(alias = "bridgeToken")]
    pub bridge_token: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "groupPolicy")]
    pub group_policy: String,
}

impl Default for WhatsAppConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bridge_url: "ws://localhost:3001".to_string(),
            bridge_token: String::new(),
            allow_from: Vec::new(),
            group_policy: "open".to_string(),
        }
    }
}

type WsWrite = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

#[derive(Clone)]
pub struct WhatsAppChannel {
    base: ChannelBase,
    config: WhatsAppConfig,
    bridge_write: Arc<AsyncMutex<Option<WsWrite>>>,
    bridge_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    bridge_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    processed_ids: Arc<Mutex<HashSet<String>>>,
}

impl WhatsAppChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: WhatsAppConfig = serde_json::from_value(config)?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            bridge_write: Arc::new(AsyncMutex::new(None)),
            bridge_shutdown: Arc::new(AsyncMutex::new(None)),
            bridge_task: Arc::new(AsyncMutex::new(None)),
            processed_ids: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(WhatsAppConfig::default()).expect("serializable whatsapp config")
    }

    fn remember_id(&self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        let mut guard = self
            .processed_ids
            .lock()
            .expect("whatsapp processed_ids lock poisoned");
        if guard.contains(id) {
            return false;
        }
        if guard.len() > 1000 {
            guard.clear();
        }
        guard.insert(id.to_string());
        true
    }

    async fn run_bridge_loop(self, mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            match self.clone().run_bridge_session(&mut shutdown_rx).await {
                Ok(()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[whatsapp] bridge session ended; reconnecting");
                }
                Err(err) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[whatsapp] bridge error: {err}");
                }
            }
            *self.bridge_write.lock().await = None;
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }

    async fn run_bridge_session(self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        let url = self.config.bridge_url.trim();
        if url.is_empty() {
            return Err(anyhow!("whatsapp bridge_url is empty"));
        }
        eprintln!("[whatsapp] connecting bridge at {url}");
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| anyhow!("whatsapp bridge connect: {e}"))?;
        let (mut write, mut read) = ws.split();

        if !self.config.bridge_token.trim().is_empty() {
            let auth = json!({
                "type": "auth",
                "token": self.config.bridge_token.trim(),
            });
            write
                .send(Message::Text(auth.to_string().into()))
                .await
                .map_err(|e| anyhow!("whatsapp bridge auth send: {e}"))?;
        }

        *self.bridge_write.lock().await = Some(write);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        *self.bridge_write.lock().await = None;
                        return Ok(());
                    }
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(t))) => {
                            if let Err(e) = self.handle_bridge_text(&t).await {
                                eprintln!("[whatsapp] handle message: {e}");
                            }
                        }
                        Some(Ok(Message::Ping(p))) => {
                            if let Some(ref mut w) = *self.bridge_write.lock().await {
                                let _ = w.send(Message::Pong(p)).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            *self.bridge_write.lock().await = None;
                            return Ok(());
                        }
                        Some(Err(e)) => {
                            *self.bridge_write.lock().await = None;
                            return Err(anyhow!(e));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_bridge_text(&self, text: &str) -> Result<()> {
        let v: Value = serde_json::from_str(text)?;
        let msg_type = v.get("type").and_then(Value::as_str).unwrap_or_default();
        if msg_type != "message" {
            return Ok(());
        }

        let message_id = v.get("id").and_then(Value::as_str).unwrap_or_default();
        if !message_id.is_empty() && !self.remember_id(message_id) {
            return Ok(());
        }

        let is_group = v.get("isGroup").and_then(Value::as_bool).unwrap_or(false);
        let was_mentioned = v
            .get("wasMentioned")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_group && self.config.group_policy.eq_ignore_ascii_case("mention") && !was_mentioned {
            return Ok(());
        }

        let pn = v.get("pn").and_then(Value::as_str).unwrap_or_default();
        let sender_full = v.get("sender").and_then(Value::as_str).unwrap_or_default();
        let user_id = if !pn.is_empty() {
            pn.split('@').next().unwrap_or(pn).to_string()
        } else {
            sender_full
                .split('@')
                .next()
                .unwrap_or(sender_full)
                .to_string()
        };
        if user_id.is_empty() {
            return Ok(());
        }

        let chat_id = if !sender_full.is_empty() {
            sender_full.to_string()
        } else {
            pn.to_string()
        };
        if chat_id.is_empty() {
            return Ok(());
        }

        let mut content = v
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if content.is_empty() && v.get("media").is_none() {
            return Ok(());
        }

        if content == "[Voice Message]" {
            content = "[Voice Message: Transcription not available for WhatsApp yet]".to_string();
        }

        let mut media_paths: Vec<String> = Vec::new();
        if let Some(arr) = v.get("media").and_then(Value::as_array) {
            for m in arr {
                if let Some(s) = m.as_str() {
                    media_paths.push(s.to_string());
                }
            }
        }

        if !media_paths.is_empty() {
            let mut tagged = String::new();
            for p in &media_paths {
                let mime = mime_guess::from_path(p).first_raw();
                let media_type = if mime.is_some_and(|m| m.starts_with("image/")) {
                    "image"
                } else {
                    "file"
                };
                let line = format!("[{media_type}: {p}]");
                tagged.push_str(&line);
                tagged.push('\n');
            }
            content = if content.trim().is_empty() {
                tagged.trim_end().to_string()
            } else {
                format!("{}\n{}", content.trim(), tagged.trim_end())
            };
        }

        let mut metadata = std::collections::BTreeMap::new();
        if !message_id.is_empty() {
            metadata.insert("message_id".to_string(), json!(message_id));
        }
        if let Some(ts) = v.get("timestamp") {
            metadata.insert("timestamp".to_string(), ts.clone());
        }
        metadata.insert("is_group".to_string(), json!(is_group));
        metadata.insert("whatsapp".to_string(), v.clone());

        self.base
            .handle_message(
                self.name(),
                &user_id,
                &chat_id,
                content.trim(),
                if media_paths.is_empty() {
                    None
                } else {
                    Some(media_paths)
                },
                Some(metadata),
                None,
            )
            .await
    }

    async fn send_text_chunks(&self, chat_id: &str, text: &str) -> Result<()> {
        let chunks = split_message(text, WHATSAPP_MAX_MESSAGE_LEN);
        if chunks.is_empty() {
            return Ok(());
        }
        let mut guard = self.bridge_write.lock().await;
        let Some(write) = guard.as_mut() else {
            return Err(anyhow!("whatsapp bridge not connected"));
        };
        for chunk in chunks {
            let payload = json!({
                "type": "send",
                "to": chat_id,
                "text": chunk,
            });
            write
                .send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|e| anyhow!("whatsapp send: {e}"))?;
        }
        Ok(())
    }

    async fn send_media_paths(&self, chat_id: &str, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut guard = self.bridge_write.lock().await;
        let Some(write) = guard.as_mut() else {
            return Err(anyhow!("whatsapp bridge not connected"));
        };
        for path in paths {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .as_ref()
                .to_string();
            let file_name = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let payload = json!({
                "type": "send_media",
                "to": chat_id,
                "filePath": path,
                "mimetype": mime,
                "fileName": file_name,
            });
            write
                .send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|e| anyhow!("whatsapp send_media: {e}"))?;
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for WhatsAppChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "whatsapp"
    }

    fn display_name(&self) -> &'static str {
        "WhatsApp"
    }

    fn supports_login(&self) -> bool {
        true
    }

    async fn login(&self, _force: bool) -> Result<bool> {
        eprintln!("[whatsapp] WhatsApp uses a Node.js Baileys bridge for authentication.");
        eprintln!();
        eprintln!("To log in:");
        eprintln!("  1. Ensure Node.js (v18+) is installed");
        eprintln!("  2. Clone the bridge: git clone https://github.com/nicepkg/whatsapp-bridge");
        eprintln!("  3. cd whatsapp-bridge && npm install && npm start");
        eprintln!("  4. Scan the QR code displayed in the bridge terminal with WhatsApp");
        eprintln!("  5. The bridge saves auth state — subsequent starts reconnect automatically");
        eprintln!();
        eprintln!("Bridge URL (default): ws://localhost:3001");
        if !self.config.bridge_url.trim().is_empty() {
            eprintln!("Configured bridge URL: {}", self.config.bridge_url);
        }
        eprintln!();
        eprintln!("Once the bridge is running and authenticated, start xbot with the");
        eprintln!("whatsapp channel enabled. xbot connects to the bridge via WebSocket.");
        Ok(true)
    }

    fn setup_instructions(&self) -> &'static str {
        "WhatsApp requires a Node.js Baileys bridge for authentication.\n\
         \n\
         1. Install Node.js v18+\n\
         2. Clone the bridge: git clone https://github.com/nicepkg/whatsapp-bridge\n\
         3. cd whatsapp-bridge && npm install && npm start\n\
         4. Scan the QR code in the bridge terminal with WhatsApp\n\
         5. Configure xbot:\n\
         \n\
            \"whatsapp\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"bridgeUrl\": \"ws://localhost:3001\"\n\
            }\n\
         \n\
         6. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if self.config.bridge_url.trim().is_empty() {
            self.base.set_running(true);
            return Ok(());
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.bridge_shutdown.lock().await = Some(shutdown_tx);
        let channel = self.clone();
        let handle = tokio::spawn(async move {
            channel.run_bridge_loop(shutdown_rx).await;
        });
        *self.bridge_task.lock().await = Some(handle);
        self.base.set_running(true);
        eprintln!(
            "[whatsapp] bridge task started; group_policy={}",
            self.config.group_policy
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(shutdown) = self.bridge_shutdown.lock().await.take() {
            let _ = shutdown.send(true);
        }
        *self.bridge_write.lock().await = None;
        if let Some(task) = self.bridge_task.lock().await.take() {
            let _ = task.await;
        }
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if !msg.content.trim().is_empty() {
            self.send_text_chunks(&msg.chat_id, &msg.content).await?;
        }
        if !msg.media.is_empty() {
            self.send_media_paths(&msg.chat_id, &msg.media).await?;
        }
        Ok(())
    }
}
