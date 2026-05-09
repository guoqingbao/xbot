//! Matrix (Client-Server API v3) channel: long-poll sync and m.room.message handling.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use html2text::from_read;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use url::Url;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::{split_message, workspace_state_dir};

/// Matrix `m.text` / HTML bodies are limited by the event size cap (~65KiB JSON); keep chunks conservative.
pub const MATRIX_MAX_MESSAGE_CHARS: usize = 16_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixConfig {
    pub enabled: bool,
    #[serde(alias = "homeserverUrl")]
    pub homeserver_url: String,
    #[serde(alias = "accessToken")]
    pub access_token: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "userId")]
    pub user_id: String,
}

impl MatrixConfig {
    pub fn default_config() -> Value {
        serde_json::to_value(Self::default()).expect("serializable matrix config")
    }
}

impl Default for MatrixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            homeserver_url: String::new(),
            access_token: String::new(),
            allow_from: Vec::new(),
            user_id: String::new(),
        }
    }
}

struct MatrixInner {
    base: ChannelBase,
    config: MatrixConfig,
    client: Client,
    sync_token: AsyncMutex<Option<String>>,
    shutdown_tx: AsyncMutex<Option<watch::Sender<bool>>>,
    sync_task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl MatrixInner {
    fn sync_state_path(&self) -> PathBuf {
        workspace_state_dir(&self.base.workspace).join("matrix_sync_token.txt")
    }

    fn load_sync_token(&self) -> Option<String> {
        let path = self.sync_state_path();
        std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn save_sync_token(&self, token: &str) -> Result<()> {
        let path = self.sync_state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, token)?;
        Ok(())
    }

    fn homeserver_base(&self) -> Result<String> {
        let url = self.config.homeserver_url.trim();
        if url.is_empty() {
            return Err(anyhow!("matrix homeserver_url not configured"));
        }
        Ok(url.trim_end_matches('/').to_string())
    }

    fn build_sync_url(&self, since: Option<&str>) -> Result<Url> {
        let base = self.homeserver_base()?;
        let mut url = Url::parse(&format!("{}/_matrix/client/v3/sync", base))?;
        url.query_pairs_mut().append_pair("timeout", "30000");
        if let Some(s) = since.filter(|s| !s.is_empty()) {
            url.query_pairs_mut().append_pair("since", s);
        }
        Ok(url)
    }

    fn build_send_url(&self, room_id: &str, txn_id: &str) -> Result<Url> {
        let base = self.homeserver_base()?;
        let mut url = Url::parse(&format!("{}/_matrix/client/v3/rooms/", base))?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid matrix homeserver url"))?
            .push(room_id)
            .push("send")
            .push("m.room.message")
            .push(txn_id);
        Ok(url)
    }

    async fn sync_once(&self, since: Option<&str>) -> Result<Value> {
        let url = self.build_sync_url(since)?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.config.access_token)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "matrix sync failed: HTTP {} — {}",
                status,
                body.chars().take(500).collect::<String>()
            ));
        }
        serde_json::from_str(&body).map_err(|e| anyhow!("matrix sync JSON: {e}: {body}"))
    }

    fn extract_message_text(content: &Value) -> Option<String> {
        let msgtype = content.get("msgtype").and_then(Value::as_str).unwrap_or("");
        if msgtype == "m.room.encrypted" {
            return None;
        }
        if let Some(body) = content.get("body").and_then(Value::as_str) {
            if content.get("format").and_then(Value::as_str) == Some("org.matrix.custom.html")
                && content
                    .get("formatted_body")
                    .and_then(Value::as_str)
                    .is_some()
            {
                let fb = content
                    .get("formatted_body")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let plain = from_read(fb.as_bytes(), 80).unwrap_or_else(|_| body.to_string());
                return Some(plain);
            }
            return Some(body.to_string());
        }
        if let Some(fb) = content.get("formatted_body").and_then(Value::as_str) {
            return Some(from_read(fb.as_bytes(), 80).unwrap_or_else(|_| fb.to_string()));
        }
        None
    }

    async fn process_sync_response(&self, payload: &Value) -> Result<()> {
        let rooms = payload
            .get("rooms")
            .and_then(|r| r.get("join"))
            .and_then(Value::as_object);
        let Some(rooms) = rooms else {
            return Ok(());
        };

        for (room_id, room_data) in rooms {
            let timeline = room_data.get("timeline").and_then(|t| t.get("events"));
            let Some(events) = timeline.and_then(Value::as_array) else {
                continue;
            };
            for ev in events {
                if ev.get("type").and_then(Value::as_str) != Some("m.room.message") {
                    continue;
                }
                let sender = ev.get("sender").and_then(Value::as_str).unwrap_or("");
                if sender == self.config.user_id.trim() {
                    continue;
                }
                let content = ev.get("content").cloned().unwrap_or(Value::Null);
                let Some(text) = Self::extract_message_text(&content) else {
                    continue;
                };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }

                self.base
                    .handle_message("matrix", sender, room_id, text, None, None, None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn run_sync_loop(self: Arc<Self>, shutdown_rx: watch::Receiver<bool>) {
        let mut since = {
            let mut guard = self.sync_token.lock().await;
            if guard.is_none() {
                *guard = self.load_sync_token();
            }
            guard.clone()
        };

        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let since_ref = since.as_deref();
            let payload = match self.sync_once(since_ref).await {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("[matrix] sync error: {err:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if let Err(err) = self.process_sync_response(&payload).await {
                eprintln!("[matrix] process events: {err:#}");
            }

            if let Some(next) = payload
                .get("next_batch")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                since = Some(next.clone());
                *self.sync_token.lock().await = Some(next.clone());
                if let Err(err) = self.save_sync_token(&next) {
                    eprintln!("[matrix] persist sync token: {err:#}");
                }
            }
        }
    }

    async fn send_room_message(&self, room_id: &str, body: &str, html: Option<&str>) -> Result<()> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = self.build_send_url(room_id, &txn_id)?;

        let content = if let Some(formatted) = html.filter(|s| !s.trim().is_empty()) {
            json!({
                "msgtype": "m.text",
                "body": body,
                "format": "org.matrix.custom.html",
                "formatted_body": formatted,
            })
        } else {
            json!({
                "msgtype": "m.text",
                "body": body,
            })
        };

        let response = self
            .client
            .put(url)
            .bearer_auth(&self.config.access_token)
            .json(&content)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "matrix send failed: HTTP {} — {}",
                status,
                body.chars().take(500).collect::<String>()
            ));
        }
        Ok(())
    }
}

pub struct MatrixChannel {
    inner: Arc<MatrixInner>,
}

impl MatrixChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: MatrixConfig = serde_json::from_value(config)?;
        let base = ChannelBase::new(
            serde_json::to_value(&config)?,
            bus,
            workspace,
            transcription_api_key,
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            inner: Arc::new(MatrixInner {
                base,
                config,
                client,
                sync_token: AsyncMutex::new(None),
                shutdown_tx: AsyncMutex::new(None),
                sync_task: AsyncMutex::new(None),
            }),
        })
    }

    pub fn default_config() -> Value {
        MatrixConfig::default_config()
    }
}

#[async_trait]
impl Channel for MatrixChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.inner.base
    }

    fn name(&self) -> &'static str {
        "matrix"
    }

    fn display_name(&self) -> &'static str {
        "Matrix"
    }

    fn setup_instructions(&self) -> &'static str {
        "Matrix uses the CS API v3 with long-poll sync.\n\
         \n\
         1. Create a bot account on your Matrix homeserver\n\
         2. Obtain an access token (e.g. via Element: Settings > Help & About > Access Token)\n\
         3. Note the full user ID (e.g. @bot:example.com)\n\
         4. Invite the bot to the rooms where it should respond\n\
         5. Configure xbot:\n\
         \n\
            \"matrix\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"homeserverUrl\": \"https://matrix.example.com\",\n\
              \"accessToken\": \"<your-access-token>\",\n\
              \"userId\": \"@bot:example.com\"\n\
            }\n\
         \n\
         6. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        let cfg = &self.inner.config;
        if cfg.access_token.trim().is_empty() || cfg.homeserver_url.trim().is_empty() {
            self.inner.base.set_running(true);
            return Ok(());
        }

        let (tx, rx) = watch::channel(false);
        *self.inner.shutdown_tx.lock().await = Some(tx);

        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            inner.run_sync_loop(rx).await;
        });
        *self.inner.sync_task.lock().await = Some(handle);
        self.inner.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.inner.shutdown_tx.lock().await.take() {
            let _ = tx.send(true);
        }
        if let Some(task) = self.inner.sync_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.inner.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if msg.channel != "matrix" {
            return Ok(());
        }

        let html = msg
            .metadata
            .get("formatted_body")
            .or_else(|| msg.metadata.get("html"))
            .and_then(Value::as_str);

        let chunks = split_message(&msg.content, MATRIX_MAX_MESSAGE_CHARS);
        if chunks.is_empty() && html.map(|s| s.trim().is_empty()).unwrap_or(true) {
            return Ok(());
        }

        let room_id = &msg.chat_id;
        if chunks.is_empty() {
            let body = msg.content.clone();
            let html_part = html.map(str::to_string);
            self.inner
                .send_room_message(room_id, &body, html_part.as_deref())
                .await?;
            return Ok(());
        }

        for (i, chunk) in chunks.iter().enumerate() {
            let html_chunk = if chunks.len() == 1 { html } else { None };
            self.inner
                .send_room_message(room_id, chunk, html_chunk)
                .await?;
            if i + 1 < chunks.len() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_body() {
        let c = json!({"msgtype":"m.text","body":"hi"});
        assert_eq!(MatrixInner::extract_message_text(&c).as_deref(), Some("hi"));
    }

    #[test]
    fn skips_encrypted() {
        let c = json!({"msgtype":"m.room.encrypted","body":"x"});
        assert!(MatrixInner::extract_message_text(&c).is_none());
    }
}
