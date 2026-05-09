//! Personal WeChat channel: HTTP long-poll against `ilinkai.weixin.qq.com` (aligned with
//! `@tencent-weixin/openclaw-weixin`). QR login persists token under `state_dir`.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use url::Url;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};
use crate::util::split_message;

pub const WEIXIN_MAX_MESSAGE_LEN: usize = 4000;

const ITEM_TEXT: i64 = 1;
const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;
const ERRCODE_SESSION_EXPIRED: i64 = -14;
const SESSION_PAUSE_DURATION_S: u64 = 60 * 60;

const CHANNEL_VERSION: &str = "1.0.3";

fn default_base_url() -> String {
    "https://ilinkai.weixin.qq.com".to_string()
}

fn default_state_dir() -> String {
    dirs::home_dir()
        .map(|h| h.join(".xbot/weixin").to_string_lossy().into_owned())
        .unwrap_or_else(|| "~/.xbot/weixin".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeixinConfig {
    pub enabled: bool,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "baseUrl")]
    pub base_url: String,
    #[serde(alias = "cdnBaseUrl")]
    pub cdn_base_url: String,
    #[serde(alias = "routeTag")]
    pub route_tag: Option<String>,
    pub token: String,
    #[serde(alias = "stateDir")]
    pub state_dir: String,
    #[serde(alias = "pollTimeout")]
    pub poll_timeout: u64,
}

impl Default for WeixinConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_from: Vec::new(),
            base_url: default_base_url(),
            cdn_base_url: "https://novac2c.cdn.weixin.qq.com/c2c".to_string(),
            route_tag: None,
            token: String::new(),
            state_dir: default_state_dir(),
            poll_timeout: 35,
        }
    }
}

#[derive(Clone)]
pub struct WeixinChannel {
    base: ChannelBase,
    config: WeixinConfig,
    client: Arc<AsyncMutex<Option<Client>>>,
    inner: Arc<WeixinInner>,
    poll_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
}

struct WeixinInner {
    token: Mutex<String>,
    get_updates_buf: Mutex<String>,
    context_tokens: Mutex<BTreeMap<String, String>>,
    processed_ids: Mutex<VecDeque<String>>,
    session_pause_until: Mutex<std::time::Instant>,
    next_poll_timeout_s: Mutex<u64>,
}

impl WeixinChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: WeixinConfig = serde_json::from_value(config)?;
        let poll_timeout_init = config.poll_timeout.max(5);
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            client: Arc::new(AsyncMutex::new(None)),
            inner: Arc::new(WeixinInner {
                token: Mutex::new(String::new()),
                get_updates_buf: Mutex::new(String::new()),
                context_tokens: Mutex::new(BTreeMap::new()),
                processed_ids: Mutex::new(VecDeque::new()),
                session_pause_until: Mutex::new(std::time::Instant::now()),
                next_poll_timeout_s: Mutex::new(poll_timeout_init),
            }),
            poll_task: Arc::new(AsyncMutex::new(None)),
            shutdown: Arc::new(AsyncMutex::new(None)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(WeixinConfig::default()).expect("serializable weixin config")
    }

    fn expand_state_dir(s: &str) -> PathBuf {
        let t = s.trim();
        if t.is_empty() {
            return dirs::home_dir()
                .map(|h| h.join(".xbot/weixin"))
                .unwrap_or_else(|| PathBuf::from(".xbot/weixin"));
        }
        if let Some(rest) = t.strip_prefix("~/") {
            if let Some(h) = dirs::home_dir() {
                return h.join(rest);
            }
        }
        PathBuf::from(t)
    }

    fn state_dir_path(&self) -> PathBuf {
        Self::expand_state_dir(&self.config.state_dir)
    }

    fn account_json_path(&self) -> PathBuf {
        self.state_dir_path().join("account.json")
    }

    fn load_state(&self) -> Result<()> {
        let path = self.account_json_path();
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path)?;
        let data: Value = serde_json::from_str(&text)?;
        if let Some(t) = data.get("token").and_then(Value::as_str) {
            *self.inner.token.lock().expect("weixin token lock") = t.to_string();
        }
        if let Some(b) = data.get("get_updates_buf").and_then(Value::as_str) {
            *self.inner.get_updates_buf.lock().expect("weixin buf lock") = b.to_string();
        }
        if let Some(m) = data.get("context_tokens").and_then(Value::as_object) {
            let mut ctx = self.inner.context_tokens.lock().expect("weixin ctx lock");
            ctx.clear();
            for (k, v) in m {
                if let Some(vs) = v.as_str() {
                    if !k.is_empty() && !vs.is_empty() {
                        ctx.insert(k.clone(), vs.to_string());
                    }
                }
            }
        }
        Ok(())
    }

    fn save_state(&self) -> Result<()> {
        let dir = self.state_dir_path();
        std::fs::create_dir_all(&dir)?;
        let data = json!({
            "token": self.inner.token.lock().expect("weixin token lock").clone(),
            "get_updates_buf": self.inner.get_updates_buf.lock().expect("weixin buf lock").clone(),
            "context_tokens": self.inner.context_tokens.lock().expect("weixin ctx lock").clone(),
            "base_url": self.config.base_url,
        });
        std::fs::write(
            self.account_json_path(),
            serde_json::to_string_pretty(&data)?,
        )?;
        Ok(())
    }

    fn random_wechat_uin() -> String {
        let n: u32 = uuid::Uuid::new_v4().as_u128() as u32;
        base64::engine::general_purpose::STANDARD.encode(n.to_string().as_bytes())
    }

    fn make_headers(&self, auth: bool) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert("X-WECHAT-UIN".to_string(), Self::random_wechat_uin());
        h.insert("Content-Type".to_string(), "application/json".to_string());
        h.insert(
            "AuthorizationType".to_string(),
            "ilink_bot_token".to_string(),
        );
        if auth {
            let tok = self.inner.token.lock().expect("weixin token lock").clone();
            if !tok.is_empty() {
                h.insert("Authorization".to_string(), format!("Bearer {tok}"));
            }
        }
        if let Some(ref tag) = self.config.route_tag {
            let t = tag.trim();
            if !t.is_empty() {
                h.insert("SKRouteTag".to_string(), t.to_string());
            }
        }
        h
    }

    async fn api_post_json(
        &self,
        client: &Client,
        endpoint: &str,
        mut body: Value,
    ) -> Result<Value> {
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/{endpoint}");
        if body.get("base_info").is_none() {
            body["base_info"] = json!({ "channel_version": CHANNEL_VERSION });
        }
        let mut req = client.post(&url).json(&body);
        for (k, v) in self.make_headers(true) {
            req = req.header(&k, v);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("weixin HTTP {} {}", endpoint, response.status()));
        }
        Ok(response.json().await?)
    }

    async fn api_get_json(
        &self,
        client: &Client,
        endpoint: &str,
        params: &[(&str, &str)],
        auth: bool,
        extra: Option<(&str, &str)>,
    ) -> Result<Value> {
        let base = self.config.base_url.trim_end_matches('/');
        let mut url = Url::parse(&format!("{base}/{endpoint}"))?;
        for (k, v) in params {
            url.query_pairs_mut().append_pair(k, v);
        }
        let mut req = client.get(url);
        for (k, v) in self.make_headers(auth) {
            req = req.header(&k, v);
        }
        if let Some((ek, ev)) = extra {
            req = req.header(ek, ev);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "weixin HTTP GET {} {}",
                endpoint,
                response.status()
            ));
        }
        Ok(response.json().await?)
    }

    async fn fetch_qr_code(&self, client: &Client) -> Result<(String, String)> {
        let data = self
            .api_get_json(
                client,
                "ilink/bot/get_bot_qrcode",
                &[("bot_type", "3")],
                false,
                None,
            )
            .await?;
        let qrcode_id = data
            .get("qrcode")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("weixin: no qrcode in response"))?
            .to_string();
        let scan = data
            .get("qrcode_img_content")
            .and_then(Value::as_str)
            .unwrap_or(&qrcode_id)
            .to_string();
        Ok((qrcode_id, scan))
    }

    async fn qr_login(&self, client: &Client) -> Result<bool> {
        let (mut qrcode_id, scan_url) = self.fetch_qr_code(client).await?;
        eprintln!("[weixin] Open this URL in WeChat to log in:\n{scan_url}\n");
        let mut refresh = 0_u32;
        loop {
            let status_data = self
                .api_get_json(
                    client,
                    "ilink/bot/get_qrcode_status",
                    &[("qrcode", qrcode_id.as_str())],
                    false,
                    Some(("iLink-App-ClientVersion", "1")),
                )
                .await;
            let status_data = match status_data {
                Ok(s) => s,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let status = status_data
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            if status == "confirmed" {
                let token = status_data
                    .get("bot_token")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if token.is_empty() {
                    return Ok(false);
                }
                *self.inner.token.lock().expect("weixin token lock") = token.to_string();
                self.save_state()?;
                eprintln!("[weixin] login successful");
                return Ok(true);
            }
            if status == "scaned" {
                eprintln!("[weixin] QR scanned, waiting for confirmation...");
            } else if status == "expired" {
                refresh += 1;
                if refresh > 3 {
                    return Ok(false);
                }
                let (id, url) = self.fetch_qr_code(client).await?;
                qrcode_id = id;
                eprintln!("[weixin] QR expired, new URL:\n{url}\n");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn session_pause_remaining(&self) -> u64 {
        let until = *self
            .inner
            .session_pause_until
            .lock()
            .expect("weixin pause lock");
        let now = std::time::Instant::now();
        if until > now {
            until.duration_since(now).as_secs()
        } else {
            0
        }
    }

    fn pause_session(&self) {
        *self
            .inner
            .session_pause_until
            .lock()
            .expect("weixin pause lock") =
            std::time::Instant::now() + Duration::from_secs(SESSION_PAUSE_DURATION_S);
    }

    async fn poll_once(&self, client: &Client) -> Result<()> {
        let remaining = self.session_pause_remaining();
        if remaining > 0 {
            tokio::time::sleep(Duration::from_secs(remaining)).await;
            return Ok(());
        }
        let buf = self
            .inner
            .get_updates_buf
            .lock()
            .expect("weixin buf lock")
            .clone();
        let body = json!({ "get_updates_buf": buf });
        let data = self
            .api_post_json(client, "ilink/bot/getupdates", body)
            .await?;

        let ret = data.get("ret").and_then(Value::as_i64).unwrap_or(0);
        let errcode = data.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        let is_error = ret != 0 || errcode != 0;
        if is_error {
            if errcode == ERRCODE_SESSION_EXPIRED || ret == ERRCODE_SESSION_EXPIRED {
                self.pause_session();
                eprintln!("[weixin] session expired; pausing polls");
                return Ok(());
            }
            return Err(anyhow!(
                "getupdates failed: ret={ret} errcode={errcode} errmsg={}",
                data.get("errmsg").and_then(Value::as_str).unwrap_or("")
            ));
        }

        if let Some(ms) = data.get("longpolling_timeout_ms").and_then(Value::as_u64) {
            if ms > 0 {
                *self
                    .inner
                    .next_poll_timeout_s
                    .lock()
                    .expect("weixin poll timeout lock") = (ms / 1000).max(5);
            }
        }
        if let Some(nb) = data.get("get_updates_buf").and_then(Value::as_str) {
            if !nb.is_empty() {
                *self.inner.get_updates_buf.lock().expect("weixin buf lock") = nb.to_string();
                let _ = self.save_state();
            }
        }

        let msgs = data
            .get("msgs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for msg in msgs {
            if let Err(e) = self.process_inbound_message(msg).await {
                eprintln!("[weixin] process message: {e}");
            }
        }
        Ok(())
    }

    async fn process_inbound_message(&self, msg: Value) -> Result<()> {
        if msg.get("message_type").and_then(Value::as_i64) == Some(MESSAGE_TYPE_BOT) {
            return Ok(());
        }
        let msg_id = msg
            .get("message_id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or_else(|| msg.get("seq").map(|v| v.to_string()))
            .unwrap_or_default();
        let msg_id = if msg_id.is_empty() {
            format!(
                "{}_{}",
                msg.get("from_user_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                msg.get("create_time_ms")
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            )
        } else {
            msg_id
        };
        {
            let mut seen = self.inner.processed_ids.lock().expect("weixin seen lock");
            if seen.iter().any(|x| x == &msg_id) {
                return Ok(());
            }
            seen.push_back(msg_id.clone());
            while seen.len() > 1000 {
                seen.pop_front();
            }
        }

        let from_user_id = msg
            .get("from_user_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if from_user_id.is_empty() {
            return Ok(());
        }

        if let Some(ctx) = msg.get("context_token").and_then(Value::as_str) {
            if !ctx.is_empty() {
                self.inner
                    .context_tokens
                    .lock()
                    .expect("weixin ctx lock")
                    .insert(from_user_id.clone(), ctx.to_string());
                let _ = self.save_state();
            }
        }

        let item_list = msg
            .get("item_list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut parts = Vec::new();
        for item in &item_list {
            let item_type = item.get("type").and_then(Value::as_i64).unwrap_or(0);
            if item_type == ITEM_TEXT {
                let text = item
                    .get("text_item")
                    .and_then(|t| t.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
        let content = parts.join("\n");
        let content = content.trim();
        if content.is_empty() {
            return Ok(());
        }
        if !self.base.is_allowed(&from_user_id) {
            return Ok(());
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("message_id".to_string(), json!(msg_id));

        self.base
            .handle_message(
                "weixin",
                &from_user_id,
                &from_user_id,
                content,
                None,
                Some(metadata),
                Some(format!("weixin:{from_user_id}")),
            )
            .await
    }

    async fn send_text(
        &self,
        client: &Client,
        to_user_id: &str,
        text: &str,
        context_token: &str,
    ) -> Result<()> {
        let client_id = format!("xbot-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let item_list = if text.is_empty() {
            vec![]
        } else {
            vec![json!({
                "type": ITEM_TEXT,
                "text_item": { "text": text }
            })]
        };
        let mut weixin_msg = json!({
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": client_id,
            "message_type": MESSAGE_TYPE_BOT,
            "message_state": MESSAGE_STATE_FINISH,
        });
        if !item_list.is_empty() {
            weixin_msg["item_list"] = json!(item_list);
        }
        if !context_token.is_empty() {
            weixin_msg["context_token"] = json!(context_token);
        }
        let body = json!({ "msg": weixin_msg });
        let data = self
            .api_post_json(client, "ilink/bot/sendmessage", body)
            .await?;
        let errcode = data.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if errcode != 0 {
            eprintln!(
                "[weixin] sendmessage errcode={errcode} {:?}",
                data.get("errmsg")
            );
        }
        Ok(())
    }

    async fn run_poll_loop(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        let client = match self.client.lock().await.clone() {
            Some(c) => c,
            None => return,
        };
        let mut failures = 0_u32;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            match self.poll_once(&client).await {
                Ok(()) => failures = 0,
                Err(e) => {
                    eprintln!("[weixin] poll error: {e}");
                    failures += 1;
                    let delay = if failures >= 3 {
                        Duration::from_secs(30)
                    } else {
                        Duration::from_secs(2)
                    };
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() { break; }
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Channel for WeixinChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "weixin"
    }

    fn display_name(&self) -> &'static str {
        "WeChat"
    }

    fn supports_login(&self) -> bool {
        true
    }

    async fn login(&self, force: bool) -> Result<bool> {
        std::fs::create_dir_all(self.state_dir_path())?;
        if !force {
            if !self.config.token.trim().is_empty() {
                return Ok(true);
            }
            let _ = self.load_state();
            if !self
                .inner
                .token
                .lock()
                .expect("weixin token lock")
                .is_empty()
            {
                eprintln!("[weixin] already logged in (token found in state)");
                return Ok(true);
            }
        }
        let client = Client::builder().timeout(Duration::from_secs(60)).build()?;
        eprintln!("[weixin] starting QR code login...");
        let ok = self.qr_login(&client).await?;
        if ok {
            let _ = self.save_state();
            eprintln!("[weixin] login successful — token saved");
        }
        Ok(ok)
    }

    fn setup_instructions(&self) -> &'static str {
        "Weixin (personal WeChat) uses QR code login.\n\
         \n\
         1. Enable the channel in config.json:\n\
         \n\
            \"weixin\": { \"enabled\": true, \"allowFrom\": [\"*\"] }\n\
         \n\
         2. Run: xbot channels login weixin\n\
         \n\
            A QR code URL will be printed. Open it in WeChat to scan.\n\
            The token is saved automatically for future sessions.\n\
         \n\
         3. Alternatively, run `xbot run` — the QR login starts automatically\n\
            if no saved token is found."
    }

    async fn start(&self) -> Result<()> {
        std::fs::create_dir_all(self.state_dir_path())?;

        if !self.config.token.trim().is_empty() {
            *self.inner.token.lock().expect("weixin token lock") =
                self.config.token.trim().to_string();
        } else {
            let _ = self.load_state();
        }

        let timeout = *self
            .inner
            .next_poll_timeout_s
            .lock()
            .expect("weixin poll timeout lock")
            + 10;
        let poll_client = Client::builder()
            .timeout(Duration::from_secs(timeout.max(45)))
            .build()?;

        if self
            .inner
            .token
            .lock()
            .expect("weixin token lock")
            .is_empty()
        {
            eprintln!("[weixin] no token; starting QR login...");
            if !self.qr_login(&poll_client).await? {
                eprintln!("[weixin] QR login failed or cancelled");
                self.base.set_running(false);
                return Ok(());
            }
        }

        let long_poll = Client::builder()
            .timeout(Duration::from_secs(timeout.max(45)))
            .build()?;
        *self.client.lock().await = Some(long_poll);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.shutdown.lock().await = Some(shutdown_tx);
        let channel = Arc::new(self.clone());
        let handle = tokio::spawn(async move {
            channel.run_poll_loop(shutdown_rx).await;
        });
        *self.poll_task.lock().await = Some(handle);
        self.base.set_running(true);
        eprintln!("[weixin] long-poll started");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(true);
        }
        if let Some(task) = self.poll_task.lock().await.take() {
            let _ = task.await;
        }
        *self.client.lock().await = None;
        let _ = self.save_state();
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let client = self
            .client
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("weixin client not running"))?;
        if self.session_pause_remaining() > 0 {
            return Err(anyhow!("weixin session paused after expiry"));
        }
        let ctx = self
            .inner
            .context_tokens
            .lock()
            .expect("weixin ctx lock")
            .get(&msg.chat_id)
            .cloned()
            .unwrap_or_default();
        if ctx.is_empty() {
            return Err(anyhow!(
                "weixin: no context_token for chat_id {}; receive a message first",
                msg.chat_id
            ));
        }
        for chunk in split_message(&msg.content, WEIXIN_MAX_MESSAGE_LEN) {
            let t = chunk.trim();
            if t.is_empty() {
                continue;
            }
            self.send_text(&client, &msg.chat_id, t, &ctx).await?;
        }
        Ok(())
    }
}
