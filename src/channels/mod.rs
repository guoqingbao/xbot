pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod matrix;
pub mod mochat;
pub mod qq;
pub mod slack;
pub mod telegram;
pub mod wecom;
pub mod weixin;
pub mod whatsapp;

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::config::ChannelsConfig;
use crate::storage::{InboundMessage, MessageBus, OutboundMessage};

pub use dingtalk::{DINGTALK_MAX_MESSAGE_LEN, DingTalkChannel, DingTalkConfig};
pub use discord::{DISCORD_MAX_MESSAGE_LEN, DiscordChannel, DiscordConfig};
pub use email::{
    EmailBackend, EmailBackendError, EmailBackendErrorKind, EmailChannel, EmailConfig,
    EmailSearchCriteria, OutgoingEmail, ParsedInboundEmail, RawEmail,
};
pub use feishu::{
    FeishuApi, FeishuChannel, FeishuConfig, FeishuMessageDetails, FeishuResource,
    extract_post_content,
};
pub use matrix::{MATRIX_MAX_MESSAGE_CHARS, MatrixChannel, MatrixConfig};
pub use mochat::{MOCHAT_MAX_MESSAGE_LEN, MochatChannel, MochatConfig};
pub use qq::{QQ_MAX_MESSAGE_LEN, QqChannel, QqConfig};
pub use slack::{SlackApi, SlackChannel, SlackConfig, SlackDmConfig};
pub use telegram::{
    ReplyParameters, TELEGRAM_MAX_MESSAGE_LEN, TELEGRAM_REPLY_CONTEXT_MAX_LEN, TelegramApi,
    TelegramBotIdentity, TelegramChannel, TelegramConfig,
};
pub use wecom::{WECOM_MAX_MESSAGE_LEN, WecomChannel, WecomConfig};
pub use weixin::{WEIXIN_MAX_MESSAGE_LEN, WeixinChannel, WeixinConfig};
pub use whatsapp::{WHATSAPP_MAX_MESSAGE_LEN, WhatsAppChannel, WhatsAppConfig};

#[derive(Debug, Clone)]
pub struct ChannelBase {
    pub config: Value,
    pub bus: MessageBus,
    pub workspace: PathBuf,
    pub transcription_api_key: String,
    running: Arc<Mutex<bool>>,
}

impl ChannelBase {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Self {
        Self {
            config,
            bus,
            workspace,
            transcription_api_key,
            running: Arc::new(Mutex::new(false)),
        }
    }

    pub async fn transcribe_audio(&self, file_path: &Path) -> String {
        if self.transcription_api_key.is_empty() {
            return String::new();
        }
        match crate::providers::transcription::GroqTranscriptionProvider::new(
            self.transcription_api_key.clone(),
        )
        .transcribe(file_path)
        .await
        {
            Ok(text) => text,
            Err(e) => {
                eprintln!("audio transcription failed: {e}");
                String::new()
            }
        }
    }

    pub fn allow_from(&self) -> Vec<String> {
        self.config
            .get("allowFrom")
            .or_else(|| self.config.get("allow_from"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn is_allowed(&self, sender_id: &str) -> bool {
        let allow = self.allow_from();
        if allow.is_empty() {
            return false;
        }
        if allow.iter().any(|item| item == "*") {
            return true;
        }
        allow.iter().any(|item| item == sender_id)
    }

    pub async fn handle_message(
        &self,
        channel_name: &str,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        media: Option<Vec<String>>,
        metadata: Option<BTreeMap<String, Value>>,
        session_key_override: Option<String>,
    ) -> Result<()> {
        if !self.is_allowed(sender_id) {
            return Ok(());
        }
        self.bus
            .publish_inbound(InboundMessage {
                channel: channel_name.to_string(),
                sender_id: sender_id.to_string(),
                chat_id: chat_id.to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: media.unwrap_or_default(),
                metadata: metadata.unwrap_or_default(),
                session_key_override,
            })
            .await?;
        Ok(())
    }

    pub fn set_running(&self, value: bool) {
        *self.running.lock().expect("channel running lock poisoned") = value;
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().expect("channel running lock poisoned")
    }
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn base(&self) -> &ChannelBase;
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str {
        self.name()
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    /// Whether this channel supports interactive login (QR code, OAuth, etc.).
    fn supports_login(&self) -> bool {
        false
    }
    /// Perform interactive login. Returns `true` on success.
    /// The default implementation is a no-op that returns `true`.
    async fn login(&self, _force: bool) -> Result<bool> {
        Ok(true)
    }
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
    async fn send_delta(
        &self,
        _chat_id: &str,
        _delta: &str,
        _metadata: &BTreeMap<String, Value>,
    ) -> Result<()> {
        Ok(())
    }
    /// Human-readable setup instructions for obtaining tokens/keys.
    fn setup_instructions(&self) -> &'static str {
        "Configure this channel via: xbot config --channel"
    }
}

type ChannelFactory =
    Arc<dyn Fn(Value, MessageBus, PathBuf, String) -> Result<Arc<dyn Channel>> + Send + Sync>;

#[derive(Clone)]
pub struct ChannelDescriptor {
    pub name: String,
    pub display_name: String,
    pub default_config: Value,
    pub factory: ChannelFactory,
}

impl ChannelDescriptor {
    pub fn new(
        name: impl Into<String>,
        display_name: impl Into<String>,
        default_config: Value,
        factory: ChannelFactory,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            default_config,
            factory,
        }
    }
}

fn plugin_registry() -> &'static Mutex<BTreeMap<String, ChannelDescriptor>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, ChannelDescriptor>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone)]
pub struct LocalChannel {
    base: ChannelBase,
    sent: Arc<Mutex<Vec<OutboundMessage>>>,
}

impl LocalChannel {
    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Self {
        Self {
            base: ChannelBase::new(config, bus, workspace, transcription_api_key),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn inject_message(
        &self,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        session_key: Option<String>,
    ) -> Result<()> {
        self.base
            .handle_message(
                self.name(),
                sender_id,
                chat_id,
                content,
                None,
                None,
                session_key,
            )
            .await
    }

    pub fn sent_messages(&self) -> Vec<OutboundMessage> {
        self.sent
            .lock()
            .expect("local channel sent lock poisoned")
            .clone()
    }

    pub fn default_config() -> Value {
        json!({"enabled": false, "allowFrom": ["*"]})
    }
}

#[async_trait]
impl Channel for LocalChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "local"
    }

    fn display_name(&self) -> &'static str {
        "Local"
    }

    async fn start(&self) -> Result<()> {
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.sent
            .lock()
            .expect("local channel sent lock poisoned")
            .push(msg);
        Ok(())
    }
}

pub fn discover_channel_names() -> Vec<String> {
    vec![
        "local".to_string(),
        "email".to_string(),
        "feishu".to_string(),
        "slack".to_string(),
        "telegram".to_string(),
        "dingtalk".to_string(),
        "matrix".to_string(),
        "discord".to_string(),
        "whatsapp".to_string(),
        "qq".to_string(),
        "wecom".to_string(),
        "weixin".to_string(),
        "mochat".to_string(),
    ]
}

pub fn register_plugin(descriptor: ChannelDescriptor) {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .insert(descriptor.name.clone(), descriptor);
}

pub fn clear_plugins() {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .clear();
}

pub fn discover_plugins() -> BTreeMap<String, ChannelDescriptor> {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .clone()
}

pub fn discover_all() -> BTreeMap<String, ChannelDescriptor> {
    let mut builtin = BTreeMap::new();
    builtin.insert(
        "local".to_string(),
        ChannelDescriptor::new(
            "local",
            "Local",
            LocalChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(LocalChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )))
            }),
        ),
    );
    builtin.insert(
        "email".to_string(),
        ChannelDescriptor::new(
            "email",
            "Email",
            EmailChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(EmailChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "feishu".to_string(),
        ChannelDescriptor::new(
            "feishu",
            "Feishu",
            FeishuChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(FeishuChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "slack".to_string(),
        ChannelDescriptor::new(
            "slack",
            "Slack",
            SlackChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(SlackChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "telegram".to_string(),
        ChannelDescriptor::new(
            "telegram",
            "Telegram",
            TelegramChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(TelegramChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "dingtalk".to_string(),
        ChannelDescriptor::new(
            "dingtalk",
            "DingTalk",
            DingTalkChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(DingTalkChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "matrix".to_string(),
        ChannelDescriptor::new(
            "matrix",
            "Matrix",
            MatrixChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(MatrixChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "discord".to_string(),
        ChannelDescriptor::new(
            "discord",
            "Discord",
            DiscordChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(DiscordChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "whatsapp".to_string(),
        ChannelDescriptor::new(
            "whatsapp",
            "WhatsApp",
            WhatsAppChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(WhatsAppChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "qq".to_string(),
        ChannelDescriptor::new(
            "qq",
            "QQ",
            QqChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(QqChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "wecom".to_string(),
        ChannelDescriptor::new(
            "wecom",
            "WeCom",
            WecomChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(WecomChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "weixin".to_string(),
        ChannelDescriptor::new(
            "weixin",
            "WeChat",
            WeixinChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(WeixinChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    builtin.insert(
        "mochat".to_string(),
        ChannelDescriptor::new(
            "mochat",
            "Mochat",
            MochatChannel::default_config(),
            Arc::new(|config, bus, workspace, transcription_api_key| {
                Ok(Arc::new(MochatChannel::new(
                    config,
                    bus,
                    workspace,
                    transcription_api_key,
                )?))
            }),
        ),
    );
    let external = discover_plugins();
    let mut merged = external;
    for (name, desc) in builtin {
        merged.insert(name, desc);
    }
    merged
}

const MUTED_TOOL_HINT_BATCH_SIZE: usize = 10;
const SESSION_KEY_METADATA: &str = "_session_key";

#[derive(Debug, Default)]
struct MutedToolHintState {
    in_tool_run: bool,
    batch_count: usize,
    batch_tool_names: BTreeSet<String>,
    emitted_batch_summaries: usize,
}

impl MutedToolHintState {
    fn record_tool_call(&mut self, tool_name: &str) -> bool {
        let started_run = !self.in_tool_run;
        self.in_tool_run = true;
        self.batch_count += 1;
        self.batch_tool_names.insert(tool_name.to_string());
        started_run
    }

    fn should_flush_batch(&self) -> bool {
        self.batch_count >= MUTED_TOOL_HINT_BATCH_SIZE
    }

    fn take_batch_summary(&mut self) -> Option<String> {
        if self.batch_count == 0 {
            return None;
        }
        let summary = format_muted_tool_hint_summary(
            self.batch_count,
            &self.batch_tool_names,
            self.emitted_batch_summaries > 0,
        );
        self.batch_count = 0;
        self.batch_tool_names.clear();
        self.emitted_batch_summaries += 1;
        Some(summary)
    }

    fn finish_tool_run(&mut self) -> Option<String> {
        self.in_tool_run = false;
        let summary = self.take_batch_summary();
        self.emitted_batch_summaries = 0;
        summary
    }

    fn is_idle(&self) -> bool {
        !self.in_tool_run && self.batch_count == 0
    }
}

fn outbound_session_key(msg: &OutboundMessage) -> String {
    msg.metadata
        .get(SESSION_KEY_METADATA)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{}:{}", msg.channel, msg.chat_id))
}

fn tool_name_from_outbound(msg: &OutboundMessage) -> &str {
    msg.metadata
        .get("_tool_name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("tool")
}

fn format_muted_tool_hint_notice(tool_name: &str) -> String {
    format!("Tool call hint is muted, I'm executing a \"{tool_name}\" tool.")
}

fn format_muted_tool_hint_summary(
    tool_count: usize,
    tool_names: &BTreeSet<String>,
    is_additional_batch: bool,
) -> String {
    let label = if tool_count == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    let names = if tool_names.is_empty() {
        "tool".to_string()
    } else {
        tool_names.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    if is_additional_batch {
        format!("I've executed another {tool_count} {label}, including {names}.")
    } else {
        format!("I've executed {tool_count} {label}, including {names}.")
    }
}

fn build_progress_update(msg: &OutboundMessage, content: String) -> OutboundMessage {
    let mut metadata = msg.metadata.clone();
    metadata.insert("_progress".to_string(), Value::Bool(true));
    metadata.remove("_tool_hint");
    metadata.remove("_tool_name");
    metadata.remove("_tool_args");
    OutboundMessage {
        channel: msg.channel.clone(),
        chat_id: msg.chat_id.clone(),
        content,
        reply_to: None,
        media: Vec::new(),
        reasoning_content: None,
        metadata,
    }
}

async fn dispatch_outbound(
    channels: &BTreeMap<String, Arc<dyn Channel>>,
    mut msg: OutboundMessage,
    max_retries: usize,
) {
    msg.reasoning_content = None;
    let channel_name = msg.channel.clone();
    let chat_id = msg.chat_id.clone();
    let content_preview = msg.content.chars().take(200).collect::<String>();

    let is_stream_delta = msg
        .metadata
        .get("_stream_delta")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_stream_end = msg
        .metadata
        .get("_stream_end")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(channel) = channels.get(&msg.channel) {
        if is_stream_delta || is_stream_end {
            if channel.supports_streaming() {
                let _ = channel
                    .send_delta(&msg.chat_id, &msg.content, &msg.metadata)
                    .await;
            }
            if is_stream_end && !msg.metadata.get("_streamed").is_some() {
                return;
            }
            if is_stream_delta {
                return;
            }
        }

        let retries = max_retries.max(1);
        for attempt in 0..retries {
            match channel.send(msg.clone()).await {
                Ok(()) => return,
                Err(err) => {
                    if attempt + 1 < retries {
                        let backoff = std::time::Duration::from_secs(1 << attempt.min(3));
                        tokio::time::sleep(backoff).await;
                    } else {
                        eprintln!(
                            "failed to send outbound message via channel '{channel_name}' to '{chat_id}' after {retries} attempts: {err}"
                        );
                    }
                }
            }
        }
    } else if channel_name == "system" {
        eprintln!("{content_preview}");
    } else {
        eprintln!(
            "dropping outbound message for unknown or disabled channel '{channel_name}' to '{chat_id}'"
        );
    }
}

pub struct ChannelManager {
    pub bus: MessageBus,
    pub channels: BTreeMap<String, Arc<dyn Channel>>,
    pub workspace: PathBuf,
    config: ChannelsConfig,
    dispatch_task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl ChannelManager {
    pub fn new(config: ChannelsConfig, bus: MessageBus, workspace: PathBuf) -> Result<Self> {
        let mut manager = Self {
            bus,
            channels: BTreeMap::new(),
            workspace,
            config,
            dispatch_task: AsyncMutex::new(None),
        };
        manager.init_channels()?;
        Ok(manager)
    }

    fn init_channels(&mut self) -> Result<()> {
        for (name, descriptor) in discover_all() {
            let Some(section) = self.config.section(&name).cloned() else {
                continue;
            };
            let enabled = section
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            let channel = (descriptor.factory)(
                section.clone(),
                self.bus.clone(),
                self.workspace.clone(),
                self.config.transcription_api_key.clone(),
            )?;
            if channel.base().allow_from().is_empty() {
                return Err(anyhow!(
                    "\"{name}\" has empty allowFrom (denies all). Set [\"*\"] or explicit IDs."
                ));
            }
            self.channels.insert(name, channel);
        }
        Ok(())
    }

    pub async fn start_all(&self) -> Result<()> {
        if self.channels.is_empty() {
            return Ok(());
        }
        let mut dispatch = self.dispatch_task.lock().await;
        if dispatch.is_none() {
            let channels = self.channels.clone();
            let bus = self.bus.clone();
            let send_progress = self.config.send_progress;
            let send_tool_hints = self.config.send_tool_hints;
            let send_max_retries = self.config.send_max_retries;
            *dispatch = Some(tokio::spawn(async move {
                let mut muted_tool_hints = BTreeMap::<String, MutedToolHintState>::new();
                loop {
                    let Some(msg) = bus.consume_outbound().await else {
                        break;
                    };
                    let session_key = outbound_session_key(&msg);
                    if msg.metadata.get("_progress").is_some() {
                        let is_tool_hint = msg
                            .metadata
                            .get("_tool_hint")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if is_tool_hint && !send_tool_hints {
                            let tool_name = tool_name_from_outbound(&msg).to_string();
                            let state = muted_tool_hints.entry(session_key).or_default();
                            if state.record_tool_call(&tool_name) {
                                dispatch_outbound(
                                    &channels,
                                    build_progress_update(
                                        &msg,
                                        format_muted_tool_hint_notice(&tool_name),
                                    ),
                                    send_max_retries,
                                )
                                .await;
                            }
                            if state.should_flush_batch() {
                                if let Some(summary) = state.take_batch_summary() {
                                    dispatch_outbound(
                                        &channels,
                                        build_progress_update(&msg, summary),
                                        send_max_retries,
                                    )
                                    .await;
                                }
                            }
                            continue;
                        }
                        if let Some(state) = muted_tool_hints.get_mut(&session_key) {
                            if let Some(summary) = state.finish_tool_run() {
                                dispatch_outbound(
                                    &channels,
                                    build_progress_update(&msg, summary),
                                    send_max_retries,
                                )
                                .await;
                            }
                            if state.is_idle() {
                                muted_tool_hints.remove(&session_key);
                            }
                        }
                        if !is_tool_hint && !send_progress {
                            continue;
                        }
                    } else if let Some(state) = muted_tool_hints.get_mut(&session_key) {
                        if let Some(summary) = state.finish_tool_run() {
                            dispatch_outbound(
                                &channels,
                                build_progress_update(&msg, summary),
                                send_max_retries,
                            )
                            .await;
                        }
                        if state.is_idle() {
                            muted_tool_hints.remove(&session_key);
                        }
                    }
                    dispatch_outbound(&channels, msg, send_max_retries).await;
                }
            }));
        }
        drop(dispatch);
        for channel in self.channels.values() {
            channel.start().await?;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        if let Some(task) = self.dispatch_task.lock().await.take() {
            task.abort();
        }
        for channel in self.channels.values() {
            channel.stop().await?;
        }
        Ok(())
    }

    pub async fn start_channel(&self, name: &str) -> Result<()> {
        let Some(channel) = self.channels.get(name) else {
            return Err(anyhow!("unknown channel '{name}'"));
        };
        channel.start().await
    }

    pub async fn stop_channel(&self, name: &str) -> Result<()> {
        let Some(channel) = self.channels.get(name) else {
            return Err(anyhow!("unknown channel '{name}'"));
        };
        channel.stop().await
    }

    pub fn get_channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels.get(name).cloned()
    }

    pub fn enabled_channels(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }

    pub fn status(&self) -> BTreeMap<String, BTreeMap<String, bool>> {
        self.channels
            .iter()
            .map(|(name, channel)| {
                (
                    name.clone(),
                    BTreeMap::from([
                        ("enabled".to_string(), true),
                        ("running".to_string(), channel.base().is_running()),
                    ]),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::time::{Instant, sleep};

    use super::*;

    fn test_channels_config(send_progress: bool, send_tool_hints: bool) -> ChannelsConfig {
        ChannelsConfig {
            send_progress,
            send_tool_hints,
            transcription_api_key: String::new(),
            send_max_retries: 1,
            sections: BTreeMap::from([(
                "local".to_string(),
                json!({
                    "enabled": true,
                    "allowFrom": ["*"],
                }),
            )]),
        }
    }

    fn local_sent_messages(manager: &ChannelManager) -> Vec<OutboundMessage> {
        manager
            .get_channel("local")
            .and_then(|channel| {
                channel
                    .as_any()
                    .downcast_ref::<LocalChannel>()
                    .map(LocalChannel::sent_messages)
            })
            .expect("local channel available")
    }

    async fn wait_for_sent_count(manager: &ChannelManager, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if local_sent_messages(manager).len() >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for outbound messages"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn tool_hint_message(chat_id: &str, session_key: &str, tool_name: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "local".to_string(),
            chat_id: chat_id.to_string(),
            content: format!("[ {tool_name} ]"),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([
                ("_progress".to_string(), Value::Bool(true)),
                ("_tool_hint".to_string(), Value::Bool(true)),
                (
                    "_tool_name".to_string(),
                    Value::String(tool_name.to_string()),
                ),
                (
                    SESSION_KEY_METADATA.to_string(),
                    Value::String(session_key.to_string()),
                ),
            ]),
        }
    }

    fn final_message(chat_id: &str, session_key: &str, content: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "local".to_string(),
            chat_id: chat_id.to_string(),
            content: content.to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([(
                SESSION_KEY_METADATA.to_string(),
                Value::String(session_key.to_string()),
            )]),
        }
    }

    #[tokio::test]
    async fn muted_tool_hints_flush_summary_before_final_message() {
        let workspace = tempdir().expect("tempdir");
        let bus = MessageBus::new(16);
        let manager = ChannelManager::new(
            test_channels_config(true, false),
            bus.clone(),
            workspace.path().to_path_buf(),
        )
        .expect("channel manager");
        manager.start_all().await.expect("start channels");

        bus.publish_outbound(tool_hint_message("chat-1", "session-1", "read_file"))
            .await
            .expect("publish tool hint");
        bus.publish_outbound(final_message("chat-1", "session-1", "Done."))
            .await
            .expect("publish final message");

        wait_for_sent_count(&manager, 3).await;
        let sent = local_sent_messages(&manager);
        let contents = sent
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            vec![
                "Tool call hint is muted, I'm executing a \"read_file\" tool.",
                "I've executed 1 tool call, including read_file.",
                "Done.",
            ]
        );

        manager.stop_all().await.expect("stop channels");
    }

    #[tokio::test]
    async fn channel_dispatch_strips_reasoning_content() {
        let workspace = tempdir().expect("tempdir");
        let bus = MessageBus::new(16);
        let manager = ChannelManager::new(
            test_channels_config(true, true),
            bus.clone(),
            workspace.path().to_path_buf(),
        )
        .expect("channel manager");
        manager.start_all().await.expect("start channels");

        let mut msg = final_message("chat-reasoning", "session-reasoning", "Visible answer.");
        msg.reasoning_content = Some("private reasoning".to_string());
        bus.publish_outbound(msg)
            .await
            .expect("publish final message");

        wait_for_sent_count(&manager, 1).await;
        let sent = local_sent_messages(&manager);
        assert_eq!(sent[0].content, "Visible answer.");
        assert_eq!(sent[0].reasoning_content, None);

        manager.stop_all().await.expect("stop channels");
    }

    #[tokio::test]
    async fn muted_tool_hints_emit_summary_every_ten_calls() {
        let workspace = tempdir().expect("tempdir");
        let bus = MessageBus::new(48);
        let manager = ChannelManager::new(
            test_channels_config(true, false),
            bus.clone(),
            workspace.path().to_path_buf(),
        )
        .expect("channel manager");
        manager.start_all().await.expect("start channels");

        for tool_name in [
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
            "edit_file",
            "read_file",
            "exec",
        ] {
            bus.publish_outbound(tool_hint_message("chat-2", "session-2", tool_name))
                .await
                .expect("publish tool hint");
        }

        wait_for_sent_count(&manager, 3).await;
        bus.publish_outbound(final_message("chat-2", "session-2", "Complete."))
            .await
            .expect("publish final message");
        wait_for_sent_count(&manager, 4).await;

        let sent = local_sent_messages(&manager);
        let contents = sent
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            vec![
                "Tool call hint is muted, I'm executing a \"read_file\" tool.",
                "I've executed 10 tool calls, including edit_file, exec, read_file.",
                "I've executed another 10 tool calls, including edit_file, exec, read_file.",
                "Complete.",
            ]
        );

        manager.stop_all().await.expect("stop channels");
    }
}
