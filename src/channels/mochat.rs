//! Mochat channel: HTTP long-polling against Claw session `watch` / panel `messages` APIs with
//! `X-Claw-Token` auth (aligned with nanobot `mochat.py` fallback workers).

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::{split_message, workspace_state_dir};

pub const MOCHAT_MAX_MESSAGE_LEN: usize = 4000;

const DEFAULT_BASE: &str = "https://mochat.io";
const DEFAULT_WATCH_TIMEOUT_MS: u64 = 25_000;
const DEFAULT_WATCH_LIMIT: u64 = 100;
const DEFAULT_RETRY_DELAY_MS: u64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MochatConfig {
    pub enabled: bool,
    #[serde(alias = "baseUrl")]
    pub base_url: String,
    #[serde(alias = "clawToken")]
    pub claw_token: String,
    pub sessions: Vec<String>,
    pub panels: Vec<String>,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "replyDelayMs")]
    pub reply_delay_ms: Option<u64>,
    #[serde(alias = "watchTimeoutMs")]
    pub watch_timeout_ms: u64,
    #[serde(alias = "watchLimit")]
    pub watch_limit: u64,
    #[serde(alias = "retryDelayMs")]
    pub retry_delay_ms: u64,
}

impl Default for MochatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_BASE.to_string(),
            claw_token: String::new(),
            sessions: Vec::new(),
            panels: Vec::new(),
            allow_from: Vec::new(),
            reply_delay_ms: None,
            watch_timeout_ms: DEFAULT_WATCH_TIMEOUT_MS,
            watch_limit: DEFAULT_WATCH_LIMIT,
            retry_delay_ms: DEFAULT_RETRY_DELAY_MS,
        }
    }
}

#[derive(Clone)]
pub struct MochatChannel {
    base: ChannelBase,
    config: MochatConfig,
    client: Client,
    workers_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
    workers: Arc<AsyncMutex<Vec<JoinHandle<()>>>>,
    session_cursors: Arc<AsyncMutex<BTreeMap<String, i64>>>,
    seen_ids: Arc<AsyncMutex<BTreeMap<String, BTreeSet<String>>>>,
}

impl MochatChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: MochatConfig = serde_json::from_value(config)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("xbot/", env!("CARGO_PKG_VERSION"), " (Mochat)"))
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
            workers_shutdown: Arc::new(AsyncMutex::new(None)),
            workers: Arc::new(AsyncMutex::new(Vec::new())),
            session_cursors: Arc::new(AsyncMutex::new(BTreeMap::new())),
            seen_ids: Arc::new(AsyncMutex::new(BTreeMap::new())),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(MochatConfig::default()).expect("serializable mochat config")
    }

    fn base_url(&self) -> String {
        self.config.base_url.trim_end_matches('/').to_string()
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}/{}", self.base_url(), path.trim_start_matches('/'));
        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Claw-Token", self.config.claw_token.trim())
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let t = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "mochat HTTP {}: {}",
                path,
                t.chars().take(200).collect::<String>()
            ));
        }
        let parsed: Value = response.json().await?;
        if let Some(code) = parsed.get("code").and_then(Value::as_i64) {
            if code != 200 {
                let msg = parsed
                    .get("message")
                    .or_else(|| parsed.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                return Err(anyhow!("mochat API {path}: {msg} (code={code})"));
            }
            return Ok(parsed.get("data").cloned().unwrap_or(Value::Null));
        }
        Ok(parsed)
    }

    fn resolve_target(chat_id: &str) -> (String, bool) {
        let raw = chat_id.trim();
        if raw.is_empty() {
            return (String::new(), false);
        }
        let lower = raw.to_ascii_lowercase();
        if let Some(r) = lower.strip_prefix("mochat:") {
            let cleaned = r.trim();
            if cleaned.is_empty() {
                return (String::new(), false);
            }
            let is_panel = !cleaned.starts_with("session_");
            return (cleaned.to_string(), is_panel);
        }
        for prefix in ["group:", "channel:", "panel:"] {
            if let Some(r) = lower.strip_prefix(prefix) {
                let cleaned = r.trim();
                if cleaned.is_empty() {
                    return (String::new(), false);
                }
                return (cleaned.to_string(), true);
            }
        }
        let is_panel = !raw.starts_with("session_");
        (raw.to_string(), is_panel)
    }

    async fn send_api(&self, msg: &OutboundMessage) -> Result<()> {
        let (id, is_panel) = Self::resolve_target(&msg.chat_id);
        if id.is_empty() {
            return Err(anyhow!("mochat: empty outbound target"));
        }
        let mut text = msg.content.trim().to_string();
        for m in &msg.media {
            if !m.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(m.trim());
            }
        }
        if text.is_empty() {
            return Ok(());
        }
        let reply_to = msg
            .reply_to
            .as_ref()
            .map(|s| s.as_str())
            .or_else(|| msg.metadata.get("message_id").and_then(Value::as_str));
        if is_panel {
            let mut body = json!({
                "panelId": id,
                "content": text,
            });
            if let Some(r) = reply_to {
                body["replyTo"] = json!(r);
            }
            if let Some(g) = msg
                .metadata
                .get("group_id")
                .or_else(|| msg.metadata.get("groupId"))
                .and_then(Value::as_str)
            {
                if !g.is_empty() {
                    body["groupId"] = json!(g);
                }
            }
            let _ = self.post_json("/api/claw/groups/panels/send", body).await?;
        } else {
            let mut body = json!({
                "sessionId": id,
                "content": text,
            });
            if let Some(r) = reply_to {
                body["replyTo"] = json!(r);
            }
            let _ = self.post_json("/api/claw/sessions/send", body).await?;
        }
        Ok(())
    }

    fn normalize_content(content: &Value) -> String {
        match content {
            Value::String(s) => s.trim().to_string(),
            Value::Null => String::new(),
            v => v.to_string(),
        }
    }

    async fn remember_seen(&self, key: &str, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let mut map = self.seen_ids.lock().await;
        let set = map.entry(key.to_string()).or_default();
        if set.contains(message_id) {
            return true;
        }
        set.insert(message_id.to_string());
        while set.len() > 2000 {
            if let Some(first) = set.iter().next().cloned() {
                set.remove(&first);
            } else {
                break;
            }
        }
        false
    }

    async fn handle_message_add(&self, target_id: &str, event: &Value) -> Result<()> {
        let payload = event.get("payload").cloned().unwrap_or(Value::Null);
        let author = payload
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if author.is_empty() {
            return Ok(());
        }
        if !self.base.is_allowed(author) {
            return Ok(());
        }
        let message_id = payload
            .get("messageId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let key = format!("session:{target_id}");
        if self.remember_seen(&key, &message_id).await {
            return Ok(());
        }
        let body = Self::normalize_content(payload.get("content").unwrap_or(&Value::Null));
        let body = if body.is_empty() {
            "[empty message]".to_string()
        } else {
            body
        };
        let mut metadata = BTreeMap::new();
        if !message_id.is_empty() {
            metadata.insert("message_id".to_string(), json!(message_id));
        }
        metadata.insert("mochat_target".to_string(), json!(target_id));
        self.base
            .handle_message(
                "mochat",
                author,
                target_id,
                &body,
                None,
                Some(metadata),
                Some(format!("mochat:{target_id}")),
            )
            .await
    }

    async fn session_watch_worker(
        self,
        session_id: String,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let cursor = self
                .session_cursors
                .lock()
                .await
                .get(&session_id)
                .copied()
                .unwrap_or(0);
            let body = json!({
                "sessionId": session_id,
                "cursor": cursor,
                "timeoutMs": self.config.watch_timeout_ms,
                "limit": self.config.watch_limit,
            });
            let result = self
                .client
                .post(format!("{}/api/claw/sessions/watch", self.base_url()))
                .header("Content-Type", "application/json")
                .header("X-Claw-Token", self.config.claw_token.trim())
                .json(&body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        tokio::select! {
                            _ = shutdown_rx.changed() => { if *shutdown_rx.borrow() { break; } }
                            _ = tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms.max(1))) => {}
                        }
                        continue;
                    }
                    let Ok(payload) = resp.json::<Value>().await else {
                        continue;
                    };
                    let new_cursor = payload
                        .get("cursor")
                        .and_then(Value::as_i64)
                        .unwrap_or(cursor);
                    if new_cursor >= 0 {
                        self.session_cursors
                            .lock()
                            .await
                            .insert(session_id.clone(), new_cursor);
                    }
                    let _ = self.save_cursors().await;
                    if let Some(events) = payload.get("events").and_then(Value::as_array) {
                        for ev in events {
                            if ev.get("type").and_then(Value::as_str) == Some("message.add") {
                                let _ = self.handle_message_add(&session_id, ev).await;
                            }
                        }
                    }
                }
                Err(_) => {
                    tokio::select! {
                        _ = shutdown_rx.changed() => { if *shutdown_rx.borrow() { break; } }
                        _ = tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms.max(1))) => {}
                    }
                }
            }
        }
    }

    async fn panel_poll_worker(self, panel_id: String, mut shutdown_rx: watch::Receiver<bool>) {
        let sleep = Duration::from_millis(1500);
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let limit = self.config.watch_limit.min(100).max(1);
            let body = json!({
                "panelId": panel_id,
                "limit": limit,
            });
            match self
                .post_json("/api/claw/groups/panels/messages", body)
                .await
            {
                Ok(resp) => {
                    let group_id = resp
                        .get("groupId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let msgs = resp
                        .get("messages")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for m in msgs.iter().rev() {
                        let message_id = m
                            .get("messageId")
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        let key = format!("panel:{panel_id}");
                        if self.remember_seen(&key, &message_id).await {
                            continue;
                        }
                        let author = m.get("author").and_then(Value::as_str).unwrap_or("").trim();
                        if author.is_empty() || !self.base.is_allowed(author) {
                            continue;
                        }
                        let content =
                            Self::normalize_content(m.get("content").unwrap_or(&Value::Null));
                        let content = if content.is_empty() {
                            "[empty message]".to_string()
                        } else {
                            content
                        };
                        let mut metadata = BTreeMap::new();
                        if !message_id.is_empty() {
                            metadata.insert("message_id".to_string(), json!(message_id));
                        }
                        if !group_id.is_empty() {
                            metadata.insert("group_id".to_string(), json!(group_id));
                        }
                        let _ = self
                            .base
                            .handle_message(
                                "mochat",
                                author,
                                &panel_id,
                                &content,
                                None,
                                Some(metadata),
                                Some(format!("mochat:{panel_id}")),
                            )
                            .await;
                    }
                }
                Err(e) => {
                    eprintln!("[mochat] panel {panel_id} poll: {e}");
                    tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms.max(1)))
                        .await;
                }
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break; }
                }
                _ = tokio::time::sleep(sleep) => {}
            }
        }
    }

    fn cursors_path(&self) -> PathBuf {
        workspace_state_dir(&self.base.workspace).join("mochat_session_cursors.json")
    }

    async fn load_cursors(&self) -> Result<()> {
        let path = self.cursors_path();
        if !path.exists() {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&path).await?;
        let v: Value = serde_json::from_str(&text)?;
        let Some(map) = v.get("cursors").and_then(Value::as_object) else {
            return Ok(());
        };
        let mut c = self.session_cursors.lock().await;
        for (k, val) in map {
            if let Some(n) = val.as_i64() {
                if n >= 0 {
                    c.insert(k.clone(), n);
                }
            }
        }
        Ok(())
    }

    async fn save_cursors(&self) -> Result<()> {
        let path = self.cursors_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let c = self.session_cursors.lock().await.clone();
        let v = json!({ "cursors": c });
        tokio::fs::write(&path, serde_json::to_string_pretty(&v)?).await?;
        Ok(())
    }

    async fn start_workers(&self) -> Result<()> {
        self.load_cursors().await?;
        let (tx, rx) = watch::channel(false);
        *self.workers_shutdown.lock().await = Some(tx);
        let mut handles = Vec::new();
        for sid in &self.config.sessions {
            let s = sid.trim();
            if s.is_empty() || s == "*" {
                continue;
            }
            let ch = self.clone();
            let id = s.to_string();
            let r = rx.clone();
            handles.push(tokio::spawn(async move {
                ch.session_watch_worker(id, r).await;
            }));
        }
        for pid in &self.config.panels {
            let p = pid.trim();
            if p.is_empty() {
                continue;
            }
            let ch = self.clone();
            let id = p.to_string();
            let r = rx.clone();
            handles.push(tokio::spawn(async move {
                ch.panel_poll_worker(id, r).await;
            }));
        }
        *self.workers.lock().await = handles;
        Ok(())
    }
}

#[async_trait]
impl Channel for MochatChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "mochat"
    }

    fn display_name(&self) -> &'static str {
        "Mochat"
    }

    fn setup_instructions(&self) -> &'static str {
        "Mochat connects to a Mochat/OpenClaw instance via HTTP polling.\n\
         \n\
         1. Obtain a Claw Token from your Mochat or OpenClaw instance admin\n\
         2. Note the session IDs and/or panel IDs you want the bot to monitor\n\
         3. Configure xbot:\n\
         \n\
            \"mochat\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"*\"],\n\
              \"baseUrl\": \"https://your-instance.com\",\n\
              \"clawToken\": \"<your-token>\",\n\
              \"sessions\": [\"session-id-1\"],\n\
              \"panels\": []\n\
            }\n\
         \n\
         4. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if self.config.claw_token.trim().is_empty() {
            self.base.set_running(true);
            return Ok(());
        }
        self.start_workers().await?;
        self.base.set_running(true);
        eprintln!(
            "[mochat] workers started (sessions={}, panels={})",
            self.config.sessions.len(),
            self.config.panels.len()
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.workers_shutdown.lock().await.take() {
            let _ = tx.send(true);
        }
        let mut workers = self.workers.lock().await;
        for h in workers.drain(..) {
            let _ = h.await;
        }
        let _ = self.save_cursors().await;
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if self.config.claw_token.trim().is_empty() {
            return Err(anyhow!("mochat clawToken not configured"));
        }
        if let Some(ms) = self.config.reply_delay_ms {
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
        let chunks = split_message(&msg.content, MOCHAT_MAX_MESSAGE_LEN);
        for (i, chunk) in chunks.iter().enumerate() {
            let mut m = msg.clone();
            m.content = chunk.clone();
            if i > 0 {
                m.media.clear();
            }
            self.send_api(&m).await?;
        }
        Ok(())
    }
}
